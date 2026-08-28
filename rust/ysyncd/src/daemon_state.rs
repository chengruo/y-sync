//! daemon 运行时状态：文件夹同步状态、暂停集合（Go daemon_state.go 对应）。
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, Default)]
pub struct FolderStatus {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "local_path")]
    pub local_path: String,
    #[serde(rename = "cursor")]
    pub cursor: i64,
    #[serde(rename = "files")]
    pub files: i64,
    #[serde(rename = "last_sync")]
    pub last_sync: String,
    #[serde(rename = "last_error")]
    pub last_error: String,
    #[serde(rename = "conflicts_total")]
    pub conflicts_total: i64,
    #[serde(rename = "paused")]
    pub paused: bool,
    #[serde(rename = "last_stats")]
    pub last_stats: String,
}

fn now_rfc3339() -> String {
    // 无外部时间 crate：用 unix 秒 + 固定时区占位会导致 UI 排序异常，
    // 因此直接输出秒级 RFC3339（UTC）。
    let secs = std::time::SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix(secs)
}

fn format_unix(secs: u64) -> String {
    // 简单 UTC RFC3339 实现（天数算法，无 y/m/d 库依赖）
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while d >= mdays[m] {
        d -= mdays[m];
        m += 1;
    }
    format!(
        "{y:04}-{:02}-{:02}T{h:02}:{mi:02}:{s:02}Z",
        m + 1,
        d + 1
    )
}

pub struct DaemonState {
    inner: Mutex<Inner>,
}

struct Inner {
    paused: HashSet<String>,
    status: HashMap<String, FolderStatus>,
}

impl DaemonState {
    pub fn new() -> Self {
        DaemonState {
            inner: Mutex::new(Inner {
                paused: HashSet::new(),
                status: HashMap::new(),
            }),
        }
    }

    pub fn init_folders(&self, folders: &[ysync_core::Folder]) {
        let mut g = self.inner.lock().unwrap();
        for f in folders {
            g.status
                .entry(f.name.clone())
                .or_insert_with(|| FolderStatus {
                    name: f.name.clone(),
                    local_path: f.local_path.clone(),
                    cursor: f.cursor,
                    ..Default::default()
                });
        }
    }

    pub fn pause(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        if name.is_empty() {
            let names: Vec<String> = g.status.keys().cloned().collect();
            for n in names {
                g.paused.insert(n.clone());
                if let Some(s) = g.status.get_mut(&n) {
                    s.paused = true;
                }
            }
            return;
        }
        g.paused.insert(name.to_string());
        if let Some(s) = g.status.get_mut(name) {
            s.paused = true;
        }
    }

    pub fn resume(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        if name.is_empty() {
            g.paused.clear();
            for s in g.status.values_mut() {
                s.paused = false;
            }
            return;
        }
        g.paused.remove(name);
        if let Some(s) = g.status.get_mut(name) {
            s.paused = false;
        }
    }

    pub fn is_paused(&self, name: &str) -> bool {
        self.inner.lock().unwrap().paused.contains(name)
    }

    pub fn begin_sync(&self, name: &str) -> bool {
        !self.is_paused(name)
    }

    pub fn finish_sync(&self, f: &ysync_core::Folder, files: i64, stats: &str) {
        let mut g = self.inner.lock().unwrap();
        // 只更新已存在的条目：已解除跟踪的文件夹不得被竞态重新加入快照
        let Some(s) = g.status.get_mut(&f.name) else { return };
        s.last_sync = now_rfc3339();
        s.cursor = f.cursor;
        s.files = files;
        s.last_error.clear();
        s.last_stats = stats.to_string();
    }

    pub fn fail_sync(&self, f: &ysync_core::Folder, err: &str) {
        let mut g = self.inner.lock().unwrap();
        let Some(s) = g.status.get_mut(&f.name) else { return };
        s.last_sync = now_rfc3339();
        s.last_error = err.to_string();
    }

    pub fn add_conflicts(&self, name: &str, n: i64) {
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g.status.get_mut(name) {
            s.conflicts_total += n;
        }
    }

    pub fn forget(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        g.status.remove(name);
        g.paused.remove(name);
    }

    pub fn snapshot(&self) -> Vec<FolderStatus> {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<FolderStatus> = g.status.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}
