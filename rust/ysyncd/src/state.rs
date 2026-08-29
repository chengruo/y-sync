//! 本地状态库（FR-C4）：每文件夹一份 SQLite，与 Go 端 schema 一致。
use rusqlite::Connection;
use std::path::{Path, PathBuf};

use ysync_core::Result;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rec {
    pub node_id: i64,
    pub hash: String,
    pub size: i64,
    pub mtime: i64,
    pub kind: String, // "file" | "dir"
}

pub struct State {
    conn: Connection,
    _path: PathBuf,
}

pub fn state_path(local_path: &Path) -> PathBuf {
    local_path.join(".y-sync").join("state.db")
}

impl State {
    pub fn open(local_path: &Path) -> Result<Self> {
        let dir = local_path.join(".y-sync");
        std::fs::create_dir_all(&dir)?;
        let path = state_path(local_path);
        let conn = Connection::open(&path)
            .map_err(|e| ysync_core::Error::Msg(format!("open state: {e}")))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS files(
               path TEXT PRIMARY KEY, node_id INTEGER NOT NULL, content_hash TEXT NOT NULL,
               size INTEGER NOT NULL, mtime INTEGER NOT NULL, type TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(|e| ysync_core::Error::Msg(format!("init state: {e}")))?;
        Ok(State {
            conn,
            _path: path,
        })
    }

    pub fn all(&self) -> Result<std::collections::HashMap<String, Rec>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, node_id, content_hash, size, mtime, type FROM files").map_err(to_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Rec {
                        node_id: r.get(1)?,
                        hash: r.get(2)?,
                        size: r.get(3)?,
                        mtime: r.get(4)?,
                        kind: r.get(5)?,
                    },
                ))
            })
            .map_err(to_err)?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (p, rec) = row.map_err(to_err)?;
            out.insert(p, rec);
        }
        Ok(out)
    }

    pub fn get(&self, path: &str) -> Result<Option<Rec>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT node_id, content_hash, size, mtime, type FROM files WHERE path = ?1",
            )
            .map_err(to_err)?;
        let mut rows = stmt.query([path]).map_err(to_err)?;
        if let Some(row) = rows.next().map_err(to_err)? {
            let rec = (|| {
                Ok(Rec {
                    node_id: row.get(0)?,
                    hash: row.get(1)?,
                    size: row.get(2)?,
                    mtime: row.get(3)?,
                    kind: row.get(4)?,
                })
            })()
            .map_err(to_err)?;
            return Ok(Some(rec));
        }
        Ok(None)
    }

    pub fn set(&self, path: &str, r: &Rec) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO files(path, node_id, content_hash, size, mtime, type)
                 VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(path) DO UPDATE SET node_id=excluded.node_id,
                   content_hash=excluded.content_hash, size=excluded.size,
                   mtime=excluded.mtime, type=excluded.type",
                rusqlite::params![path, r.node_id, r.hash, r.size, r.mtime, r.kind],
            )
            .map_err(to_err)?;
        Ok(())
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", [path])
            .map_err(to_err)?;
        Ok(())
    }

    /// 单事务批量持久化（B2）：删除 + 覆写 + 游标原子提交。
    pub fn persist(
        &self,
        deletes: &[String],
        sets: &[(String, Rec)],
        cursor: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| ysync_core::Error::Msg(format!("state tx: {e}")))?;
        for p in deletes {
            tx.execute("DELETE FROM files WHERE path = ?1", [p])
                .map_err(to_err)?;
        }
        for (p, r) in sets {
            tx.execute(
                "INSERT INTO files(path, node_id, content_hash, size, mtime, type)
                 VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(path) DO UPDATE SET node_id=excluded.node_id,
                   content_hash=excluded.content_hash, size=excluded.size,
                   mtime=excluded.mtime, type=excluded.type",
                rusqlite::params![p, r.node_id, r.hash, r.size, r.mtime, r.kind],
            )
            .map_err(to_err)?;
        }
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('cursor', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [cursor.to_string()],
        )
        .map_err(to_err)?;
        tx.commit().map_err(|e| ysync_core::Error::Msg(format!("state commit: {e}")))?;
        Ok(())
    }

    pub fn cursor(&self) -> Result<i64> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key='cursor'")
            .map_err(to_err)?;
        let mut rows = stmt.query([]).map_err(to_err)?;
        if let Some(row) = rows.next().map_err(to_err)? {
            let v: String = row.get(0).map_err(to_err)?;
            return Ok(v.parse().unwrap_or(0));
        }
        Ok(0)
    }

    pub fn set_cursor(&self, c: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES('cursor', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [c.to_string()],
            )
            .map_err(to_err)?;
        Ok(())
    }

    // ---------- 分块上传会话持久化（FR-S11 断点续传） ----------

    pub fn get_upload_session(&self, rel: &str, hash: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [format!("upload:{rel}:{hash}")],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    pub fn set_upload_session(&self, rel: &str, hash: &str, session: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![format!("upload:{rel}:{hash}"), session],
            )
            .map_err(to_err)?;
        Ok(())
    }

    pub fn clear_upload_session(&self, rel: &str, hash: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM meta WHERE key = ?1",
                [format!("upload:{rel}:{hash}")],
            )
            .map_err(to_err)?;
        Ok(())
    }
}

fn to_err(e: rusqlite::Error) -> ysync_core::Error {
    ysync_core::Error::Msg(format!("state db: {e}"))
}

// ---------- 崩溃恢复标记（M2） ----------

pub fn pending_marker_path(root: &Path) -> PathBuf {
    root.join(".y-sync").join("pending.json")
}

pub fn pending_marker_exists(root: &Path) -> bool {
    pending_marker_path(root).exists()
}

pub fn write_pending_marker(root: &Path) {
    let _ = std::fs::create_dir_all(root.join(".y-sync"));
    let _ = std::fs::write(
        pending_marker_path(root),
        format!(r#"{{"note":"ops in flight","ts":{}}}"#, now_millis()),
    );
}

pub fn clear_pending_marker(root: &Path) {
    let _ = std::fs::remove_file(pending_marker_path(root));
}

pub fn reset_state_db(root: &Path) {
    let sp = state_path(root);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", sp.display(), suffix));
    }
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
