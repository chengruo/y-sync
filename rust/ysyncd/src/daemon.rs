//! daemon 运行时：控制服务 + FS 监听 + WS 订阅 + 兜底轮询（Go internal/client/daemon.go 对应）。
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;



use crate::conflicts::{list_conflicts, resolve_keep_copy, resolve_keep_local};
use crate::ctx;
use crate::daemon_state::DaemonState;
use crate::engine::{Engine, FolderCfg};
use crate::httpd;

#[derive(Clone)]
pub struct Daemon {
    pub engine: Arc<Engine>,
    pub state: Arc<DaemonState>,
    pub log: Arc<dyn Fn(String) + Send + Sync>,
    pub only: String,
    pub http_addr: String,
    pub token: String,
    pub stop_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    /// setup 模式（UI 配置访问）：config.json 尚未初始化时，
    /// daemon 仅提供控制服务，用户经浏览器完成登录后再进入正常同步循环。
    pub setup_mode: bool,
    pub setup_done: Arc<std::sync::atomic::AtomicBool>,
    pub watched: Arc<Mutex<std::collections::HashSet<String>>>,
    pub watcher_slot: Arc<Mutex<Option<crate::watcher::Watcher>>>,
    pub watch_tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,
}

impl Daemon {
    pub fn log(&self, msg: String) {
        (self.log)(msg);
    }

    /// 由本地路径定位文件夹并同步（FS 事件用）。
    pub fn sync_by_local_path(&self, local_path: String) {
        ctx::maybe_reload();
        let abs = std::path::Path::new(&local_path).to_path_buf();
        let folder = ctx::with_cfg(|c| {
            c.folders
                .iter()
                .find(|f| {
                    std::path::Path::new(&f.local_path) == abs
                        && (self.only.is_empty() || self.only == f.name)
                })
                .cloned()
        });
        if let Some(f) = folder {
            self.sync_folder(&f);
        }
    }

    pub fn sync_all(&self) {
        ctx::maybe_reload();
        // 热重载后新文件夹需要进入状态快照（/status 可见性）
        self.state.init_folders(&ctx::snapshot().folders);
        // 热重载后同步 engine 连接参数（A7：server_url/token 变更即时生效）
        ctx::with_cfg(|c| self.engine.api.set_connection(&c.server_url, &c.token));
        // 新增文件夹补挂 FS 监听（独立 watcher，事件并入同一防抖循环）
        if let Some(tx) = self.watch_tx.lock().unwrap().clone() {
            let folders = ctx::with_cfg(|c| c.folders.clone());
            let mut watched = self.watched.lock().unwrap();
            for f in &folders {
                if watched.insert(f.local_path.clone()) {
                    match crate::watcher::Watcher::new(tx.clone()) {
                        Ok(mut w) => {
                            if let Err(e) = w.add_recursive(std::path::Path::new(&f.local_path)) {
                                self.log(format!(
                                    "level=WARN msg=\"监听失败\" folder={:?} err={e:?}",
                                    f.name
                                ));
                            }
                            *self.watcher_slot.lock().unwrap() = Some(w);
                        }
                        Err(e) => self.log(format!(
                            "level=WARN msg=\"监听失败\" folder={:?} err={e:?}",
                            f.name
                        )),
                    }
                }
            }
        }
        let folders = ctx::with_cfg(|c| {
            c.folders
                .iter()
                .filter(|f| self.only.is_empty() || self.only == f.name)
                .cloned()
                .collect::<Vec<_>>()
        });
        for f in folders {
            self.sync_folder(&f);
        }
    }

    pub fn sync_folder(&self, f: &ysync_core::Folder) {
        // 已暂停或本进程内正在同步（WS/事件/轮询重叠）则跳过
        if !self.state.try_begin_sync(&f.name) {
            return;
        }
        let _sync_guard = SyncGuard {
            state: self.state.clone(),
            name: f.name.clone(),
        };
        let mut fc = FolderCfg {
            name: f.name.clone(),
            local_path: std::path::PathBuf::from(&f.local_path),
            root_node_id: f.root_node_id,
            cursor: f.cursor,
            excludes: f.excludes.clone(),
            use_gitignore: f.use_gitignore,
        };
        let stats = match self.engine.sync_folder(&mut fc) {
            Ok(s) => s,
            Err(e) => {
                self.log(format!(
                    "level=ERROR msg=\"sync failed\" folder={:?} err={e:?}",
                    f.name
                ));
                self.state.fail_sync(f, &format!("{e:?}"));
                return;
            }
        };
        if stats.uploaded + stats.downloaded + stats.moved + stats.deleted + stats.conflicts > 0 {
            self.log(format!(
                "level=INFO msg=synced folder={:?} up={} down={} moved={} deleted={} conflicts={}",
                f.name, stats.uploaded, stats.downloaded, stats.moved, stats.deleted, stats.conflicts
            ));
        }
        let files = files_tracked(&f.local_path);
        self.state.finish_sync(
            f,
            files,
            &format!(
                "↑{} ↓{} 移{} 删{}",
                stats.uploaded, stats.downloaded, stats.moved, stats.deleted
            ),
        );
        if stats.conflicts > 0 {
            self.state.add_conflicts(&f.name, stats.conflicts);
        }
    }

