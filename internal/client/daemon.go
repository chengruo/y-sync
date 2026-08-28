// daemon 运行时与本地管理界面。
// 架构约定（FR-C3 / §3.5）：同步引擎是唯一的事实来源，GUI/CLI 都是控制 API 的薄客户端。
// 本文件包含：FS 事件监听（防抖）、WebSocket 订阅、兜底轮询、
// 本地控制 API（token 认证）与内置管理页（加文件夹/处理冲突/暂停/恢复）。
package client

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/fsnotify/fsnotify"
	"github.com/gorilla/websocket"
)

// ---------- FS 事件监听（FR-S3：事件 + 防抖） ----------

// Watcher 递归监听文件夹根目录；事件经防抖后回调。
type Watcher struct {
	w        *fsnotify.Watcher
	debounce time.Duration
}

func NewWatcher(debounce time.Duration) (*Watcher, error) {
	w, err := fsnotify.NewWatcher()
	if err != nil {
		return nil, err
	}
	return &Watcher{w: w, debounce: debounce}, nil
}

// AddRecursive 递归添加目录监听（跳过 .y-sync/.git 等）。
func (w *Watcher) AddRecursive(root string) error {
	return filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			switch d.Name() {
			case ".y-sync", ".git", ".svn", ".hg", "node_modules":
				return filepath.SkipDir
			}
			return w.w.Add(p)
		}
		return nil
	})
}

// Run 阻塞循环：per-folder 防抖触发 onSync(目录绝对路径)。
func (w *Watcher) Run(onSync func(localPath string)) {
	var mu sync.Mutex
	timers := map[string]*time.Timer{}
	fire := func(p string) {
		mu.Lock()
		if t := timers[p]; t != nil {
			t.Stop()
		}
		timers[p] = time.AfterFunc(w.debounce, func() { onSync(p) })
		mu.Unlock()
	}
	for {
		select {
		case ev, ok := <-w.w.Events:
			if !ok {
				return
			}
			if ev.Op&fsnotify.Create != 0 {
				if d, err := os.Stat(ev.Name); err == nil && d.IsDir() {
					w.w.Add(ev.Name)
				}
			}
			fire(filepath.Dir(ev.Name))
		case _, ok := <-w.w.Errors:
			if !ok {
				return
			}
		}
	}
}

// ---------- WebSocket 订阅（§4.2 notify） ----------

// SubscribeNotify 连接服务端 WS；断线自动重连（指数退避封顶 30s），收到通知即回调。
func SubscribeNotify(api *API, onNotify func()) {
	go func() {
		backoff := time.Second
		for {
			url := strings.Replace(api.BaseURL, "http", "ws", 1) + "/api/v1/notify?token=" + api.Token
			conn, _, err := websocket.DefaultDialer.Dial(url, nil)
			if err != nil {
				time.Sleep(backoff)
				backoff *= 2
				if backoff > 30*time.Second {
					backoff = 30 * time.Second
				}
				continue
			}
			backoff = time.Second
			for {
				if _, _, err := conn.ReadMessage(); err != nil {
					break
				}
				onNotify() // 只推事件不推数据：客户端拉增量
			}
			conn.Close()
		}
	}()
}

// ---------- daemon 运行时 ----------

// Daemon 捆绑配置、引擎、状态与控制服务；Run 阻塞运行。
type Daemon struct {
	Cfg      *Config
	API      *API
	Engine   *Engine
	State    *DaemonState
	Log      *slog.Logger
	Only     string // -only 过滤
	HTTPAddr string // 控制服务地址（"off" 关闭）

	cfgMu   sync.Mutex // 保护 Cfg.Folders 的并发修改
	watcher *Watcher
	token   string // 管理页/API 认证 token（每次 daemon 启动随机生成）
}

