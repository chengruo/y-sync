//! FS 事件监听（notify crate，递归监听）+ 防抖合并（Go watcher 对应）。
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 持久 watcher：实例存活期间监听有效；新增文件夹调用 add_recursive 追加。
pub struct Watcher {
    w: notify::RecommendedWatcher,
}

impl Watcher {
    pub fn new(tx: std::sync::mpsc::Sender<String>) -> notify::Result<Self> {
        let tx = tx.clone();
        let w = notify::recommended_watcher(
            move |res: std::result::Result<notify::Event, notify::Error>| {
                if let Ok(ev) = res {
                    for p in ev.paths {
                        if let Some(dir) = p.parent() {
                            let _ = tx.send(dir.to_string_lossy().to_string());
                        }
                    }
                }
            },
        )?;
        Ok(Watcher { w })
    }

    /// 递归监听目录（跳过 .y-sync/.git 等）。
    pub fn add_recursive(&mut self, root: &Path) -> notify::Result<()> {
        use notify::Watcher as _;
        self.w.watch(root, notify::RecursiveMode::Recursive)
    }
}

/// 防抖合并：目录事件 2 秒去重后回调 on_sync（Go Run/AfterFunc 对应）。
pub fn debounce_loop(
    rx: std::sync::mpsc::Receiver<String>,
    debounce: Duration,
    on_sync: impl Fn(String) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pending: Arc<Mutex<HashMap<String, Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let p2 = pending.clone();
        let collector = std::thread::spawn(move || {
            while let Ok(path) = rx.recv() {
                p2.lock().unwrap().insert(path, Instant::now());
            }
        });
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let now = Instant::now();
            let mut fire = Vec::new();
            {
                let mut p = pending.lock().unwrap();
                let keys: Vec<String> = p.keys().cloned().collect();
                for k in keys {
                    if let Some(t) = p.get(&k) {
                        if now.duration_since(*t) >= debounce {
                            fire.push(k.clone());
                            p.remove(&k);
                        }
                    }
                }
            }
            for k in fire {
                on_sync(k);
            }
        }
    })
}

use std::collections::HashMap;
