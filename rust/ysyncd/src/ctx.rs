//! 进程级共享上下文：单一 Config 实例（daemon 与引擎共用，互不覆盖）。
use std::sync::{Mutex, OnceLock};

use ysync_core::{Config, Result};

struct Ctx {
    cfg: Mutex<Config>,
    device_id: i64,
}

static CTX: OnceLock<Ctx> = OnceLock::new();

/// daemon 启动时安装（引擎与 daemon 读写同一份配置）。
pub fn install(cfg: Config, device_id: i64) {
    let _ = CTX.set(Ctx {
        cfg: Mutex::new(cfg),
        device_id,
    });
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
