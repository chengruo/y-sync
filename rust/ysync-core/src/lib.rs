// ysync-core：Rust 客户端（ysyncd / Tauri 壳）与 Go 服务端之间的共享层。
// 协议类型与 Go 端 internal/protocol 一一对应（JSON tag 相同）——协议是契约（M0 伏笔）。
pub mod config;
pub mod control;
pub mod protocol;

pub use config::{
    clear_daemon_info, default_device_name, is_sub_path, load_config, read_daemon_info,
    save_config, write_daemon_info, Config, DaemonInfo, Folder,
};
pub use control::ControlClient;

/// 客户端通用错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Msg(String),
}

pub type Result<T> = std::result::Result<T, Error>;
