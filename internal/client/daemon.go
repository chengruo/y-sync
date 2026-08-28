// daemon 附件：FS 事件监听（防抖）、WebSocket 订阅、控制 API、状态页。
package client

import (
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/fsnotify/fsnotify"
	"github.com/gorilla/websocket"
)

// ---------- FS 事件监听（FR-S3：事件 + 防抖） ----------

// Watcher 递归监听各文件夹根目录；事件经 2s 防抖后回调（per folder 一次）。
type Watcher struct {
	w        *fsnotify.Watcher
	debounce time.Duration
	igCache  sync.Map // localPath -> *Ignore
}

func NewWatcher(debounce time.Duration) (*Watcher, error) {
	w, err := fsnotify.NewWatcher()
	if err != nil {
		return nil, err
	}
	return &Watcher{w: w, debounce: debounce}, nil
}

// AddRecursive 递归添加目录监听（跳过 .y-sync / .git 等默认忽略目录）。
func (w *Watcher) AddRecursive(root string) error {
	return filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			name := d.Name()
			if name == ".y-sync" || name == ".git" || name == ".svn" || name == ".hg" || name == "node_modules" {
				return filepath.SkipDir
			}
			return w.w.Add(p)
		}
		return nil
	})
}

// Run 阻塞循环：per-folder 防抖触发 onSync(folderLocalPath)。
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
			// 新建目录：补挂监听
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

// SubscribeNotify 连接服务端 WS；收到通知即回调。阻塞；断线自动重连（指数退避封顶 30s）。
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
				_, _, err := conn.ReadMessage()
				if err != nil {
					break
				}
				onNotify() // 只推事件不推数据：客户端拉增量
			}
			conn.Close()
		}
	}()
}

// ---------- 控制 API + 状态页（M3） ----------

// ControlAPI daemon 本地控制接口：状态查询、暂停/恢复、手动触发同步。
type ControlAPI struct {
	State   *DaemonState
	Cfg     func() *Config
	Trigger func()
	Log     *slog.Logger
}

func (c *ControlAPI) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /", c.handlePage)
	mux.HandleFunc("GET /status", c.handleStatus)
	mux.HandleFunc("POST /pause", c.pauseResume(true))
	mux.HandleFunc("POST /resume", c.pauseResume(false))
	mux.HandleFunc("POST /sync", func(w http.ResponseWriter, r *http.Request) {
		if c.Trigger != nil {
			c.Trigger()
		}
		writeJSONLocal(w, map[string]bool{"ok": true})
	})
	return mux
}

func writeJSONLocal(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(v)
}

func (c *ControlAPI) handleStatus(w http.ResponseWriter, r *http.Request) {
	writeJSONLocal(w, map[string]any{"folders": c.State.Snapshot()})
}

func (c *ControlAPI) pauseResume(pause bool) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		name := r.URL.Query().Get("folder") // 状态页按钮用 query；也接受 JSON body
		if name == "" {
			var req struct {
				Folder string `json:"folder"`
			}
			json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req)
			name = req.Folder
		}
		if pause {
			c.State.Pause(name)
		} else {
			c.State.Resume(name)
		}
		http.Redirect(w, r, "/", http.StatusSeeOther)
	}
}

func (c *ControlAPI) handlePage(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	fmt.Fprint(w, RenderStatusPage(c.State.Snapshot()))
}

const statusPageHTML = `<!doctype html>
<html lang="zh"><head><meta charset="utf-8"><title>y-sync</title>
<meta http-equiv="refresh" content="5">
<style>
 body{font-family:-apple-system,sans-serif;max-width:760px;margin:40px auto;padding:0 16px;color:#222}
 h1{font-size:20px} table{width:100%;border-collapse:collapse;margin-top:12px}
 th,td{padding:8px 10px;border-bottom:1px solid #eee;text-align:left;font-size:14px}
 .ok{color:#1a7f37}.warn{color:#9a6700}.err{color:#cf222e}
 button{padding:4px 12px;font-size:13px;cursor:pointer}
 form{display:inline}
</style></head><body>
<h1>y-sync 同步状态</h1>
<table><tr><th>文件夹</th><th>文件数</th><th>游标</th><th>最近同步</th><th>状态</th><th></th></tr>
{{ROWS}}
</table>
<p style="font-size:12px;color:#888">页面每 5 秒自动刷新 · 冲突副本以 "conflict from" 命名，需人工取舍</p>
</body></html>`

// RenderStatusPage 由 daemon 用当前快照渲染（{{ROWS}} 占位）。
func RenderStatusPage(snapshot []FolderStatus) string {
	var rows strings.Builder
	for _, s := range snapshot {
		state, cls := "空闲", "ok"
		switch {
		case s.Paused:
			state, cls = "已暂停", "warn"
		case s.LastError != "":
			state, cls = "错误: "+s.LastError, "err"
		case s.Conflicts > 0:
			state, cls = fmt.Sprintf("有 %d 个冲突待处理", s.Conflicts), "warn"
		case !s.LastSync.IsZero():
			state = "正常 · " + s.LastStats
		}
		action := fmt.Sprintf("<form method=\"post\" action=\"/pause?folder=%s\"><button>暂停</button></form>", s.Name)
		if s.Paused {
			action = fmt.Sprintf("<form method=\"post\" action=\"/resume?folder=%s\"><button>恢复</button></form>", s.Name)
		}
		last := "-"
		if !s.LastSync.IsZero() {
			last = s.LastSync.Format("15:04:05")
		}
		fmt.Fprintf(&rows, "<tr><td>%s</td><td>%d</td><td>%d</td><td>%s</td><td class=\"%s\">%s</td><td>%s</td></tr>\n",
			s.Name, s.Files, s.Cursor, last, cls, state, action)
	}
	return strings.Replace(statusPageHTML, "{{ROWS}}", rows.String(), 1)
}

// ServeControl 启动本地控制 HTTP（仅监听 loopback）。
func ServeControl(addr string, c *ControlAPI) error {
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return err
	}
	srv := &http.Server{Handler: c.Handler()}
	go srv.Serve(ln)
	return nil
}
