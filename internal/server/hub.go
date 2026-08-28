// WebSocket 通知 Hub（§4.2 WS /api/v1/notify）：服务端变更后向同用户在线设备
// 推送"有新 cursor"提示，客户端收到后立即拉取增量。只推事件，不推数据。
package server

import (
	"encoding/json"
	"log/slog"
	"net/http"
	"sync"
	"time"

	"github.com/gorilla/websocket"

	"ysync/internal/protocol"
)

type wsClient struct {
	userID int64
	conn   *websocket.Conn
	send   chan []byte
}

// Hub 按用户分组管理连接；ApplyOps 成功后调用 Notify。
type Hub struct {
	mu     sync.Mutex
	byUser map[int64]map[*wsClient]bool
	log    *slog.Logger
}

func NewHub() *Hub {
	return &Hub{byUser: map[int64]map[*wsClient]bool{}}
}

func (h *Hub) SetLogger(l *slog.Logger) { h.log = l }

func (h *Hub) add(userID int64, c *wsClient) {
	h.mu.Lock()
	if h.byUser[userID] == nil {
		h.byUser[userID] = map[*wsClient]bool{}
	}
	h.byUser[userID][c] = true
	h.mu.Unlock()
}

func (h *Hub) remove(userID int64, c *wsClient) {
	// 读泵与写泵都会调用：仅在连接仍在册时关闭 send（幂等）
	h.mu.Lock()
	set := h.byUser[userID]
	if set == nil || !set[c] {
		h.mu.Unlock()
		return
	}
	delete(set, c)
	if len(set) == 0 {
		delete(h.byUser, userID)
	}
	h.mu.Unlock()
	close(c.send)
}

// Notify 通知某用户的所有在线设备（非阻塞，慢连接直接丢弃）。
func (h *Hub) Notify(userID int64, msg []byte) {
	h.mu.Lock()
	defer h.mu.Unlock()
	for c := range h.byUser[userID] {
		select {
		case c.send <- msg:
		default:
		}
	}
}

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
	CheckOrigin:     func(r *http.Request) bool { return true },
}

const wsWriteWait = 10 * time.Second

// ServeWS 处理 /api/v1/notify 升级（token 经查询参数传递，兼容浏览器端）。
func (s *Server) ServeWS(w http.ResponseWriter, r *http.Request) {
	token := r.URL.Query().Get("token")
	uid, _, err := s.store.AuthToken(token)
	if err != nil {
		writeErr(w, http.StatusUnauthorized, "unauthorized")
		return
	}
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	c := &wsClient{userID: uid, conn: conn, send: make(chan []byte, 8)}
	s.Hub.add(uid, c)

	// 写泵
	go func() {
		defer func() {
			s.Hub.remove(uid, c)
			conn.Close()
		}()
		for msg := range c.send {
			conn.SetWriteDeadline(time.Now().Add(wsWriteWait))
			if err := conn.WriteMessage(websocket.TextMessage, msg); err != nil {
				return
			}
		}
	}()
	// 读泵：仅保活/检测断开
	go func() {
		defer func() {
			s.Hub.remove(uid, c)
			conn.Close()
		}()
		conn.SetReadLimit(4096)
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				return
			}
		}
	}()
	// 接入即告知当前 head，客户端可校准
	if head, err := s.store.HeadCursor(uid); err == nil {
		b, _ := json.Marshal(protocol.HeadResp{Cursor: head})
		select {
		case c.send <- b:
		default:
		}
	}
}