// Run 阻塞：启动控制服务、FS 监听、WS 订阅与兜底轮询。
func (d *Daemon) Run(interval, reconcile time.Duration) {
	d.Log.Info("daemon started", "interval", interval.String(), "reconcile", reconcile.String())
	d.State.InitFolders(d.Cfg.Folders)

	// 控制服务（写操作需要 token）
	if d.HTTPAddr != "off" {
		d.token = randomToken()
		if err := d.serveControl(); err != nil {
			d.Log.Warn("控制 API 启动失败（继续运行）", "addr", d.HTTPAddr, "err", err)
		}
		defer d.clearDaemonInfo()
	}

	// FS 事件监听
	if w, err := NewWatcher(2 * time.Second); err == nil {
		d.watcher = w
		d.cfgMu.Lock()
		for i := range d.Cfg.Folders {
			f := d.Cfg.Folders[i]
			if err := w.AddRecursive(f.LocalPath); err != nil {
				d.Log.Warn("监听失败", "folder", f.Name, "err", err)
			}
		}
		d.cfgMu.Unlock()
		go w.Run(d.SyncByLocalPath)
		d.Log.Info("FS 事件监听已启用")
	} else {
		d.Log.Warn("FS 事件监听不可用，退化为纯轮询", "err", err)
	}

	// WebSocket 订阅（准实时；断线退化为轮询）
	SubscribeNotify(d.API, d.SyncAll)

	tick := time.NewTicker(interval)
	defer tick.Stop()
	rc := time.NewTicker(reconcile)
	defer rc.Stop()
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	for {
		select {
		case <-tick.C:
			d.SyncAll()
		case <-rc.C:
			d.SyncAll()
		case <-sig:
			d.Log.Info("daemon 退出（SIGTERM/SIGINT）")
			if d.HTTPAddr != "off" {
				d.clearDaemonInfo()
			}
			return
		}
	}
}

func randomToken() string {
	raw := make([]byte, 24)
	rand.Read(raw)
	return hex.EncodeToString(raw)
}

// ---------- 同步入口（供轮询/事件/WS/控制 API 复用） ----------

func (d *Daemon) syncOne(f *Folder) {
	if !d.State.BeginSync(f.Name) {
		return // 已暂停
	}
	stats, err := d.Engine.SyncFolder(f)
	if err != nil {
		d.Log.Error("sync failed", "folder", f.Name, "err", err)
		d.State.FailSync(f, err)
		return
	}
	if stats.Uploaded+stats.Downloaded+stats.Moved+stats.Deleted+stats.Conflicts > 0 {
		d.Log.Info("synced", "folder", f.Name,
			"up", stats.Uploaded, "down", stats.Downloaded,
			"moved", stats.Moved, "deleted", stats.Deleted, "conflicts", stats.Conflicts)
	}
	files := 0
	if st, err := OpenState(f.LocalPath); err == nil {
		if m, err := st.All(); err == nil {
			files = len(m)
		}
		st.Close()
	}
	d.State.FinishSync(f, files, fmt.Sprintf("↑%d ↓%d 移%d 删%d",
		stats.Uploaded, stats.Downloaded, stats.Moved, stats.Deleted))
	if stats.Conflicts > 0 {
		d.State.AddConflicts(f.Name, stats.Conflicts)
	}
}

// SyncAll 遍历全部（或 -only 过滤的）文件夹同步一轮。加锁避免与管理操作竞争配置。
func (d *Daemon) SyncAll() {
	d.cfgMu.Lock()
	defer d.cfgMu.Unlock()
	for i := range d.Cfg.Folders {
		f := &d.Cfg.Folders[i]
		if d.Only != "" && f.Name != d.Only {
			continue
		}
		d.syncOne(f)
	}
}

// SyncByLocalPath 由本地路径定位文件夹并同步（FS 事件用）。
func (d *Daemon) SyncByLocalPath(localPath string) {
	abs, err := filepath.Abs(localPath)
	if err != nil {
		return
	}
	d.cfgMu.Lock()
	defer d.cfgMu.Unlock()
	for i := range d.Cfg.Folders {
		f := &d.Cfg.Folders[i]
		if filepath.Clean(f.LocalPath) == filepath.Clean(abs) && (d.Only == "" || d.Only == f.Name) {
			d.syncOne(f)
			return
		}
	}
}

// ---------- 管理操作（控制 API 调用） ----------

