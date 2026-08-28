// 回收站与文件版本（FR-V1~V3、FR-S5）。
// 引用计数语义：refcount = 存活节点 + 版本行 + 回收站文件行 的引用总数。
// 节点内容进版本/回收站时"引用转移"（不减 refcount）；裁剪/彻底删除时才 decRef。
package server

import (
	"database/sql"
	"errors"
	"fmt"
	"strings"
	"time"

	"ysync/internal/protocol"
)

// saveVersion 在覆盖节点内容前保存旧版本（引用转移，不动 refcount），并按上限裁剪。
// 必须在事务内调用。
func saveVersion(tx *sql.Tx, userID, nodeID int64, n *protocol.NodeInfo, maxVersions int) error {
	if n.ContentHash == "" || n.Type != protocol.TypeFile {
		return nil
	}
	_, err := tx.Exec(`INSERT INTO versions(user_id, node_id, path, content_hash, size, mtime, created)
		VALUES(?,?,?,?,?,?,?)`,
		userID, nodeID, n.Path, n.ContentHash, n.Size, n.MTime, time.Now().Unix())
	if err != nil {
		return err
	}
	// 裁剪：仅保留最近 maxVersions 个（按 created/id 倒序）
	rows, err := tx.Query(`SELECT id, content_hash FROM versions WHERE user_id=? AND node_id=?
		ORDER BY id DESC LIMIT -1 OFFSET ?`, userID, nodeID, maxVersions)
	if err != nil {
		return err
	}
	var pruneIDs []int64
	var pruneHashes []string
	for rows.Next() {
		var id int64
		var h string
		if err := rows.Scan(&id, &h); err != nil {
			rows.Close()
			return err
		}
		pruneIDs = append(pruneIDs, id)
		pruneHashes = append(pruneHashes, h)
	}
	rows.Close()
	for i := range pruneIDs {
		if _, err := tx.Exec(`DELETE FROM versions WHERE id=?`, pruneIDs[i]); err != nil {
			return err
		}
		if err := decRef(tx, pruneHashes[i]); err != nil {
			return err
		}
	}
	return nil
}

// trashNodeLocked 将节点移入回收站（引用转移，不 decRef），必须在事务内。
// 目录会将其所有后代文件逐条入站（保留各自原始路径），目录自身也入站（type=dir）。
func trashNodeLocked(tx *sql.Tx, userID int64, n *protocol.NodeInfo, now int64) error {
	if n.Type == protocol.TypeDir {
		kids, err := listDescendants(tx, userID, n.Path)
		if err != nil {
			return err
		}
		for i := range kids {
			k := kids[i]
			if k.Type == protocol.TypeFile && k.ContentHash != "" {
				if _, err := tx.Exec(`INSERT INTO trash(user_id, orig_path, name, type, content_hash, size, mtime, deleted_at)
					VALUES(?,?,?,?,?,?,?,?)`,
					userID, k.Path, k.Name, protocol.TypeFile, k.ContentHash, k.Size, k.MTime, now); err != nil {
					return err
				}
			}
		}
	}
	_, err := tx.Exec(`INSERT INTO trash(user_id, orig_path, name, type, content_hash, size, mtime, deleted_at)
		VALUES(?,?,?,?,?,?,?,?)`,
		userID, n.Path, n.Name, n.Type, n.ContentHash, n.Size, n.MTime, now)
	return err
}

