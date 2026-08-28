// 只读 WebDAV 兼容层（M4）：PROPFIND/GET 映射到节点树，Finder/资源管理器
// 可直接"连接服务器"只读浏览。写入操作一律拒绝。
package server

import (
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path"
	"strings"
	"time"

	"golang.org/x/net/webdav"

	"ysync/internal/protocol"
)

type davFS struct {
	store  *Store
	userID int64
}

var errReadOnly = errors.New("y-sync webdav is read-only")

func (f davFS) Mkdir(ctx context.Context, name string, perm os.FileMode) error { return errReadOnly }
func (f davFS) RemoveAll(ctx context.Context, name string) error               { return errReadOnly }
func (f davFS) Rename(ctx context.Context, oldName, newName string) error      { return errReadOnly }
func (f davFS) OpenFile(ctx context.Context, name string, flags int, perm os.FileMode) (webdav.File, error) {
	if flags&(os.O_WRONLY|os.O_RDWR|os.O_CREATE|os.O_APPEND|os.O_TRUNC) != 0 {
		return nil, errReadOnly
	}
	n, err := f.node(name)
	if err != nil {
		return nil, err
	}
	if n.Type == protocol.TypeDir {
		return &davDir{fs: f, node: n}, nil
	}
	bf, _, err := f.store.Blobs.Open(n.ContentHash)
	if err != nil {
		return nil, err
	}
	return &davFile{File: bf, info: davInfo{node: n}}, nil
}

func (f davFS) Stat(ctx context.Context, name string) (os.FileInfo, error) {
	n, err := f.node(name)
	if err != nil {
		return nil, err
	}
	fi := davInfo{node: n}
	return fi, nil
}

// node 将 webdav 路径（/a/b/c，根为用户空间）解析为节点。
func (f davFS) node(name string) (*protocol.NodeInfo, error) {
	p := strings.Trim(path.Clean("/"+name), "/")
	if p == "" || p == "." {
		return &protocol.NodeInfo{Type: protocol.TypeDir, Path: "", Name: ""}, nil
	}
	n, err := f.store.nodeByPath(f.userID, p)
	if err != nil {
		return nil, os.ErrNotExist
	}
	return n, nil
}

// ---------- FileInfo ----------

type davInfo struct{ node *protocol.NodeInfo }

func (d davInfo) Name() string { return path.Base(d.node.Path) }
func (d davInfo) Size() int64  { return d.node.Size }
func (d davInfo) Mode() fs.FileMode {
	if d.node.Type == protocol.TypeDir {
		return os.ModeDir | 0o755
	}
	return 0o644
}
func (d davInfo) ModTime() time.Time { return time.UnixMilli(d.node.MTime) }
func (d davInfo) IsDir() bool        { return d.node.Type == protocol.TypeDir }
func (d davInfo) Sys() any           { return nil }

// ---------- File 实现 ----------

type davFile struct {
	*os.File
	info davInfo
}

func (f *davFile) Stat() (os.FileInfo, error) { return f.info, nil }
func (f *davFile) Readdir(int) ([]os.FileInfo, error) {
	return nil, errReadOnly
}
func (f *davFile) Write([]byte) (int, error) { return 0, errReadOnly }

type davDir struct {
	fs   davFS
	node *protocol.NodeInfo
	off  int // 已读偏移（Readdir 顺序游标）
}

func (d *davDir) Stat() (os.FileInfo, error)     { return davInfo{node: d.node}, nil }
func (d *davDir) Close() error                   { return nil }
func (d *davDir) Read([]byte) (int, error)       { return 0, errReadOnly }
func (d *davDir) Write([]byte) (int, error)      { return 0, errReadOnly }
func (d *davDir) Seek(int64, int) (int64, error) { return 0, errReadOnly }

