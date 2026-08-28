// HTTP API 层：JSON over HTTP，Bearer token 认证，元数据与内容端点分离（§4.2）。
package server

import (
	"encoding/json"
	"errors"
	"io"
	"io/fs"
	"log/slog"
	"net/http"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"ysync/internal/protocol"
)

type Server struct {
	store   *Store
	log     *slog.Logger
	Uploads *UploadManager
	Hub     *Hub // WebSocket 通知（M3）
}

func NewServer(store *Store, log *slog.Logger) *Server {
	return &Server{store: store, log: log, Uploads: NewUploadManager(filepath.Join(store.Blobs.root, "tmp")), Hub: NewHub()}
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, code int, msg string) {
	writeJSON(w, code, map[string]string{"error": msg})
}

// auth 中间件：校验 Bearer token 并注入 user_id / device_id。
func (s *Server) auth(next func(w http.ResponseWriter, r *http.Request, userID, deviceID int64)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		tok := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
		if tok == "" {
			tok = r.URL.Query().Get("token") // 浏览器直开场景（内容/浏览页）
		}
		uid, did, err := s.store.AuthToken(tok)
		if err != nil {
			writeErr(w, http.StatusUnauthorized, "unauthorized")
			return
		}
		next(w, r, uid, did)
	}
}

func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, map[string]string{"status": "ok", "time": time.Now().Format(time.RFC3339)})
	})

	mux.HandleFunc("POST /api/v1/auth/login", s.handleLogin)
	mux.HandleFunc("GET /api/v1/nodes", s.auth(s.handleNodes))
	mux.HandleFunc("GET /api/v1/sync/head", s.auth(s.handleHead))
	mux.HandleFunc("GET /api/v1/sync/changes", s.auth(s.handleChanges))
	mux.HandleFunc("PUT /api/v1/content", s.auth(s.handlePutContent))
	mux.HandleFunc("GET /api/v1/content/{hash}", s.auth(s.handleGetContent))
	mux.HandleFunc("POST /api/v1/ops", s.auth(s.handleOps))
	mux.HandleFunc("GET /api/v1/trash", s.auth(s.handleTrashList))
	mux.HandleFunc("POST /api/v1/trash/{id}/restore", s.auth(s.handleTrashRestore))
	mux.HandleFunc("DELETE /api/v1/trash/{id}", s.auth(s.handleTrashDelete))
	mux.HandleFunc("GET /api/v1/nodes/{id}/versions", s.auth(s.handleVersions))
	mux.HandleFunc("GET /api/v1/versions/{id}/content", s.auth(s.handleVersionContent))
	mux.HandleFunc("/api/v1/notify", s.ServeWS)
	mux.HandleFunc("POST /api/v1/shares", s.auth(s.handleShareCreate))
	mux.HandleFunc("GET /api/v1/shares", s.auth(s.handleShareList))
	mux.HandleFunc("DELETE /api/v1/shares/{token}", s.auth(s.handleShareDelete))
	mux.HandleFunc("GET /s/{token}", s.handlePublicShare)
	mux.HandleFunc("GET /s/{token}/{rel...}", s.handlePublicShare)
	mux.HandleFunc("/dav", s.ServeWebDAV)
	mux.HandleFunc("/dav/", s.ServeWebDAV)
	mux.HandleFunc("GET /browse", s.handleBrowse)
	mux.HandleFunc("POST /api/v1/uploads", s.auth(s.handleUploadCreate))
	mux.HandleFunc("GET /api/v1/uploads/{id}", s.auth(s.handleUploadStatus))
	mux.HandleFunc("PUT /api/v1/uploads/{id}", s.auth(s.handleUploadChunk))
	mux.HandleFunc("POST /api/v1/uploads/{id}/complete", s.auth(s.handleUploadComplete))
	return mux
}

func (s *Server) handleLogin(w http.ResponseWriter, r *http.Request) {
	var req protocol.LoginReq
	if err := json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req); err != nil {
		writeErr(w, 400, "bad request")
		return
	}
	uid, err := s.store.Authenticate(req.User, req.Password)
	if err != nil {
		writeErr(w, 401, "invalid credentials")
		return
	}
	name := req.DeviceName
	if name == "" {
		name = "unnamed-device"
	}
	devID, token, err := s.store.CreateDevice(uid, name)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	s.log.Info("login", "user", req.User, "device", name)
	writeJSON(w, 200, protocol.LoginResp{Token: token, UserID: uid, DeviceID: devID, DeviceName: name})
}

func (s *Server) handleNodes(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	nodes, err := s.store.Nodes(uid)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, map[string]any{"nodes": nodes})
}

func (s *Server) handleHead(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	c, err := s.store.HeadCursor(uid)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, protocol.HeadResp{Cursor: c})
}

func (s *Server) handleChanges(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	q := r.URL.Query()
	var since, rootID int64
	since, _ = strconv.ParseInt(q.Get("cursor"), 10, 64)
	rootID, _ = strconv.ParseInt(q.Get("root"), 10, 64)
	limit, _ := strconv.ParseInt(q.Get("limit"), 10, 64)
	changes, head, err := s.store.Changes(uid, since, limit, rootID)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, protocol.ChangesResp{Cursor: head, Changes: changes})
}

