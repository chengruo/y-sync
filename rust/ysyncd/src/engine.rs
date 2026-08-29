//! 同步引擎（§4.3）：从 Go internal/client/engine.go 移植，分支语义逐一对齐。
//! 双向 reconcile、移动语义（FR-S6）、冲突副本（FR-S7）、崩溃恢复、按文件夹独立游标。
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};

use ysync_core::protocol::{self, NodeInfo, Op, OpResult};

use crate::api::{hash_and_size, hash_local_file_helper, now_millis, set_mtime, Api};
use crate::conflicts;
use crate::ignore::{load_layer_patterns, Ignore};
use crate::state::{Rec, State};

#[derive(Debug, Clone, Copy, Default)]
pub struct DiskInfo {
    pub size: i64,
    pub mtime: i64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Default)]
pub struct MoveRec {
    pub old: String,
    pub new: String,
    pub hash: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SyncStats {
    pub uploaded: i64,
    pub downloaded: i64,
    pub moved: i64,
    pub deleted: i64,
    pub conflicts: i64,
}

pub struct Engine {
    /// P1：Api 内部状态可变（连接字段 Mutex），外层不再互斥——并行传输前提
    pub api: std::sync::Arc<Api>,
    pub device_name: String,
}

pub fn abs_join(root: &Path, rel: &str) -> PathBuf {
    root.join(rel)
}

fn split_dir(rel: &str) -> (String, String) {
    match rel.rfind('/') {
        Some(i) => (rel[..i].to_string(), rel[i + 1..].to_string()),
        None => (String::new(), rel.to_string()),
    }
}

fn parent_of(rel: &str) -> String {
    split_dir(rel).0
}

fn depth(rel: &str) -> usize {
    rel.matches('/').count()
}

fn under_any(set: &[String], rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    set.iter().any(|s| s == rel || rel.starts_with(&format!("{s}/")))
}

/// 探测文件夹所在文件系统是否大小写不敏感（macOS/Windows 默认；P0-3）。
fn fs_case_insensitive(root: &Path) -> bool {
    let probe = root.join(".y-sync").join("caseprobe.tmp");
    if std::fs::write(&probe, b"p").is_err() {
        return false;
    }
    let upper = root.join(".y-sync").join("CASEPROBE.TMP");
    let insensitive = upper.exists();
    let _ = std::fs::remove_file(&probe);
    insensitive
}

/// 构建 casefold 索引：(目录, 小写名) → 相对路径列表（P0-3 下载保护用）。
fn build_casefold_index(scan: &HashMap<String, DiskInfo>) -> HashMap<(String, String), Vec<String>> {
    let mut idx: HashMap<(String, String), Vec<String>> = HashMap::new();
    for rel in scan.keys() {
        let (dir, name) = split_dir(rel);
        idx.entry((dir, name.to_lowercase())).or_default().push(rel.clone());
    }
    idx
}

/// 与 Go os.RemoveAll 对齐：文件与目录都能删（Rust 的 remove_dir_all 不接受文件）。
pub fn remove_all(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn has_descendant(scan: &HashMap<String, DiskInfo>, dir_rel: &str) -> bool {
    let prefix = format!("{dir_rel}/");
    scan.keys().any(|r| r.starts_with(&prefix))
}

fn apply_moved_prefix(moved: &[(String, String)], rel: &str) -> Option<String> {
    for (old, new) in moved {
        if rel == old || rel.starts_with(&format!("{old}/")) {
            return Some(format!("{new}{}", rel.trim_start_matches(old)));
        }
    }
    None
}

// ---------- 本地扫描 ----------

/// 分层忽略栈：深层 .syncignore 覆盖浅层（gitignore 语义）。
#[derive(Clone)]
struct IgnLayer {
    base: String,
    ig: std::rc::Rc<Ignore>,
}
type IgnStack = Vec<IgnLayer>;

fn stack_matches(stack: &IgnStack, rel: &str, is_dir: bool) -> bool {
    let mut ignored = false;
    for layer in stack {
        let sub = if layer.base.is_empty() {
            rel.to_string()
        } else if rel == layer.base || !rel.starts_with(&format!("{}/", layer.base)) {
            continue;
        } else {
            rel[layer.base.len() + 1..].to_string()
        };
        if layer.ig.matches(&sub, is_dir) {
            ignored = true;
        }
    }
    ignored
}

fn walk_local(
    root: &Path,
    root_ig: Ignore,
    use_gitignore: bool,
    excluded: &dyn Fn(&str) -> bool,
) -> Result<HashMap<String, DiskInfo>, ysync_core::Error> {
    let mut out: HashMap<String, DiskInfo> = HashMap::new();
    let stack: IgnStack = vec![IgnLayer {
        base: String::new(),
        ig: std::rc::Rc::new(root_ig),
    }];
    walk_dir(root, "", &stack, use_gitignore, excluded, &mut out)?;
    Ok(out)
}

fn walk_dir(
    abs_dir: &Path,
    rel: &str,
    stack: &IgnStack,
    use_gitignore: bool,
    excluded: &dyn Fn(&str) -> bool,
    out: &mut HashMap<String, DiskInfo>,
) -> Result<(), ysync_core::Error> {
    let mut layer: Option<IgnLayer> = None;
    if let Some(patterns) = load_layer_patterns(abs_dir, use_gitignore) {
        layer = Some(IgnLayer {
            base: rel.to_string(),
            ig: std::rc::Rc::new(Ignore::new(&[]).with_extra(patterns)),
        });
    }
    let mut stack = stack.clone();
    if let Some(l) = layer {
        stack.push(l);
    }
    let entries = std::fs::read_dir(abs_dir)
        .map_err(|e| ysync_core::Error::Msg(format!("read dir {}: {e}", abs_dir.display())))?;
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(ft) = e.file_type() else { continue };
        let is_dir = ft.is_dir();
        if excluded(&child_rel) {
            continue; // 选择性同步排除（FR-S9）
        }
        if stack_matches(&stack, &child_rel, is_dir) {
            continue; // 被忽略：目录不递归（FR-S8）
        }
        let Ok(meta) = e.metadata() else { continue };
        if is_dir {
            out.insert(child_rel.clone(), DiskInfo { is_dir: true, ..Default::default() });
            walk_dir(
                &abs_dir.join(&name),
                &child_rel,
                &stack,
                use_gitignore,
                excluded,
                out,
            )?;
            continue;
        }
        if !ft.is_file() {
            continue; // 符号链接等非常规文件不同步（§4.4）
        }
        out.insert(
            child_rel,
            DiskInfo {
                size: meta.len() as i64,
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                is_dir: false,
            },
        );
    }
    Ok(())
}

fn detect_moves(
    del: &mut HashSet<String>,
    new_files: &mut HashMap<String, String>,
    baseline: &HashMap<String, Rec>,
    scan: &HashMap<String, DiskInfo>,
) -> Vec<MoveRec> {
    let mut moves = Vec::new();
    let mut new_snapshot: Vec<(String, String)> =
        new_files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let baseline_sorted: BTreeMap<&String, &Rec> = baseline.iter().collect();
    for (d_rel, d_rec) in baseline_sorted {
        if d_rec.kind != protocol::TYPE_FILE || !del.contains(d_rel) {
            continue;
        }
        for (n_rel, h) in &new_snapshot {
            if h != &d_rec.hash || new_files.get(n_rel) != Some(h) {
                continue;
            }
            if scan.get(n_rel).map(|d| d.size) != Some(d_rec.size) {
                continue;
            }
            moves.push(MoveRec {
                old: d_rel.clone(),
                new: n_rel.clone(),
                hash: h.clone(),
            });
            del.remove(d_rel);
            new_files.remove(n_rel);
            break;
        }
    }
    moves
}

fn rec_from_node(n: &NodeInfo) -> Rec {
    Rec {
        node_id: n.id,
        hash: n.content_hash.clone(),
        size: n.size,
        mtime: n.mtime,
        kind: n.kind.clone(),
    }
}

fn set_local_mtime(root: &Path, rel: &str, mtime_milli: i64) {
    if mtime_milli > 0 {
        set_mtime(&abs_join(root, rel), mtime_milli);
    }
}

pub struct FolderCfg {
    pub name: String,
    pub local_path: PathBuf,
    pub root_node_id: i64,
    pub cursor: i64,
    pub excludes: Vec<String>,
    pub use_gitignore: bool,
}

impl Engine {
    /// ops 分批提交（P0-2）：单请求 ≤500 条，避免巨批压垮服务端/网络。
    fn ops_chunked(&self, ops: &[Op]) -> Result<Vec<OpResult>, ysync_core::Error> {
        let mut out = Vec::with_capacity(ops.len());
        for chunk in ops.chunks(500) {
            out.extend(self.with_api(|a| a.ops(chunk))?);
        }
        Ok(out)
    }

