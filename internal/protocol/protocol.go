// Package protocol 定义 y-sync v1 客户端/服务端共享的协议类型。
// 元数据（JSON）与内容（原始字节流）在此分离：类型只描述元数据。
package protocol

// Op 常量：变更/操作类型
const (
	OpPut    = "put"
	OpMkdir  = "mkdir"
	OpMove   = "move"
	OpUnlink = "unlink"
)

// Node 类型常量
const (
	TypeFile = "file"
	TypeDir  = "dir"
)

// LoginReq POST /api/v1/auth/login
type LoginReq struct {
	User       string `json:"user"`
	Password   string `json:"password"`
	DeviceName string `json:"device_name"`
}

// LoginResp 登录成功返回每设备独立 token。
type LoginResp struct {
	Token      string `json:"token"`
	UserID     int64  `json:"user_id"`
	DeviceID   int64  `json:"device_id"`
	DeviceName string `json:"device_name"`
}

// NodeInfo 服务端文件树中的一个节点。parent_id==0 表示位于用户根目录下。
// Path 是用户空间内的完整相对路径（仅作展示/子树过滤；身份是 ID）。
type NodeInfo struct {
	ID          int64  `json:"id"`
	ParentID    int64  `json:"parent_id"`
	Name        string `json:"name"`
	Type        string `json:"type"`
	Path        string `json:"path"`
	Size        int64  `json:"size"`
	MTime       int64  `json:"mtime"` // unix 毫秒
	ContentHash string `json:"content_hash,omitempty"`
}

// Change 变更日志中的一条记录。客户端凭 cursor 增量拉取。
// 语义：该 node_id 当前位于 Path（unlink 表示已从 Path 移除）。
// 目录移动/重命名会为目录自身及其每个后代各产生一条 move 记录。
type Change struct {
	Cursor      int64  `json:"cursor"`
	DeviceID    int64  `json:"device_id"` // 产生该变更的设备（客户端跳过自己的变更）
	NodeID      int64  `json:"node_id"`
	Op          string `json:"op"`
	Path        string `json:"path"`
	ParentID    int64  `json:"parent_id"`
	Name        string `json:"name"`
	Type        string `json:"type"`
	Size        int64  `json:"size"`
	MTime       int64  `json:"mtime"`
	ContentHash string `json:"content_hash,omitempty"`
}

// ChangesResp GET /api/v1/sync/changes
type ChangesResp struct {
	Cursor  int64    `json:"cursor"` // 服务端当前 head cursor（即使返回条数少于 limit）
	Changes []Change `json:"changes"`
}

// HeadResp GET /api/v1/sync/head
type HeadResp struct {
	Cursor int64 `json:"cursor"`
}

// Op POST /api/v1/ops 批量元数据操作（服务端按序、原子应用）。
// mkdir: ParentID+Name；put: NodeID>0 表示覆盖已有节点内容，否则在 ParentID/Name 新建
// （ContentHash 必须已通过 /content 上传）；move: NodeID 移动到 ParentID+Name；
// unlink: NodeID（目录则递归）。
type Op struct {
	Op          string `json:"op"`
	NodeID      int64  `json:"node_id,omitempty"`
	ParentID    int64  `json:"parent_id,omitempty"`
	Name        string `json:"name"`
	Type        string `json:"type,omitempty"`
	ContentHash string `json:"content_hash,omitempty"`
	Size        int64  `json:"size,omitempty"`
	MTime       int64  `json:"mtime,omitempty"`
}

// OpResult 与 Ops 一一对应。
type OpResult struct {
	Ok     bool   `json:"ok"`
	Error  string `json:"error,omitempty"`
	NodeID int64  `json:"node_id,omitempty"`
	Cursor int64  `json:"cursor,omitempty"`
}

// OpsResp POST /api/v1/ops
type OpsResp struct {
	Results []OpResult `json:"results"`
}

// DedupResp PUT /api/v1/content
type DedupResp struct {
	Hash  string `json:"hash"`
	Dedup bool   `json:"dedup"`
}

// TrashItem 回收站条目（GET /api/v1/trash）。
type TrashItem struct {
	ID        int64  `json:"id"`
	OrigPath  string `json:"orig_path"`
	Name      string `json:"name"`
	Type      string `json:"type"`
	Hash      string `json:"content_hash,omitempty"`
	Size      int64  `json:"size"`
	MTime     int64  `json:"mtime"`
	DeletedAt int64  `json:"deleted_at"`
}

// VersionItem 文件历史版本条目。
type VersionItem struct {
	ID      int64  `json:"id"`
	NodeID  int64  `json:"node_id"`
	Path    string `json:"path"`
	Hash    string `json:"content_hash"`
	Size    int64  `json:"size"`
	MTime   int64  `json:"mtime"`
	Created int64  `json:"created"`
}

// ShareInfo 分享链接（FR-H1）。
type ShareInfo struct {
	Token     string `json:"token"`
	Path      string `json:"path"`
	NodeID    int64  `json:"node_id"`
	HasPwd    bool   `json:"has_password"`
	ExpiresAt int64  `json:"expires_at,omitempty"`
	Created   int64  `json:"created"`
}

// UploadSessionResp POST /api/v1/uploads（分块上传会话，FR-S11）。
type UploadSessionResp struct {
	ID       string  `json:"id"`
	Received []int64 `json:"received"` // 已收到的 chunk 序号（断点续传查询用）
}
