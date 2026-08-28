// 客户端本地状态库（FR-C4）：每文件夹一份 SQLite，记录上次同步基线与 cursor，
// 使对账无需重新哈希全部文件。
package client

import (
	"database/sql"
	"os"
	"path/filepath"
	"strconv"

	_ "modernc.org/sqlite"
)

type Rec struct {
	NodeID int64
	Hash   string
	Size   int64
	MTime  int64
	Type   string // protocol.TypeFile / TypeDir
}

type State struct {
	db *sql.DB
}

func StatePath(localPath string) string {
	return filepath.Join(localPath, ".y-sync", "state.db")
}

func OpenState(localPath string) (*State, error) {
	if err := mkdirState(localPath); err != nil {
		return nil, err
	}
	db, err := sql.Open("sqlite", "file:"+StatePath(localPath)+"?_pragma=journal_mode(WAL)&_pragma=busy_timeout(5000)")
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	_, err = db.Exec(`
CREATE TABLE IF NOT EXISTS files(
  path TEXT PRIMARY KEY, node_id INTEGER NOT NULL, content_hash TEXT NOT NULL,
  size INTEGER NOT NULL, mtime INTEGER NOT NULL, type TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);`)
	if err != nil {
		db.Close()
		return nil, err
	}
	return &State{db: db}, nil
}

func mkdirState(localPath string) error {
	return os.MkdirAll(filepath.Join(localPath, ".y-sync"), 0o755)
}

func (s *State) Close() error { return s.db.Close() }

func (s *State) All() (map[string]Rec, error) {
	rows, err := s.db.Query(`SELECT path, node_id, content_hash, size, mtime, type FROM files`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := map[string]Rec{}
	for rows.Next() {
		var p string
		var r Rec
		if err := rows.Scan(&p, &r.NodeID, &r.Hash, &r.Size, &r.MTime, &r.Type); err != nil {
			return nil, err
		}
		out[p] = r
	}
	return out, rows.Err()
}

func (s *State) Get(path string) (Rec, bool, error) {
	var r Rec
	err := s.db.QueryRow(`SELECT node_id, content_hash, size, mtime, type FROM files WHERE path=?`, path).
		Scan(&r.NodeID, &r.Hash, &r.Size, &r.MTime, &r.Type)
	if err == sql.ErrNoRows {
		return Rec{}, false, nil
	}
	if err != nil {
		return Rec{}, false, err
	}
	return r, true, nil
}

func (s *State) Set(path string, r Rec) error {
	_, err := s.db.Exec(`INSERT INTO files(path, node_id, content_hash, size, mtime, type)
		VALUES(?,?,?,?,?,?)
		ON CONFLICT(path) DO UPDATE SET node_id=excluded.node_id, content_hash=excluded.content_hash,
		  size=excluded.size, mtime=excluded.mtime, type=excluded.type`,
		path, r.NodeID, r.Hash, r.Size, r.MTime, r.Type)
	return err
}

func (s *State) Delete(path string) error {
	_, err := s.db.Exec(`DELETE FROM files WHERE path=?`, path)
	return err
}

func (s *State) Cursor() (int64, error) {
	var v string
	err := s.db.QueryRow(`SELECT value FROM meta WHERE key='cursor'`).Scan(&v)
	if err == sql.ErrNoRows {
		return 0, nil
	}
	if err != nil {
		return 0, err
	}
	var n int64
	for _, c := range v {
		if c < '0' || c > '9' {
			return 0, nil
		}
		n = n*10 + int64(c-'0')
	}
	return n, nil
}

func (s *State) SetCursor(c int64) error {
	_, err := s.db.Exec(`INSERT INTO meta(key, value) VALUES('cursor', ?)
		ON CONFLICT(key) DO UPDATE SET value=excluded.value`, itoa(c))
	return err
}

func itoa(v int64) string { return strconv.FormatInt(v, 10) }

// ---------- 分块上传会话持久化（FR-S11 断点续传） ----------

func (s *State) GetUploadSession(rel, hash string) (string, bool) {
	var v string
	err := s.db.QueryRow(`SELECT value FROM meta WHERE key=?`, "upload:"+rel+":"+hash).Scan(&v)
	return v, err == nil
}

func (s *State) SetUploadSession(rel, hash, sessionID string) error {
	_, err := s.db.Exec(`INSERT INTO meta(key, value) VALUES(?,?)
		ON CONFLICT(key) DO UPDATE SET value=excluded.value`, "upload:"+rel+":"+hash, sessionID)
	return err
}

func (s *State) ClearUploadSession(rel, hash string) error {
	_, err := s.db.Exec(`DELETE FROM meta WHERE key=?`, "upload:"+rel+":"+hash)
	return err
}