// AddFolder 接入新文件夹（与 CLI add 同一套校验，FR-S15）。
func (d *Daemon) AddFolder(localPath, name string, excludes []string, useGitignore bool) error {
	abs, err := filepath.Abs(localPath)
	if err != nil {
		return err
	}
	if fi, err := os.Stat(abs); err != nil {
		if err := os.MkdirAll(abs, 0o755); err != nil {
			return err
		}
	} else if !fi.IsDir() {
		return fmt.Errorf("%s 不是目录", abs)
	}
	if name == "" {
		name = filepath.Base(abs)
	}
	if name == "" || name == "." || name == ".." || strings.Contains(name, "/") {
		return fmt.Errorf("非法的文件夹名 %q", name)
	}
	d.cfgMu.Lock()
	defer d.cfgMu.Unlock()
	for _, f := range d.Cfg.Folders {
		if f.Name == name {
			return fmt.Errorf("文件夹 %q 已存在", name)
		}
		if isSubPathOf(f.LocalPath, abs) || isSubPathOf(abs, f.LocalPath) {
			return fmt.Errorf("文件夹不得嵌套或重叠（FR-S15）：%s 与 %s", f.LocalPath, abs)
		}
	}
	d.Cfg.Folders = append(d.Cfg.Folders, Folder{
		Name: name, LocalPath: abs, Excludes: excludes, UseGitignore: useGitignore,
	})
	if err := SaveConfig(d.Cfg); err != nil {
		d.Cfg.Folders = d.Cfg.Folders[:len(d.Cfg.Folders)-1]
		return err
	}
	d.State.InitFolders(d.Cfg.Folders)
	if d.watcher != nil {
		d.watcher.AddRecursive(abs)
	}
	d.Log.Info("folder added (via UI)", "name", name, "local", abs)
	return nil
}

// RemoveFolder 解除跟踪（保留服务端副本与本地文件，FR-S15）。
func (d *Daemon) RemoveFolder(name string) error {
	d.cfgMu.Lock()
	defer d.cfgMu.Unlock()
	idx := -1
	for i := range d.Cfg.Folders {
		if d.Cfg.Folders[i].Name == name {
			idx = i
			break
		}
	}
	if idx < 0 {
		return fmt.Errorf("文件夹 %q 不存在", name)
	}
	f := d.Cfg.Folders[idx]
	d.Cfg.Folders = append(d.Cfg.Folders[:idx], d.Cfg.Folders[idx+1:]...)
	if err := SaveConfig(d.Cfg); err != nil {
		d.Cfg.Folders = append(d.Cfg.Folders, f) // 回滚
		return err
	}
	d.State.Forget(name)
	d.Log.Info("folder removed (via UI)", "name", name)
	return nil
}

// Conflicts 列出全部文件夹的冲突副本。
func (d *Daemon) Conflicts() []Conflict {
	d.cfgMu.Lock()
	defer d.cfgMu.Unlock()
	var out []Conflict
	for i := range d.Cfg.Folders {
		f := d.Cfg.Folders[i]
		cs, err := ListConflicts(f.LocalPath, f.Name)
		if err != nil {
			continue
		}
		out = append(out, cs...)
	}
	return out
}

// ResolveConflict 处理一条冲突：choice = "local"（保留原名）| "copy"（采用副本）。
// copy_rel 唯一定位冲突副本（同一原文件可能存在多条副本）。
// 处理是纯文件操作，后续同步自动传播到服务端与其他设备。
func (d *Daemon) ResolveConflict(folderName, rel, copyRel, choice string) error {
	d.cfgMu.Lock()
	defer d.cfgMu.Unlock()
	for i := range d.Cfg.Folders {
		f := &d.Cfg.Folders[i]
		if f.Name != folderName {
			continue
		}
		cs, err := ListConflicts(f.LocalPath, f.Name)
		if err != nil {
			return err
		}
		for _, c := range cs {
			if c.Rel != rel || c.CopyRel != copyRel {
				continue
			}
			switch choice {
			case "local":
				return ResolveKeepLocal(f.LocalPath, c)
			case "copy":
				return ResolveKeepCopy(f.LocalPath, c)
			default:
				return fmt.Errorf("choice 必须是 local 或 copy")
			}
		}
		return fmt.Errorf("未找到 %q 的冲突副本（%s）", rel, copyRel)
	}
	return fmt.Errorf("文件夹 %q 不存在", folderName)
}