    // ---------- setup 模式（UI 配置访问） ----------

    /// 首次配置：校验凭据 → 落盘 → 切出 setup 模式。
    pub fn setup(
        &self,
        server_url: &str,
        user: &str,
        password: &str,
        device_name: &str,
    ) -> Result<(), String> {
        let resp = crate::api::Api::login(user, password, device_name, server_url)
            .map_err(|e| format!("登录失败: {e}"))?;
        let server_url = server_url.trim_end_matches('/').to_string();
        let device_name = if device_name.is_empty() {
            ysync_core::default_device_name()
        } else {
            device_name.to_string()
        };
        ctx::with_cfg(|c| {
            *c = ysync_core::Config {
                server_url,
                user: user.to_string(),
                token: resp.token.clone(),
                device_name: device_name.clone(),
                device_id: resp.device_id,
                ..Default::default()
            };
            c.defaults(); // 防 chunk_size=0 除零（P1 修复）
        });
        if let Err(e) = ctx::save_result() {
            return Err(format!("{e:?}"));
        }
        self.setup_done
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.state.init_folders(&ctx::snapshot().folders);
        self.log(format!(
            "level=INFO msg=\"setup 完成\" user={user:?} device={:?}",
            device_name
        ));
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        !self.setup_mode || self.setup_done.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 创建 watcher 并挂载当前全部文件夹（run 启动 / setup 完成后调用）。
    fn attach_watcher_now(&self) {
        if let Some(tx) = self.watch_tx.lock().unwrap().clone() {
            self.attach_watcher(&tx);
        }
    }

    fn attach_watcher(&self, tx: &mpsc::Sender<String>) {
        let mut slot = self.watcher_slot.lock().unwrap();
        let mut w = match crate::watcher::Watcher::new(tx.clone()) {
            Ok(w) => w,
            Err(e) => {
                self.log(format!("level=WARN msg=\"watcher 创建失败\" err={e:?}"));
                return;
            }
        };
        let folders = ctx::with_cfg(|c| c.folders.clone());
        for f in &folders {
            if let Err(e) = w.add_recursive(std::path::Path::new(&f.local_path)) {
                self.log(format!(
                    "level=WARN msg=\"监听失败\" folder={:?} err={e:?}",
                    f.name
                ));
            }
        }
        *slot = Some(w);
        self.log("level=INFO msg=\"FS 事件监听已启用\"".into());
    }

    // ---------- 服务端数据管理（P1：管理台代理服务端 API） ----------

    pub fn server_trash_list(&self) -> Result<Vec<ysync_core::protocol::TrashItem>, String> {
        self.engine.api.trash_list().map_err(|e| format!("{e:?}"))
    }
    pub fn server_trash_restore(&self, id: i64) -> Result<(), String> {
        self.engine.api.trash_restore(id).map_err(|e| format!("{e:?}"))
    }
    pub fn server_trash_delete(&self, id: i64) -> Result<(), String> {
        self.engine.api.trash_delete(id).map_err(|e| format!("{e:?}"))
    }
    pub fn server_versions(&self, folder: &str, rel: &str) -> Result<(i64, Vec<ysync_core::protocol::VersionItem>), String> {
        let target = format!("{folder}/{rel}");
        let node = self
            .engine
            .api
            .nodes()
            .map_err(|e| format!("{e:?}"))?
            .into_iter()
            .find(|n| n.path == target)
            .ok_or_else(|| format!("服务端不存在 {rel:?}"))?;
        let versions = self
            .engine
            .api
            .node_versions(node.id)
            .map_err(|e| format!("{e:?}"))?;
        Ok((node.id, versions))
    }
    pub fn server_version_restore(&self, folder: &str, rel: &str, version_id: i64) -> Result<(), String> {
        let local = ctx::with_cfg(|c| {
            c.folders
                .iter()
                .find(|f| f.name == folder)
                .map(|f| f.local_path.clone())
        })
        .ok_or_else(|| format!("文件夹 {folder:?} 不存在"))?;
        let dest = std::path::Path::new(&local).join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        self.engine
            .api
            .download_version_to(version_id, &dest, 0)
            .map_err(|e| format!("{e:?}"))
    }

    // ---------- 管理操作 ----------

    pub fn add_folder(
        &self,
        local_path: &str,
        name: &str,
        excludes: &[String],
        use_gitignore: bool,
    ) -> Result<(), String> {
        let abs = std::path::PathBuf::from(local_path);
        let abs = abs.canonicalize().unwrap_or(abs);
        if !abs.exists() {
            std::fs::create_dir_all(&abs).map_err(|e| format!("mkdir: {e}"))?;
        } else if !abs.is_dir() {
            return Err(format!("{} 不是目录", abs.display()));
        }
        let name = if name.is_empty() {
            abs.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            name.to_string()
        };
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(format!("非法的文件夹名 {name:?}"));
        }
        let abs_str = abs.to_string_lossy().to_string();
        let nested_err: Option<String> = ctx::with_cfg(|c| {
            for f in &c.folders {
                if f.name == name {
                    return Some(format!("文件夹 {name:?} 已存在"));
                }
                if ysync_core::is_sub_path(&f.local_path, &abs_str)
                    || ysync_core::is_sub_path(&abs_str, &f.local_path)
                {
                    return Some(format!(
                        "文件夹不得嵌套或重叠（FR-S15）：{} 与 {}",
                        f.local_path,
                        abs.display()
                    ));
                }
            }
            None
        });
        if let Some(e) = nested_err {
            return Err(e);
        }
        ctx::with_cfg(|c| {
            c.folders.push(ysync_core::Folder {
                name: name.clone(),
                local_path: abs_str.clone(),
                root_node_id: 0,
                cursor: 0,
                excludes: excludes.to_vec(),
                use_gitignore,
            });
        });
        if let Err(e) = ctx::save_result() {
            ctx::with_cfg(|c| {
                c.folders.retain(|f| f.name != name);
            });
            return Err(format!("{e:?}"));
        }
        self.state.init_folders(&ctx::snapshot().folders);
        self.log(format!(
            "level=INFO msg=\"folder added (via UI)\" name={name:?} local={:?}",
            abs.display()
        ));
        Ok(())
    }