    fn with_api<T>(&self, f: impl FnOnce(&Api) -> Result<T, ysync_core::Error>) -> Result<T, ysync_core::Error> {
        f(&self.api)
    }

    fn resolve_root(&self, f: &mut FolderCfg) -> Result<i64, ysync_core::Error> {
        let nodes = self.with_api(|a| a.nodes())?;
        for n in &nodes {
            if n.path == f.name && n.kind == protocol::TYPE_DIR {
                return Ok(n.id);
            }
        }
        let res = self.with_api(|a| a.ops(&[Op {
            op: protocol::OP_MKDIR.into(),
            name: f.name.clone(),
            ..Default::default()
        }]))?;
        if res.len() == 1 && res[0].ok {
            return Ok(res[0].node_id);
        }
        Err(ysync_core::Error::Msg(format!(
            "resolve root {:?}: 建立失败",
            f.name
        )))
    }

    /// 冲突副本命名（FR-S7：`name (conflict from 设备名).ext`）。
    fn conflict_copy(
        &self,
        rel: &str,
        srec: &NodeInfo,
        downloads: &mut HashMap<String, NodeInfo>,
        scan: &HashMap<String, DiskInfo>,
    ) -> Result<String, ysync_core::Error> {
        if srec.kind == protocol::TYPE_DIR {
            return Err(ysync_core::Error::Msg(format!(
                "cannot conflict-copy dir {rel}"
            )));
        }
        let (dir, name) = split_dir(rel);
        let ext = match name.rfind('.') {
            Some(i) if i > 0 => &name[i..],
            _ => "",
        };
        let base = name.trim_end_matches(ext);
        let mk = |i: usize| -> String {
            let n = if i == 1 {
                format!("{base} (conflict from {}){ext}", self.device_name)
            } else {
                format!("{base} (conflict from {}) {i}{ext}", self.device_name)
            };
            if dir.is_empty() { n } else { format!("{dir}/{n}") }
        };
        let mut cc = mk(1);
        let mut i = 2;
        loop {
            if !scan.contains_key(&cc) && !downloads.contains_key(&cc) {
                break;
            }
            cc = mk(i);
            i += 1;
        }
        downloads.insert(cc.clone(), srec.clone());
        Ok(cc)
    }