func isSubPathOf(parent, child string) bool {
	rel, err := filepath.Rel(parent, child)
	if err != nil {
		return false
	}
	return rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

// ---------- 控制服务（token 认证） ----------

type daemonInfo struct {
	PID     int    `json:"pid"`
	Addr    string `json:"addr"`
	Token   string `json:"token"`
	Started int64  `json:"started"`
}

// DaemonInfoPath daemon 运行信息文件（ysync ui 用）。
func DaemonInfoPath() (string, error) {
	p, err := ConfigPath()
	if err != nil {
		return "", err
	}
	return filepath.Join(filepath.Dir(p), "daemon.json"), nil
}

func (d *Daemon) writeDaemonInfo() {
	p, err := DaemonInfoPath()
	if err != nil {
		return
	}
	b, _ := json.Marshal(daemonInfo{PID: os.Getpid(), Addr: d.HTTPAddr, Token: d.token, Started: time.Now().Unix()})
	os.WriteFile(p, b, 0o600)
}

func (d *Daemon) clearDaemonInfo() {
	if p, err := DaemonInfoPath(); err == nil {
		os.Remove(p)
	}
}

// ReadDaemonInfo 读取运行中的 daemon 信息（ysync ui 用）；尽力校验 pid 存活。
func ReadDaemonInfo() (*daemonInfo, error) {
	p, err := DaemonInfoPath()
	if err != nil {
		return nil, err
	}
	b, err := os.ReadFile(p)
	if err != nil {
		return nil, fmt.Errorf("daemon 未运行（先执行 ysync daemon）")
	}
	var info daemonInfo
	if err := json.Unmarshal(b, &info); err != nil {
		return nil, fmt.Errorf("daemon 信息损坏")
	}
	if runtime.GOOS != "windows" && info.PID > 0 {
		if proc, err := os.FindProcess(info.PID); err == nil {
			if proc.Signal(syscall.Signal(0)) != nil {
				return nil, fmt.Errorf("daemon 未运行（残留信息，PID %d）", info.PID)
			}
		}
	}
	return &info, nil
}

func (d *Daemon) serveControl() error {
	ln, err := net.Listen("tcp", d.HTTPAddr)
	if err != nil {
		return err
	}
	d.writeDaemonInfo()
	mux := http.NewServeMux()

	// 认证：数据/写操作要求 ?token= 或 Bearer 匹配（管理 token 仅本机随机生成）
	authed := func(next http.HandlerFunc) http.HandlerFunc {
		return func(w http.ResponseWriter, r *http.Request) {
			tok := r.URL.Query().Get("token")
			if tok == "" {
				tok = strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
			}
			if tok != d.token {
				http.Error(w, "unauthorized", 401)
				return
			}
			next(w, r)
		}
	}

	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		writeJSONLocal(w, map[string]string{"status": "ok"})
	})
	mux.HandleFunc("GET /", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		fmt.Fprintf(w, managementPageHTML, d.token)
	})
	mux.HandleFunc("GET /status", authed(func(w http.ResponseWriter, r *http.Request) {
		writeJSONLocal(w, map[string]any{"folders": d.State.Snapshot()})
	}))
	mux.HandleFunc("GET /conflicts", authed(func(w http.ResponseWriter, r *http.Request) {
		writeJSONLocal(w, map[string]any{"conflicts": d.Conflicts()})
	}))
	mux.HandleFunc("POST /sync", authed(func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Folder string `json:"folder"`
		}
		json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req)
		if req.Folder == "" {
			go d.SyncAll()
		} else {
			go func() {
				d.cfgMu.Lock()
				defer d.cfgMu.Unlock()
				for i := range d.Cfg.Folders {
					if d.Cfg.Folders[i].Name == req.Folder {
						d.syncOne(&d.Cfg.Folders[i])
					}
				}
			}()
		}
		writeJSONLocal(w, map[string]bool{"ok": true})
	}))
	mux.HandleFunc("POST /pause", authed(d.pauseResume(true)))
	mux.HandleFunc("POST /resume", authed(d.pauseResume(false)))
	mux.HandleFunc("POST /add", authed(func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			LocalPath    string   `json:"local_path"`
			Name         string   `json:"name"`
			Excludes     []string `json:"excludes"`
			UseGitignore bool     `json:"use_gitignore"`
		}
		if err := json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req); err != nil {
			http.Error(w, "bad request", 400)
			return
		}
		if err := d.AddFolder(req.LocalPath, req.Name, req.Excludes, req.UseGitignore); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		go d.SyncAll()
		writeJSONLocal(w, map[string]bool{"ok": true})
	}))
	mux.HandleFunc("POST /remove", authed(func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Name string `json:"name"`
		}
		json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req)
		if err := d.RemoveFolder(req.Name); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		writeJSONLocal(w, map[string]bool{"ok": true})
	}))
	mux.HandleFunc("POST /resolve", authed(func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Folder  string `json:"folder"`
			Rel     string `json:"rel"`
			CopyRel string `json:"copy_rel"`
			Choice  string `json:"choice"`
		}
		json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req)
		if err := d.ResolveConflict(req.Folder, req.Rel, req.CopyRel, req.Choice); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		go d.SyncAll()
		writeJSONLocal(w, map[string]bool{"ok": true})
	}))

	srv := &http.Server{Handler: mux}
	go srv.Serve(ln)
	d.Log.Info("控制 API/管理页已启动", "addr", "http://"+d.HTTPAddr+"/?token="+d.token[:8]+"…")
	return nil
}

