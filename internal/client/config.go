// 客户端配置：~/.config/y-sync/config.json（token、设备名、多文件夹映射）。
package client

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
)

type Folder struct {
	Name         string   `json:"name"`                    // 服务端子树名（顶层目录）
	LocalPath    string   `json:"local_path"`              // 本地绝对路径
	RootNodeID   int64    `json:"root_node_id"`            // 服务端子树根节点 ID（0=未解析）
	Cursor       int64    `json:"cursor"`                  // 该文件夹独立的同步游标（FR-S14）
	Excludes     []string `json:"excludes,omitempty"`      // 选择性同步：排除的子树前缀（FR-S9）
	UseGitignore bool     `json:"use_gitignore,omitempty"` // 沿用 .gitignore（FR-S8）
}

type Config struct {
	ServerURL  string   `json:"server_url"`
	User       string   `json:"user"`
	Token      string   `json:"token"`
	DeviceName string   `json:"device_name"`
	DeviceID   int64    `json:"device_id"` // 跳过自己设备的变更重放
	Folders    []Folder `json:"folders"`

	// 传输策略（FR-S11/S12），0/空 = 默认值
	ChunkThresholdMB int64 `json:"chunk_threshold_mb,omitempty"` // 超过则分块上传，默认 100
	ChunkSizeMB      int64 `json:"chunk_size_mb,omitempty"`      // 分块大小，默认 8
	UploadLimitKBs   int64 `json:"upload_limit_kbs,omitempty"`   // 上行限速 KB/s，0=不限
	DownloadLimitKBs int64 `json:"download_limit_kbs,omitempty"` // 下行限速 KB/s，0=不限
}

// Defaults 填充传输策略默认值。
func (c *Config) Defaults() {
	if c.ChunkThresholdMB == 0 {
		c.ChunkThresholdMB = 100
	}
	if c.ChunkSizeMB == 0 {
		c.ChunkSizeMB = 8
	}
}

func ConfigPath() (string, error) {
	// 测试/多设备模拟：允许显式覆盖配置目录
	if dir := os.Getenv("YSYNC_CONFIG_DIR"); dir != "" {
		return filepath.Join(dir, "config.json"), nil
	}
	base, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(base, "y-sync", "config.json"), nil
}

func LoadConfig() (*Config, error) {
	p, err := ConfigPath()
	if err != nil {
		return nil, err
	}
	b, err := os.ReadFile(p)
	if os.IsNotExist(err) {
		return nil, fmt.Errorf("尚未初始化，请先执行 ysync init")
	}
	if err != nil {
		return nil, err
	}
	var c Config
	if err := json.Unmarshal(b, &c); err != nil {
		return nil, fmt.Errorf("配置文件损坏: %w", err)
	}
	return &c, nil
}

func SaveConfig(c *Config) error {
	p, err := ConfigPath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o700); err != nil {
		return err
	}
	b, _ := json.MarshalIndent(c, "", "  ")
	tmp := p + ".tmp"
	if err := os.WriteFile(tmp, b, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, p)
}

func DefaultDeviceName() string {
	host, err := os.Hostname()
	if err != nil || host == "" {
		host = "unknown-host"
	}
	return host + "-" + runtime.GOOS
}
