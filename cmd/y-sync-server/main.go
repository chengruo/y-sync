// y-sync-server：单二进制服务端。子命令 serve / adduser / passwd / list-users / gc / version（SR1）。
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"

	"github.com/BurntSushi/toml"

	"ysync/internal/server"
)

type Config struct {
	Addr               string `toml:"addr"`
	DataDir            string `toml:"data_dir"`
	LogFile            string `toml:"log_file"`
	MaxVersions        int    `toml:"max_versions"`         // FR-V1，默认 10
	TrashRetentionDays int    `toml:"trash_retention_days"` // FR-V2，默认 30
}

func (c *Config) applyDefaults() {
	if c.MaxVersions <= 0 {
		c.MaxVersions = 10
	}
	if c.TrashRetentionDays <= 0 {
		c.TrashRetentionDays = 30
	}
}

func main() {
	if len(os.Args) < 2 {
		usage()
	}
	log := slog.New(slog.NewTextHandler(os.Stderr, nil))
	var err error
	switch os.Args[1] {
	case "serve":
		err = cmdServe(log)
	case "adduser":
		err = cmdAddUser(os.Args[2:])
	case "passwd":
		err = cmdPasswd(os.Args[2:])
	case "list-users":
		err = cmdListUsers()
	case "gc":
		err = cmdGC()
	case "backup":
		err = cmdBackup(os.Args[2:])
	case "version":
		fmt.Println("y-sync-server v0.1.0 (M1)")
	default:
		usage()
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprintf(os.Stderr, `y-sync-server — 轻量文件同步服务端

用法:
  y-sync-server serve   [-config FILE] [-addr ADDR] [-data DIR]
  y-sync-server adduser <name>
  y-sync-server passwd  <name>
  y-sync-server list-users
  y-sync-server gc
  y-sync-server backup -out <dir>
  y-sync-server version

配置文件 (TOML, 可选):
  addr     = "127.0.0.1:8720"
  data_dir = "./y-sync-data"
`)
	os.Exit(2)
}

func cmdServe(log *slog.Logger) error {
	var storeCfg *Config
	fs := flag.NewFlagSet("serve", flag.ExitOnError)
	cfgPath := fs.String("config", "", "TOML 配置文件路径")
	addr := fs.String("addr", "127.0.0.1:8720", "监听地址")
	dataDir := fs.String("data", "./y-sync-data", "数据目录")
	fs.Parse(os.Args[2:])

	if *cfgPath != "" {
		var cfg Config
		if _, err := toml.DecodeFile(*cfgPath, &cfg); err != nil {
			return fmt.Errorf("read config: %w", err)
		}
		if cfg.Addr != "" {
			*addr = cfg.Addr
		}
		if cfg.DataDir != "" {
			*dataDir = cfg.DataDir
		}
		cfg.applyDefaults()
		storeCfg = &cfg
	}
	if env := os.Getenv("YSYNC_ADDR"); env != "" {
		*addr = env
	}
	if env := os.Getenv("YSYNC_DATA"); env != "" {
		*dataDir = env
	}

	if err := os.MkdirAll(filepath.Join(*dataDir, "tmp"), 0o755); err != nil {
		return err
	}
	store, err := server.OpenStore(*dataDir)
	if err != nil {
		return err
	}
	defer store.Close()
	if storeCfg != nil {
		store.MaxVersions = storeCfg.MaxVersions
		store.TrashRetentionDays = storeCfg.TrashRetentionDays
	} else {
		store.MaxVersions, store.TrashRetentionDays = 10, 30
	}

	hs := server.NewServer(store, log)
	store.OnOpsCommit = func(userID, deviceID, head int64) {
		b, _ := json.Marshal(map[string]any{"user_id": userID, "device_id": deviceID, "cursor": head})
		hs.Hub.Notify(userID, b)
	}
	srv := &http.Server{Addr: *addr, Handler: hs.Handler()}
	go func() {
		sig := make(chan os.Signal, 1)
		signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
		<-sig
		log.Info("shutting down (SR6: 优雅退出)")
		srv.Close()
	}()
	log.Info("y-sync-server listening", "addr", *addr, "data", *dataDir)
	return srv.ListenAndServe()
}

func openStore() (*server.Store, error) {
	data := os.Getenv("YSYNC_DATA")
	if data == "" {
		data = "./y-sync-data"
	}
	if err := os.MkdirAll(filepath.Join(data, "tmp"), 0o755); err != nil {
		return nil, err
	}
	return server.OpenStore(data)
}

func cmdAddUser(args []string) error {
	if len(args) != 1 {
		return fmt.Errorf("usage: y-sync-server adduser <name>")
	}
	fmt.Print("password: ")
	pw, err := readPassword()
	if err != nil {
		return err
	}
	store, err := openStore()
	if err != nil {
		return err
	}
	defer store.Close()
	id, err := store.CreateUser(args[0], pw)
	if err != nil {
		return err
	}
	fmt.Printf("user %q created (id=%d)\n", args[0], id)
	return nil
}

func cmdPasswd(args []string) error {
	if len(args) != 1 {
		return fmt.Errorf("usage: y-sync-server passwd <name>")
	}
	fmt.Print("new password: ")
	pw, err := readPassword()
	if err != nil {
		return err
	}
	store, err := openStore()
	if err != nil {
		return err
	}
	defer store.Close()
	if err := store.ResetPassword(args[0], pw); err != nil {
		return err
	}
	fmt.Printf("password of %q updated\n", args[0])
	return nil
}

func cmdListUsers() error {
	store, err := openStore()
	if err != nil {
		return err
	}
	defer store.Close()
	for _, u := range store.ListUsers() {
		fmt.Println(u)
	}
	return nil
}

func cmdGC() error {
	store, err := openStore()
	if err != nil {
		return err
	}
	defer store.Close()
	store.MaxVersions, store.TrashRetentionDays = 10, 30
	purged, blobs, err := store.GC()
	if err != nil {
		return err
	}
	fmt.Printf("gc: purged %d trash entries, removed %d unreferenced blobs\n", purged, blobs)
	return nil
}