func (d *Daemon) pauseResume(pause bool) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		name := r.URL.Query().Get("folder")
		if name == "" {
			var req struct {
				Folder string `json:"folder"`
			}
			json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req)
			name = req.Folder
		}
		if pause {
			d.State.Pause(name)
		} else {
			d.State.Resume(name)
		}
		writeJSONLocal(w, map[string]bool{"ok": true})
	}
}

func writeJSONLocal(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(v)
}

// ---------- 内置管理页（无构建步骤的静态 HTML+JS） ----------

// %s 处注入管理 token；%% 转义 fmt.Sprintf。
const managementPageHTML = `<!doctype html>
<html lang="zh"><head><meta charset="utf-8"><title>y-sync 管理台</title>
<style>
 body{font-family:-apple-system,"PingFang SC",sans-serif;max-width:860px;margin:36px auto;padding:0 16px;color:#1f2328}
 h1{font-size:20px} h2{font-size:16px;margin-top:28px}
 table{width:100%%;border-collapse:collapse;margin-top:8px}
 th,td{padding:7px 10px;border-bottom:1px solid #eaeef2;text-align:left;font-size:13.5px}
 .ok{color:#1a7f37}.warn{color:#9a6700}.err{color:#cf222e}
 button{padding:3px 10px;font-size:12.5px;cursor:pointer;border:1px solid #d0d7de;border-radius:6px;background:#f6f8fa}
 button.danger{color:#cf222e}
 .card{border:1px solid #eaeef2;border-radius:8px;padding:12px 14px;margin-top:10px}
 input[type=text]{padding:5px 8px;border:1px solid #d0d7de;border-radius:6px;font-size:13px}
 label{font-size:13px;margin-right:8px}
 #msg{font-size:13px;margin:8px 0;color:#57606a;min-height:1.2em}
 code{background:#f6f8fa;padding:1px 5px;border-radius:4px;font-size:12px}
</style></head><body>
<h1>y-sync 管理台</h1><div id="msg"></div>

<h2>同步文件夹</h2>
<table><thead><tr><th>名称</th><th>本地路径</th><th>文件</th><th>游标</th><th>最近同步</th><th>状态</th><th>操作</th></tr></thead>
<tbody id="rows"></tbody></table>
<p><button onclick="syncAll()">立即全部同步</button></p>

<h2>待处理冲突 <span id="ccount" style="font-weight:normal;color:#57606a"></span></h2>
<div id="conflicts"></div>

<h2>接入新文件夹</h2>
<div class="card">
 <div><label>本地路径</label><input type="text" id="f-path" placeholder="/Users/me/code/my-project" style="width:70%%"></div>
 <div style="margin-top:6px"><label>名称</label><input type="text" id="f-name" placeholder="默认取目录名">
 <label><input type="checkbox" id="f-gi"> 沿用 .gitignore</label>
 <label>排除 <input type="text" id="f-ex" placeholder="node_modules,dist" style="width:30%%"></label></div>
 <div style="margin-top:8px"><button onclick="addFolder()">接入并同步</button></div>
</div>
<p style="font-size:12px;color:#8b949e">冲突处理说明：「保留当前」= 保留原名文件并删除冲突副本；「采用副本」= 用副本内容覆盖原名文件。结果会同步到所有设备。</p>
<script>
const TOKEN = %q;
const api = (p) => p + (p.includes("?") ? "&" : "?") + "token=" + TOKEN;

async function refresh() {
  try {
    const s = await (await fetch(api("/status"))).json();
    const rows = document.getElementById("rows");
    rows.innerHTML = "";
    for (const f of [...s.folders].sort((a,b)=>a.name.localeCompare(b.name))) {
      let state = "空闲", cls = "ok";
      if (f.paused) { state = "已暂停"; cls = "warn"; }
      else if (f.last_error) { state = "错误: " + f.last_error; cls = "err"; }
      else if (f.conflicts_total > 0) { state = "有 " + f.conflicts_total + " 个冲突待处理"; cls = "warn"; }
      else if (f.last_sync) { state = "正常 · " + (f.last_stats || ""); }
      const last = f.last_sync ? new Date(f.last_sync).toLocaleTimeString() : "-";
      rows.insertAdjacentHTML("beforeend",
        "<tr><td><b>" + esc(f.name) + "</b></td><td>" + esc(f.local_path) + "</td><td>" + f.files +
        "</td><td>" + f.cursor + "</td><td>" + last + '</td><td class="' + cls + '">' + esc(state) + "</td>" +
        "<td>" + (f.paused
          ? btn("resume", f.name, "恢复")
          : btn("pause", f.name, "暂停")) +
        ' <button class="danger" onclick=\\'removeFolder("' + esc(f.name) + '")\\'>移除</button></td></tr>');
    }
  } catch (e) { if (String(e).indexOf("401") >= 0) msg("token 无效，请通过 ysync ui 重新打开"); }
  try {
    const c = await (await fetch(api("/conflicts"))).json();
    const box = document.getElementById("conflicts");
    const list = c.conflicts || [];
    document.getElementById("ccount").textContent = "(" + list.length + ")";
    box.innerHTML = list.length ? "" : '<div style="color:#1a7f37;font-size:13px">没有待处理的冲突 ✓</div>';
    for (const it of list) {
      box.insertAdjacentHTML("beforeend", '<div class="card"><b>' + esc(it.folder) + "</b> / " + esc(it.rel) +
        " <code>副本: " + esc(it.copy_rel) + "</code> (" + (it.size/1024).toFixed(1) + " KB)" +
        ' <button onclick=\\'resolve("' + esc(it.folder) + '","' + esc(it.rel) + '","local")\\'>保留当前</button>' +
        ' <button onclick=\\'resolve("' + esc(it.folder) + '","' + esc(it.rel) + '","copy")\\'>采用副本</button></div>');
    }
  } catch (e) {}
}
function btn(path, folder, label) {
  return '<button onclick=\\'op("' + path + '","' + esc(folder) + '")\\'>' + label + "</button>";
}
async function op(path, folder) {
  const r = await fetch(api(path), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder})});
  if (!r.ok) msg("操作失败: " + await r.text()); else msg("已执行 " + path + (folder ? " " + folder : ""));
  refresh();
}
async function syncAll() { await op("/sync", ""); }
async function removeFolder(name) {
  if (!confirm("解除跟踪 " + name + "？（本地文件与服务端副本都保留）")) return;
  const r = await fetch(api("/remove"), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({name})});
  if (!r.ok) msg("失败: " + await r.text());
  refresh();
}
async function addFolder() {
  const body = {
    local_path: document.getElementById("f-path").value.trim(),
    name: document.getElementById("f-name").value.trim(),
    use_gitignore: document.getElementById("f-gi").checked,
    excludes: document.getElementById("f-ex").value.split(",").map(s=>s.trim()).filter(Boolean)
  };
  if (!body.local_path) { msg("请填写本地路径"); return; }
  const r = await fetch(api("/add"), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body)});
  if (!r.ok) msg("接入失败: " + await r.text());
  else { msg("已接入 " + body.local_path); document.getElementById("f-path").value=""; document.getElementById("f-name").value=""; }
  refresh();
}
async function resolve(folder, rel, copyRel, choice) {
  const r = await fetch(api("/resolve"), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, copy_rel: copyRel, choice})});
  if (!r.ok) msg("失败: " + await r.text()); else msg("冲突已处理，同步传播中");
  refresh();
}
function esc(s) { return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/"/g,"&quot;"); }
function msg(t) { document.getElementById("msg").textContent = t; }
refresh();
setInterval(refresh, 3000);
</script></body></html>`
