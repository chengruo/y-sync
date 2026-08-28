// 服务端存储层：SQLite（WAL）承载全部元数据，blob 文件按 SHA-256 内容寻址。
package server

import (
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"database/sql"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"fmt"
	"path"
	"strings"
	"time"

	_ "modernc.org/sqlite"

	"golang.org/x/crypto/argon2"

	"ysync/internal/protocol"
)

var ErrNotFound = errors.New("not found")
var ErrConflict = errors.New("conflict")

// Store 线程安全：连接池限制为 1（SQLite 单写者），WAL 模式。
type Store struct {
	db    *sql.DB
	Blobs *BlobStore
	// 策略配置（SR2）
	MaxVersions        int // 每文件保留版本数（FR-V1），默认 10
	TrashRetentionDays int // 回收站保留天数（FR-V2），默认 30

	// OnOpsCommit ops 事务提交后回调（WebSocket 通知用；可为 nil）
	OnOpsCommit func(userID, deviceID int64, head int64)
}

func OpenStore(dataDir string) (*Store, error) {
	db, err := sql.Open("sqlite", "file:"+path.Join(dataDir, "y-sync.db")+"?_pragma=journal_mode(WAL)&_pragma=busy_timeout(5000)&_pragma=foreign_keys(ON)&_pragma=synchronous(NORMAL)")
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	s := &Store{db: db, Blobs: NewBlobStore(dataDir)}
	if err := s.migrate(); err != nil {
		db.Close()
		return nil, err
	}
	if err := s.migrateShares(); err != nil {
		db.Close()
		return nil, err
	}
	return s, nil
}

func (s *Store) Close() error { return s.db.Close() }