// Readdir 输出当前目录的子项（子目录排在前面由调用方无所谓）。
func (d *davDir) Readdir(count int) ([]os.FileInfo, error) {
	prefix := ""
	if d.node.Path != "" {
		prefix = d.node.Path + "/"
	}
	nodes, err := d.fs.store.Nodes(d.fs.userID)
	if err != nil {
		return nil, err
	}
	var all []os.FileInfo
	for _, k := range nodes {
		if prefix == "" {
			// 根层：path 不含 '/'
			if strings.Contains(k.Path, "/") {
				continue
			}
		} else {
			if !strings.HasPrefix(k.Path, prefix) || strings.Contains(strings.TrimPrefix(k.Path, prefix), "/") {
				continue
			}
		}
		all = append(all, davInfo{node: &k})
	}
	// 顺序游标
	if d.off >= len(all) && count <= 0 {
		return nil, io.EOF
	}
	end := len(all)
	if count > 0 {
		end = min(d.off+count, len(all))
		if d.off >= end {
			return nil, io.EOF
		}
	}
	out := all[d.off:end]
	d.off = end
	return out, nil
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// ServeWebDAV 挂载只读 WebDAV（Basic Auth 复用用户口令）。
func (s *Server) ServeWebDAV(w http.ResponseWriter, r *http.Request) {
	user, pass, ok := r.BasicAuth()
	if !ok {
		w.Header().Set("WWW-Authenticate", `Basic realm="y-sync"`)
		http.Error(w, "unauthorized", 401)
		return
	}
	uid, err := s.store.Authenticate(user, pass)
	if err != nil {
		w.Header().Set("WWW-Authenticate", `Basic realm="y-sync"`)
		http.Error(w, "unauthorized", 401)
		return
	}
	handler := &webdav.Handler{
		Prefix:     "/dav",
		FileSystem: davFS{store: s.store, userID: uid},
		LockSystem: webdav.NewMemLS(),
		Logger: func(req *http.Request, err error) {
			if err != nil && s.log != nil {
				s.log.Debug("webdav", "method", req.Method, "path", req.URL.Path, "err", err)
			}
		},
	}
	handler.ServeHTTP(w, r)
}

// ---------- Web 只读浏览页（M4 可选）----------

// handleBrowse GET /browse?token=&path= —— 极简只读列表（FR 与 WebDAV 覆盖不同场景：浏览器直开）。
func (s *Server) handleBrowse(w http.ResponseWriter, r *http.Request) {
	token := r.URL.Query().Get("token")
	if token == "" {
		http.Error(w, "需要 token（ysync init 后的设备 token）", 401)
		return
	}
	uid, _, err := s.store.AuthToken(token)
	if err != nil {
		http.Error(w, "unauthorized", 401)
		return
	}
	p := strings.Trim(r.URL.Query().Get("path"), "/")
	nodes, err := s.store.Nodes(uid)
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	fmt.Fprintf(w, "<!doctype html><meta charset=utf-8><title>y-sync 浏览</title><h3>/%s</h3><ul>", p)
	cur := p
	if cur != "" {
		parent := path.Dir(cur)
		if parent == "." {
			parent = ""
		}
		fmt.Fprintf(w, `<li><a href="/browse?token=%s&path=%s">../</a></li>`, token, parent)
	}
	prefix := ""
	if cur != "" {
		prefix = cur + "/"
	}
	for _, n := range nodes {
		if prefix == "" {
			if strings.Contains(n.Path, "/") {
				continue
			}
		} else if !strings.HasPrefix(n.Path, prefix) || strings.Contains(strings.TrimPrefix(n.Path, prefix), "/") {
			continue
		}
		rel := strings.TrimPrefix(n.Path, prefix)
		if n.Type == protocol.TypeDir {
			fmt.Fprintf(w, `<li><a href="/browse?token=%s&path=%s">%s/</a></li>`, token, n.Path, rel)
		} else {
			fmt.Fprintf(w, `<li><a href="/api/v1/content/%s?token=%s">%s (%.1f KB)</a></li>`,
				n.ContentHash, token, rel, float64(n.Size)/1024)
		}
	}
	fmt.Fprint(w, "</ul>")
}
