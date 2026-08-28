// 只读分享链接（FR-H1）：带随机 token 的公开 URL，可设过期与密码。
// 文件直接下载；目录输出 HTML 列表页（只读）。
package server

import (
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"ysync/internal/protocol"
)

func (s *Store) migrateShares() error {
	_, err := s.db.Exec(`CREATE TABLE IF NOT EXISTS shares(
		id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, token TEXT NOT NULL UNIQUE,
		node_id INTEGER NOT NULL, password_hash TEXT NOT NULL DEFAULT '',
		expires_at INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL)`)
	return err
}

// CreateShare 生成分享；hours<=0 表示永不过期。
func (s *Store) CreateShare(userID int64, path string, hours int64, password string) (*protocol.ShareInfo, error) {
	if path == "" || strings.Contains(path, "..") {
		return nil, fmt.Errorf("invalid path")
	}
	n, err := s.nodeByPath(userID, path)
	if err != nil {
		return nil, ErrNotFound
	}
	raw := make([]byte, 12)
	rand.Read(raw)
	token := hex.EncodeToString(raw)
	var pwdHash string
	if password != "" {
		sum := sha256.Sum256([]byte("ysync-share:" + password))
		pwdHash = hex.EncodeToString(sum[:])
	}
	var expires int64
	if hours > 0 {
		expires = time.Now().Unix() + hours*3600
	}
	res, err := s.db.Exec(`INSERT INTO shares(user_id, token, node_id, password_hash, expires_at, created)
		VALUES(?,?,?,?,?,?)`,
		userID, token, n.ID, pwdHash, expires, time.Now().Unix())
	if err != nil {
		return nil, err
	}
	id, _ := res.LastInsertId()
	_ = id
	return &protocol.ShareInfo{Token: token, Path: path, NodeID: n.ID, HasPwd: password != "", ExpiresAt: expires, Created: time.Now().Unix()}, nil
}

func (s *Store) ListShares(userID int64) ([]protocol.ShareInfo, error) {
	rows, err := s.db.Query(`SELECT token, node_id, password_hash!='', expires_at, created FROM shares WHERE user_id=? ORDER BY id DESC`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []protocol.ShareInfo
	for rows.Next() {
		var it protocol.ShareInfo
		if err := rows.Scan(&it.Token, &it.NodeID, &it.HasPwd, &it.ExpiresAt, &it.Created); err != nil {
			return nil, err
		}
		// 补充 path
		if n, err := s.nodeByID(userID, it.NodeID); err == nil {
			it.Path = n.Path
		}
		out = append(out, it)
	}
	return out, rows.Err()
}

func (s *Store) DeleteShare(userID int64, token string) error {
	res, err := s.db.Exec(`DELETE FROM shares WHERE user_id=? AND token=?`, userID, token)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrNotFound
	}
	return nil
}

type shareRow struct {
	userID    int64
	nodeID    int64
	pwdHash   string
	expiresAt int64
}

func (s *Store) getShare(token string) (*shareRow, error) {
	var r shareRow
	err := s.db.QueryRow(`SELECT user_id, node_id, password_hash, expires_at FROM shares WHERE token=?`, token).
		Scan(&r.userID, &r.nodeID, &r.pwdHash, &r.expiresAt)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	if r.expiresAt > 0 && time.Now().Unix() > r.expiresAt {
		return nil, ErrNotFound
	}
	return &r, nil
}

// ---------- 公开 HTTP 处理 ----------

func (s *Server) shareAuthOK(r *shareRow, pwd string) bool {
	if r.pwdHash == "" {
		return true
	}
	sum := sha256.Sum256([]byte("ysync-share:" + pwd))
	return subtle.ConstantTimeCompare([]byte(hex.EncodeToString(sum[:])), []byte(r.pwdHash)) == 1
}