    pub fn remove_folder(&self, name: &str) -> Result<(), String> {
        let removed = ctx::with_cfg(|c| {
            c.folders
                .iter()
                .position(|f| f.name == name)
                .map(|i| c.folders.remove(i))
        });
        let Some(f) = removed else {
            return Err(format!("文件夹 {name:?} 不存在"));
        };
        if let Err(e) = ctx::save_result() {
            ctx::with_cfg(|c| c.folders.push(f)); // 回滚
            return Err(format!("{e:?}"));
        }
        self.state.forget(name);
        self.log(format!("level=INFO msg=\"folder removed (via UI)\" name={name:?}"));
        Ok(())
    }

    pub fn conflicts(&self) -> Vec<crate::conflicts::Conflict> {
        let folders = ctx::with_cfg(|c| c.folders.clone());
        let mut out = Vec::new();
        for f in &folders {
            out.extend(list_conflicts(std::path::Path::new(&f.local_path), &f.name));
        }
        out
    }

    pub fn resolve_conflict(
        &self,
        folder_name: &str,
        rel: &str,
        copy_rel: &str,
        choice: &str,
    ) -> Result<(), String> {
        let local = ctx::with_cfg(|c| {
            c.folders
                .iter()
                .find(|f| f.name == folder_name)
                .map(|f| f.local_path.clone())
        })
        .ok_or_else(|| format!("文件夹 {folder_name:?} 不存在"))?;
        let root = std::path::Path::new(&local);
        let cs = list_conflicts(root, folder_name);
        let c = cs
            .iter()
            .find(|c| c.rel == rel && c.copy_rel == copy_rel)
            .ok_or_else(|| format!("未找到 {rel:?} 的冲突副本（{copy_rel}）"))?;
        match choice {
            "local" => resolve_keep_local(root, c).map_err(|e| format!("{e:?}")),
            "copy" => resolve_keep_copy(root, c).map_err(|e| format!("{e:?}")),
            _ => Err("choice 必须是 local 或 copy".into()),
        }
    }

