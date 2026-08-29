//! 服务端存储层：SQLite（WAL）元数据 + 变更日志 + 引用计数。
//! 从 Go internal/server/store.go 与 versions.go 移植，语义逐一对齐。
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, Transaction};

use crate::blob::BlobStore;
use crate::util::{base64, hex, now_millis, now_secs, sha256_hex, unhex};

pub struct Store {
    pub db: Mutex<Connection>,
    pub blobs: BlobStore,
    pub max_versions: i64,
    pub trash_retention_days: i64,
}

// ---------- 协议返回结构 ----------

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NodeInfo {
    #[serde(rename = "id")]
    pub id: i64,
    #[serde(rename = "parent_id")]
    pub parent_id: i64,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "path")]
    pub path: String,
    #[serde(rename = "size")]
    pub size: i64,
    #[serde(rename = "mtime")]
    pub mtime: i64,
    #[serde(rename = "content_hash", skip_serializing_if = "String::is_empty")]
    pub content_hash: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Change {
    #[serde(rename = "cursor")]
    pub cursor: i64,
    #[serde(rename = "device_id")]
    pub device_id: i64,
    #[serde(rename = "node_id")]
    pub node_id: i64,
    #[serde(rename = "op")]
    pub op: String,
    #[serde(rename = "path")]
    pub path: String,
    #[serde(rename = "parent_id")]
    pub parent_id: i64,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "size")]
    pub size: i64,
    #[serde(rename = "mtime")]
    pub mtime: i64,
    #[serde(rename = "content_hash", skip_serializing_if = "String::is_empty")]
    pub content_hash: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct OpResult {
    #[serde(rename = "ok")]
    pub ok: bool,
    #[serde(rename = "error", skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(rename = "node_id", skip_serializing_if = "is_zero")]
    pub node_id: i64,
    #[serde(rename = "cursor", skip_serializing_if = "is_zero")]
    pub cursor: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrashItem {
    #[serde(rename = "id")]
    pub id: i64,
    #[serde(rename = "orig_path")]
    pub orig_path: String,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "content_hash", skip_serializing_if = "String::is_empty")]
    pub hash: String,
    #[serde(rename = "size")]
    pub size: i64,
    #[serde(rename = "mtime")]
    pub mtime: i64,
    #[serde(rename = "deleted_at")]
    pub deleted_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VersionItem {
    #[serde(rename = "id")]
    pub id: i64,
    #[serde(rename = "node_id")]
    pub node_id: i64,
    #[serde(rename = "path")]
    pub path: String,
    #[serde(rename = "content_hash")]
    pub hash: String,
    #[serde(rename = "size")]
    pub size: i64,
    #[serde(rename = "mtime")]
    pub mtime: i64,
    #[serde(rename = "created")]
    pub created: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareInfo {
    #[serde(rename = "token")]
    pub token: String,
    #[serde(rename = "path")]
    pub path: String,
    #[serde(rename = "node_id")]
    pub node_id: i64,
    #[serde(rename = "has_password")]
    pub has_password: bool,
    #[serde(rename = "expires_at", skip_serializing_if = "is_zero")]
    pub expires_at: i64,
    #[serde(rename = "created")]
    pub created: i64,
}

pub const ERR_NOT_FOUND: &str = "not found";

#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub users: i64,
    pub devices: i64,
    pub files: i64,
    pub dirs: i64,
    pub blobs: i64,
    pub blob_bytes: i64,
    pub shares: i64,
    pub trash: i64,
}

fn to_serr(e: rusqlite::Error) -> String {
    format!("db: {e}")
}

/// 内容是否为清单（CDC 分块文件的内容即清单，GET 时重组装）。
fn is_manifest(conn: &Connection, hash: &str) -> bool {
    conn.query_row("SELECT 1 FROM manifests WHERE hash=?1", [hash], |_| Ok(()))
        .is_ok()
}



