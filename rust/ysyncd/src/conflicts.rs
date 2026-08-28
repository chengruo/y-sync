//! 冲突副本的发现与处理（FR-S7）：与 Go internal/client/conflicts.go 行为一致。
//! 处理是纯文件操作，后续同步自动传播。
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::abs_join;

pub const CONFLICT_MARKER: &str = " (conflict from ";

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

/// 扫描文件夹根下的冲突副本文件。
pub fn list_conflicts(root: &Path, folder_name: &str) -> Vec<Conflict> {
    let mut out = Vec::new();
    walk(root, root, folder_name, &mut out);
    out
}

fn walk(base: &Path, dir: &Path, folder: &str, out: &mut Vec<Conflict>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            match e.file_name().to_str() {
                Some(".y-sync") | Some(".git") | Some(".svn") | Some(".hg") => continue,
                _ => {}
            }
            walk(base, &p, folder, out);
            continue;
        }
        let Some(base_name) = e.file_name().to_str().map(|s| s.to_string()) else { continue };
        let Some(i) = base_name.find(CONFLICT_MARKER) else { continue };
        let ext = match base_name.rfind('.') {
            Some(j) if j > i => &base_name[j..],
            _ => "",
        };
        let orig_name = format!("{}{ext}", &base_name[..i]);
        let Ok(rel) = p.strip_prefix(base) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let copy_rel = rel.clone();
        let dir_part = match rel.rfind('/') {
            Some(j) => &rel[..j],
            None => "",
        };
        let orig_rel = if dir_part.is_empty() {
            orig_name.clone()
        } else {
            format!("{dir_part}/{orig_name}")
        };
        let (size, mtime) = match e.metadata() {
            Ok(m) => (
                m.len() as i64,
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            ),
            Err(_) => (0, 0),
        };
        out.push(Conflict {
            folder: folder.to_string(),
            rel: orig_rel,
            copy_rel,
            size,
            mtime,
        });
    }
}

fn is_conflict_copy(name: &str) -> bool {
    name.contains(CONFLICT_MARKER)
}

/// 保留原名文件：删除冲突副本（删除将同步传播）。
pub fn resolve_keep_local(root: &Path, c: &Conflict) -> Result<(), ysync_core::Error> {
    let copy_base = c
        .copy_rel
        .rsplit('/')
        .next()
        .unwrap_or(&c.copy_rel)
        .to_string();
    if !is_conflict_copy(&copy_base) {
        return Err(ysync_core::Error::Msg(format!("{:?} 不是冲突副本", c.copy_rel)));
    }
    std::fs::remove_file(abs_join(root, &c.copy_rel))
        .map_err(|e| ysync_core::Error::Msg(format!("remove: {e}")))
}

/// 采用副本：副本内容覆盖原名文件，然后删除副本。
pub fn resolve_keep_copy(root: &Path, c: &Conflict) -> Result<(), ysync_core::Error> {
    let copy_base = c
        .copy_rel
        .rsplit('/')
        .next()
        .unwrap_or(&c.copy_rel)
        .to_string();
    if !is_conflict_copy(&copy_base) {
        return Err(ysync_core::Error::Msg(format!("{:?} 不是冲突副本", c.copy_rel)));
    }
    let copy_abs = abs_join(root, &c.copy_rel);
    let orig_abs = abs_join(root, &c.rel);
    let data = std::fs::read(&copy_abs).map_err(|e| ysync_core::Error::Msg(format!("read: {e}")))?;
    let tmp = orig_abs.with_file_name(format!(
        ".ysync-resolve-{}",
        std::process::id()
    ));
    std::fs::write(&tmp, &data).map_err(|e| ysync_core::Error::Msg(format!("write: {e}")))?;
    std::fs::rename(&tmp, &orig_abs).map_err(|e| ysync_core::Error::Msg(format!("rename: {e}")))?;
    std::fs::remove_file(&copy_abs).map_err(|e| ysync_core::Error::Msg(format!("remove: {e}")))
}