    /// 同步一个文件夹（Go Engine.SyncFolder 的完整移植）。
    pub fn sync_folder(&self, f: &mut FolderCfg) -> Result<SyncStats, ysync_core::Error> {
        let mut stats = SyncStats::default();
        let root = f.local_path.clone();
        std::fs::create_dir_all(&root)?;

        // 并发互斥：跨进程 flock（daemon 与 CLI / 双 CLI 并发时后来者跳过）。
        // 锁随 fd 关闭自动释放，持有至本函数返回。
        std::fs::create_dir_all(root.join(".y-sync"))?;
        let sync_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(".y-sync").join("sync.lock"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(sync_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(ysync_core::Error::SyncBusy);
            }
        }
        let _sync_lock_guard = sync_lock; // 保持持有到函数结束

        // 崩溃恢复（M2）
        if crate::state::pending_marker_exists(&root) {
            eprintln!("level=WARN msg=\"检测到未完成的元数据提交，重建本地状态\" folder={:?}", f.name);
            crate::state::reset_state_db(&root);
            crate::state::clear_pending_marker(&root);
            f.cursor = 0;
        }

        // 日志水位检查（A1）：游标低于服务端裁剪水位 → 基线不可信，全量重同步
        if f.cursor > 0 {
            let head = self.with_api(|a| a.head())?;
            if head.watermark > 0 && f.cursor < head.watermark {
                eprintln!(
                    "level=WARN msg=\"游标低于日志水位，全量重同步\" folder={:?} cursor={} watermark={}",
                    f.name, f.cursor, head.watermark
                );
                crate::state::reset_state_db(&root);
                f.cursor = 0;
            }
        }

        let st = State::open(&root)?;
        let excludes = f.excludes.clone();
        let excluded_fn = move |rel: &str| -> bool {
            excludes
                .iter()
                .any(|ex| rel == ex || rel.starts_with(&format!("{ex}/")))
        };

        // 1. 解析服务端子树根
        if f.root_node_id == 0 {
            f.root_node_id = self.resolve_root(f)?;
            self.persist_folder(f)?;
        }

        // 2. 基线
        let baseline = st.all()?;
        let mut baseline_by_node: HashMap<i64, String> = HashMap::new();
        for (rel, r) in &baseline {
            baseline_by_node.insert(r.node_id, rel.clone());
        }

        // 3. 服务端当前视图
        let prefix = format!("{}/", f.name);
        let mut server_now: HashMap<String, NodeInfo> = HashMap::new();
        let mut changed_set: HashSet<String> = HashSet::new();
        let new_cursor;

        if f.cursor == 0 {
            // 初次同步：P0-1 按 id 分页拉取，免大树单响应尖峰
            let mut after_id = 0i64;
            loop {
                let (page, has_more) = self.with_api(|a| a.nodes_paged(after_id, 5000))?;
                for n in page {
                    if n.path != f.name && !n.path.starts_with(&prefix) {
                        continue;
                    }
                    let rel = n.path.trim_start_matches(&prefix).to_string();
                    if rel.is_empty() || rel == f.name {
                        continue; // 根节点本身不属于子树内容（避免泄漏出杂散目录）
                    }
                    after_id = n.id.max(after_id);
                    changed_set.insert(rel.clone());
                    server_now.insert(rel, n);
                }
                if !has_more {
                    break;
                }
            }
            new_cursor = self.with_api(|a| a.head())?.cursor;
        } else {
            for (rel, r) in &baseline {
                server_now.insert(
                    rel.clone(),
                    NodeInfo {
                        id: r.node_id,
                        content_hash: r.hash.clone(),
                        size: r.size,
                        mtime: r.mtime,
                        kind: r.kind.clone(),
                        name: rel.rsplit('/').next().unwrap_or(rel).to_string(),
                        path: format!("{}/{}", f.name, rel),
                        parent_id: 0,
                    },
                );
            }
            let mut head = 0i64;
            let mut cursor = f.cursor;
            loop {
                let resp = self.with_api(|a| a.changes(cursor, 1000, f.root_node_id))?;
                head = resp.cursor;
                for c in &resp.changes {
                    if c.device_id == self.own_device_id() {
                        continue; // 跳过自己设备的变更（避免伪冲突）
                    }
                    if c.path != f.name && !c.path.starts_with(&prefix) {
                        continue;
                    }
                    let rel = c.path.trim_start_matches(&prefix).to_string();
                    if rel.is_empty() {
                        continue;
                    }
                    if c.op == protocol::OP_UNLINK {
                        if let Some(old_rel) = baseline_by_node.get(&c.node_id).cloned() {
                            server_now.remove(&old_rel);
                            changed_set.insert(old_rel);
                            baseline_by_node.remove(&c.node_id);
                        } else {
                            server_now.remove(&rel);
                            changed_set.insert(rel.clone());
                        }
                        continue;
                    }
                    // put/mkdir/move：节点现在位于 rel
                    if let Some(old_rel) = baseline_by_node.get(&c.node_id).cloned() {
                        if old_rel != rel {
                            server_now.remove(&old_rel);
                            changed_set.insert(old_rel);
                        }
                    }
                    baseline_by_node.insert(c.node_id, rel.clone());
                    server_now.insert(
                        rel.clone(),
                        NodeInfo {
                            id: c.node_id,
                            parent_id: c.parent_id,
                            name: c.name.clone(),
                            kind: c.kind.clone(),
                            size: c.size,
                            mtime: c.mtime,
                            content_hash: c.content_hash.clone(),
                            path: c.path.clone(),
                        },
                    );
                    changed_set.insert(rel);
                }
                if resp.changes.len() < 1000 {
                    break;
                }
                cursor = resp.changes.last().map(|c| c.cursor).unwrap_or(cursor);
            }
            new_cursor = head;
        }

        // 4. 本地扫描
        let ig = ignore_root(&root, f.use_gitignore);
        let mut scan = walk_local(&root, ig, f.use_gitignore, &excluded_fn)?;
        let case_insensitive = fs_case_insensitive(&root);
        let mut casefold = build_casefold_index(&scan);

        // 5. 计算本地变更集
        let mut modified: HashMap<String, String> = HashMap::new();
        let mut mtime_only: HashSet<String> = HashSet::new();
        let mut new_files: HashMap<String, String> = HashMap::new();
        let mut local_dirs: HashSet<String> = HashSet::new();
        let mut del: HashSet<String> = HashSet::new();

        for (rel, d) in &scan {
            let base = baseline.get(rel);
            if d.is_dir {
                match base {
                    Some(b) if b.kind == protocol::TYPE_DIR => {}
                    _ => {
                        local_dirs.insert(rel.clone());
                    }
                }
                continue;
            }
            match base {
                Some(b) if b.kind == protocol::TYPE_FILE => {
                    if d.size == b.size && d.mtime == b.mtime {
                        continue;
                    }
                    let (h, _) = hash_local_file_helper(&root, rel)?;
                    if h == b.hash {
                        mtime_only.insert(rel.clone());
                    } else {
                        modified.insert(rel.clone(), h);
                    }
                }
                _ => {
                    let (h, _) = hash_local_file_helper(&root, rel)?;
                    new_files.insert(rel.clone(), h);
                }
            }
        }
        for (rel, base) in &baseline {
            if !scan.contains_key(rel) {
                if base.kind == protocol::TYPE_DIR && has_descendant(&scan, rel) {
                    continue; // 目录内还有文件：按文件级删除处理
                }
                del.insert(rel.clone());
            }
        }

        // 6. 本地重命名检测（FR-S6）
        let moves = detect_moves(&mut del, &mut new_files, &baseline, &scan);
        let move_src: HashSet<&String> = moves.iter().map(|m| &m.old).collect();

        // 7. 下行处理
        let mut downloads: HashMap<String, NodeInfo> = HashMap::new();
        let mut mkdirs_local: HashSet<String> = HashSet::new();
        let mut state_del: HashSet<String> = HashSet::new();
        let mut state_set: HashMap<String, Rec> = HashMap::new();
        let mut skip_upload: HashSet<String> = HashSet::new();
        let mut moved_prefixes: Vec<(String, String)> = Vec::new();

        let mut changed_rels: Vec<String> = changed_set.iter().cloned().collect();
        changed_rels.sort();

        for rel_raw in &changed_rels {
            let mut rel = rel_raw.clone();
            let Some(srec) = server_now.get(&rel).cloned() else {
                // 服务端已删除 rel
                let is_del = del.contains(&rel);
                let has_scan = scan.contains_key(&rel);
                let lrec = baseline.get(&rel).cloned();
                let is_mod = modified.contains_key(&rel);
                let is_new = new_files.contains_key(&rel);
                if is_del || move_src.contains(&rel) {
                    del.remove(&rel);
                    state_del.insert(rel.clone());
                } else if has_scan && lrec.is_some() && !is_mod && !is_new {
                    remove_all(&abs_join(&root, &rel));
                    state_del.insert(rel.clone());
                    stats.deleted += 1;
                } else {
                    state_del.insert(rel.clone());
                }
                continue;
            };
            let lrec = baseline.get(&rel).cloned();
            let lhas = lrec.is_some();
            let drec = scan.get(&rel).cloned();
            let dhas = drec.is_some();
            let is_mod = modified.contains_key(&rel);
            let is_new = new_files.contains_key(&rel);
            let is_del = del.contains(&rel);

            if srec.kind.is_empty() {
                continue;
            }
            let known_node = baseline_by_node.get(&srec.id).cloned();
            if let Some(old_rel) = &known_node {
                if let Some(np) = apply_moved_prefix(&moved_prefixes, old_rel) {
                    // 该节点随已改名的父目录一起移动，磁盘已就位
                    state_del.insert(old_rel.clone());
                    state_set.insert(np.clone(), rec_from_node(&srec));
                    if np == rel {
                        continue;
                    }
                    rel = np;
                }
            }

            let local_changed_here =
                is_mod || (is_new && known_node.as_deref() == Some(rel.as_str()));

            if srec.kind == protocol::TYPE_FILE && local_changed_here {
                // 双方都改了同一文件
                let lh = modified
                    .get(&rel)
                    .or_else(|| new_files.get(&rel))
                    .cloned()
                    .unwrap_or_default();
                if lh == srec.content_hash {
                    skip_upload.insert(rel.clone());
                    modified.remove(&rel);
                    new_files.remove(&rel);
                    del.remove(&rel);
                    state_set.insert(rel.clone(), rec_from_node(&srec));
                    set_local_mtime(&root, &rel, srec.mtime);
                } else {
                    // 冲突：本地版本留在原位，服务端版本存冲突副本（FR-S7）
                    let cc = self.conflict_copy(&rel, &srec, &mut downloads, &scan)?;
                    stats.conflicts += 1;
                    state_set.insert(
                        cc.clone(),
                        Rec { node_id: 0, hash: srec.content_hash.clone(), size: srec.size, mtime: srec.mtime, kind: protocol::TYPE_FILE.into() },
                    );
                    new_files.insert(cc.clone(), srec.content_hash.clone());
                    scan.insert(cc, DiskInfo { size: srec.size, mtime: srec.mtime, is_dir: false });
                    del.remove(&rel);
                }
            } else if let Some(old_rel) = &known_node {
                if old_rel != &rel && !is_del && !local_changed_here {
                    // 服务端移动/改名：本地跟随（rename 语义）
                    if dhas {
                        // 目标路径本地仍有文件：服务端版本存冲突副本，避免覆盖丢数据
                        let cc = self.conflict_copy(&rel, &srec, &mut downloads, &scan)?;
                        stats.conflicts += 1;
                        state_set.insert(
                            cc.clone(),
                            Rec { node_id: 0, hash: srec.content_hash.clone(), size: srec.size, mtime: srec.mtime, kind: protocol::TYPE_FILE.into() },
                        );
                        new_files.insert(cc.clone(), srec.content_hash.clone());
                        scan.insert(cc, DiskInfo { size: srec.size, mtime: srec.mtime, is_dir: false });
                        continue;
                    }
                    let old_path = abs_join(&root, old_rel);
                    if let Some(dir) = abs_join(&root, &rel).parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    if let Err(e) = std::fs::rename(&old_path, abs_join(&root, &rel)) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            return Err(ysync_core::Error::Msg(format!("rename: {e}")));
                        }
                    }
                    if srec.kind == protocol::TYPE_DIR {
                        moved_prefixes.push((old_rel.clone(), rel.clone()));
                    } else {
                        set_local_mtime(&root, &rel, srec.mtime);
                    }
                    stats.moved += 1;
                    state_del.insert(old_rel.clone());
                    state_set.insert(rel.clone(), rec_from_node(&srec));
                } else if !dhas {
                    // 已知节点、本地旧位置无文件
                    let lrec_ref = lrec.clone();
                    if is_del
                        && srec.kind == protocol::TYPE_FILE
                        && lrec_ref.as_ref().map(|l| l.hash.clone()) != Some(srec.content_hash.clone())
                    {
                        // 本地删除 + 服务端修改：保留服务端版本为冲突副本
                        let cc = self.conflict_copy(&rel, &srec, &mut downloads, &scan)?;
                        stats.conflicts += 1;
                        state_set.insert(
                            cc.clone(),
                            Rec { node_id: 0, hash: srec.content_hash.clone(), size: srec.size, mtime: srec.mtime, kind: protocol::TYPE_FILE.into() },
                        );
                        new_files.insert(cc.clone(), srec.content_hash.clone());
                        scan.insert(cc, DiskInfo { size: srec.size, mtime: srec.mtime, is_dir: false });
                        del.remove(&rel);
                        state_del.insert(rel.clone());
                    } else if is_del || move_src.contains(old_rel) {
                        if move_src.contains(old_rel)
                            && srec.kind == protocol::TYPE_FILE
                            && lrec_ref.as_ref().map(|l| l.hash.clone()) != Some(srec.content_hash.clone())
                        {
                            let cc = self.conflict_copy(&rel, &srec, &mut downloads, &scan)?;
                            stats.conflicts += 1;
                            state_set.insert(
                                cc.clone(),
                                Rec { node_id: 0, hash: srec.content_hash.clone(), size: srec.size, mtime: srec.mtime, kind: protocol::TYPE_FILE.into() },
                            );
                            new_files.insert(cc.clone(), srec.content_hash.clone());
                            scan.insert(cc, DiskInfo { size: srec.size, mtime: srec.mtime, is_dir: false });
                        }
                        state_del.insert(rel.clone());
                    } else {
                        // 本地缺失但未记录删除（异常）：恢复
                        if srec.kind == protocol::TYPE_DIR {
                            mkdirs_local.insert(rel.clone());
                        } else {
                            downloads.insert(rel.clone(), srec.clone());
                        }
                        state_set.insert(rel.clone(), rec_from_node(&srec));
                    }
                } else {
                    // 同一节点同路径（oldRel == rel），本地未改：与服务端对齐
                    if let (true, Some(l)) = (lhas, &lrec) {
                        if srec.content_hash == l.hash && srec.kind == l.kind {
                            state_set.insert(rel.clone(), rec_from_node(&srec));
                            set_local_mtime(&root, &rel, srec.mtime);
                            mtime_only.remove(&rel);
                            continue;
                        }
                    }
                    if srec.kind == protocol::TYPE_DIR {
                        mkdirs_local.insert(rel.clone());
                        state_set.insert(rel.clone(), rec_from_node(&srec));
                    } else {
                        downloads.insert(rel.clone(), srec.clone());
                        state_set.insert(rel.clone(), rec_from_node(&srec));
                        mtime_only.remove(&rel);
                    }
                }
            } else if !dhas {
                // 纯服务端新增
                if srec.kind == protocol::TYPE_DIR {
                    mkdirs_local.insert(rel.clone());
                } else {
                    downloads.insert(rel.clone(), srec.clone());
                }
                state_set.insert(rel.clone(), rec_from_node(&srec));
            } else {
                // 本地新增与服务端新增撞路径
                let d = drec.unwrap();
                if srec.kind == protocol::TYPE_DIR || d.is_dir {
                    if srec.kind != protocol::TYPE_DIR || !d.is_dir {
                        eprintln!("level=WARN msg=\"类型冲突，跳过该路径（M1）\" path={rel:?}");
                        continue;
                    }
                    mkdirs_local.insert(rel.clone());
                    state_set.insert(rel.clone(), rec_from_node(&srec));
                    continue;
                }
                let lh = match new_files.get(&rel) {
                    Some(h) => h.clone(),
                    None => hash_local_file_helper(&root, &rel)?.0,
                };
                if lh == srec.content_hash {
                    skip_upload.insert(rel.clone());
                    new_files.remove(&rel);
                    state_set.insert(rel.clone(), rec_from_node(&srec));
                } else {
                    let cc = self.conflict_copy(&rel, &srec, &mut downloads, &scan)?;
                    stats.conflicts += 1;
                    state_set.insert(
                        cc.clone(),
                        Rec { node_id: 0, hash: srec.content_hash.clone(), size: srec.size, mtime: srec.mtime, kind: protocol::TYPE_FILE.into() },
                    );
                    new_files.insert(cc.clone(), srec.content_hash.clone());
                    scan.insert(cc, DiskInfo { size: srec.size, mtime: srec.mtime, is_dir: false });
                }
            }
        }