// ListTrash 回收站列表；顺带惰性清理过期条目。
func (s *Store) ListTrash(userID int64) ([]protocol.TrashItem, error) {
	if s.TrashRetentionDays > 0 {
		cutoff := time.Now().Unix() - int64(s.TrashRetentionDays)*86400
		s.PurgeTrashBefore(userID, cutoff)
	}
	rows, err := s.db.Query(`SELECT id, orig_path, name, type, content_hash, size, mtime, deleted_at
		FROM trash WHERE user_id=? ORDER BY deleted_at DESC, id DESC`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []protocol.TrashItem{}
	for rows.Next() {
		var it protocol.TrashItem
		var h string
		if err := rows.Scan(&it.ID, &it.OrigPath, &it.Name, &it.Type, &h, &it.Size, &it.MTime, &it.DeletedAt); err != nil {
			return nil, err
		}
		it.Hash = h
		out = append(out, it)
	}
	return out, rows.Err()
}

// PurgeTrashBefore 彻底删除 cutoff 之前的条目（decRef）。
func (s *Store) PurgeTrashBefore(userID, cutoff int64) (int64, error) {
	tx, err := s.db.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	rows, err := tx.Query(`SELECT id, type, content_hash FROM trash WHERE user_id=? AND deleted_at<?`,
		userID, cutoff)
	if err != nil {
		return 0, err
	}
	type entry struct {
		id   int64
		typ  string
		hash string
	}
	var entries []entry
	for rows.Next() {
		var e entry
		if err := rows.Scan(&e.id, &e.typ, &e.hash); err != nil {
			rows.Close()
			return 0, err
		}
		entries = append(entries, e)
	}
	rows.Close()
	for _, e := range entries {
		if e.typ == protocol.TypeFile && e.hash != "" {
			if err := decRef(tx, e.hash); err != nil {
				return 0, err
			}
		}
		if _, err := tx.Exec(`DELETE FROM trash WHERE id=?`, e.id); err != nil {
			return 0, err
		}
	}
	return int64(len(entries)), tx.Commit()
}

// PurgeAllTrashBefore 跨用户清理（GC 用）：先收集条目，逐条 decRef，再删除。
func (s *Store) PurgeAllTrashBefore(cutoff int64) (int64, error) {
	rows, err := s.db.Query(`SELECT id, type, content_hash FROM trash WHERE deleted_at<?`, cutoff)
	if err != nil {
		return 0, err
	}
	type entry struct {
		id   int64
		typ  string
		hash string
	}
	var entries []entry
	for rows.Next() {
		var e entry
		if err := rows.Scan(&e.id, &e.typ, &e.hash); err != nil {
			rows.Close()
			return 0, err
		}
		entries = append(entries, e)
	}
	rows.Close()
	if len(entries) == 0 {
		return 0, nil
	}
	tx, err := s.db.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	for _, e := range entries {
		if e.typ == protocol.TypeFile && e.hash != "" {
			if err := decRef(tx, e.hash); err != nil {
				return 0, err
			}
		}
		if _, err := tx.Exec(`DELETE FROM trash WHERE id=?`, e.id); err != nil {
			return 0, err
		}
	}
	return int64(len(entries)), tx.Commit()
}

// RestoreTrash 从回收站恢复条目（FR-V2）。目标路径被占用时使用 "name (restored).ext"。
// 中间缺失的父目录会重建并写入变更日志，保证其他设备同步到目录结构。
func (s *Store) RestoreTrash(userID, trashID int64) (*protocol.NodeInfo, error) {
	tx, err := s.db.Begin()
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	var it protocol.TrashItem
	var h string
	err = tx.QueryRow(`SELECT orig_path, name, type, content_hash, size, mtime FROM trash WHERE id=? AND user_id=?`,
		trashID, userID).Scan(&it.OrigPath, &it.Name, &it.Type, &h, &it.Size, &it.MTime)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	it.Hash = h

	// 目标路径处理
	path := it.OrigPath
	if existing, _ := nodeByPathTx(tx, userID, path); existing != nil {
		dir, name := splitServerPath(path)
		ext := ""
		base := name
		if i := strings.LastIndex(name, "."); i > 0 {
			ext = name[i:]
			base = name[:i]
		}
		path = joinPath(dir, base+" (restored)"+ext)
		if existing, _ := nodeByPathTx(tx, userID, path); existing != nil {
			return nil, fmt.Errorf("restore target occupied")
		}
	}

	// 重建缺失的父目录
	if err := ensureParentsLocked(tx, userID, path); err != nil {
		return nil, err
	}

	now := time.Now().UnixMilli()
	var n *protocol.NodeInfo
	if it.Type == protocol.TypeFile {
		res, err := tx.Exec(`INSERT INTO nodes(user_id, parent_id, name, type, content_hash, size, mtime, path)
			VALUES(?,?,?,?,?,?,?,?)`,
			userID, 0, serverBaseName(path), protocol.TypeFile, it.Hash, it.Size, it.MTime, path)
		if err != nil {
			return nil, err
		}
		id, _ := res.LastInsertId()
		if err := incRef(tx, it.Hash); err != nil {
			return nil, err
		}
		n = &protocol.NodeInfo{ID: id, Name: serverBaseName(path), Type: protocol.TypeFile,
			Path: path, ContentHash: it.Hash, Size: it.Size, MTime: it.MTime}
		if _, err := journalChange(tx, userID, 0, n, protocol.OpPut); err != nil {
			return nil, err
		}
	} else {
		res, err := tx.Exec(`INSERT INTO nodes(user_id, parent_id, name, type, path, mtime) VALUES(?,?,?,?,?,?)`,
			userID, 0, serverBaseName(path), protocol.TypeDir, path, now)
		if err != nil {
			return nil, err
		}
		id, _ := res.LastInsertId()
		n = &protocol.NodeInfo{ID: id, Name: serverBaseName(path), Type: protocol.TypeDir, Path: path, MTime: now}
		if _, err := journalChange(tx, userID, 0, n, protocol.OpMkdir); err != nil {
			return nil, err
		}
	}
	// parent_id 修正（ensureParents 已建好父链）
	if dir, _ := splitServerPath(path); dir != "" {
		if p, _ := nodeByPathTx(tx, userID, dir); p != nil {
			if _, err := tx.Exec(`UPDATE nodes SET parent_id=? WHERE id=?`, p.ID, n.ID); err != nil {
				return nil, err
			}
		}
	}
	if _, err := tx.Exec(`DELETE FROM trash WHERE id=?`, trashID); err != nil {
		return nil, err
	}
	return n, tx.Commit()
}

// DeleteTrash 彻底删除单条回收站条目（decRef）。
func (s *Store) DeleteTrash(userID, trashID int64) error {
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	var typ, hash string
	err = tx.QueryRow(`SELECT type, content_hash FROM trash WHERE id=? AND user_id=?`,
		trashID, userID).Scan(&typ, &hash)
	if errors.Is(err, sql.ErrNoRows) {
		return ErrNotFound
	}
	if err != nil {
		return err
	}
	if typ == protocol.TypeFile && hash != "" {
		if err := decRef(tx, hash); err != nil {
			return err
		}
	}
	if _, err := tx.Exec(`DELETE FROM trash WHERE id=?`, trashID); err != nil {
		return err
	}
	return tx.Commit()
}

// ListVersions 文件历史版本（新→旧）。
func (s *Store) ListVersions(userID, nodeID int64) ([]protocol.VersionItem, error) {
	if _, err := s.nodeByID(userID, nodeID); err != nil {
		return nil, ErrNotFound
	}
	rows, err := s.db.Query(`SELECT id, node_id, path, content_hash, size, mtime, created
		FROM versions WHERE user_id=? AND node_id=? ORDER BY id DESC`, userID, nodeID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []protocol.VersionItem{}
	for rows.Next() {
		var v protocol.VersionItem
		if err := rows.Scan(&v.ID, &v.NodeID, &v.Path, &v.Hash, &v.Size, &v.MTime, &v.Created); err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, rows.Err()
}

// VersionContent 校验归属并返回版本内容哈希（供内容下载）。
func (s *Store) VersionContent(userID, versionID int64) (string, error) {
	var hash string
	err := s.db.QueryRow(`SELECT content_hash FROM versions WHERE id=? AND user_id=?`,
		versionID, userID).Scan(&hash)
	if errors.Is(err, sql.ErrNoRows) {
		return "", ErrNotFound
	}
	return hash, err
}

// ---------- 辅助 ----------

func splitServerPath(p string) (string, string) {
	i := strings.LastIndex(p, "/")
	if i < 0 {
		return "", p
	}
	return p[:i], p[i+1:]
}

func serverBaseName(p string) string {
	_, n := splitServerPath(p)
	return n
}

// ensureParentsLocked 自顶向下创建 path 的全部父目录并写日志。必须在事务内。
func ensureParentsLocked(tx *sql.Tx, userID int64, path string) error {
	parts := strings.Split(path, "/")
	cur := ""
	for i := 0; i < len(parts)-1; i++ {
		cur = joinPath(cur, parts[i])
		if existing, _ := nodeByPathTx(tx, userID, cur); existing != nil {
			if existing.Type != protocol.TypeDir {
				return fmt.Errorf("parent %q is a file", cur)
			}
			continue
		}
		parentID := int64(0)
		if dir, _ := splitServerPath(cur); dir != "" {
			if p, _ := nodeByPathTx(tx, userID, dir); p != nil {
				parentID = p.ID
			}
		}
		res, err := tx.Exec(`INSERT INTO nodes(user_id, parent_id, name, type, path, mtime) VALUES(?,?,?,?,?,?)`,
			userID, parentID, parts[i], protocol.TypeDir, cur, time.Now().UnixMilli())
		if err != nil {
			return err
		}
		id, _ := res.LastInsertId()
		n := &protocol.NodeInfo{ID: id, ParentID: parentID, Name: parts[i], Type: protocol.TypeDir, Path: cur}
		if _, err := journalChange(tx, userID, 0, n, protocol.OpMkdir); err != nil {
			return err
		}
	}
	return nil
}

// GC 清理过期回收站条目与无引用 blob 行（引用归零的 blob 文件一并删除）。
func (s *Store) GC() (purged int64, removedBlobs int64, err error) {
	var cutoff int64
	if s.TrashRetentionDays > 0 {
		cutoff = time.Now().Unix() - int64(s.TrashRetentionDays)*86400
	} else {
		cutoff = time.Now().Unix()
	}
	purged, err = s.PurgeAllTrashBefore(cutoff)
	if err != nil {
		return purged, 0, err
	}
	rows, err := s.db.Query(`SELECT hash FROM blobs WHERE refcount<=0`)
	if err != nil {
		return purged, 0, err
	}
	var hashes []string
	for rows.Next() {
		var h string
		if err := rows.Scan(&h); err != nil {
			rows.Close()
			return purged, 0, err
		}
		hashes = append(hashes, h)
	}
	rows.Close()
	for _, h := range hashes {
		if _, err := s.db.Exec(`DELETE FROM blobs WHERE hash=? AND refcount<=0`, h); err != nil {
			return purged, 0, err
		}
		s.Blobs.Remove(h)
	}
	return purged, int64(len(hashes)), nil
}