// handlePublicShare GET /s/{token} 及 /s/{token}/{rel...}
func (s *Server) handlePublicShare(w http.ResponseWriter, r *http.Request) {
	token := r.PathValue("token")
	row, err := s.store.getShare(token)
	if err != nil {
		http.Error(w, "链接不存在或已过期", 404)
		return
	}
	if !s.shareAuthOK(row, r.URL.Query().Get("p")) {
		http.Error(w, "需要密码（?p=）", 401)
		return
	}
	root, err := s.store.nodeByID(row.userID, row.nodeID)
	if err != nil {
		http.Error(w, "内容不存在", 404)
		return
	}
	rel := r.PathValue("rel")
	target := root
	if rel != "" {
		if root.Type != protocol.TypeDir {
			http.Error(w, "not found", 404)
			return
		}
		if strings.Contains(rel, "..") {
			http.Error(w, "bad path", 400)
			return
		}
		target, err = s.store.nodeByPath(row.userID, root.Path+"/"+rel)
		if err != nil {
			http.Error(w, "not found", 404)
			return
		}
	}
	if target.Type == protocol.TypeDir {
		s.shareListPage(w, r, token, row, target, rel)
		return
	}
	f, _, err := s.store.Blobs.Open(target.ContentHash)
	if err != nil {
		http.Error(w, "content missing", 404)
		return
	}
	defer f.Close()
	w.Header().Set("Content-Disposition", fmt.Sprintf("attachment; filename*=UTF-8''%s", url_PathEscape(target.Name)))
	http.ServeContent(w, r, target.Name, time.UnixMilli(target.MTime), f)
}

func (s *Server) shareListPage(w http.ResponseWriter, r *http.Request, token string, row *shareRow, dir *protocol.NodeInfo, rel string) {
	base := "/s/" + token
	if rel != "" {
		base += "/" + rel
	}
	kids, _ := s.store.Nodes(row.userID)
	prefix := dir.Path + "/"
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	fmt.Fprintf(w, "<!doctype html><meta charset=utf-8><title>%s</title><h3>%s</h3><ul>", dir.Path, dir.Path)
	if rel != "" {
		parent := base[:strings.LastIndex(base, "/")]
		if parent == "/s" {
			parent = "/s/" + token
		}
		fmt.Fprintf(w, `<li><a href="%s?p=%s">../</a></li>`, parent, r.URL.Query().Get("p"))
	}
	for _, k := range kids {
		if !strings.HasPrefix(k.Path, prefix) {
			continue
		}
		relChild := strings.TrimPrefix(k.Path, prefix)
		if strings.Contains(relChild, "/") {
			continue // 仅当前层
		}
		href := base + "/" + relChild
		label := relChild + "/"
		if k.Type == protocol.TypeFile {
			label = fmt.Sprintf("%s (%.1f KB)", relChild, float64(k.Size)/1024)
		}
		fmt.Fprintf(w, `<li><a href="%s?p=%s">%s</a></li>`, href, r.URL.Query().Get("p"), label)
	}
	fmt.Fprint(w, "</ul>")
	_ = json.Marshal // 保持导入
}

func url_PathEscape(s string) string {
	// 简化转义：仅用于 Content-Disposition
	return strings.ReplaceAll(strings.ReplaceAll(s, "\"", ""), "\n", "")
}

// ---------- 管理 API（需登录）----------

func (s *Server) handleShareCreate(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	var req struct {
		Path     string `json:"path"`
		Hours    int64  `json:"hours"`
		Password string `json:"password"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req); err != nil {
		writeErr(w, 400, "bad request")
		return
	}
	info, err := s.store.CreateShare(uid, req.Path, req.Hours, req.Password)
	if err != nil {
		writeErr(w, 400, err.Error())
		return
	}
	writeJSON(w, 200, info)
}

func (s *Server) handleShareList(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	items, err := s.store.ListShares(uid)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, map[string]any{"shares": items})
}

func (s *Server) handleShareDelete(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	if err := s.store.DeleteShare(uid, r.PathValue("token")); err != nil {
		writeErr(w, 404, "share not found")
		return
	}
	writeJSON(w, 200, map[string]bool{"ok": true})
}
