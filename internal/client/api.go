// 客户端 API 封装：全部协议端点的类型安全访问。
package client

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"time"

	"ysync/internal/protocol"
)

type API struct {
	BaseURL string
	Token   string
	HTTP    *http.Client
	// 限速（FR-S12），KB/s；由 SetLimits 配置
	ul, dl *rateLimiter
}

func NewAPI(baseURL, token string) *API {
	return &API{BaseURL: baseURL, Token: token, HTTP: &http.Client{Timeout: 30 * time.Minute}}
}

// SetLimits 配置上/下行限速（0 = 不限）。
func (a *API) SetLimits(uploadKBs, downloadKBs int64) {
	if uploadKBs > 0 {
		a.ul = newRateLimiter(uploadKBs * 1024)
	} else {
		a.ul = nil
	}
	if downloadKBs > 0 {
		a.dl = newRateLimiter(downloadKBs * 1024)
	} else {
		a.dl = nil
	}
}

func (a *API) do(method, path string, body io.Reader, hdr map[string]string, out any) error {
	if a.ul != nil && body != nil {
		body = &rateReader{r: body, rl: a.ul} // 上行限速作用于读侧
	}
	req, err := http.NewRequest(method, a.BaseURL+path, body)
	if err != nil {
		return err
	}
	if a.Token != "" {
		req.Header.Set("Authorization", "Bearer "+a.Token)
	}
	for k, v := range hdr {
		req.Header.Set(k, v)
	}
	resp, err := a.HTTP.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 300 {
		var e struct{ Error string }
		json.NewDecoder(io.LimitReader(resp.Body, 1<<16)).Decode(&e)
		if e.Error != "" {
			return fmt.Errorf("%s %s: %d %s", method, path, resp.StatusCode, e.Error)
		}
		return fmt.Errorf("%s %s: HTTP %d", method, path, resp.StatusCode)
	}
	if out != nil {
		return json.NewDecoder(resp.Body).Decode(out)
	}
	return nil
}

func (a *API) Login(user, password, device string) (*protocol.LoginResp, error) {
	var out protocol.LoginResp
	b, _ := json.Marshal(protocol.LoginReq{User: user, Password: password, DeviceName: device})
	err := a.do("POST", "/api/v1/auth/login", bytes.NewReader(b), map[string]string{"Content-Type": "application/json"}, &out)
	return &out, err
}

func (a *API) Nodes() ([]protocol.NodeInfo, error) {
	var out struct{ Nodes []protocol.NodeInfo }
	err := a.do("GET", "/api/v1/nodes", nil, nil, &out)
	return out.Nodes, err
}

func (a *API) Head() (int64, error) {
	var out protocol.HeadResp
	err := a.do("GET", "/api/v1/sync/head", nil, nil, &out)
	return out.Cursor, err
}

func (a *API) Changes(cursor int64, limit int64, rootID int64) (*protocol.ChangesResp, error) {
	var out protocol.ChangesResp
	err := a.do("GET", fmt.Sprintf("/api/v1/sync/changes?cursor=%d&limit=%d&root=%d", cursor, limit, rootID), nil, nil, &out)
	return &out, err
}

// PutContent 两阶段之一：上传内容，服务端去重命中时返回 dedup=true。
func (a *API) PutContent(path string) (hash string, size int64, dedup bool, err error) {
	f, err := os.Open(path)
	if err != nil {
		return "", 0, false, err
	}
	defer f.Close()
	h := sha256.New()
	n, err := io.Copy(h, f)
	if err != nil {
		return "", 0, false, err
	}
	hash = hex.EncodeToString(h.Sum(nil))
	if _, err := f.Seek(0, io.SeekStart); err != nil {
		return "", 0, false, err
	}
	var out protocol.DedupResp
	err = a.do("PUT", "/api/v1/content", f, map[string]string{"X-Content-SHA256": hash}, &out)
	return out.Hash, n, out.Dedup, err
}

