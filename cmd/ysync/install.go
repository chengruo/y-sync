package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"ysync/internal/client"
)

// ---------- 分享（FR-H1） ----------

func cmdShare(args []string) error {
	// flag-after-arg：先抽取 flag，其余为位置参数（与 add 一致）
	var positional []string
	var flagArgs []string
	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "-hours", "--hours", "-password", "--password":
			if i+1 >= len(args) {
				return fmt.Errorf("%s 需要参数", args[i])
			}
			flagArgs = append(flagArgs, args[i], args[i+1])
			i++
		default:
			if strings.HasPrefix(args[i], "-") {
				flagArgs = append(flagArgs, args[i])
			} else {
				positional = append(positional, args[i])
			}
		}
	}
	fs := flag.NewFlagSet("share", flag.ExitOnError)
	hours := fs.Int64("hours", 0, "过期小时数（0=永不过期）")
	password := fs.String("password", "", "访问密码（可选）")
	fs.Parse(flagArgs)
	if len(positional) != 2 {
		return fmt.Errorf("usage: ysync share <folder> <相对路径> [-hours N] [-password pw]")
	}
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	api := client.NewAPI(cfg.ServerURL, cfg.Token)
	serverPath := positional[0] + "/" + positional[1]
	info, err := api.CreateShare(serverPath, *hours, *password)
	if err != nil {
		return err
	}
	fmt.Printf("分享链接: %s/s/%s\n", cfg.ServerURL, info.Token)
	if *password != "" {
		fmt.Printf("访问密码: %s\n", *password)
	}
	if info.ExpiresAt > 0 {
		fmt.Printf("过期时间: %s\n", time.Unix(info.ExpiresAt, 0).Format("2006-01-02 15:04"))
	}
	return nil
}

func cmdShares() error {
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	api := client.NewAPI(cfg.ServerURL, cfg.Token)
	shares, err := api.ListShares()
	if err != nil {
		return err
	}
	if len(shares) == 0 {
		fmt.Println("(无分享)")
		return nil
	}
	for _, s := range shares {
		exp := "永久"
		if s.ExpiresAt > 0 {
			exp = time.Unix(s.ExpiresAt, 0).Format("2006-01-02 15:04")
		}
		pwd := ""
		if s.HasPwd {
			pwd = " [密码]"
		}
		fmt.Printf("  %s  %-50s%s 过期: %s\n", s.Token, s.Path, pwd, exp)
	}
	return nil
}

func cmdUnshare(args []string) error {
	if len(args) != 1 {
		return fmt.Errorf("usage: ysync unshare <token>")
	}
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	api := client.NewAPI(cfg.ServerURL, cfg.Token)
	if err := api.DeleteShare(args[0]); err != nil {
		return err
	}
	fmt.Println("已撤销分享")
	return nil
}

// ---------- 开机自启（M3） ----------

const launchdLabel = "app.ysync.daemon"

func cmdInstall(args []string) error {
	fs := flag.NewFlagSet("install", flag.ExitOnError)
	interval := fs.Duration("interval", 3*time.Second, "同步轮询间隔")
	fs.Parse(args)
	exe, err := os.Executable()
	if err != nil {
		return err
	}
	exe, _ = filepath.Abs(exe)
	home, _ := os.UserHomeDir()

	switch runtime.GOOS {
	case "darwin":
		dir := filepath.Join(home, "Library", "LaunchAgents")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
		plist := filepath.Join(dir, launchdLabel+".plist")
		content := fmt.Sprintf(`<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>%s</string>
  <key>ProgramArguments</key><array>
    <string>%s</string><string>daemon</string><string>-interval</string><string>%s</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>%s/Library/Logs/ysync.log</string>
  <key>StandardErrorPath</key><string>%s/Library/Logs/ysync.log</string>
</dict></plist>
`, launchdLabel, exe, interval.String(), home, home)
		if err := os.WriteFile(plist, []byte(content), 0o644); err != nil {
			return err
		}
		fmt.Printf("已写入 %s\n加载: launchctl load %s\n", plist, plist)

	case "linux":
		dir := filepath.Join(home, ".config", "systemd", "user")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
		unit := filepath.Join(dir, "ysync.service")
		content := fmt.Sprintf(`[Unit]
Description=y-sync client daemon
After=network-online.target

[Service]
ExecStart=%s daemon -interval %s
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
`, exe, interval.String())
		if err := os.WriteFile(unit, []byte(content), 0o644); err != nil {
			return err
		}
		fmt.Printf("已写入 %s\n启用: systemctl --user daemon-reload && systemctl --user enable --now ysync\n", unit)

	default:
		return fmt.Errorf("暂不支持 %s（可手动把 %q daemon 加入启动项）", runtime.GOOS, exe)
	}
	return nil
}

func cmdUninstall() error {
	home, _ := os.UserHomeDir()
	var paths []string
	switch runtime.GOOS {
	case "darwin":
		paths = []string{filepath.Join(home, "Library", "LaunchAgents", launchdLabel+".plist")}
	case "linux":
		paths = []string{filepath.Join(home, ".config", "systemd", "user", "ysync.service")}
	}
	for _, p := range paths {
		if err := os.Remove(p); err != nil && !os.IsNotExist(err) {
			return err
		}
		fmt.Printf("已删除 %s（如已加载请执行 launchctl unload / systemctl --user disable ysync）\n", p)
	}
	return nil
}