        // 8. 执行本地写入（建目录 / 下载）
        for rel in &mkdirs_local {
            // P0-3：大小写不敏感 FS 上 casefold 重名目录 = 现有目录合并（不再新建）
            if case_insensitive {
                let (dir, name) = split_dir(rel);
                let key = (dir.clone(), name.to_lowercase());
                let dup = casefold
                    .get(&key)
                    .map(|v| v.iter().any(|r| r.as_str() != rel))
                    .unwrap_or(false);
                if dup {
                    eprintln!("level=WARN msg=\"大小写重名目录，合并到现有目录\" path={rel:?}");
                    continue;
                }
            }
            std::fs::create_dir_all(abs_join(&root, rel))?;
        }
        let mut dl_items: Vec<(String, NodeInfo)> = downloads.into_iter().collect();
        dl_items.sort_by(|a, b| a.0.cmp(&b.0));
        // P0-3 casefold 串行预决策（快）：确定每个下载的最终落盘路径
        let mut planned: Vec<(String, NodeInfo, bool)> = Vec::new();
        for (rel, n) in dl_items {
            let mut rel = rel;
            let mut is_cc = false;
            if case_insensitive {
                let (dir, name) = split_dir(&rel);
                let key = (dir.clone(), name.to_lowercase());
                let dup = casefold
                    .get(&key)
                    .map(|v| v.iter().any(|r| r.as_str() != rel))
                    .unwrap_or(false);
                if dup {
                    let ext = match name.rfind('.') {
                        Some(i) if i > 0 => name[i..].to_string(),
                        _ => String::new(),
                    };
                    let base = name.trim_end_matches(&ext);
                    let mk = |i: usize| -> String {
                        let nm = if i == 1 {
                            format!("{base} (case conflict){ext}")
                        } else {
                            format!("{base} (case conflict) {i}{ext}")
                        };
                        if dir.is_empty() { nm } else { format!("{dir}/{nm}") }
                    };
                    let mut cc = mk(1);
                    let mut i = 2;
                    while scan.contains_key(&cc) {
                        cc = mk(i);
                        i += 1;
                    }
                    eprintln!(
                        "level=WARN msg=\"大小写冲突，下载为副本\" path={rel:?} copy={cc:?}"
                    );
                    rel = cc;
                    is_cc = true;
                }
                let (dir, name) = split_dir(&rel);
                casefold
                    .entry((dir, name.to_lowercase()))
                    .or_default()
                    .push(rel.clone());
            }
            planned.push((rel, n, is_cc));
        }
        // 建父目录（并行抓取前统一完成）
        for (rel, _, _) in &planned {
            if let Some(dir) = abs_join(&root, rel).parent() {
                std::fs::create_dir_all(dir)?;
            }
        }
        // P1 并行下载：4 线程消费队列；GetContent 临时名唯一，落盘原子
        let queue: Arc<Mutex<std::collections::VecDeque<(usize, String, NodeInfo, bool)>>> =
            Arc::new(Mutex::new(
                planned
                    .iter()
                    .enumerate()
                    .map(|(i, (rel, n, cc))| (i, rel.clone(), n.clone(), *cc))
                    .collect(),
            ));
        let out: Arc<Mutex<Vec<(usize, Rec)>>> = Arc::new(Mutex::new(Vec::new()));
        let cc_out: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new())); // (副本rel, 原node_id字符串占位)
        let first_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        {
            let root = root.clone();
            std::thread::scope(|s| {
                for _ in 0..4.min(planned.len().max(1)) {
                    let queue = queue.clone();
                    let out = out.clone();
                    let cc_out = cc_out.clone();
                    let first_err = first_err.clone();
                    let root = root.clone();
                    s.spawn(move || {
                        loop {
                            let item = { queue.lock().unwrap().pop_back() };
                            let Some((idx, rel, n, is_cc)) = item else { break };
                            let abs = abs_join(&root, &rel);
                            let r = self
                                .with_api(|a| a.get_content(&n.content_hash, &abs, n.mtime))
                                .and_then(|_| hash_and_size(&abs))
                                .map(|(h, size)| {
                                    (idx, Rec { node_id: if is_cc { 0 } else { n.id }, hash: h, size, mtime: n.mtime, kind: protocol::TYPE_FILE.into() })
                                });
                            match r {
                                Ok(v) => {
                                    out.lock().unwrap().push((idx, v.1));
                                    if is_cc {
                                        cc_out.lock().unwrap().push((rel, n.content_hash.clone()));
                                    }
                                }
                                Err(e) => {
                                    let mut g = first_err.lock().unwrap();
                                    if g.is_none() {
                                        *g = Some(format!("download {rel}: {e}"));
                                    }
                                }
                            }
                        }
                    });
                }
            });
        }
        if let Some(e) = first_err.lock().unwrap().take() {
            return Err(ysync_core::Error::Msg(e));
        }
        // 串行落账（快）：state/scan/new_files
        let done = out.lock().unwrap();
        let mut done_sorted: Vec<(usize, Rec)> =
            done.iter().map(|(i, r)| (*i, r.clone())).collect();
        done_sorted.sort_by_key(|(i, _)| *i);
        for (idx, rec) in &done_sorted {
            let (rel, _, is_cc) = &planned[*idx];
            state_set.insert(rel.clone(), rec.clone());
            scan.insert(rel.clone(), DiskInfo { size: rec.size, mtime: rec.mtime, is_dir: false });
            if *is_cc {
                // 冲突副本回传服务端（put 新建），其他设备可见
                new_files.insert(rel.clone(), rec.hash.clone());
            }
        }
        stats.downloaded += done_sorted.len() as i64;
        drop(done);

        // 9. 上行之内容上传（去重）
        let node_at: HashMap<String, i64> =
            server_now.iter().map(|(k, v)| (k.clone(), v.id)).collect();
        let mut created_dirs: HashMap<String, i64> = HashMap::new();

        let mut uploads: Vec<(String, String)> = Vec::new();
        for (rel, h) in &modified {
            if !skip_upload.contains(rel) {
                uploads.push((rel.clone(), h.clone()));
            }
        }
        for (rel, h) in &new_files {
            if !skip_upload.contains(rel) {
                uploads.push((rel.clone(), h.clone()));
            }
        }
        let snap = crate::ctx::snapshot();
        let cdc_threshold = snap.chunk_threshold_mb << 20;
        // P1-8 CDC 参数（配置可调，测试可调小）
        let cdc = crate::chunk::CdcParams::from_config(
            snap.cdc_min_kb,
            snap.cdc_avg_kb,
            snap.cdc_max_kb,
        );
        // 大文件走 CDC 增量上传（仅变化块重传）；普通小文件并行
        let mut plain: Vec<(String, String)> = Vec::new();
        for (rel, hash) in &uploads {
            let abs = abs_join(&root, rel);
            let size = scan.get(rel).map(|d| d.size).unwrap_or(0);
            if size >= cdc_threshold {
                // FR-S11/P1-8：CDC 切块 → 缺块补传 → 清单上传
                let mhash = self.with_api(|a| {
                    a.put_content_cdc(&abs, hash, size, &cdc)
                })?;
                let _ = st.clear_upload_session(rel, hash);
                if mhash != *hash {
                    return Err(ysync_core::Error::Msg(format!(
                        "upload {rel}: hash changed during sync"
                    )));
                }
                stats.uploaded += 1;
            } else {
                plain.push((rel.clone(), hash.clone()));
            }
        }
        // 普通上传并行：4 线程消费队列（内容去重由服务端完成）
        {
            let q: Arc<Mutex<std::collections::VecDeque<(String, String)>>> = Arc::new(
                Mutex::new(plain.into_iter().collect()),
            );
            let first_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let okcnt = Arc::new(std::sync::atomic::AtomicI64::new(0));
            std::thread::scope(|s| {
                for _ in 0..4 {
                    let q = q.clone();
                    let fe = first_err.clone();
                    let ok = okcnt.clone();
                    let root = root.clone();
                    let api = self.api.clone();
                    s.spawn(move || loop {
                        let item = { q.lock().unwrap().pop_back() };
                        let Some((rel, want)) = item else { break };
                        let abs = abs_join(&root, &rel);
                        let r = api
                            .put_content(&abs, &want)
                            .and_then(|(h, _)| {
                                if h == want {
                                    Ok(())
                                } else {
                                    Err(ysync_core::Error::Msg(format!(
                                        "upload {rel}: hash changed during sync"
                                    )))
                                }
                            });
                        match r {
                            Ok(()) => {
                                ok.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(e) => {
                                let mut g = fe.lock().unwrap();
                                if g.is_none() {
                                    *g = Some(format!("upload {rel}: {e}"));
                                }
                            }
                        }
                    });
                }
            });
            if let Some(e) = first_err.lock().unwrap().take() {
                return Err(ysync_core::Error::Msg(e));
            }
            stats.uploaded += okcnt.load(std::sync::atomic::Ordering::Relaxed);
        }

        // 10. 上行之元数据操作（mkdir → unlink → move → put）
        crate::state::write_pending_marker(&root);

        // 10a. mkdir：按深度逐层分批——同批内父目录 id 尚不可知，
        // 必须等上一层结果回填 created_dirs 后再发下一层（多级目录链的正确性关键）
        let mut pending_dirs: Vec<String> = local_dirs
            .iter()
            .filter(|rel| node_at.get(*rel).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        pending_dirs.sort_by_key(|r| depth(r));
        while !pending_dirs.is_empty() {
            let min_depth = pending_dirs.iter().map(|r| depth(r)).min().unwrap();
            let this_batch: Vec<String> = pending_dirs
                .iter()
                .filter(|r| depth(r) == min_depth)
                .cloned()
                .collect();
            pending_dirs.retain(|r| depth(r) != min_depth);
            let mut mkdir_ops: Vec<Op> = Vec::new();
            let mut mkdir_rels: Vec<String> = Vec::new();
            for rel in &this_batch {
                let (parent_id, pname) = match self.parent_for(rel, f, &node_at, &created_dirs) {
                    Some(v) => v,
                    None => continue, // 父目录缺失：留待后续批次
                };
                mkdir_ops.push(Op { op: protocol::OP_MKDIR.into(), parent_id, name: pname, ..Default::default() });
                mkdir_rels.push(rel.clone());
            }
            if mkdir_ops.is_empty() {
                continue;
            }
            let res = self.with_api(|a| a.ops(&mkdir_ops))?;
            for (i, rel) in mkdir_rels.iter().enumerate() {
                if let Some(r) = res.get(i) {
                    if r.ok {
                        created_dirs.insert(rel.clone(), r.node_id);
                        state_set.insert(rel.clone(), Rec { node_id: r.node_id, kind: protocol::TYPE_DIR.into(), ..Default::default() });
                    } else {
                        eprintln!("level=WARN msg=\"mkdir failed\" path={rel:?} err={:?}", r.error);
                    }
                }
            }
        }

        // 10b. unlink（先深后浅）
        let mut del_rels: Vec<String> = Vec::new();
        for rel in &del {
            if node_at.get(rel).copied().unwrap_or(0) != 0 {
                del_rels.push(rel.clone());
            } else {
                state_del.insert(rel.clone());
            }
        }
        del_rels.sort_by_key(|r| std::cmp::Reverse(depth(r)));
        if !del_rels.is_empty() {
            let ops: Vec<Op> = del_rels
                .iter()
                .map(|rel| Op { op: protocol::OP_UNLINK.into(), node_id: node_at.get(rel).copied().unwrap_or(0), ..Default::default() })
                .collect();
            let res = self.with_api(|a| a.ops(&ops))?;
            for (i, rel) in del_rels.iter().enumerate() {
                if let Some(r) = res.get(i) {
                    if r.ok {
                        state_del.insert(rel.clone());
                        stats.deleted += 1;
                    } else {
                        eprintln!("level=WARN msg=\"unlink failed\" path={rel:?} err={:?}", r.error);
                    }
                }
            }
        }

        // 10c. move（降级为删除+重建）
        let mut move_ops: Vec<Op> = Vec::new();
        let mut move_plans: Vec<MoveRec> = Vec::new();
        let mut demoted: Vec<String> = Vec::new();
        for mv in &moves {
            let node_id = node_at.get(&mv.old).copied().unwrap_or(0);
            if node_id == 0 || under_any(&del_rels, &mv.old) || under_any(&del_rels, &parent_of(&mv.new)) {
                demoted.push(mv.new.clone());
                state_del.insert(mv.old.clone());
                continue;
            }
            let (parent_id, pname) = match self.parent_for(&mv.new, f, &node_at, &created_dirs) {
                Some(v) => v,
                None => {
                    demoted.push(mv.new.clone());
                    state_del.insert(mv.old.clone());
                    continue;
                }
            };
            move_ops.push(Op {
                op: protocol::OP_MOVE.into(),
                node_id,
                parent_id,
                name: pname,
                ..Default::default()
            });
            move_plans.push(mv.clone());
        }
        if !move_ops.is_empty() {
            let res = self.ops_chunked(&move_ops)?;
            for (i, mv) in move_plans.iter().enumerate() {
                if let Some(r) = res.get(i) {
                    if r.ok {
                        state_del.insert(mv.old.clone());
                        let d = scan.get(&mv.new).cloned().unwrap_or_default();
                        if d.is_dir {
                            state_set.insert(mv.new.clone(), Rec { node_id: node_at.get(&mv.old).copied().unwrap_or(0), kind: protocol::TYPE_DIR.into(), ..Default::default() });
                        } else {
                            state_set.insert(mv.new.clone(), Rec { node_id: node_at.get(&mv.old).copied().unwrap_or(0), hash: mv.hash.clone(), size: d.size, mtime: d.mtime, kind: protocol::TYPE_FILE.into() });
                        }
                        stats.moved += 1;
                    } else {
                        eprintln!("level=WARN msg=\"move failed\" path={:?} err={:?}", mv.old, r.error);
                        demoted.push(mv.new.clone());
                    }
                }
            }
        }
        for rel in &demoted {
            if let Some(d) = scan.get(rel) {
                if !d.is_dir {
                    let (h, _) = hash_local_file_helper(&root, rel)?;
                    new_files.insert(rel.clone(), h);
                }
            }
        }

        // 10d. put
        let mut put_ops: Vec<Op> = Vec::new();
        let mut put_rels: Vec<String> = Vec::new();
        let mut put_hash: Vec<String> = Vec::new();
        let mut put_mtime: Vec<i64> = Vec::new();
        let mut add_put = |rel: &str, hash: &str, node_id: i64,
                           node_at: &HashMap<String, i64>,
                           created_dirs: &HashMap<String, i64>,
                           f: &FolderCfg,
                           scan: &HashMap<String, DiskInfo>,
                           put_ops: &mut Vec<Op>, put_rels: &mut Vec<String>,
                           put_hash: &mut Vec<String>, put_mtime: &mut Vec<i64>| {
            let mut node_id = node_id;
            if node_id == 0 {
                node_id = node_at.get(rel).copied().unwrap_or(0);
            }
            let (dir, name) = split_dir(rel);
            let parent_id = if dir.is_empty() {
                f.root_node_id
            } else {
                match created_dirs.get(&dir).or_else(|| node_at.get(&dir)) {
                    Some(id) if *id != 0 => *id,
                    _ => {
                        eprintln!("level=WARN msg=\"缺少父目录，跳过上传\" path={rel:?}");
                        return;
                    }
                }
            };
            let size = scan.get(rel).map(|d| d.size).unwrap_or(0);
            let mtime = scan.get(rel).map(|d| d.mtime).unwrap_or(0);
            if node_id > 0 {
                put_ops.push(Op {
                    op: protocol::OP_PUT.into(),
                    node_id,
                    content_hash: hash.to_string(),
                    size,
                    mtime,
                    name: name.clone(),
                    ..Default::default()
                });
            } else {
                put_ops.push(Op {
                    op: protocol::OP_PUT.into(),
                    parent_id,
                    name: name.clone(),
                    content_hash: hash.to_string(),
                    size,
                    mtime,
                    ..Default::default()
                });
            }
            put_rels.push(rel.to_string());
            put_hash.push(hash.to_string());
            put_mtime.push(mtime);
        };
        for rel in &mtime_only {
            if skip_upload.contains(rel) {
                continue;
            }
            let (hash, node_id) = match baseline.get(rel) {
                Some(b) => (b.hash.clone(), b.node_id),
                None => continue,
            };
            add_put(rel, &hash, node_id, &node_at, &created_dirs, f, &scan, &mut put_ops, &mut put_rels, &mut put_hash, &mut put_mtime);
        }
        for (rel, h) in &modified {
            if skip_upload.contains(rel) {
                continue;
            }
            add_put(rel, h, 0, &node_at, &created_dirs, f, &scan, &mut put_ops, &mut put_rels, &mut put_hash, &mut put_mtime);
        }
        for (rel, h) in &new_files {
            if skip_upload.contains(rel) {
                continue;
            }
            add_put(rel, h, 0, &node_at, &created_dirs, f, &scan, &mut put_ops, &mut put_rels, &mut put_hash, &mut put_mtime);
        }
        if !put_ops.is_empty() {
            let res = self.ops_chunked(&put_ops)?;
            for i in 0..put_rels.len() {
                if let Some(r) = res.get(i) {
                    if r.ok {
                        let d = scan.get(&put_rels[i]).cloned().unwrap_or_default();
                        state_set.insert(
                            put_rels[i].clone(),
                            Rec { node_id: r.node_id, hash: put_hash[i].clone(), size: d.size, mtime: put_mtime[i], kind: protocol::TYPE_FILE.into() },
                        );
                    } else {
                        eprintln!("level=WARN msg=\"put failed\" path={:?} err={:?}", put_rels[i], r.error);
                    }
                }
            }
        }

        // 11. 持久化状态与新 cursor（FR-S14；B2：单事务，大文件夹免逐行 fsync）
        let dels: Vec<String> = state_del.iter().cloned().collect();
        let sets: Vec<(String, Rec)> =
            state_set.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        st.persist(&dels, &sets, new_cursor)?;
        f.cursor = new_cursor;
        self.persist_folder(f)?;
        crate::state::clear_pending_marker(&root);
        Ok(stats)
    }

    fn parent_for(
        &self,
        rel: &str,
        f: &FolderCfg,
        node_at: &HashMap<String, i64>,
        created_dirs: &HashMap<String, i64>,
    ) -> Option<(i64, String)> {
        let (dir, name) = split_dir(rel);
        if dir.is_empty() {
            return Some((f.root_node_id, name));
        }
        if let Some(id) = created_dirs.get(&dir) {
            return Some((*id, name));
        }
        if let Some(id) = node_at.get(&dir) {
            if *id != 0 {
                return Some((*id, name));
            }
        }
        None
    }
}

fn ignore_root(root: &Path, use_gitignore: bool) -> Ignore {
    let mut patterns: Vec<String> = crate::ignore::DEFAULT_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Ok(b) = std::fs::read(root.join(".syncignore")) {
        patterns.extend(String::from_utf8_lossy(&b).lines().map(|s| s.to_string()));
    }
    if use_gitignore {
        if let Ok(b) = std::fs::read(root.join(".gitignore")) {
            patterns.extend(String::from_utf8_lossy(&b).lines().map(|s| s.to_string()));
        }
    }
    Ignore::new(&[]).with_extra(patterns)
}

impl Engine {
    /// 把游标/根节点写回共享配置并持久化（Go 版 SaveConfig(e.Cfg) 对应）。
    fn persist_folder(&self, f: &FolderCfg) -> Result<(), ysync_core::Error> {
        crate::ctx::with_cfg(|c| {
            if let Some(x) = c.folders.iter_mut().find(|x| x.name == f.name) {
                x.cursor = f.cursor;
                x.root_node_id = f.root_node_id;
            }
        });
        crate::ctx::save();
        Ok(())
    }
    fn own_device_id(&self) -> i64 {
        crate::ctx::device_id()
    }
}