// GetContent 两阶段之二：按哈希下载内容到 destPath（先写临时文件再原子改名）。
func (a *API) GetContent(hash, destPath string, mtimeMilli int64) error {
	if err := os.MkdirAll(tmpDirOf(destPath), 0o755); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(tmpDirOf(destPath), ".ysync-dl-*")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	cleanup := func() { tmp.Close(); os.Remove(tmpName) }

	req, err := http.NewRequest("GET", a.BaseURL+"/api/v1/content/"+url.PathEscape(hash), nil)
	if err != nil {
		cleanup()
		return err
	}
	req.Header.Set("Authorization", "Bearer "+a.Token)
	resp, err := a.HTTP.Do(req)
	if err != nil {
		cleanup()
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 && resp.StatusCode != 206 {
		cleanup()
		return fmt.Errorf("download %s: HTTP %d", hash[:8], resp.StatusCode)
	}
	var src io.Reader = resp.Body
	if a.dl != nil {
		src = &rateReader{r: resp.Body, rl: a.dl}
	}
	if _, err := io.Copy(tmp, src); err != nil {
		cleanup()
		return err
	}
	if err := tmp.Sync(); err != nil {
		cleanup()
		return err
	}
	tmp.Close()
	// 校验下载内容哈希，杜绝静默损坏
	got, err := fileHash(tmpName)
	if err != nil {
		cleanup()
		return err
	}
	if got != hash {
		cleanup()
		return fmt.Errorf("download hash mismatch: want %s got %s", hash[:8], got[:8])
	}
	if err := os.Rename(tmpName, destPath); err != nil {
		cleanup()
		return err
	}
	if mtimeMilli > 0 {
		t := time.UnixMilli(mtimeMilli)
		os.Chtimes(destPath, t, t)
	}
	return nil
}

func (a *API) Ops(ops []protocol.Op) ([]protocol.OpResult, error) {
	var out protocol.OpsResp
	b, _ := json.Marshal(ops)
	err := a.do("POST", "/api/v1/ops", bytes.NewReader(b), map[string]string{"Content-Type": "application/json"}, &out)
	return out.Results, err
}

func fileHash(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

func tmpDirOf(p string) string { return filepath.Dir(p) }

// ---------- 回收站 / 版本 ----------

func (a *API) TrashList() ([]protocol.TrashItem, error) {
	var out struct{ Items []protocol.TrashItem }
	err := a.do("GET", "/api/v1/trash", nil, nil, &out)
	return out.Items, err
}

func (a *API) TrashRestore(id int64) error {
	return a.do("POST", fmt.Sprintf("/api/v1/trash/%d/restore", id), nil, nil, nil)
}

func (a *API) TrashDelete(id int64) error {
	return a.do("DELETE", fmt.Sprintf("/api/v1/trash/%d", id), nil, nil, nil)
}

func (a *API) NodeVersions(nodeID int64) ([]protocol.VersionItem, error) {
	var out struct{ Versions []protocol.VersionItem }
	err := a.do("GET", fmt.Sprintf("/api/v1/nodes/%d/versions", nodeID), nil, nil, &out)
	return out.Versions, err
}

// DownloadVersionTo 把某版本内容下载到 destPath（临时文件+原子改名+回设 mtime）。
func (a *API) DownloadVersionTo(versionID int64, destPath string, mtimeMilli int64) error {
	req, err := http.NewRequest("GET", a.BaseURL+fmt.Sprintf("/api/v1/versions/%d/content", versionID), nil)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+a.Token)
	resp, err := a.HTTP.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return fmt.Errorf("version download: HTTP %d", resp.StatusCode)
	}
	if err := os.MkdirAll(filepath.Dir(destPath), 0o755); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(destPath), ".ysync-ver-*")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	if _, err := io.Copy(tmp, resp.Body); err != nil {
		tmp.Close()
		os.Remove(tmpName)
		return err
	}
	tmp.Close()
	if err := os.Rename(tmpName, destPath); err != nil {
		os.Remove(tmpName)
		return err
	}
	if mtimeMilli > 0 {
		t := time.UnixMilli(mtimeMilli)
		os.Chtimes(destPath, t, t)
	}
	return nil
}

// ---------- 分享（FR-H1） ----------

func (a *API) CreateShare(path string, hours int64, password string) (*protocol.ShareInfo, error) {
	var out protocol.ShareInfo
	b, _ := json.Marshal(map[string]any{"path": path, "hours": hours, "password": password})
	err := a.do("POST", "/api/v1/shares", bytes.NewReader(b),
		map[string]string{"Content-Type": "application/json"}, &out)
	return &out, err
}

func (a *API) ListShares() ([]protocol.ShareInfo, error) {
	var out struct{ Shares []protocol.ShareInfo }
	err := a.do("GET", "/api/v1/shares", nil, nil, &out)
	return out.Shares, err
}

func (a *API) DeleteShare(token string) error {
	return a.do("DELETE", "/api/v1/shares/"+token, nil, nil, nil)
}

// ---------- 分块上传（FR-S11）----------

// PutContentChunked 大文件分块上传，支持断点续传（resumeID 非空时续传）。
// 返回 (会话 ID, 哈希)；会话完成返回 resumeID=""。
func (a *API) PutContentChunked(path, resumeID, wantHash string, size, chunkSize int64) (string, string, error) {
	// 创建或续传会话
	var sess protocol.UploadSessionResp
	if resumeID != "" {
		err := a.do("GET", "/api/v1/uploads/"+resumeID, nil, nil, &sess)
		if err != nil {
			return "", wantHash, err
		}
	} else {
		b, _ := json.Marshal(map[string]any{"size": size, "sha256": wantHash, "chunk_size": chunkSize})
		err := a.do("POST", "/api/v1/uploads", bytes.NewReader(b),
			map[string]string{"Content-Type": "application/json"}, &sess)
		if err != nil {
			return "", wantHash, err
		}
	}
	received := map[int64]bool{}
	for _, n := range sess.Received {
		received[n] = true
	}
	f, err := os.Open(path)
	if err != nil {
		return sess.ID, wantHash, err
	}
	defer f.Close()
	total := (size + chunkSize - 1) / chunkSize
	buf := make([]byte, chunkSize)
	for i := int64(0); i < total; i++ {
		if received[i] {
			continue
		}
		n, err := io.ReadFull(f, buf)
		if err != nil && err != io.ErrUnexpectedEOF {
			return sess.ID, wantHash, err
		}
		if err := a.do("PUT", fmt.Sprintf("/api/v1/uploads/%s?chunk=%d", sess.ID, i),
			bytes.NewReader(buf[:n]), nil, nil); err != nil {
			return sess.ID, wantHash, err
		}
	}
	var done struct{ Hash string }
	err = a.do("POST", "/api/v1/uploads/"+sess.ID+"/complete", nil, nil, &done)
	if err != nil {
		return sess.ID, wantHash, err
	}
	return "", done.Hash, nil // 会话完成
}
