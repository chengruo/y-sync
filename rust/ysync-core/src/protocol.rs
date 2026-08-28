//! 协议类型：与 Go 端 internal/protocol 完全一致的 JSON 形状（单一事实来源的两端镜像）。
use serde::{Deserialize, Serialize};

pub const OP_PUT: &str = "put";
pub const OP_MKDIR: &str = "mkdir";
pub const OP_MOVE: &str = "move";
pub const OP_UNLINK: &str = "unlink";
pub const TYPE_FILE: &str = "file";
pub const TYPE_DIR: &str = "dir";

#[derive(Debug, Clone, Serialize)]
pub struct LoginReq {
    #[serde(rename = "user")]
    pub user: String,
    #[serde(rename = "password")]
    pub password: String,
    #[serde(rename = "device_name")]
    pub device_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResp {
    #[serde(rename = "token", default)]
    pub token: String,
    #[serde(rename = "user_id", default)]
    pub user_id: i64,
    #[serde(rename = "device_id", default)]
    pub device_id: i64,
    #[serde(rename = "device_name", default)]
    pub device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeInfo {
    #[serde(rename = "id", default)]
    pub id: i64,
    #[serde(rename = "parent_id", default)]
    pub parent_id: i64,
    #[serde(rename = "name", default)]
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "path", default)]
    pub path: String,
    #[serde(rename = "size", default)]
    pub size: i64,
    #[serde(rename = "mtime", default)]
    pub mtime: i64,
    #[serde(rename = "content_hash", default, skip_serializing_if = "String::is_empty")]
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    #[serde(rename = "cursor", default)]
    pub cursor: i64,
    #[serde(rename = "device_id", default)]
    pub device_id: i64,
    #[serde(rename = "node_id", default)]
    pub node_id: i64,
    #[serde(rename = "op", default)]
    pub op: String,
    #[serde(rename = "path", default)]
    pub path: String,
    #[serde(rename = "parent_id", default)]
    pub parent_id: i64,
    #[serde(rename = "name", default)]
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "size", default)]
    pub size: i64,
    #[serde(rename = "mtime", default)]
    pub mtime: i64,
    #[serde(rename = "content_hash", default)]
    pub content_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangesResp {
    #[serde(rename = "cursor", default)]
    pub cursor: i64,
    #[serde(rename = "changes", default)]
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeadResp {
    #[serde(rename = "cursor", default)]
    pub cursor: i64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Op {
    #[serde(rename = "op")]
    pub op: String,
    #[serde(rename = "node_id", skip_serializing_if = "is_zero")]
    pub node_id: i64,
    #[serde(rename = "parent_id", skip_serializing_if = "is_zero")]
    pub parent_id: i64,
    #[serde(rename = "name", default)]
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty", default)]
    pub kind: String,
    #[serde(rename = "content_hash", skip_serializing_if = "String::is_empty", default)]
    pub content_hash: String,
    #[serde(rename = "size", skip_serializing_if = "is_zero")]
    pub size: i64,
    #[serde(rename = "mtime", skip_serializing_if = "is_zero")]
    pub mtime: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpResult {
    #[serde(rename = "ok", default)]
    pub ok: bool,
    #[serde(rename = "error", default)]
    pub error: String,
    #[serde(rename = "node_id", default)]
    pub node_id: i64,
    #[serde(rename = "cursor", default)]
    pub cursor: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpsResp {
    #[serde(rename = "results", default)]
    pub results: Vec<OpResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DedupResp {
    #[serde(rename = "hash", default)]
    pub hash: String,
    #[serde(rename = "dedup", default)]
    pub dedup: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrashItem {
    #[serde(rename = "id", default)]
    pub id: i64,
    #[serde(rename = "orig_path", default)]
    pub orig_path: String,
    #[serde(rename = "name", default)]
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "content_hash", default)]
    pub content_hash: String,
    #[serde(rename = "size", default)]
    pub size: i64,
    #[serde(rename = "mtime", default)]
    pub mtime: i64,
    #[serde(rename = "deleted_at", default)]
    pub deleted_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VersionItem {
    #[serde(rename = "id", default)]
    pub id: i64,
    #[serde(rename = "node_id", default)]
    pub node_id: i64,
    #[serde(rename = "path", default)]
    pub path: String,
    #[serde(rename = "content_hash", default)]
    pub content_hash: String,
    #[serde(rename = "size", default)]
    pub size: i64,
    #[serde(rename = "mtime", default)]
    pub mtime: i64,
    #[serde(rename = "created", default)]
    pub created: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSessionResp {
    #[serde(rename = "id", default)]
    pub id: String,
    #[serde(rename = "received", default)]
    pub received: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareInfo {
    #[serde(rename = "token", default)]
    pub token: String,
    #[serde(rename = "path", default)]
    pub path: String,
    #[serde(rename = "node_id", default)]
    pub node_id: i64,
    #[serde(rename = "has_password", default)]
    pub has_password: bool,
    #[serde(rename = "expires_at", default)]
    pub expires_at: i64,
    #[serde(rename = "created", default)]
    pub created: i64,
}

/// daemon 控制端点的 JSON 类型（与 Go internal/client/daemon*.go 对应）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FolderStatus {
    #[serde(rename = "name", default)]
    pub name: String,
    #[serde(rename = "local_path", default)]
    pub local_path: String,
    #[serde(rename = "cursor", default)]
    pub cursor: i64,
    #[serde(rename = "files", default)]
    pub files: i64,
    #[serde(rename = "last_sync", default)]
    pub last_sync: String,
    #[serde(rename = "last_error", default)]
    pub last_error: String,
    #[serde(rename = "conflicts_total", default)]
    pub conflicts_total: i64,
    #[serde(rename = "paused", default)]
    pub paused: bool,
    #[serde(rename = "last_stats", default)]
    pub last_stats: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatusResp {
    #[serde(rename = "folders", default)]
    pub folders: Option<Vec<FolderStatus>>,
}

impl StatusResp {
    pub fn folders_or_empty(&self) -> Vec<FolderStatus> {
        self.folders.clone().unwrap_or_default()
    }
}

/// null 容忍：Go 端历史实现可能输出 null 数组。
pub fn null_to_vec<'de, D, T>(de: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let opt: Option<Vec<T>> = Option::deserialize(de)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Conflict {
    #[serde(rename = "folder", default)]
    pub folder: String,
    #[serde(rename = "rel", default)]
    pub rel: String,
    #[serde(rename = "copy_rel", default)]
    pub copy_rel: String,
    #[serde(rename = "size", default)]
    pub size: i64,
    #[serde(rename = "mtime", default)]
    pub mtime: i64,
}
