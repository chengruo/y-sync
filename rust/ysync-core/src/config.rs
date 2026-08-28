//! 配置：与 Go 客户端完全一致的 config.json / daemon.json（同目录同 schema，双实现可互换）。
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Folder {
    #[serde(rename = "name", default)]
    pub name: String,
    #[serde(rename = "local_path", default)]
    pub local_path: String,
    #[serde(rename = "root_node_id", default)]
    pub root_node_id: i64,
    #[serde(rename = "cursor", default)]
    pub cursor: i64,
    #[serde(rename = "excludes", default)]
    pub excludes: Vec<String>,
    #[serde(rename = "use_gitignore", default)]
    pub use_gitignore: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(rename = "server_url", default)]
    pub server_url: String,
    #[serde(rename = "user", default)]
    pub user: String,
    #[serde(rename = "token", default)]
    pub token: String,
    #[serde(rename = "device_name", default)]
    pub device_name: String,
    #[serde(rename = "device_id", default)]
    pub device_id: i64,
    #[serde(rename = "folders", default)]
    pub folders: Vec<Folder>,
    #[serde(rename = "chunk_threshold_mb", default)]
    pub chunk_threshold_mb: i64,
    #[serde(rename = "chunk_size_mb", default)]
    pub chunk_size_mb: i64,
    #[serde(rename = "upload_limit_kbs", default)]
    pub upload_limit_kbs: i64,
    #[serde(rename = "download_limit_kbs", default)]
    pub download_limit_kbs: i64,
}

impl Config {
    pub fn defaults(&mut self) {
        if self.chunk_threshold_mb == 0 {
            self.chunk_threshold_mb = 100;
        }
        if self.chunk_size_mb == 0 {
            self.chunk_size_mb = 8;
        }
    }
}

/// 与 Go os.UserConfigDir()/dirs::config_dir() 一致的目录约定；
/// YSYNC_CONFIG_DIR 覆盖（多设备模拟/测试）。
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("YSYNC_CONFIG_DIR") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d));
        }
    }
    dirs::config_dir()
        .map(|p| p.join("y-sync"))
        .ok_or_else(|| crate::Error::Msg("无法确定配置目录".into()))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub fn load_config() -> Result<Config> {
    let p = config_path()?;
    let b = std::fs::read(&p).map_err(|e| {
        crate::Error::Msg(if e.kind() == std::io::ErrorKind::NotFound {
            "尚未初始化，请先执行 ysync init".into()
        } else {
            format!("读取配置失败: {e}")
        })
    })?;
    serde_json::from_slice(&b).map_err(|e| crate::Error::Msg(format!("配置文件损坏: {e}")))
}

pub fn save_config(c: &Config) -> Result<()> {
    let p = config_path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let b = serde_json::to_vec_pretty(c)?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, b)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// daemon.json：daemon 运行信息（Go 端 internal/client/daemon.go 对应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    #[serde(rename = "pid")]
    pub pid: i32,
    #[serde(rename = "addr")]
    pub addr: String,
    #[serde(rename = "token")]
    pub token: String,
    #[serde(rename = "started")]
    pub started: i64,
}

pub fn daemon_info_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon.json"))
}

pub fn write_daemon_info(info: &DaemonInfo) -> Result<()> {
    let p = daemon_info_path()?;
    let b = serde_json::to_vec(info)?;
    std::fs::write(p, b)?;
    Ok(())
}

pub fn clear_daemon_info() {
    if let Ok(p) = daemon_info_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// 读取运行中的 daemon 信息；尽力校验 pid 存活（unix kill 0）。
pub fn read_daemon_info() -> Result<DaemonInfo> {
    let p = daemon_info_path()?;
    let b = std::fs::read(&p)
        .map_err(|_| crate::Error::Msg("daemon 未运行（先执行 ysync daemon）".into()))?;
    let info: DaemonInfo = serde_json::from_slice(&b)
        .map_err(|_| crate::Error::Msg("daemon 信息损坏".into()))?;
    #[cfg(unix)]
    if info.pid > 0 {
        let alive = unsafe { libc::kill(info.pid, 0) } == 0;
        if !alive {
            return Err(crate::Error::Msg(format!(
                "daemon 未运行（残留信息，PID {}）",
                info.pid
            )));
        }
    }
    Ok(info)
}

pub fn default_device_name() -> String {
    let host = hostname().unwrap_or_else(|| "unknown-host".into());
    format!("{host}-{}", std::env::consts::OS)
}

fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = vec![0u8; 256];
        let n = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
        if n == 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            return Some(String::from_utf8_lossy(&buf[..end]).to_string());
        }
        None
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").ok()
    }
}

/// 路径规范化工具：与 Go 端 filepath.Clean 对齐的本地路径比较。
pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b
}

/// parent 是否为 child 或其祖先（FR-S15 嵌套校验）。
pub fn is_sub_path(parent: &str, child: &str) -> bool {
    let (p, c) = (Path::new(parent), Path::new(child));
    match c.strip_prefix(p) {
        Ok(rel) => !rel.as_os_str().is_empty(),
        Err(_) => false,
    }
}