// handlePutContent 两阶段提交的第一阶段：先传内容。
func (s *Server) handlePutContent(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	want := r.Header.Get("X-Content-SHA256")
	hash, dedup, size, err := s.store.Blobs.Put(r.Body, want)
	if errors.Is(err, ErrHashMismatch) {
		writeErr(w, 400, "hash mismatch")
		return
	}
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	if err := s.store.EnsureBlobRow(hash, size); err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	s.log.Debug("content put", "user", uid, "hash", hash[:8], "dedup", dedup, "size", size)
	writeJSON(w, 200, protocol.DedupResp{Hash: hash, Dedup: dedup})
}

// handleGetContent 第二阶段：按哈希取内容，ServeContent 免费提供 Range（FR-S11）。
func (s *Server) handleGetContent(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	hash := safeHashParam(r.PathValue("hash"))
	f, size, err := s.store.Blobs.Open(hash)
	if errors.Is(err, ErrNotFound) || errors.Is(err, fs.ErrNotExist) {
		writeErr(w, 404, "content not found")
		return
	}
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	defer f.Close()
	w.Header().Set("X-Content-SHA256", hash)
	http.ServeContent(w, r, hash, time.Unix(0, 0), f)
	_ = size
}

// ---------- 分块上传（FR-S11）----------

func (s *Server) handleUploadCreate(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	var req struct {
		Size   int64  `json:"size"`
		Sha256 string `json:"sha256"`
		Chunk  int64  `json:"chunk_size"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req); err != nil {
		writeErr(w, 400, "bad request")
		return
	}
	if req.Chunk <= 0 {
		req.Chunk = 8 << 20
	}
	sess, err := s.Uploads.Create(req.Size, req.Sha256, req.Chunk)
	if err != nil {
		writeErr(w, 400, err.Error())
		return
	}
	writeJSON(w, 200, protocol.UploadSessionResp{ID: sess.id, Received: sess.Received()})
}

func (s *Server) handleUploadStatus(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	sess := s.Uploads.Get(r.PathValue("id"))
	if sess == nil {
		writeErr(w, 404, "session not found")
		return
	}
	writeJSON(w, 200, protocol.UploadSessionResp{ID: sess.id, Received: sess.Received()})
}

func (s *Server) handleUploadChunk(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	sess := s.Uploads.Get(r.PathValue("id"))
	if sess == nil {
		writeErr(w, 404, "session not found")
		return
	}
	chunkNo, err := strconv.ParseInt(r.URL.Query().Get("chunk"), 10, 64)
	if err != nil {
		writeErr(w, 400, "bad chunk")
		return
	}
	data, err := io.ReadAll(io.LimitReader(r.Body, sess.chunk+1))
	if err != nil || int64(len(data)) > sess.chunk {
		writeErr(w, 400, "chunk too large")
		return
	}
	if err := sess.WriteChunk(chunkNo, data); err != nil {
		writeErr(w, 400, err.Error())
		return
	}
	w.WriteHeader(204)
}

func (s *Server) handleUploadComplete(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	sess := s.Uploads.Get(r.PathValue("id"))
	if sess == nil {
		writeErr(w, 404, "session not found")
		return
	}
	hash, size, err := sess.Complete(s.store.Blobs)
	if err != nil {
		writeErr(w, 400, err.Error())
		return
	}
	if err := s.store.EnsureBlobRow(hash, size); err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	s.Uploads.Drop(sess.id)
	writeJSON(w, 200, protocol.DedupResp{Hash: hash})
}

func (s *Server) handleTrashList(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	items, err := s.store.ListTrash(uid)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, map[string]any{"items": items})
}

func (s *Server) handleTrashRestore(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		writeErr(w, 400, "bad id")
		return
	}
	n, err := s.store.RestoreTrash(uid, id)
	if errors.Is(err, ErrNotFound) {
		writeErr(w, 404, "trash item not found")
		return
	}
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, n)
}

func (s *Server) handleTrashDelete(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		writeErr(w, 400, "bad id")
		return
	}
	if err := s.store.DeleteTrash(uid, id); err != nil {
		writeErr(w, 404, "trash item not found")
		return
	}
	writeJSON(w, 200, map[string]bool{"ok": true})
}

func (s *Server) handleVersions(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		writeErr(w, 400, "bad id")
		return
	}
	versions, err := s.store.ListVersions(uid, id)
	if errors.Is(err, ErrNotFound) {
		writeErr(w, 404, "node not found")
		return
	}
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, map[string]any{"versions": versions})
}

func (s *Server) handleVersionContent(w http.ResponseWriter, r *http.Request, uid, _ int64) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		writeErr(w, 400, "bad id")
		return
	}
	hash, err := s.store.VersionContent(uid, id)
	if err != nil {
		writeErr(w, 404, "version not found")
		return
	}
	f, _, err := s.store.Blobs.Open(hash)
	if err != nil {
		writeErr(w, 404, "content not found")
		return
	}
	defer f.Close()
	http.ServeContent(w, r, hash, time.Unix(0, 0), f)
}

func (s *Server) handleOps(w http.ResponseWriter, r *http.Request, uid, deviceID int64) {
	var ops []protocol.Op
	if err := json.NewDecoder(io.LimitReader(r.Body, 64<<20)).Decode(&ops); err != nil {
		writeErr(w, 400, "bad request")
		return
	}
	results, err := s.store.ApplyOps(uid, deviceID, ops)
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, protocol.OpsResp{Results: results})
}