    /// 阻塞运行全部循环（轮询/对账/信号）。
    pub fn run(&self, interval: Duration, reconcile: Duration) {
        self.log(format!(
            "level=INFO msg=\"daemon started\" interval={:?} reconcile={:?}",
            interval, reconcile
        ));
        self.state.init_folders(&ctx::snapshot().folders);

        // 控制服务（绑定成功后用实际地址写 daemon.json，支持随机端口）
        let mut actual_addr = self.http_addr.clone();
        if self.http_addr != "off" {
            let me = self.clone();
            match httpd::serve(&self.http_addr, self.token.clone(), me) {
                Err(e) => {
                    self.log(format!(
                        "level=WARN msg=\"控制 API 启动失败（继续运行）\" addr={} err={e:?}",
                        self.http_addr
                    ));
                }
                Ok(actual) => {
                    actual_addr = actual.clone();
                    self.log(format!(
                        "level=INFO msg=\"控制 API/管理页已启动\" addr=\"http://{actual}/?token={}…\"",
                        &self.token[..8.min(self.token.len())]
                    ));
                }
            }
            let _ = ysync_core::write_daemon_info(&ysync_core::DaemonInfo {
                pid: std::process::id() as i32,
                addr: actual_addr,
                token: self.token.clone(),
                started: crate::api::now_millis(),
            });
        }

        // FS 事件监听（递归 + 防抖）；setup 模式在配置完成后自动补建
        let (tx, rx) = mpsc::channel::<String>();
        *self.watch_tx.lock().unwrap() = Some(tx.clone());
        if !self.setup_mode {
            self.attach_watcher(&tx);
        }
        {
            let me = self.clone();
            std::thread::spawn(move || {
                crate::watcher::debounce_loop(rx, Duration::from_secs(2), move |p| {
                    me.sync_by_local_path(p);
                });
            });
        }

        // WebSocket 订阅（准实时；断线退化为轮询；重连时读取最新 base/token——A7）
        {
            let me = self.clone();
            let api = self.engine.api.clone();
            std::thread::spawn(move || ws_loop(api, move || me.sync_all()));
        }

        // setup 模式：等待用户经管理台完成首次配置，再拉起同步循环
        if self.setup_mode {
            self.log("level=INFO msg=\"setup 模式：请在管理台完成服务器与账号配置\"".into());
            while !self
                .setup_done
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                std::thread::sleep(Duration::from_millis(500));
            }
            self.log("level=INFO msg=\"配置完成，进入正常同步循环\"".into());
            self.attach_watcher_now();
        }

        // 立即先同步一轮（与 Go daemon 首轮行为对齐）
        self.sync_all();

        // 兜底轮询 + 定时对账
        let me = self.clone();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        *self.stop_tx.lock().unwrap() = Some(stop_tx);
        std::thread::spawn(move || {
            let mut last_reconcile = std::time::Instant::now();
            loop {
                match stop_rx.recv_timeout(interval) {
                    Ok(()) => return, // 收到停止信号
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        me.sync_all();
                        if last_reconcile.elapsed() >= reconcile {
                            last_reconcile = std::time::Instant::now();
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        // 信号等待（Windows 无 signal-hook iterator：依赖服务管理器终止，daemon.json 残留可忽略）
        #[cfg(unix)]
        {
            if let Ok(mut signals) = signal_hook::iterator::Signals::new([
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
            ]) {
                for sig in signals.forever() {
                    self.log(format!("level=INFO msg=\"daemon 退出（信号 {sig}）\""));
                    if self.http_addr != "off" {
                        ysync_core::clear_daemon_info();
                    }
                    std::process::exit(0);
                }
            }
        }
    }
}

/// 进程内同步守卫：任何退出路径都释放 syncing 标记。
struct SyncGuard {
    state: std::sync::Arc<DaemonState>,
    name: String,
}
impl Drop for SyncGuard {
    fn drop(&mut self) {
        self.state.end_sync(&self.name);
    }
}

fn files_tracked(local: &str) -> i64 {
    crate::state::State::open(std::path::Path::new(local))
        .and_then(|s| s.all().map(|m| m.len() as i64))
        .unwrap_or(0)
}

/// WS 订阅循环（tokio-tungstenite；断线重连指数退避封顶 30s）。
fn ws_loop(api: Arc<crate::api::Api>, on_notify: impl Fn() + Send + 'static) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let mut backoff = 1u64;
        loop {
            let url = {
                let ws_base = api.get_base().replacen("http", "ws", 1);
                format!("{ws_base}/api/v1/notify?token={}", api.get_token())
            };
            match tokio_tungstenite::connect_async(&url).await {
                Ok((mut ws, _)) => {
                    backoff = 1;
                    use futures_util::StreamExt;
                    loop {
                        match ws.next().await {
                            Some(Ok(_)) => on_notify(), // 只推事件不推数据
                            _ => break,
                        }
                    }
                }
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}