impl Store {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir.join("tmp")).map_err(|e| format!("{e}"))?;
        let conn = Connection::open(data_dir.join("y-sync.db")).map_err(to_serr)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS users(
               id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, pass_hash TEXT NOT NULL, created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS devices(
               id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id),
               name TEXT NOT NULL, token_hash TEXT NOT NULL UNIQUE, last_seen INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS nodes(
               id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL,
               parent_id INTEGER NOT NULL DEFAULT 0, name TEXT NOT NULL, type TEXT NOT NULL,
               content_hash TEXT NOT NULL DEFAULT '', size INTEGER NOT NULL DEFAULT 0,
               mtime INTEGER NOT NULL DEFAULT 0, path TEXT NOT NULL);
             CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_user_path ON nodes(user_id, path);
             CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(user_id, parent_id);
             CREATE TABLE IF NOT EXISTS blobs(
               hash TEXT PRIMARY KEY, size INTEGER NOT NULL, refcount INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS changes(
               cursor INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, device_id INTEGER NOT NULL DEFAULT 0,
               node_id INTEGER NOT NULL, op TEXT NOT NULL, path TEXT NOT NULL, parent_id INTEGER NOT NULL,
               name TEXT NOT NULL, type TEXT NOT NULL, content_hash TEXT NOT NULL DEFAULT '',
               size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS idx_changes_user ON changes(user_id, cursor);
             CREATE TABLE IF NOT EXISTS versions(
               id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, node_id INTEGER NOT NULL,
               path TEXT NOT NULL, content_hash TEXT NOT NULL, size INTEGER NOT NULL,
               mtime INTEGER NOT NULL, created INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_versions ON versions(user_id, node_id, id);
             CREATE TABLE IF NOT EXISTS trash(
               id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, orig_path TEXT NOT NULL,
               name TEXT NOT NULL, type TEXT NOT NULL, content_hash TEXT NOT NULL DEFAULT '',
               size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL, deleted_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_trash ON trash(user_id, deleted_at);
             CREATE TABLE IF NOT EXISTS shares(
               id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, token TEXT NOT NULL UNIQUE,
               node_id INTEGER NOT NULL, password_hash TEXT NOT NULL DEFAULT '',
               expires_at INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS meta(
               key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS manifests(
               hash TEXT PRIMARY KEY, size INTEGER NOT NULL, chunks_json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS manifest_chunks(
               file_hash TEXT NOT NULL, chunk_hash TEXT NOT NULL, idx INTEGER NOT NULL,
               PRIMARY KEY(file_hash, idx));",
        )
        .map_err(to_serr)?;
        // 幂等迁移：quota_bytes（P1-8）
        let has_quota: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('users') WHERE name='quota_bytes'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_quota == 0 {
            conn.execute(
                "ALTER TABLE users ADD COLUMN quota_bytes INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(to_serr)?;
        }
        Ok(Store {
            db: Mutex::new(conn),
            blobs: BlobStore::new(data_dir),
            max_versions: 10,
            trash_retention_days: 30,
        })
    }

    // ---------- 用户与设备 ----------

    pub fn create_user(&self, name: &str, password: &str, quota_bytes: i64) -> Result<i64, String> {
        if name.is_empty() || password.is_empty() || name.contains([' ', '\t', '/']) {
            return Err("invalid user name".into());
        }
        let salt: [u8; 16] = rand_bytes16();
        let key = argon2id_key(password, &salt);
        let hash = format!(
            "argon2id${}${}",
            base64(&salt).trim_end_matches('='),
            base64(&key).trim_end_matches('=')
        );
        let conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT INTO users(name, pass_hash, created, quota_bytes) VALUES(?1,?2,?3,?4)",
            rusqlite::params![name, hash, now_secs(), quota_bytes],
        )
        .map_err(to_serr)?;
        Ok(conn.last_insert_rowid())
    }

    /// list-users + 用量/配额（管理用）。
    pub fn list_users_with_usage(&self) -> Vec<serde_json::Value> {
        let conn = self.db.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT u.name, u.quota_bytes, COALESCE(SUM(CASE WHEN n.type='file' THEN n.size ELSE 0 END),0)
             FROM users u LEFT JOIN nodes n ON n.user_id = u.id GROUP BY u.id ORDER BY u.id",
        ) else { return Vec::new() };
        let rows = stmt
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "name": r.get::<_, String>(0)?,
                    "quota_bytes": r.get::<_, i64>(1)?,
                    "used_bytes": r.get::<_, i64>(2)?,
                }))
            })
            .unwrap_or_else(|_| panic!());
        rows.filter_map(|r| r.ok()).collect()
    }

    pub fn authenticate(&self, name: &str, password: &str) -> Result<i64, String> {
        let conn = self.db.lock().unwrap();
        let hash: Option<String> = conn
            .query_row(
                "SELECT pass_hash FROM users WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .ok();
        let Some(hash) = hash else {
            // 常量时间兜底，避免用户枚举
            let _ = argon2id_key(password, &[0u8; 16]);
            return Err(ERR_NOT_FOUND.into());
        };
        if !verify_password(password, &hash) {
            return Err(ERR_NOT_FOUND.into());
        }
        let id: i64 = conn
            .query_row("SELECT id FROM users WHERE name = ?1", [name], |r| r.get(0))
            .map_err(to_serr)?;
        Ok(id)
    }

    pub fn list_users(&self) -> Vec<String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT name FROM users ORDER BY id") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    pub fn reset_password(&self, name: &str, password: &str) -> Result<(), String> {
        if password.is_empty() {
            return Err("empty password".into());
        }
        let salt: [u8; 16] = rand_bytes16();
        let key = argon2id_key(password, &salt);
        let hash = format!(
            "argon2id${}${}",
            base64(&salt).trim_end_matches('='),
            base64(&key).trim_end_matches('=')
        );
        let conn = self.db.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE users SET pass_hash=?1 WHERE name=?2",
                rusqlite::params![hash, name],
            )
            .map_err(to_serr)?;
        if n == 0 {
            return Err(ERR_NOT_FOUND.into());
        }
        conn.execute(
            "DELETE FROM devices WHERE user_id=(SELECT id FROM users WHERE name=?1)",
            [name],
        )
        .map_err(to_serr)?;
        Ok(())
    }

    pub fn create_device(&self, user_id: i64, device_name: &str) -> Result<(i64, String), String> {
        let raw: Vec<u8> = (0..32).map(|_| rand_byte()).collect();
        let token = hex(&raw);
        let th = sha256_hex(token.as_bytes());
        let conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT INTO devices(user_id, name, token_hash, last_seen) VALUES(?1,?2,?3,?4)",
            rusqlite::params![user_id, device_name, th, now_secs()],
        )
        .map_err(to_serr)?;
        Ok((conn.last_insert_rowid(), token))
    }

    /// 校验 Bearer token → (user_id, device_id)。
    pub fn auth_token(&self, token: &str) -> Result<(i64, i64), String> {
        if token.is_empty() {
            return Err(ERR_NOT_FOUND.into());
        }
        let th = sha256_hex(token.as_bytes());
        let conn = self.db.lock().unwrap();
        let row: (i64, i64) = conn
            .query_row(
                "SELECT user_id, id FROM devices WHERE token_hash = ?1",
                [&th],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| ERR_NOT_FOUND.to_string())?;
        let _ = conn.execute(
            "UPDATE devices SET last_seen=?1 WHERE id=?2",
            rusqlite::params![now_secs(), row.1],
        );
        Ok(row)
    }

    // ---------- 节点树 ----------

    fn node_path(conn: &Connection, user_id: i64, node_id: i64) -> Result<String, String> {
        if node_id == 0 {
            return Ok(String::new());
        }
        conn.query_row(
            "SELECT path FROM nodes WHERE user_id=?1 AND id=?2",
            rusqlite::params![user_id, node_id],
            |r| r.get(0),
        )
        .map_err(|_| ERR_NOT_FOUND.to_string())
    }

    fn node_by_id(conn: &Connection, user_id: i64, node_id: i64) -> Result<NodeInfo, String> {
        if node_id == 0 {
            return Err(ERR_NOT_FOUND.into());
        }
        conn.query_row(
            "SELECT id, parent_id, name, type, path, size, mtime, content_hash
             FROM nodes WHERE user_id=?1 AND id=?2",
            rusqlite::params![user_id, node_id],
            |r| {
                Ok(NodeInfo {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    name: r.get(2)?,
                    kind: r.get(3)?,
                    path: r.get(4)?,
                    size: r.get(5)?,
                    mtime: r.get(6)?,
                    content_hash: r.get(7)?,
                })
            },
        )
        .map_err(|_| ERR_NOT_FOUND.to_string())
    }

    fn node_by_path(conn: &Connection, user_id: i64, p: &str) -> Result<NodeInfo, String> {
        conn.query_row(
            "SELECT id, parent_id, name, type, path, size, mtime, content_hash
             FROM nodes WHERE user_id=?1 AND path=?2",
            rusqlite::params![user_id, p],
            |r| {
                Ok(NodeInfo {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    name: r.get(2)?,
                    kind: r.get(3)?,
                    path: r.get(4)?,
                    size: r.get(5)?,
                    mtime: r.get(6)?,
                    content_hash: r.get(7)?,
                })
            },
        )
        .map_err(|_| ERR_NOT_FOUND.to_string())
    }

    fn journal_change(
        tx: &Transaction,
        user_id: i64,
        device_id: i64,
        n: &NodeInfo,
        op: &str,
    ) -> Result<i64, String> {
        tx.execute(
            "INSERT INTO changes(user_id, device_id, node_id, op, path, parent_id, name, type, content_hash, size, mtime)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                user_id, device_id, n.id, op, n.path, n.parent_id, n.name, n.kind,
                n.content_hash, n.size, n.mtime
            ],
        )
        .map_err(to_serr)?;
        Ok(tx.last_insert_rowid())
    }

    fn tx_cursor(tx: &Transaction) -> Result<i64, String> {
        tx.query_row("SELECT MAX(cursor) FROM changes", [], |r| r.get(0))
            .map_err(to_serr)
    }

    fn list_descendants(tx: &Transaction, user_id: i64, dir_path: &str) -> Result<Vec<NodeInfo>, String> {
        let upper = format!("{dir_path}/\u{10FFFF}");
        let mut stmt = tx
            .prepare(
                "SELECT id, parent_id, name, type, path, size, mtime, content_hash
                 FROM nodes WHERE user_id=?1 AND path>?2 AND path<?3 ORDER BY path",
            )
            .map_err(to_serr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![user_id, format!("{dir_path}/"), upper],
                |r| {
                    Ok(NodeInfo {
                        id: r.get(0)?,
                        parent_id: r.get(1)?,
                        name: r.get(2)?,
                        kind: r.get(3)?,
                        path: r.get(4)?,
                        size: r.get(5)?,
                        mtime: r.get(6)?,
                        content_hash: r.get(7)?,
                    })
                },
            )
            .map_err(to_serr)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn dec_ref(tx: &Transaction, hash: &str) -> Result<(), String> {
        if hash.is_empty() {
            return Ok(());
        }
        tx.execute(
            "UPDATE blobs SET refcount=refcount-1 WHERE hash=?1 AND refcount>0",
            [hash],
        )
        .map_err(to_serr)?;
        Ok(())
    }

    fn inc_ref(tx: &Transaction, hash: &str) -> Result<(), String> {
        if hash.is_empty() {
            return Ok(());
        }
        tx.execute("UPDATE blobs SET refcount=refcount+1 WHERE hash=?1", [hash])
            .map_err(to_serr)?;
        Ok(())
    }

    /// 将节点移入回收站（引用转移，不 decRef）；目录后代文件逐条入站。
    fn trash_node_locked(
        tx: &Transaction,
        user_id: i64,
        n: &NodeInfo,
        now: i64,
    ) -> Result<(), String> {
        if n.kind == "dir" {
            let kids = Self::list_descendants(tx, user_id, &n.path)?;
            for k in &kids {
                if k.kind == "file" && !k.content_hash.is_empty() {
                    tx.execute(
                        "INSERT INTO trash(user_id, orig_path, name, type, content_hash, size, mtime, deleted_at)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                        rusqlite::params![user_id, k.path, k.name, "file", k.content_hash, k.size, k.mtime, now],
                    )
                    .map_err(to_serr)?;
                }
            }
        }
        tx.execute(
            "INSERT INTO trash(user_id, orig_path, name, type, content_hash, size, mtime, deleted_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![user_id, n.path, n.name, n.kind, n.content_hash, n.size, n.mtime, now],
        )
        .map_err(to_serr)?;
        Ok(())
    }

    /// 覆盖节点内容前保存旧版本（引用转移），按上限裁剪（FR-V1）。
    fn save_version(tx: &Transaction, user_id: i64, node_id: i64, n: &NodeInfo, max: i64) -> Result<(), String> {
        if n.content_hash.is_empty() || n.kind != "file" {
            return Ok(());
        }
        tx.execute(
            "INSERT INTO versions(user_id, node_id, path, content_hash, size, mtime, created)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![user_id, node_id, n.path, n.content_hash, n.size, n.mtime, now_secs()],
        )
        .map_err(to_serr)?;
        let mut stmt = tx
            .prepare(
                "SELECT id, content_hash FROM versions WHERE user_id=?1 AND node_id=?2
                 ORDER BY id DESC LIMIT -1 OFFSET ?3",
            )
            .map_err(to_serr)?;
        let rows = stmt
            .query_map(rusqlite::params![user_id, node_id, max], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(to_serr)?;
        let prune: Vec<(i64, String)> = rows.filter_map(|r| r.ok()).collect();
        drop(stmt);
        for (id, h) in prune {
            tx.execute("DELETE FROM versions WHERE id=?1", [id]).map_err(to_serr)?;
            Self::dec_ref(tx, &h)?;
        }
        Ok(())
    }

    fn valid_name(name: &str) -> bool {
        !name.is_empty() && name != "." && name != ".." && !name.contains('/')
    }

    fn join_path(parent: &str, name: &str) -> String {
        if parent.is_empty() {
            name.to_string()
        } else {
            format!("{parent}/{name}")
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_op(
        tx: &Transaction,
        s: &Store,
        user_id: i64,
        device_id: i64,
        op: &ysync_core::protocol::Op,
        quota_state: Option<&(i64, i64)>,
        quota_delta: &mut i64,
    ) -> Result<(i64, i64), String> {
        match op.op.as_str() {
            "mkdir" => {
                let parent_path = Self::node_path(tx, user_id, op.parent_id)?;
                if !Self::valid_name(&op.name) {
                    return Err("invalid name".into());
                }
                let p = Self::join_path(&parent_path, &op.name);
                if let Ok(existing) = Self::node_by_path(tx, user_id, &p) {
                    if existing.kind != "dir" {
                        return Err("path exists as file".into());
                    }
                    let cur = Self::tx_cursor(tx)?;
                    return Ok((existing.id, cur)); // 幂等
                }
                tx.execute(
                    "INSERT INTO nodes(user_id, parent_id, name, type, path, mtime) VALUES(?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![user_id, op.parent_id, op.name, "dir", p, now_millis()],
                )
                .map_err(to_serr)?;
                let id = tx.last_insert_rowid();
                let n = NodeInfo { id, parent_id: op.parent_id, name: op.name.clone(), kind: "dir".into(), path: p, ..Default::default() };
                let cur = Self::journal_change(tx, user_id, device_id, &n, "mkdir")?;
                Ok((id, cur))
            }
            "put" => {
                if op.content_hash.is_empty() {
                    return Err("content_hash required".into());
                }
                // P1-8：内容可为普通 blob 或 CDC 清单（清单以原文件哈希为键登记 size）
                let bsize: i64 = tx
                    .query_row(
                        "SELECT COALESCE(
                           (SELECT size FROM blobs WHERE hash=?1),
                           (SELECT size FROM manifests WHERE hash=?1))",
                        [&op.content_hash],
                        |r| r.get(0),
                    )
                    .map_err(|_| "content not uploaded yet".to_string())?;
                let size = if op.size == 0 { bsize } else { op.size };
                // 配额强制（P1-8）：quota_bytes=0 不限；used 为批次起点快照，
                // delta 累计同批次前序 put 的增量（替换旧文件记差值）
                if let Some((quota, used)) = quota_state {
                    let replaced: i64 = if op.node_id > 0 {
                        tx.query_row(
                            "SELECT COALESCE(size,0) FROM nodes WHERE id=?1 AND type='file'",
                            [op.node_id],
                            |r| r.get(0),
                        )
                        .unwrap_or(0)
                    } else {
                        let pp = Self::node_path(tx, user_id, op.parent_id)?;
                        let p_full = Self::join_path(&pp, &op.name);
                        tx.query_row(
                            "SELECT COALESCE(size,0) FROM nodes WHERE user_id=?1 AND path=?2 AND type='file'",
                            rusqlite::params![user_id, p_full],
                            |r| r.get(0),
                        )
                        .unwrap_or(0)
                    };
                    if *used + *quota_delta + size - replaced > *quota {
                        return Err(format!(
                            "quota exceeded: used {used}B, incoming {size}B, quota {quota}B"
                        ));
                    }
                    *quota_delta += size - replaced;
                }
                if op.node_id > 0 {
                    let n = Self::node_by_id(tx, user_id, op.node_id)
                        .map_err(|_| "node not found".to_string())?;
                    if n.kind != "file" {
                        return Err("not a file".into());
                    }
                    if n.content_hash != op.content_hash {
                        Self::save_version(tx, user_id, n.id, &n, s.max_versions)?;
                    }
                    tx.execute(
                        "UPDATE nodes SET content_hash=?1, size=?2, mtime=?3 WHERE id=?4",
                        rusqlite::params![op.content_hash, size, op.mtime, n.id],
                    )
                    .map_err(to_serr)?;
                    Self::inc_ref(tx, &op.content_hash)?;
                    let updated = NodeInfo { content_hash: op.content_hash.clone(), size, mtime: op.mtime, ..n.clone() };
                    let cur = Self::journal_change(tx, user_id, device_id, &updated, "put")?;
                    return Ok((n.id, cur));
                }
                let parent_path = Self::node_path(tx, user_id, op.parent_id)?;
                if !Self::valid_name(&op.name) {
                    return Err("invalid name".into());
                }
                let p = Self::join_path(&parent_path, &op.name);
                if let Ok(existing) = Self::node_by_path(tx, user_id, &p) {
                    // 并发覆盖：旧节点先下线（目录则递归），再落新节点
                    Self::unlink_node_locked(tx, s, user_id, device_id, &existing)?;
                }
                tx.execute(
                    "INSERT INTO nodes(user_id, parent_id, name, type, content_hash, size, mtime, path)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    rusqlite::params![user_id, op.parent_id, op.name, "file", op.content_hash, size, op.mtime, p],
                )
                .map_err(to_serr)?;
                let id = tx.last_insert_rowid();
                Self::inc_ref(tx, &op.content_hash)?;
                let n = NodeInfo {
                    id, parent_id: op.parent_id, name: op.name.clone(), kind: "file".into(),
                    path: p, content_hash: op.content_hash.clone(), size, mtime: op.mtime,
                };
                let cur = Self::journal_change(tx, user_id, device_id, &n, "put")?;
                Ok((id, cur))
            }
            "move" => {
                let n = Self::node_by_id(tx, user_id, op.node_id)
                    .map_err(|_| "node not found".to_string())?;
                let parent_path = Self::node_path(tx, user_id, op.parent_id)?;
                if !Self::valid_name(&op.name) {
                    return Err("invalid name".into());
                }
                let new_path = Self::join_path(&parent_path, &op.name);
                if new_path == n.path {
                    let cur = Self::tx_cursor(tx)?;
                    return Ok((n.id, cur)); // 幂等
                }
                if new_path.starts_with(&format!("{}/", n.path)) {
                    return Err("cannot move dir into itself".into());
                }
                if let Ok(existing) = Self::node_by_path(tx, user_id, &new_path) {
                    Self::unlink_node_locked(tx, s, user_id, device_id, &existing)?;
                }
                let old_path = n.path.clone();
                tx.execute(
                    "UPDATE nodes SET parent_id=?1, name=?2, path=?3 WHERE id=?4",
                    rusqlite::params![op.parent_id, op.name, new_path, n.id],
                )
                .map_err(to_serr)?;
                let mut moved = n.clone();
                moved.parent_id = op.parent_id;
                moved.name = op.name.clone();
                moved.path = new_path.clone();
                let mut cur = Self::journal_change(tx, user_id, device_id, &moved, "move")?;
                if n.kind == "dir" {
                    let kids = Self::list_descendants(tx, user_id, &old_path)?;
                    for mut k in kids {
                        k.path = format!("{new_path}{}", k.path.trim_start_matches(&old_path));
                        tx.execute("UPDATE nodes SET path=?1 WHERE id=?2", rusqlite::params![k.path, k.id])
                            .map_err(to_serr)?;
                        cur = Self::journal_change(tx, user_id, device_id, &k, "move")?;
                    }
                }
                Ok((n.id, cur))
            }
            "unlink" => {
                let n = match Self::node_by_id(tx, user_id, op.node_id) {
                    Ok(n) => n,
                    Err(_) => {
                        let cur = Self::tx_cursor(tx)?;
                        return Ok((op.node_id, cur)); // 幂等
                    }
                };
                let cur = Self::tx_cursor(tx)?;
                Self::unlink_node_locked(tx, s, user_id, device_id, &n)?;
                Ok((op.node_id, cur))
            }
            other => Err(format!("unknown op {other:?}")),
        }
    }

    /// 递归删除节点：后代先入回收站并写日志，最后节点本身（引用转移，不 decRef）。
    fn unlink_node_locked(
        tx: &Transaction,
        _s: &Store,
        user_id: i64,
        device_id: i64,
        n: &NodeInfo,
    ) -> Result<(), String> {
        if n.kind == "dir" {
            let kids = Self::list_descendants(tx, user_id, &n.path)?;
            for k in kids.iter().rev() {
                tx.execute("DELETE FROM nodes WHERE id=?1", [k.id]).map_err(to_serr)?;
                Self::journal_change(tx, user_id, device_id, k, "unlink")?;
            }
        }
        tx.execute("DELETE FROM nodes WHERE id=?1", [n.id]).map_err(to_serr)?;
        Self::trash_node_locked(tx, user_id, n, now_secs())?;
        Self::journal_change(tx, user_id, device_id, n, "unlink")?;
        Ok(())
    }

    /// 批量、按序、原子应用元数据操作；journal 归属调用设备。
    pub fn apply_ops(
        &self,
        user_id: i64,
        device_id: i64,
        ops: &[ysync_core::protocol::Op],
    ) -> Result<Vec<OpResult>, String> {
        let results = {
            let mut conn = self.db.lock().unwrap();
            let tx = conn.transaction().map_err(to_serr)?;
        // 配额状态批次级计算（B3）：一次 SUM，put 之间用内存 delta 累计
        let quota_state: Option<(i64, i64)> = if ops.iter().any(|o| o.op == "put") {
            let quota: i64 = tx
                .query_row("SELECT quota_bytes FROM users WHERE id=?1", [user_id], |r| r.get(0))
                .map_err(to_serr)?;
            if quota > 0 {
                let used: i64 = tx
                    .query_row(
                        "SELECT COALESCE(SUM(size),0) FROM nodes WHERE user_id=?1 AND type='file'",
                        [user_id],
                        |r| r.get(0),
                    )
                    .map_err(to_serr)?;
                Some((quota, used))
            } else {
                None
            }
        } else {
            None
        };
        let mut delta: i64 = 0;
        let mut results = Vec::with_capacity(ops.len());
        for op in ops {
            let r = match Self::apply_op(&tx, self, user_id, device_id, op, quota_state.as_ref(), &mut delta) {
                Ok((node_id, cursor)) => OpResult { ok: true, node_id, cursor, ..Default::default() },
                Err(e) => OpResult { ok: false, error: e, ..Default::default() },
            };
            results.push(r);
        }
            tx.commit().map_err(to_serr)?;
            results
        };
        // 事务与锁释放后再做机会式日志裁剪（避免同线程重复加锁死锁）
        let _ = self.trim_journal();
        Ok(results)
    }

    pub fn nodes(&self, user_id: i64) -> Result<Vec<NodeInfo>, String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, parent_id, name, type, path, size, mtime, content_hash
                 FROM nodes WHERE user_id=?1 ORDER BY path",
            )
            .map_err(to_serr)?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok(NodeInfo {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    name: r.get(2)?,
                    kind: r.get(3)?,
                    path: r.get(4)?,
                    size: r.get(5)?,
                    mtime: r.get(6)?,
                    content_hash: r.get(7)?,
                })
            })
            .map_err(to_serr)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 节点树分页列举（P0-1）：按 id 游标翻页，has_more 指示是否还有后续页。
    pub fn nodes_paged(
        &self,
        user_id: i64,
        after_id: i64,
        limit: i64,
    ) -> Result<(Vec<NodeInfo>, bool), String> {
        let limit = if limit <= 0 || limit > 10_000 { 5_000 } else { limit };
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, parent_id, name, type, path, size, mtime, content_hash
                 FROM nodes WHERE user_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            )
            .map_err(to_serr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![user_id, after_id, limit + 1],
                |r| {
                    Ok(NodeInfo {
                        id: r.get(0)?,
                        parent_id: r.get(1)?,
                        name: r.get(2)?,
                        kind: r.get(3)?,
                        path: r.get(4)?,
                        size: r.get(5)?,
                        mtime: r.get(6)?,
                        content_hash: r.get(7)?,
                    })
                },
            )
            .map_err(to_serr)?;
        let mut out: Vec<NodeInfo> = rows.filter_map(|r| r.ok()).collect();
        let has_more = out.len() as i64 > limit;
        if has_more {
            out.truncate(limit as usize);
        }
        Ok((out, has_more))
    }

    pub fn head_cursor(&self, user_id: i64) -> Result<i64, String> {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(MAX(cursor), 0) FROM changes WHERE user_id=?1",
            [user_id],
            |r| r.get(0),
        )
        .map_err(to_serr)
    }

    /// 日志水位（A1）：低于此 cursor 的变更已被裁剪，客户端需全量重同步。
    pub fn journal_watermark(&self) -> i64 {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(CAST(value AS INTEGER), 0) FROM meta WHERE key='journal_watermark'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// 机会式裁剪（A1）：保留最近 keep 条；触发条件 max-last_check ≥ trim_min。
    /// YSYNC_JOURNAL_KEEP / YSYNC_JOURNAL_TRIM_MIN 环境变量可覆盖（测试用）。
    fn trim_journal(&self) -> Result<(), String> {
        let keep: i64 = std::env::var("YSYNC_JOURNAL_KEEP")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(100_000);
        let trim_min: i64 = std::env::var("YSYNC_JOURNAL_TRIM_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_000);
        let conn = self.db.lock().unwrap();
        let max: i64 = conn
            .query_row("SELECT COALESCE(MAX(cursor),0) FROM changes", [], |r| r.get(0))
            .unwrap_or(0);
        let last: i64 = conn
            .query_row(
                "SELECT COALESCE(CAST(value AS INTEGER),0) FROM meta WHERE key='last_trim_check'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if max - last < trim_min {
            return Ok(());
        }
        conn.execute(
            "INSERT INTO meta(key,value) VALUES('last_trim_check',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [max.to_string()],
        )
        .map_err(to_serr)?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM changes", [], |r| r.get(0))
            .map_err(to_serr)?;
        if count <= keep {
            return Ok(());
        }
        let watermark: i64 = conn
            .query_row(
                "SELECT COALESCE(MIN(cursor),0) FROM
                 (SELECT cursor FROM changes ORDER BY cursor DESC LIMIT ?1)",
                [keep],
                |r| r.get(0),
            )
            .map_err(to_serr)?;
        if watermark <= 0 {
            return Ok(());
        }
        conn.execute("DELETE FROM changes WHERE cursor < ?1", [watermark])
            .map_err(to_serr)?;
        conn.execute(
            "INSERT INTO meta(key,value) VALUES('journal_watermark',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [watermark.to_string()],
        )
        .map_err(to_serr)?;
        Ok(())
    }

    pub fn changes(
        &self,
        user_id: i64,
        since: i64,
        limit: i64,
        root_id: i64,
    ) -> Result<(Vec<Change>, i64, i64), String> {
        let conn = self.db.lock().unwrap();
        let root_path: String = if root_id > 0 {
            Self::node_by_id(&conn, user_id, root_id).map_err(|_| ERR_NOT_FOUND.to_string())?.path
        } else {
            String::new()
        };
        let limit = if limit <= 0 || limit > 5000 { 1000 } else { limit };
        let upper = format!("{root_path}/\u{10FFFF}");
        let mut stmt = conn
            .prepare(
                if root_path.is_empty() {
                    "SELECT cursor, device_id, node_id, op, path, parent_id, name, type, content_hash, size, mtime
                     FROM changes WHERE user_id=?1 AND cursor>?2 ORDER BY cursor LIMIT ?3"
                } else {
                    "SELECT cursor, device_id, node_id, op, path, parent_id, name, type, content_hash, size, mtime
                     FROM changes WHERE user_id=?1 AND cursor>?2 AND (path=?4 OR (path>?5 AND path<?6)) ORDER BY cursor LIMIT ?3"
                },
            )
            .map_err(to_serr)?;
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<Change> {
            Ok(Change {
                cursor: r.get(0)?,
                device_id: r.get(1)?,
                node_id: r.get(2)?,
                op: r.get(3)?,
                path: r.get(4)?,
                parent_id: r.get(5)?,
                name: r.get(6)?,
                kind: r.get(7)?,
                content_hash: r.get(8)?,
                size: r.get(9)?,
                mtime: r.get(10)?,
            })
        };
        let rows = if root_path.is_empty() {
            stmt.query_map(rusqlite::params![user_id, since, limit], map_row)
        } else {
            stmt.query_map(
                rusqlite::params![user_id, since, limit, root_path, format!("{root_path}/"), upper],
                map_row,
            )
        }
        .map_err(to_serr)?;
        let changes: Vec<Change> = rows.filter_map(|r| r.ok()).collect();
        let head = conn
            .query_row(
                "SELECT COALESCE(MAX(cursor), 0) FROM changes WHERE user_id=?1",
                [user_id],
                |r| r.get(0),
            )
            .map_err(to_serr)?;
        let watermark: i64 = conn
            .query_row(
                "SELECT COALESCE(CAST(value AS INTEGER), 0) FROM meta WHERE key='journal_watermark'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok((changes, head, watermark))
    }

    pub fn ensure_blob_row(&self, hash: &str, size: i64) -> Result<(), String> {
        if !crate::util::valid_hash(hash) {
            return Err("invalid hash".into());
        }
        let conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT INTO blobs(hash, size, refcount) VALUES(?1,?2,0)
             ON CONFLICT(hash) DO NOTHING",
            rusqlite::params![hash, size],
        )
        .map_err(to_serr)?;
        Ok(())
    }

    /// /metrics 用计数（P1-7）。
    pub fn counts(&self) -> Result<Counts, String> {
        let conn = self.db.lock().unwrap();
        let q = |sql: &str| -> i64 {
            conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
        };
        Ok(Counts {
            users: q("SELECT COUNT(*) FROM users"),
            devices: q("SELECT COUNT(*) FROM devices"),
            files: q("SELECT COUNT(*) FROM nodes WHERE type='file'"),
            dirs: q("SELECT COUNT(*) FROM nodes WHERE type='dir'"),
            blobs: q("SELECT COUNT(*) FROM blobs"),
            blob_bytes: q("SELECT COALESCE(SUM(size),0) FROM blobs"),
            shares: q("SELECT COUNT(*) FROM shares"),
            trash: q("SELECT COUNT(*) FROM trash"),
        })
    }

    // ---------- CDC 清单（P1-8） ----------

    /// 上传清单：以【原文件哈希】为键。验证全部块存在后登记；
    /// 返回缺失块列表（非空 = 客户端需补传这些块后重试）。
    pub fn create_manifest(
        &self,
        file_hash: &str,
        size: i64,
        chunks: &[String],
    ) -> Result<Vec<String>, String> {
        if !crate::util::valid_hash(file_hash) || chunks.is_empty() || size <= 0 {
            return Err("bad manifest".into());
        }
        let conn = self.db.lock().unwrap();
        let mut missing = Vec::new();
        for c in chunks {
            let ok: bool = conn
                .query_row("SELECT 1 FROM blobs WHERE hash=?1", [c], |_| Ok(true))
                .unwrap_or(false);
            if !ok {
                missing.push(c.clone());
            }
        }
        if !missing.is_empty() {
            return Ok(missing);
        }
        // 覆盖旧清单行（同文件重传）
        conn.execute("DELETE FROM manifest_chunks WHERE file_hash=?1", [file_hash])
            .map_err(to_serr)?;
        conn.execute("DELETE FROM manifests WHERE hash=?1", [file_hash])
            .map_err(to_serr)?;
        conn.execute(
            "INSERT INTO manifests(hash, size, chunks_json) VALUES(?1,?2,?3)
             ON CONFLICT(hash) DO UPDATE SET size=excluded.size, chunks_json=excluded.chunks_json",
            rusqlite::params![file_hash, size, serde_json::to_string(chunks).unwrap_or_default()],
        )
        .map_err(to_serr)?;
        for (i, c) in chunks.iter().enumerate() {
            conn.execute(
                "INSERT INTO manifest_chunks(file_hash, chunk_hash, idx) VALUES(?1,?2,?3)
                 ON CONFLICT(file_hash, idx) DO NOTHING",
                rusqlite::params![file_hash, c, i as i64],
            )
            .map_err(to_serr)?;
        }
        Ok(Vec::new())
    }

    pub fn is_manifest(&self, hash: &str) -> bool {
        let conn = self.db.lock().unwrap();
        conn.query_row("SELECT 1 FROM manifests WHERE hash=?1", [hash], |_| Ok(()))
            .is_ok()
    }

    /// 清单信息：chunks（按序）与重组后的总长度。
    pub fn manifest_info(&self, file_hash: &str) -> Option<(Vec<String>, i64)> {
        let conn = self.db.lock().unwrap();
        let (size, chunks_json): (i64, String) = conn
            .query_row(
                "SELECT size, chunks_json FROM manifests WHERE hash=?1",
                [file_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        let chunks: Vec<String> = serde_json::from_str(&chunks_json).ok()?;
        Some((chunks, size))
    }

    // ---------- 设备管理（P1-6） ----------

    pub fn list_devices(&self, user_id: i64) -> Result<Vec<serde_json::Value>, String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, last_seen FROM devices WHERE user_id=?1 ORDER BY id",
            )
            .map_err(to_serr)?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "last_seen": r.get::<_, i64>(2)?,
                }))
            })
            .map_err(to_serr)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 吊销单台设备（P1-6）：删除 token；不属于该用户则 404。
    pub fn revoke_device(&self, user_id: i64, device_id: i64) -> Result<(), String> {
        let conn = self.db.lock().unwrap();
        let n = conn
            .execute(
                "DELETE FROM devices WHERE id=?1 AND user_id=?2",
                rusqlite::params![device_id, user_id],
            )
            .map_err(to_serr)?;
        if n == 0 {
            return Err(ERR_NOT_FOUND.into());
        }
        Ok(())
    }

    /// 用户已用字节数（文件节点 size 汇总；配额强制与 list-users 用量用）。
    pub fn used_bytes(&self, user_id: i64) -> Result<i64, String> {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(size),0) FROM nodes WHERE user_id=?1 AND type='file'",
            [user_id],
            |r| r.get(0),
        )
        .map_err(to_serr)
    }

    pub fn user_quota(&self, user_id: i64) -> Result<i64, String> {
        let conn = self.db.lock().unwrap();
        conn.query_row("SELECT quota_bytes FROM users WHERE id=?1", [user_id], |r| r.get(0))
            .map_err(to_serr)
    }

    // ---------- 回收站（FR-V2） ----------

    pub fn list_trash(&self, user_id: i64) -> Result<Vec<TrashItem>, String> {
        if self.trash_retention_days > 0 {
            let cutoff = now_secs() - self.trash_retention_days * 86400;
            self.purge_trash_before(user_id, cutoff)?; // 先清理（自加锁），再持 conn 读
        }
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, orig_path, name, type, content_hash, size, mtime, deleted_at
                 FROM trash WHERE user_id=?1 ORDER BY deleted_at DESC, id DESC",
            )
            .map_err(to_serr)?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok(TrashItem {
                    id: r.get(0)?,
                    orig_path: r.get(1)?,
                    name: r.get(2)?,
                    kind: r.get(3)?,
                    hash: r.get(4)?,
                    size: r.get(5)?,
                    mtime: r.get(6)?,
                    deleted_at: r.get(7)?,
                })
            })
            .map_err(to_serr)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn purge_trash_before(&self, user_id: i64, cutoff: i64) -> Result<i64, String> {
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction().map_err(to_serr)?;
        let mut stmt = tx
            .prepare("SELECT id, type, content_hash FROM trash WHERE user_id=?1 AND deleted_at<?2")
            .map_err(to_serr)?;
        let rows = stmt
            .query_map(rusqlite::params![user_id, cutoff], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .map_err(to_serr)?;
        let entries: Vec<(i64, String, String)> = rows.filter_map(|r| r.ok()).collect();
        drop(stmt);
        for (id, kind, hash) in &entries {
            if kind == "file" && !hash.is_empty() {
                Self::dec_ref(&tx, hash)?;
            }
            tx.execute("DELETE FROM trash WHERE id=?1", [id]).map_err(to_serr)?;
        }
        tx.commit().map_err(to_serr)?;
        Ok(entries.len() as i64)
    }

    pub fn restore_trash(&self, user_id: i64, trash_id: i64) -> Result<NodeInfo, String> {
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction().map_err(to_serr)?;
        let row: (String, String, String, String, i64, i64) = tx
            .query_row(
                "SELECT orig_path, name, type, content_hash, size, mtime FROM trash WHERE id=?1 AND user_id=?2",
                rusqlite::params![trash_id, user_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .map_err(|_| ERR_NOT_FOUND.to_string())?;
        let (orig_path, _name, kind, hash, size, mtime) = row;

        let mut path = orig_path.clone();
        if Self::node_by_path(&tx, user_id, &path).is_ok() {
            let (dir, name) = split_path(&path);
            let (base, ext) = match name.rfind('.') {
                Some(i) if i > 0 => (&name[..i], &name[i..]),
                _ => (name.as_str(), ""),
            };
            path = Self::join_path(&dir, &format!("{base} (restored){ext}"));
            if Self::node_by_path(&tx, user_id, &path).is_ok() {
                return Err("restore target occupied".into());
            }
        }
        ensure_parents_locked(&tx, user_id, &path)?;

        let now = now_millis();
        let n = if kind == "file" {
            tx.execute(
                "INSERT INTO nodes(user_id, parent_id, name, type, content_hash, size, mtime, path)
                 VALUES(?1,0,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![user_id, base_name(&path), "file", hash, size, mtime, path],
            )
            .map_err(to_serr)?;
            let id = tx.last_insert_rowid();
            Self::inc_ref(&tx, &hash)?;
            let n = NodeInfo {
                id,
                name: base_name(&path),
                kind: "file".into(),
                path: path.clone(),
                content_hash: hash.clone(),
                size,
                mtime,
                parent_id: 0,
            };
            Self::journal_change(&tx, user_id, 0, &n, "put")?;
            n
        } else {
            tx.execute(
                "INSERT INTO nodes(user_id, parent_id, name, type, path, mtime) VALUES(?1,0,?2,?3,?4,?5)",
                rusqlite::params![user_id, base_name(&path), "dir", path, now],
            )
            .map_err(to_serr)?;
            let id = tx.last_insert_rowid();
            let n = NodeInfo {
                id,
                name: base_name(&path),
                kind: "dir".into(),
                path: path.clone(),
                mtime: now,
                parent_id: 0,
                ..Default::default()
            };
            Self::journal_change(&tx, user_id, 0, &n, "mkdir")?;
            n
        };
        // parent_id 修正
        let (dir, _) = split_path(&path);
        if !dir.is_empty() {
            if let Ok(p) = Self::node_by_path(&tx, user_id, &dir) {
                let _ = tx.execute("UPDATE nodes SET parent_id=?1 WHERE id=?2", rusqlite::params![p.id, n.id]);
            }
        }
        tx.execute("DELETE FROM trash WHERE id=?1", [trash_id]).map_err(to_serr)?;
        tx.commit().map_err(to_serr)?;
        Ok(n)
    }

    pub fn delete_trash(&self, user_id: i64, trash_id: i64) -> Result<(), String> {
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction().map_err(to_serr)?;
        let row: (String, String) = tx
            .query_row(
                "SELECT type, content_hash FROM trash WHERE id=?1 AND user_id=?2",
                rusqlite::params![trash_id, user_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| ERR_NOT_FOUND.to_string())?;
        if row.0 == "file" && !row.1.is_empty() {
            Self::dec_ref(&tx, &row.1)?;
        }
        tx.execute("DELETE FROM trash WHERE id=?1", [trash_id]).map_err(to_serr)?;
        tx.commit().map_err(to_serr)?;
        Ok(())
    }

    // ---------- 版本（FR-V1） ----------

    pub fn list_versions(&self, user_id: i64, node_id: i64) -> Result<Vec<VersionItem>, String> {
        let conn = self.db.lock().unwrap();
        if Self::node_by_id(&conn, user_id, node_id).is_err() {
            return Err(ERR_NOT_FOUND.into());
        }
        let mut stmt = conn
            .prepare(
                "SELECT id, node_id, path, content_hash, size, mtime, created
                 FROM versions WHERE user_id=?1 AND node_id=?2 ORDER BY id DESC",
            )
            .map_err(to_serr)?;
        let rows = stmt
            .query_map(rusqlite::params![user_id, node_id], |r| {
                Ok(VersionItem {
                    id: r.get(0)?,
                    node_id: r.get(1)?,
                    path: r.get(2)?,
                    hash: r.get(3)?,
                    size: r.get(4)?,
                    mtime: r.get(5)?,
                    created: r.get(6)?,
                })
            })
            .map_err(to_serr)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn version_content(&self, user_id: i64, version_id: i64) -> Result<String, String> {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT content_hash FROM versions WHERE id=?1 AND user_id=?2",
            rusqlite::params![version_id, user_id],
            |r| r.get(0),
        )
        .map_err(|_| ERR_NOT_FOUND.to_string())
    }

    // ---------- 分享（FR-H1） ----------

    pub fn create_share(&self, user_id: i64, path: &str, hours: i64, password: &str) -> Result<ShareInfo, String> {
        if path.is_empty() || path.contains("..") {
            return Err("invalid path".into());
        }
        let conn = self.db.lock().unwrap();
        let node = Self::node_by_path(&conn, user_id, path).map_err(|_| ERR_NOT_FOUND.to_string())?;
        let raw: Vec<u8> = (0..12).map(|_| rand_byte()).collect();
        let token = hex(&raw);
        let pwd_hash = if password.is_empty() {
            String::new()
        } else {
            sha256_hex(format!("ysync-share:{password}").as_bytes())
        };
        let expires = if hours > 0 { now_secs() + hours * 3600 } else { 0 };
        conn.execute(
            "INSERT INTO shares(user_id, token, node_id, password_hash, expires_at, created) VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![user_id, token, node.id, pwd_hash, expires, now_secs()],
        )
        .map_err(to_serr)?;
        Ok(ShareInfo {
            token,
            path: path.to_string(),
            node_id: node.id,
            has_password: !password.is_empty(),
            expires_at: expires,
            created: now_secs(),
        })
    }

    pub fn list_shares(&self, user_id: i64) -> Result<Vec<ShareInfo>, String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT token, node_id, password_hash!='', expires_at, created FROM shares WHERE user_id=?1 ORDER BY id DESC")
            .map_err(to_serr)?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok(ShareInfo {
                    token: r.get(0)?,
                    node_id: r.get(1)?,
                    has_password: r.get(2)?,
                    expires_at: r.get(3)?,
                    created: r.get(4)?,
                    path: String::new(),
                })
            })
            .map_err(to_serr)?;
        let mut out: Vec<ShareInfo> = rows.filter_map(|r| r.ok()).collect();
        drop(stmt);
        for s in &mut out {
            if let Ok(n) = Self::node_by_id(&conn, user_id, s.node_id) {
                s.path = n.path;
            }
        }
        Ok(out)
    }

    pub fn delete_share(&self, user_id: i64, token: &str) -> Result<(), String> {
        let conn = self.db.lock().unwrap();
        let n = conn
            .execute("DELETE FROM shares WHERE user_id=?1 AND token=?2", rusqlite::params![user_id, token])
            .map_err(to_serr)?;
        if n == 0 {
            return Err(ERR_NOT_FOUND.into());
        }
        Ok(())
    }

    pub fn get_share(&self, token: &str) -> Result<(i64, i64, String, i64), String> {
        let conn = self.db.lock().unwrap();
        let r: (i64, i64, String, i64) = conn
            .query_row(
                "SELECT user_id, node_id, password_hash, expires_at FROM shares WHERE token=?1",
                [token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|_| ERR_NOT_FOUND.to_string())?;
        if r.3 > 0 && now_secs() > r.3 {
            return Err(ERR_NOT_FOUND.into());
        }
        Ok(r)
    }

    // ---------- GC ----------

    pub fn gc(&self) -> Result<(i64, i64), String> {
        let cutoff = if self.trash_retention_days > 0 {
            now_secs() - self.trash_retention_days * 86400
        } else {
            now_secs()
        };
        // 跨用户清理（先收集后 decRef）
        let purged = {
            let mut conn = self.db.lock().unwrap();
            let tx = conn.transaction().map_err(to_serr)?;
            let mut stmt = tx
                .prepare("SELECT id, type, content_hash FROM trash WHERE deleted_at<?1")
                .map_err(to_serr)?;
            let rows = stmt
                .query_map([cutoff], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })
                .map_err(to_serr)?;
            let entries: Vec<(i64, String, String)> = rows.filter_map(|r| r.ok()).collect();
            drop(stmt);
            for (id, kind, hash) in &entries {
                if kind == "file" && !hash.is_empty() {
                    Self::dec_ref(&tx, hash)?;
                }
                tx.execute("DELETE FROM trash WHERE id=?1", [id]).map_err(to_serr)?;
            }
            let n = entries.len() as i64;
            tx.commit().map_err(to_serr)?;
            n
        };

        let hashes: Vec<String> = {
            let conn = self.db.lock().unwrap_or_else(|p| p.into_inner());
            let mut stmt = conn
                .prepare(
                    "SELECT hash FROM blobs WHERE refcount<=0
                     AND hash NOT IN (SELECT chunk_hash FROM manifest_chunks)",
                )
                .map_err(to_serr)?;
            let rows = stmt.query_map([], |r| r.get(0)).map_err(to_serr)?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for h in &hashes {
            let conn = self.db.lock().unwrap();
            conn.execute("DELETE FROM blobs WHERE hash=?1 AND refcount<=0", [h])
                .map_err(to_serr)?;
            drop(conn);
            self.blobs.remove(h);
        }
        // A5：清理 upload tmp 孤儿（会话丢失/上传中断残留，>24h 删除）
        let tmp_dir = self.blobs.root.join("tmp");
        if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
            let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
            for e in entries.filter_map(|e| e.ok()) {
                if let Ok(meta) = e.metadata() {
                    if meta.modified().map(|m| m < cutoff).unwrap_or(false) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        Ok((purged, hashes.len() as i64))
    }
}

// ---------- 辅助 ----------

fn split_path(p: &str) -> (String, String) {
    match p.rfind('/') {
        Some(i) => (p[..i].to_string(), p[i + 1..].to_string()),
        None => (String::new(), p.to_string()),
    }
}

fn base_name(p: &str) -> String {
    split_path(p).1
}

/// 自顶向下创建父目录并写日志（restore 用）。必须在事务内。
fn ensure_parents_locked(tx: &Transaction, user_id: i64, path: &str) -> Result<(), String> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut cur = String::new();
    for part in &parts[..parts.len() - 1] {
        let next = if cur.is_empty() {
            part.to_string()
        } else {
            format!("{cur}/{part}")
        };
        cur = next.clone();
        if let Ok(existing) = Store::node_by_path(tx, user_id, &cur) {
            if existing.kind != "dir" {
                return Err(format!("parent {cur:?} is a file"));
            }
            continue;
        }
        let parent_id = {
            let (dir, _) = split_path(&cur);
            if dir.is_empty() {
                0
            } else {
                Store::node_by_path(tx, user_id, &dir).map(|p| p.id).unwrap_or(0)
            }
        };
        tx.execute(
            "INSERT INTO nodes(user_id, parent_id, name, type, path, mtime) VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![user_id, parent_id, part, "dir", cur, now_millis()],
        )
        .map_err(to_serr)?;
        let id = tx.last_insert_rowid();
        let n = NodeInfo {
            id,
            parent_id,
            name: part.to_string(),
            kind: "dir".into(),
            path: next,
            ..Default::default()
        };
        Store::journal_change(tx, user_id, 0, &n, "mkdir")?;
    }
    Ok(())
}

// ---------- 密码（与 Go 端 argon2id$<salt>$<key> 格式互通） ----------

fn rand_bytes16() -> [u8; 16] {
    let mut b = [0u8; 16];
    fill_random(&mut b);
    b
}

fn rand_byte() -> u8 {
    let mut b = [0u8; 1];
    fill_random(&mut b);
    b[0]
}

fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // 兜底：时间熵（仅测试环境兜底）
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((t >> (i % 16)) as u8) ^ (i as u8);
    }
}

fn argon2id_key(password: &str, salt: &[u8]) -> Vec<u8> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(64 * 1024, 1, 4, Some(32)).expect("argon2 params");
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = vec![0u8; 32];
    let _ = a2.hash_password_into(password.as_bytes(), salt, &mut out);
    out
}

fn b64_decode(s: &str) -> Vec<u8> {
    // RawStd base64 解码（容忍缺省 padding）
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s = s.trim_end_matches('=');
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match T.iter().position(|t| *t == c) {
            Some(v) => v as u32,
            None => return out,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 3 || parts[0] != "argon2id" {
        return false;
    }
    let Some(salt) = unhex_safe_b64(parts[1]) else { return false };
    let want = match unhex_safe_b64(parts[2]) {
        Some(v) => v,
        None => return false,
    };
    let got = argon2id_key(password, &salt);
    constant_time_eq(&got, &want)
}

fn unhex_safe_b64(s: &str) -> Option<Vec<u8>> {
    Some(b64_decode(s))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
