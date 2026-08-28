//! 进程级共享上下文：单一 Config 实例（daemon 与引擎共用，互不覆盖）。
use std::sync::{Mutex, OnceLock};

use ysync_core::{Config, Result};

struct Ctx {
    cfg: Mutex<Config>,
    device_id: i64,
    last_load_mtime: Mutex<i64>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();

/// daemon 启动时安装（引擎与 daemon 读写同一份配置）。
pub fn install(cfg: Config, device_id: i64) {
    let mtime = config_mtime();
    let _ = CTX.set(Ctx {
        cfg: Mutex::new(cfg),
        device_id,
        last_load_mtime: Mutex::new(mtime),
    });
}

fn config_mtime() -> i64 {
    ysync_core::config_path()
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 配置热重载：CLI 在 daemon 运行期间 add/remove 时，daemon 从磁盘感知变更。
/// 以文件 mtime 判定；daemon 自身的游标落盘也会触发一次幂等重载。
pub fn maybe_reload() {
    let ctx = ctx();
    let mtime = config_mtime();
    {
        let mut last = ctx.last_load_mtime.lock().unwrap();
        if mtime == *last {
            return;
        }
        *last = mtime;
    }
    if let Ok(cfg) = ysync_core::load_config() {
        let mut c = ctx.cfg.lock().unwrap();
        *c = cfg;
    }
}

/// 独占访问共享配置（调用方负责变更后调用 save()）。
pub fn with_cfg<T>(f: impl FnOnce(&mut Config) -> T) -> T {
    let ctx = ctx();
    let mut cfg = ctx.cfg.lock().unwrap();
    f(&mut cfg)
}

pub fn save() {
    let _ = save_result();
}

pub fn save_result() -> Result<()> {
    let ctx = ctx();
    let cfg = ctx.cfg.lock().unwrap();
    ysync_core::save_config(&cfg)
}

pub fn device_id() -> i64 {
    ctx().device_id
}

pub fn snapshot() -> Config {
    ctx().cfg.lock().unwrap().clone()
}

fn ctx() -> &'static Ctx {
    CTX.get().expect("ctx 未安装（先调用 ctx::install）")
}