// RawExec 供 backup 等管理操作使用（VACUUM INTO 等）。
func (s *Store) RawExec(query string) (int64, error) {
	res, err := s.db.Exec(query)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

func (s *Store) migrate() error {
	_, err := s.db.Exec(`
CREATE TABLE IF NOT EXISTS users(
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, pass_hash TEXT NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS devices(
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id),
  name TEXT NOT NULL, token_hash TEXT NOT NULL UNIQUE, last_seen INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS nodes(
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL,
  parent_id INTEGER NOT NULL DEFAULT 0, name TEXT NOT NULL, type TEXT NOT NULL,
  content_hash TEXT NOT NULL DEFAULT '', size INTEGER NOT NULL DEFAULT 0,
  mtime INTEGER NOT NULL DEFAULT 0, path TEXT NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_user_path ON nodes(user_id, path);
CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(user_id, parent_id);
CREATE TABLE IF NOT EXISTS blobs(
  hash TEXT PRIMARY KEY, size INTEGER NOT NULL, refcount INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS changes(
  cursor INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, device_id INTEGER NOT NULL DEFAULT 0,
  node_id INTEGER NOT NULL, op TEXT NOT NULL, path TEXT NOT NULL, parent_id INTEGER NOT NULL,
  name TEXT NOT NULL, type TEXT NOT NULL, content_hash TEXT NOT NULL DEFAULT '',
  size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_changes_user ON changes(user_id, cursor);
CREATE TABLE IF NOT EXISTS versions(
  id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, node_id INTEGER NOT NULL,
  path TEXT NOT NULL, content_hash TEXT NOT NULL, size INTEGER NOT NULL,
  mtime INTEGER NOT NULL, created INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_versions ON versions(user_id, node_id, id);
CREATE TABLE IF NOT EXISTS trash(
  id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, orig_path TEXT NOT NULL,
  name TEXT NOT NULL, type TEXT NOT NULL, content_hash TEXT NOT NULL DEFAULT '',
  size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL, deleted_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_trash ON trash(user_id, deleted_at);
`)
	return err
}

// ---------- 用户与设备 ----------

func hashPassword(password string) string {
	salt := make([]byte, 16)
	rand.Read(salt)
	key := argon2.IDKey([]byte(password), salt, 1, 64*1024, 4, 32)
	return fmt.Sprintf("argon2id$%s$%s",
		base64.RawStdEncoding.EncodeToString(salt),
		base64.RawStdEncoding.EncodeToString(key))
}

func verifyPassword(password, stored string) bool {
	parts := strings.Split(stored, "$")
	if len(parts) != 3 || parts[0] != "argon2id" {
		return false
	}
	salt, err := base64.RawStdEncoding.DecodeString(parts[1])
	if err != nil {
		return false
	}
	want, err := base64.RawStdEncoding.DecodeString(parts[2])
	if err != nil {
		return false
	}
	got := argon2.IDKey([]byte(password), salt, 1, 64*1024, 4, uint32(len(want)))
	return subtle.ConstantTimeCompare(got, want) == 1
}

func (s *Store) CreateUser(name, password string) (int64, error) {
	if name == "" || password == "" || strings.ContainsAny(name, " \t/") {
		return 0, fmt.Errorf("invalid user name")
	}
	res, err := s.db.Exec(`INSERT INTO users(name, pass_hash, created) VALUES(?,?,?)`,
		name, hashPassword(password), time.Now().Unix())
	if err != nil {
		return 0, err
	}
	return res.LastInsertId()
}

func (s *Store) Authenticate(name, password string) (int64, error) {
	var id int64
	var hash string
	err := s.db.QueryRow(`SELECT id, pass_hash FROM users WHERE name=?`, name).Scan(&id, &hash)
	if errors.Is(err, sql.ErrNoRows) {
		// 常量时间兜底，避免用户枚举
		verifyPassword(password, "argon2id$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
		return 0, ErrNotFound
	}
	if err != nil {
		return 0, err
	}
	if !verifyPassword(password, hash) {
		return 0, ErrNotFound
	}
	return id, nil
}

// ResetPassword 管理员重置密码；同时吊销该用户全部设备 token。
func (s *Store) ResetPassword(name, password string) error {
	if password == "" {
		return fmt.Errorf("empty password")
	}
	res, err := s.db.Exec(`UPDATE users SET pass_hash=? WHERE name=?`, hashPassword(password), name)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return ErrNotFound
	}
	_, err = s.db.Exec(`DELETE FROM devices WHERE user_id=(SELECT id FROM users WHERE name=?)`, name)
	return err
}

func (s *Store) ListUsers() []string {
	rows, err := s.db.Query(`SELECT name FROM users ORDER BY id`)
	if err != nil {
		return nil
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var n string
		rows.Scan(&n)
		out = append(out, n)
	}
	return out
}

// CreateDevice 登录成功后调用，返回一次性生成的明文 token（服务端只存哈希）。
func (s *Store) CreateDevice(userID int64, deviceName string) (int64, string, error) {
	raw := make([]byte, 32)
	if _, err := rand.Read(raw); err != nil {
		return 0, "", err
	}
	token := hex.EncodeToString(raw)
	th := sha256.Sum256([]byte(token))
	res, err := s.db.Exec(`INSERT INTO devices(user_id, name, token_hash, last_seen) VALUES(?,?,?,?)`,
		userID, deviceName, hex.EncodeToString(th[:]), time.Now().Unix())
	if err != nil {
		return 0, "", err
	}
	id, err := res.LastInsertId()
	return id, token, err
}

// AuthToken 校验 Bearer token，返回 (userID, deviceID)。
func (s *Store) AuthToken(token string) (int64, int64, error) {
	if token == "" {
		return 0, 0, ErrNotFound
	}
	th := sha256.Sum256([]byte(token))
	var uid, did int64
	err := s.db.QueryRow(`SELECT user_id, id FROM devices WHERE token_hash=?`,
		hex.EncodeToString(th[:])).Scan(&uid, &did)
	if err != nil {
		return 0, 0, ErrNotFound
	}
	s.db.Exec(`UPDATE devices SET last_seen=? WHERE id=?`, time.Now().Unix(), did)
	return uid, did, nil
}

// ---------- 节点树 ----------

func joinPath(parent, name string) string {
	if parent == "" {
		return name
	}
	return parent + "/" + name
}

func (s *Store) nodePath(userID, nodeID int64) (string, error) {
	if nodeID == 0 {
		return "", nil // 用户根
	}
	var p string
	err := s.db.QueryRow(`SELECT path FROM nodes WHERE user_id=? AND id=?`, userID, nodeID).Scan(&p)
	if errors.Is(err, sql.ErrNoRows) {
		return "", ErrNotFound
	}
	return p, err
}

// tx 系列辅助：在 ApplyOps 事务内使用（连接池为 1，事务内绝不能再走 s.db，
// 否则自锁死锁）。
func nodePathTx(tx *sql.Tx, userID, nodeID int64) (string, error) {
	if nodeID == 0 {
		return "", nil
	}
	var p string
	err := tx.QueryRow(`SELECT path FROM nodes WHERE user_id=? AND id=?`, userID, nodeID).Scan(&p)
	if errors.Is(err, sql.ErrNoRows) {
		return "", ErrNotFound
	}
	return p, err
}

func nodeByIDTx(tx *sql.Tx, userID, nodeID int64) (*protocol.NodeInfo, error) {
	if nodeID == 0 {
		return nil, ErrNotFound
	}
	var n protocol.NodeInfo
	var ch string
	err := tx.QueryRow(`SELECT id, parent_id, name, type, path, size, mtime, content_hash
		FROM nodes WHERE user_id=? AND id=?`, userID, nodeID).
		Scan(&n.ID, &n.ParentID, &n.Name, &n.Type, &n.Path, &n.Size, &n.MTime, &ch)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	n.ContentHash = ch
	return &n, nil
}

func nodeByPathTx(tx *sql.Tx, userID int64, p string) (*protocol.NodeInfo, error) {
	var n protocol.NodeInfo
	var ch string
	err := tx.QueryRow(`SELECT id, parent_id, name, type, path, size, mtime, content_hash
		FROM nodes WHERE user_id=? AND path=?`, userID, p).
		Scan(&n.ID, &n.ParentID, &n.Name, &n.Type, &n.Path, &n.Size, &n.MTime, &ch)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	n.ContentHash = ch
	return &n, nil
}

func (s *Store) nodeByID(userID, nodeID int64) (*protocol.NodeInfo, error) {
	if nodeID == 0 {
		return nil, ErrNotFound
	}
	var n protocol.NodeInfo
	var ch string
	err := s.db.QueryRow(`SELECT id, parent_id, name, type, path, size, mtime, content_hash
		FROM nodes WHERE user_id=? AND id=?`, userID, nodeID).
		Scan(&n.ID, &n.ParentID, &n.Name, &n.Type, &n.Path, &n.Size, &n.MTime, &ch)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	n.ContentHash = ch
	return &n, nil
}

func (s *Store) nodeByPath(userID int64, p string) (*protocol.NodeInfo, error) {
	var n protocol.NodeInfo
	var ch string
	err := s.db.QueryRow(`SELECT id, parent_id, name, type, path, size, mtime, content_hash
		FROM nodes WHERE user_id=? AND path=?`, userID, p).
		Scan(&n.ID, &n.ParentID, &n.Name, &n.Type, &n.Path, &n.Size, &n.MTime, &ch)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	n.ContentHash = ch
	return &n, nil
}

// journalChange 必须在事务内调用，返回新 cursor。
func journalChange(tx *sql.Tx, userID, deviceID int64, n *protocol.NodeInfo, op string) (int64, error) {
	res, err := tx.Exec(`INSERT INTO changes(user_id, device_id, node_id, op, path, parent_id, name, type, content_hash, size, mtime)
		VALUES(?,?,?,?,?,?,?,?,?,?,?)`,
		userID, deviceID, n.ID, op, n.Path, n.ParentID, n.Name, n.Type, n.ContentHash, n.Size, n.MTime)
	if err != nil {
		return 0, err
	}
	return res.LastInsertId()
}

// txCursor 返回事务内最新 journal cursor（用于结果回传）。
func txCursor(tx *sql.Tx) (int64, error) {
	var c sql.NullInt64
	err := tx.QueryRow(`SELECT MAX(cursor) FROM changes`).Scan(&c)
	return c.Int64, err
}

// listDescendants 返回 path 目录下的全部后代（不含自身），必须在事务内。
func listDescendants(tx *sql.Tx, userID int64, dirPath string) ([]protocol.NodeInfo, error) {
	rows, err := tx.Query(`SELECT id, parent_id, name, type, path, size, mtime, content_hash
		FROM nodes WHERE user_id=? AND path>? AND path<? ORDER BY path`,
		userID, dirPath+"/", dirPath+"/\U0010FFFF")
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []protocol.NodeInfo
	for rows.Next() {
		var n protocol.NodeInfo
		var ch string
		if err := rows.Scan(&n.ID, &n.ParentID, &n.Name, &n.Type, &n.Path, &n.Size, &n.MTime, &ch); err != nil {
			return nil, err
		}
		n.ContentHash = ch
		out = append(out, n)
	}
	return out, rows.Err()
}

// decRef 必须在事务内。
func decRef(tx *sql.Tx, hash string) error {
	if hash == "" {
		return nil
	}
	_, err := tx.Exec(`UPDATE blobs SET refcount=refcount-1 WHERE hash=? AND refcount>0`, hash)
	return err
}

func incRef(tx *sql.Tx, hash string) error {
	if hash == "" {
		return nil
	}
	_, err := tx.Exec(`UPDATE blobs SET refcount=refcount+1 WHERE hash=?`, hash)
	return err
}

// unlinkNodeLocked 递归删除节点并逐节点写日志（后代在前），必须在事务内。
func unlinkNodeLocked(tx *sql.Tx, userID, deviceID int64, n *protocol.NodeInfo) error {
	if n.Type == protocol.TypeDir {
		kids, err := listDescendants(tx, userID, n.Path)
		if err != nil {
			return err
		}
		// 后代按深度倒序：先文件后目录，日志顺序保持树形一致
		for i := len(kids) - 1; i >= 0; i-- {
			k := kids[i]
			if _, err := tx.Exec(`DELETE FROM nodes WHERE id=?`, k.ID); err != nil {
				return err
			}
			if _, err := journalChange(tx, userID, deviceID, &k, protocol.OpUnlink); err != nil {
				return err
			}
		}
	}
	// FR-S5/FR-V2：内容移入回收站（引用转移，不 decRef；后代文件已在上方逐条入站）
	if err := trashNodeLocked(tx, userID, n, time.Now().Unix()); err != nil {
		return err
	}
	if _, err := tx.Exec(`DELETE FROM nodes WHERE id=?`, n.ID); err != nil {
		return err
	}
	_, jerr := journalChange(tx, userID, deviceID, n, protocol.OpUnlink)
	return jerr
}

// ApplyOps 批量、按序、原子应用元数据操作，返回每条结果。
// 所有 journal 条目归属调用设备（deviceID），客户端凭此跳过自己的变更重放。
func (s *Store) ApplyOps(userID, deviceID int64, ops []protocol.Op) ([]protocol.OpResult, error) {
	results := make([]protocol.OpResult, len(ops))
	tx, err := s.db.Begin()
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()

	for i := range ops {
		op := ops[i]
		nodeID, cur, opErr := s.applyOp(tx, userID, deviceID, &op)
		results[i].Cursor = cur
		results[i] = protocol.OpResult{Ok: opErr == nil, NodeID: nodeID}
		if opErr != nil {
			results[i].Error = opErr.Error()
		}
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	if s.OnOpsCommit != nil && len(ops) > 0 {
		if head, err := s.HeadCursor(userID); err == nil {
			go s.OnOpsCommit(userID, deviceID, head)
		}
	}
	return results, nil
}

func (s *Store) applyOp(tx *sql.Tx, userID, deviceID int64, op *protocol.Op) (int64, int64, error) {
	switch op.Op {
	case protocol.OpMkdir:
		parentPath, err := nodePathTx(tx, userID, op.ParentID)
		if err != nil {
			return 0, 0, fmt.Errorf("parent not found")
		}
		if !validName(op.Name) {
			return 0, 0, fmt.Errorf("invalid name")
		}
		p := joinPath(parentPath, op.Name)
		if existing, _ := nodeByPathTx(tx, userID, p); existing != nil {
			if existing.Type != protocol.TypeDir {
				return 0, 0, fmt.Errorf("path exists as file")
			}
			cur, _ := txCursor(tx)
			return existing.ID, cur, nil // 幂等
		}
		res, err := tx.Exec(`INSERT INTO nodes(user_id, parent_id, name, type, path, mtime) VALUES(?,?,?,?,?,?)`,
			userID, op.ParentID, op.Name, protocol.TypeDir, p, time.Now().UnixMilli())
		if err != nil {
			return 0, 0, err
		}
		id, _ := res.LastInsertId()
		n := &protocol.NodeInfo{ID: id, ParentID: op.ParentID, Name: op.Name, Type: protocol.TypeDir, Path: p}
		cur, err := journalChange(tx, userID, deviceID, n, protocol.OpMkdir)
		return id, cur, err

	case protocol.OpPut:
		if op.ContentHash == "" {
			return 0, 0, fmt.Errorf("content_hash required")
		}
		var bsize int64
		err := tx.QueryRow(`SELECT size FROM blobs WHERE hash=?`, op.ContentHash).Scan(&bsize)
		if errors.Is(err, sql.ErrNoRows) {
			return 0, 0, fmt.Errorf("content not uploaded yet")
		}
		if err != nil {
			return 0, 0, err
		}
		size := op.Size
		if size == 0 {
			size = bsize
		}
		if op.NodeID > 0 {
			n, err := nodeByIDTx(tx, userID, op.NodeID)
			if err != nil {
				return 0, 0, fmt.Errorf("node not found")
			}
			if n.Type != protocol.TypeFile {
				return 0, 0, fmt.Errorf("not a file")
			}
			if n.ContentHash != op.ContentHash {
				// FR-V1：覆盖前保存旧版本（引用从节点转移到版本行）
				if err := saveVersion(tx, userID, n.ID, n, s.MaxVersions); err != nil {
					return 0, 0, err
				}
			}
			if _, err := tx.Exec(`UPDATE nodes SET content_hash=?, size=?, mtime=? WHERE id=?`,
				op.ContentHash, size, op.MTime, n.ID); err != nil {
				return 0, 0, err
			}
			if err := incRef(tx, op.ContentHash); err != nil {
				return 0, 0, err
			}
			updated := *n
			updated.ContentHash, updated.Size, updated.MTime = op.ContentHash, size, op.MTime
			cur, err := journalChange(tx, userID, deviceID, &updated, protocol.OpPut)
			return n.ID, cur, err
		}
		parentPath, err := nodePathTx(tx, userID, op.ParentID)
		if err != nil {
			return 0, 0, fmt.Errorf("parent not found")
		}
		if !validName(op.Name) {
			return 0, 0, fmt.Errorf("invalid name")
		}
		p := joinPath(parentPath, op.Name)
		if existing, _ := nodeByPathTx(tx, userID, p); existing != nil {
			// 并发覆盖：旧节点先下线（目录则递归），再落新节点
			if err := unlinkNodeLocked(tx, userID, deviceID, existing); err != nil {
				return 0, 0, err
			}
		}
		res, err := tx.Exec(`INSERT INTO nodes(user_id, parent_id, name, type, content_hash, size, mtime, path)
			VALUES(?,?,?,?,?,?,?,?)`,
			userID, op.ParentID, op.Name, protocol.TypeFile, op.ContentHash, size, op.MTime, p)
		if err != nil {
			return 0, 0, err
		}
		id, _ := res.LastInsertId()
		if err := incRef(tx, op.ContentHash); err != nil {
			return 0, 0, err
		}
		n := &protocol.NodeInfo{ID: id, ParentID: op.ParentID, Name: op.Name, Type: protocol.TypeFile,
			Path: p, ContentHash: op.ContentHash, Size: size, MTime: op.MTime}
		cur, err := journalChange(tx, userID, deviceID, n, protocol.OpPut)
		return id, cur, err

	case protocol.OpMove:
		n, err := nodeByIDTx(tx, userID, op.NodeID)
		if err != nil {
			return 0, 0, fmt.Errorf("node not found")
		}
		parentPath, err := nodePathTx(tx, userID, op.ParentID)
		if err != nil {
			return 0, 0, fmt.Errorf("parent not found")
		}
		if !validName(op.Name) {
			return 0, 0, fmt.Errorf("invalid name")
		}
		newPath := joinPath(parentPath, op.Name)
		if newPath == n.Path {
			cur, _ := txCursor(tx)
			return n.ID, cur, nil // 幂等
		}
		if strings.HasPrefix(n.Path+"/", newPath+"/") {
			return 0, 0, fmt.Errorf("cannot move dir into itself")
		}
		if existing, _ := nodeByPathTx(tx, userID, newPath); existing != nil {
			if err := unlinkNodeLocked(tx, userID, deviceID, existing); err != nil {
				return 0, 0, err
			}
		}
		oldPath := n.Path
		if _, err := tx.Exec(`UPDATE nodes SET parent_id=?, name=?, path=? WHERE id=?`,
			op.ParentID, op.Name, newPath, n.ID); err != nil {
			return 0, 0, err
		}
		moved := *n
		moved.ParentID, moved.Name, moved.Path = op.ParentID, op.Name, newPath
		cur, err := journalChange(tx, userID, deviceID, &moved, protocol.OpMove)
		if err != nil {
			return 0, 0, err
		}
		if n.Type == protocol.TypeDir {
			kids, err := listDescendants(tx, userID, oldPath)
			if err != nil {
				return 0, 0, err
			}
			for i := range kids {
				k := kids[i]
				k.Path = newPath + strings.TrimPrefix(k.Path, oldPath)
				if _, err := tx.Exec(`UPDATE nodes SET path=? WHERE id=?`, k.Path, k.ID); err != nil {
					return 0, 0, err
				}
				if _, err := journalChange(tx, userID, deviceID, &k, protocol.OpMove); err != nil {
					return 0, 0, err
				}
			}
		}
		return n.ID, cur, nil

	case protocol.OpUnlink:
		n, err := nodeByIDTx(tx, userID, op.NodeID)
		if err != nil {
			cur, _ := txCursor(tx)
			return op.NodeID, cur, nil // 幂等：已不存在视为成功
		}
		cur, _ := txCursor(tx)
		return op.NodeID, cur, unlinkNodeLocked(tx, userID, deviceID, n)
	}
	return 0, 0, fmt.Errorf("unknown op %q", op.Op)
}

func validName(name string) bool {
	return name != "" && name != "." && name != ".." && !strings.Contains(name, "/")
}

// Nodes 全量列举用户文件树。
func (s *Store) Nodes(userID int64) ([]protocol.NodeInfo, error) {
	rows, err := s.db.Query(`SELECT id, parent_id, name, type, path, size, mtime, content_hash
		FROM nodes WHERE user_id=? ORDER BY path`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []protocol.NodeInfo{}
	for rows.Next() {
		var n protocol.NodeInfo
		var ch string
		if err := rows.Scan(&n.ID, &n.ParentID, &n.Name, &n.Type, &n.Path, &n.Size, &n.MTime, &ch); err != nil {
			return nil, err
		}
		n.ContentHash = ch
		out = append(out, n)
	}
	return out, rows.Err()
}

// HeadCursor 当前变更日志 head。
func (s *Store) HeadCursor(userID int64) (int64, error) {
	var c sql.NullInt64
	err := s.db.QueryRow(`SELECT MAX(cursor) FROM changes WHERE user_id=?`, userID).Scan(&c)
	if err != nil {
		return 0, err
	}
	return c.Int64, nil
}

// Changes 增量拉取 cursor 之后的变更；rootID>0 时按子树路径前缀过滤。
func (s *Store) Changes(userID, since, limit int64, rootID int64) ([]protocol.Change, int64, error) {
	rootPath := ""
	if rootID > 0 {
		n, err := s.nodeByID(userID, rootID)
		if err != nil {
			return nil, 0, err
		}
		rootPath = n.Path
	}
	if limit <= 0 || limit > 5000 {
		limit = 1000
	}
	var rows *sql.Rows
	var err error
	if rootPath == "" {
		rows, err = s.db.Query(`SELECT cursor, device_id, node_id, op, path, parent_id, name, type, content_hash, size, mtime
			FROM changes WHERE user_id=? AND cursor>? ORDER BY cursor LIMIT ?`, userID, since, limit)
	} else {
		rows, err = s.db.Query(`SELECT cursor, device_id, node_id, op, path, parent_id, name, type, content_hash, size, mtime
			FROM changes WHERE user_id=? AND cursor>? AND (path=? OR (path>? AND path<?)) ORDER BY cursor LIMIT ?`,
			userID, since, rootPath, rootPath+"/", rootPath+"/\U0010FFFF", limit)
	}
	if err != nil {
		return nil, 0, err
	}
	defer rows.Close()
	out := []protocol.Change{}
	for rows.Next() {
		var c protocol.Change
		var ch string
		if err := rows.Scan(&c.Cursor, &c.DeviceID, &c.NodeID, &c.Op, &c.Path, &c.ParentID, &c.Name,
			&c.Type, &ch, &c.Size, &c.MTime); err != nil {
			return nil, 0, err
		}
		c.ContentHash = ch
		out = append(out, c)
	}
	head, err := s.HeadCursor(userID)
	if err != nil {
		return nil, 0, err
	}
	return out, head, rows.Err()
}
