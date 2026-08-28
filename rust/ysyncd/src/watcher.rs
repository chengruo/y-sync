//! FS 事件监听（notify crate，递归监听）+ 防抖合并（Go watcher 对应）。
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Watcher {
    _watcher: notify::RecommendedWatcher,
}

impl Watcher {
    pub fn add_recursive(root: &Path, tx: &std::sync::mpsc::Sender<String>) -> notify::Result<()> {
        let tx = tx.clone();
        let mut watcher = notify::recommended_watcher(
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
        use notify::Watcher as _;
        watcher.watch(root, notify::RecursiveMode::Recursive)?;
        Ok(())
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
        // 收集线程
        let collector = std::thread::spawn(move || {
            while let Ok(path) = rx.recv() {
                p2.lock().unwrap().insert(path, Instant::now());
            }
        });
        // 到期分发线程
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
