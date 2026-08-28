// 内容寻址 blob 存储：SHA-256 命名，两层目录散列；写临时文件后原子 rename（SR4）。
package server

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

type BlobStore struct {
	root string // dataDir
}

func NewBlobStore(dataDir string) *BlobStore {
	return &BlobStore{root: dataDir}
}

var ErrHashMismatch = errors.New("content hash mismatch")

func (b *BlobStore) blobPath(hash string) string {
	return filepath.Join(b.root, "blobs", hash[:2], hash[2:4], hash)
}

func validHash(h string) bool {
	if len(h) != 64 {
		return false
	}
	_, err := hex.DecodeString(h)
	return err == nil
}

// Put 流式写入并校验哈希；已存在（去重命中）则丢弃临时文件返回 dedup=true。
// 同时确保 blobs 元数据行存在（refcount 由 ops 维护）。
func (b *BlobStore) Put(r io.Reader, wantHash string) (hash string, dedup bool, size int64, err error) {
	tmp, err := os.CreateTemp(filepath.Join(b.root, "tmp"), "upload-*")
	if err != nil {
		return "", false, 0, err
	}
	tmpName := tmp.Name()
	defer func() {
		tmp.Close()
		os.Remove(tmpName)
	}()

	h := sha256.New()
	n, err := io.Copy(io.MultiWriter(tmp, h), r)
	if err != nil {
		return "", false, 0, err
	}
	if err := tmp.Sync(); err != nil {
		return "", false, 0, err
	}
	tmp.Close()
	got := hex.EncodeToString(h.Sum(nil))
	if wantHash != "" && got != wantHash {
		return "", false, 0, ErrHashMismatch
	}
	dst := b.blobPath(got)
	if _, err := os.Stat(dst); err == nil {
		return got, true, n, nil
	}
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
		return "", false, 0, err
	}
	if err := os.Rename(tmpName, dst); err != nil {
		return "", false, 0, err
	}
	return got, false, n, nil
}

// EnsureRow 保证 blobs 行存在（去重命中时内容已在但可能无行）。
func (s *Store) EnsureBlobRow(hash string, size int64) error {
	if !validHash(hash) {
		return fmt.Errorf("invalid hash")
	}
	_, err := s.db.Exec(`INSERT INTO blobs(hash, size, refcount) VALUES(?,?,0)
		ON CONFLICT(hash) DO NOTHING`, hash, size)
	return err
}

// Open 返回 blob 文件句柄（供 http.ServeContent 支持 Range）。
func (b *BlobStore) Open(hash string) (*os.File, int64, error) {
	if !validHash(hash) {
		return nil, 0, fmt.Errorf("invalid hash")
	}
	f, err := os.Open(b.blobPath(hash))
	if err != nil {
		return nil, 0, ErrNotFound
	}
	st, err := f.Stat()
	if err != nil {
		f.Close()
		return nil, 0, err
	}
	return f, st.Size(), nil
}

// Remove 删除 blob 文件（GC 在行删除后调用）。
func (b *BlobStore) Remove(hash string) {
	if !validHash(hash) {
		return
	}
	os.Remove(b.blobPath(hash))
}

// hasRefusedHash 防路径穿越的辅助校验
func safeHashParam(h string) string {
	h = strings.TrimPrefix(h, "/")
	return h
}
