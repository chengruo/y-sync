// ysync：轻量文件同步客户端 CLI（FR-C1）。
// 子命令：init / add / sync / daemon / status / folders
package main

import (
	"flag"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"time"

	"ysync/internal/client"
)

func main() {
	log := slog.New(slog.NewTextHandler(os.Stderr, nil))
	if len(os.Args) < 2 {
		usage()
	}
	var err error
	switch os.Args[1] {
	case "init":
		err = cmdInit(os.Args[2:])
	case "add":
		err = cmdAdd(os.Args[2:])
	case "sync":
		err = cmdSync(log, os.Args[2:])
	case "daemon":
		err = cmdDaemon(log, os.Args[2:])
	case "status":
		err = cmdStatus()
	case "trash":
		err = cmdTrash(os.Args[2:])
	case "versions":
		err = cmdVersions(os.Args[2:])
	case "share":
		err = cmdShare(os.Args[2:])
	case "shares":
		err = cmdShares()
	case "unshare":
		err = cmdUnshare(os.Args[2:])
	case "install":
		err = cmdInstall(os.Args[2:])
	case "uninstall":
		err = cmdUninstall()
	case "version":
		fmt.Println("ysync v0.1.0 (M1)")
	default:
		usage()
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprintf(os.Stderr, `ysync — 轻量文件同步客户端

用法:
  ysync init   -server URL -user NAME            首次登录（交互输入密码）
  ysync add    <本地目录> [-as 服务端名]         接入一个散落项目（FR-S15）
  ysync sync   [-only NAME]                     同步一次（全部或指定文件夹）
  ysync daemon [-interval 3s] [-only NAME]      常驻循环同步
  ysync status                                  查看各文件夹状态
  ysync trash   list | restore <id> | rm <id>   回收站（FR-V2）
  ysync versions list|restore <folder> <path>   文件版本（FR-V1）
  ysync share   <folder> <path> [-hours N] [-password pw]   只读分享（FR-H1）
  ysync shares / ysync unshare <token>          分享管理
  ysync install / uninstall                     开机自启（launchd/systemd）
  ysync version
`)
	os.Exit(2)
}

func cmdInit(args []string) error {
	fs := flag.NewFlagSet("init", flag.ExitOnError)
	server := fs.String("server", "http://127.0.0.1:8720", "服务端地址")
	user := fs.String("user", "", "用户名")
	device := fs.String("device", client.DefaultDeviceName(), "设备名")
	fs.Parse(args)
	if *user == "" {
		return fmt.Errorf("需要 -user")
	}
	fmt.Print("password: ")
	pw, err := readPassword()
	if err != nil {
		return err
	}
	api := client.NewAPI(*server, "")
	resp, err := api.Login(*user, pw, *device)
	if err != nil {
		return err
	}
	cfg := &client.Config{
		ServerURL:  strings.TrimRight(*server, "/"),
		User:       *user,
		Token:      resp.Token,
		DeviceName: *device,
		DeviceID:   resp.DeviceID,
	}
	if err := client.SaveConfig(cfg); err != nil {
		return err
	}
	fmt.Printf("已登录为 %s（设备 %s）\n", *user, *device)
	return nil
}

func cmdAdd(args []string) error {
	// Go flag 不支持 flag-after-arg：手动把 flag 与位置参数分开
	var positional []string
	var flagArgs []string
	for i := 0; i < len(args); i++ {
		if args[i] == "-as" || args[i] == "--as" {
			if i+1 >= len(args) {
				return fmt.Errorf("-as 需要参数")
			}
			flagArgs = append(flagArgs, "-as", args[i+1])
			i++
			continue
		}
		if strings.HasPrefix(args[i], "-") {
			flagArgs = append(flagArgs, args[i])
			continue
		}
		positional = append(positional, args[i])
	}
	fs := flag.NewFlagSet("add", flag.ExitOnError)
	as := fs.String("as", "", "服务端子树名（默认取目录名）")
	useGitignore := fs.Bool("use-gitignore", false, "沿用 .gitignore 规则（FR-S8）")
	var excludes excludeFlags
	fs.Var(&excludes, "exclude", "选择性同步排除子树（可多次，FR-S9）")
	fs.Parse(flagArgs)
	if len(positional) != 1 {
		return fmt.Errorf("usage: ysync add <本地目录> [-as 名字]")
	}
	local, err := filepath.Abs(positional[0])
	if err != nil {
		return err
	}
	if fi, err := os.Stat(local); err != nil {
		// 新设备下拉场景：本地目录尚不存在则创建（FR-S15）
		if err := os.MkdirAll(local, 0o755); err != nil {
			return err
		}
	} else if !fi.IsDir() {
		return fmt.Errorf("%s 不是目录", local)
	}
	name := *as
	if name == "" {
		name = filepath.Base(local)
	}
	if strings.Contains(name, "/") || name == "" || name == "." || name == ".." {
		return fmt.Errorf("非法的文件夹名 %q", name)
	}
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	for _, f := range cfg.Folders {
		if f.Name == name {
			return fmt.Errorf("文件夹 %q 已存在", name)
		}
		if samePath(f.LocalPath, local) {
			return fmt.Errorf("目录 %s 已接入（作为 %q）", local, f.Name)
		}
		if isSubPath(f.LocalPath, local) || isSubPath(local, f.LocalPath) {
			return fmt.Errorf("文件夹不得嵌套或重叠（FR-S15）：%s 与 %s", f.LocalPath, local)
		}
	}
	cfg.Folders = append(cfg.Folders, client.Folder{
		Name: name, LocalPath: local, Excludes: excludes, UseGitignore: *useGitignore,
	})
	if err := client.SaveConfig(cfg); err != nil {
		return err
	}
	fmt.Printf("已接入 %q → 服务端子树 %q，执行 ysync sync 开始同步\n", local, name)
	return nil
}

func cmdSync(log *slog.Logger, args []string) error {
	fs := flag.NewFlagSet("sync", flag.ExitOnError)
	only := fs.String("only", "", "仅同步指定文件夹")
	fs.Parse(args)
	return runSync(log, *only)
}

func runSync(log *slog.Logger, only string) error {
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	api := client.NewAPI(cfg.ServerURL, cfg.Token)
	eng := &client.Engine{Cfg: cfg, API: api, Log: log}
	hadErr := false
	for i := range cfg.Folders {
		f := &cfg.Folders[i]
		if only != "" && f.Name != only {
			continue
		}
		stats, err := eng.SyncFolder(f)
		if err != nil {
			log.Error("sync failed", "folder", f.Name, "err", err)
			hadErr = true
			continue
		}
		if stats.Uploaded+stats.Downloaded+stats.Moved+stats.Deleted+stats.Conflicts > 0 {
			log.Info("synced", "folder", f.Name,
				"up", stats.Uploaded, "down", stats.Downloaded,
				"moved", stats.Moved, "deleted", stats.Deleted, "conflicts", stats.Conflicts)
		}
	}
	if hadErr {
		return fmt.Errorf("部分文件夹同步失败")
	}
	return nil
}

func cmdDaemon(log *slog.Logger, args []string) error {
	fs := flag.NewFlagSet("daemon", flag.ExitOnError)
	interval := fs.Duration("interval", 3*time.Second, "轮询间隔（兜底）")
	only := fs.String("only", "", "仅同步指定文件夹")
	httpAddr := fs.String("http", "127.0.0.1:8730", "本地控制 API/状态页地址（off 关闭）")
	reconcile := fs.Duration("reconcile", 5*time.Minute, "全量对账间隔（防事件丢失）")
	fs.Parse(args)

	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	cfg.Defaults()
	api := client.NewAPI(cfg.ServerURL, cfg.Token)
	eng := &client.Engine{Cfg: cfg, API: api, Log: log}
	state := client.NewDaemonState()
	state.InitFolders(cfg.Folders)

	syncOne := func(f *client.Folder) {
		if !state.BeginSync(f.Name) {
			return // 已暂停（M3）
		}
		stats, err := eng.SyncFolder(f)
		if err != nil {
			log.Error("sync failed", "folder", f.Name, "err", err)
			state.FailSync(f, err)
			return
		}
		if stats.Uploaded+stats.Downloaded+stats.Moved+stats.Deleted+stats.Conflicts > 0 {
			log.Info("synced", "folder", f.Name,
				"up", stats.Uploaded, "down", stats.Downloaded,
				"moved", stats.Moved, "deleted", stats.Deleted, "conflicts", stats.Conflicts)
		}
		files := 0
		if st, err := client.OpenState(f.LocalPath); err == nil {
			if m, err := st.All(); err == nil {
				files = len(m)
			}
			st.Close()
		}
		state.FinishSync(f, files, fmt.Sprintf("↑%d ↓%d 移%d 删%d", stats.Uploaded, stats.Downloaded, stats.Moved, stats.Deleted))
		if stats.Conflicts > 0 {
			state.AddConflicts(f.Name, stats.Conflicts)
		}
	}
	syncAll := func() {
		for i := range cfg.Folders {
			f := &cfg.Folders[i]
			if *only != "" && f.Name != *only {
				continue
			}
			syncOne(f)
		}
	}

	// M3：本地控制 API / 状态页（冲突与错误可见性）
	if *httpAddr != "off" {
		ctl := &client.ControlAPI{
			State:   state,
			Trigger: syncAll,
			Log:     log,
		}
		if err := client.ServeControl(*httpAddr, ctl); err != nil {
			log.Warn("控制 API 启动失败（继续运行）", "addr", *httpAddr, "err", err)
		} else {
			log.Info("控制 API/状态页", "addr", "http://"+*httpAddr)
		}
	}

	// M2：FS 事件监听（防抖触发）
	watcher, err := client.NewWatcher(2 * time.Second)
	if err == nil {
		localToName := map[string]string{}
		for i := range cfg.Folders {
			f := cfg.Folders[i]
			if err := watcher.AddRecursive(f.LocalPath); err != nil {
				log.Warn("监听失败", "folder", f.Name, "err", err)
			}
			localToName[f.LocalPath] = f.Name
		}
		go watcher.Run(func(localPath string) {
			name, ok := localToName[localPath]
			if !ok || (*only != "" && name != *only) {
				return
			}
			for i := range cfg.Folders {
				if cfg.Folders[i].Name == name {
					syncOne(&cfg.Folders[i])
				}
			}
		})
		log.Info("FS 事件监听已启用")
	} else {
		log.Warn("FS 事件监听不可用，退化为纯轮询", "err", err)
	}

	// M3：WebSocket 订阅（准实时；断线退化为轮询）
	client.SubscribeNotify(api, syncAll)

	log.Info("daemon started", "interval", interval.String(), "reconcile", reconcile.String())

	// 兜底轮询 + 定时全量对账
	tick := time.NewTicker(*interval)
	defer tick.Stop()
	rc := time.NewTicker(*reconcile)
	defer rc.Stop()
	for {
		select {
		case <-tick.C:
			syncAll()
		case <-rc.C:
			syncAll()
		}
	}
}

func cmdStatus() error {
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	fmt.Printf("server: %s  user: %s  device: %s\n", cfg.ServerURL, cfg.User, cfg.DeviceName)
	if len(cfg.Folders) == 0 {
		fmt.Println("(无同步文件夹，使用 ysync add 接入)")
	}
	for _, f := range cfg.Folders {
		n := 0
		if st, err := client.OpenState(f.LocalPath); err == nil {
			if m, err := st.All(); err == nil {
				n = len(m)
			}
			st.Close()
		}
		fmt.Printf("  %-20s %-40s cursor=%d files=%d\n", f.Name, f.LocalPath, f.Cursor, n)
	}
	return nil
}

// excludeFlags 可重复的 -exclude 项（FR-S9）。
type excludeFlags []string

func (e *excludeFlags) String() string     { return strings.Join(*e, ",") }
func (e *excludeFlags) Set(v string) error { *e = append(*e, strings.Trim(v, "/")); return nil }

func samePath(a, b string) bool { return filepath.Clean(a) == filepath.Clean(b) }

func isSubPath(parent, child string) bool {
	rel, err := filepath.Rel(parent, child)
	if err != nil {
		return false
	}
	return rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}
