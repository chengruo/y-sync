// 分块上传会话（FR-S11）：大文件分块（默认 8MB）上传，支持断点续传。
// 会话状态保存在内存，接收数据写入 tmp 下的稀疏文件；服务端重启后会话失效，
// 客户端重建会话重传（已上传块的代价由内容去重兜底）。
package server

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"sync"
)

type uploadSession struct {
	mu       sync.Mutex
	id       string
	size     int64
	sha256   string
	chunk    int64
	path     string // tmp 数据文件
	received map[int64]bool
}

func (u *uploadSession) totalChunks() int64 {
	n := u.size / u.chunk
	if u.size%u.chunk != 0 {
		n++
	}
	return n
}

type UploadManager struct {
	mu       sync.Mutex
	sessions map[string]*uploadSession
	tmpDir   string
}

func NewUploadManager(tmpDir string) *UploadManager {
	return &UploadManager{sessions: map[string]*uploadSession{}, tmpDir: tmpDir}
}

var ErrBadOffset = errors.New("offset out of range")

// Create 新建会话。
func (m *UploadManager) Create(size int64, sha string, chunk int64) (*uploadSession, error) {
	if size <= 0 || chunk <= 0 || size > 512<<30 {
		return nil, fmt.Errorf("bad size/chunk")
	}
	raw := make([]byte, 16)
	rand.Read(raw)
	id := hex.EncodeToString(raw)
	s := &uploadSession{
		id: id, size: size, sha256: sha, chunk: chunk,
		path:     filepath.Join(m.tmpDir, "upload-"+id),
		received: map[int64]bool{},
	}
	if err := os.WriteFile(s.path, make([]byte, size), 0o600); err != nil {
		return nil, err
	}
	m.mu.Lock()
	m.sessions[id] = s
	m.mu.Unlock()
	return s, nil
}

func (m *UploadManager) Get(id string) *uploadSession {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.sessions[id]
}

func (m *UploadManager) Drop(id string) {
	m.mu.Lock()
	s := m.sessions[id]
	delete(m.sessions, id)
	m.mu.Unlock()
	if s != nil {
		os.Remove(s.path)
	}
}

// WriteChunk 写入指定序号的块。
func (s *uploadSession) WriteChunk(chunkNo int64, data []byte) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	off := chunkNo * s.chunk
	if off < 0 || off >= s.size || int64(len(data)) > s.chunk {
		return ErrBadOffset
	}
	f, err := os.OpenFile(s.path, os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer f.Close()
	if _, err := f.WriteAt(data, off); err != nil {
		return err
	}
	s.received[chunkNo] = true
	return nil
}

// Received 返回已收到的 chunk 序号（升序）。
func (s *uploadSession) Received() []int64 {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]int64, 0, len(s.received))
	for k := range s.received {
		out = append(out, k)
	}
	sort.Slice(out, func(i, j int) bool { return out[i] < out[j] })
	return out
}

// Complete 校验完整性并落为 blob。
func (s *uploadSession) Complete(blobs *BlobStore) (hash string, size int64, err error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	total := s.totalChunks()
	for i := int64(0); i < total; i++ {
		if !s.received[i] {
			return "", 0, fmt.Errorf("missing chunk %d", i)
		}
	}
	f, err := os.Open(s.path)
	if err != nil {
		return "", 0, err
	}
	h := sha256.New()
	size, err = io.Copy(h, f)
	f.Close()
	if err != nil {
		return "", 0, err
	}
	got := hex.EncodeToString(h.Sum(nil))
	if s.sha256 != "" && got != s.sha256 {
		return "", 0, ErrHashMismatch
	}
	rf, err := os.Open(s.path)
	if err != nil {
		return "", 0, err
	}
	defer rf.Close()
	hash, dedup, _, err := blobs.Put(rf, got)
	if err != nil {
		return "", 0, err
	}
	_ = dedup
	return hash, size, nil
}
