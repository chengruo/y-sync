//! daemon 本地控制 API 客户端（Tauri 壳与 ysyncd 管理操作共用）。
//! 端点与 Go daemon 的 internal/client/daemon.go 完全一致。
use serde::Deserialize;
use serde_json::json;

use crate::protocol::{Conflict, FolderStatus, StatusResp};
use crate::{Error, Result};

#[derive(Clone)]
pub struct ControlClient {
    pub base: String,
    pub token: String,
    http: reqwest::blocking::Client,
}

impl ControlClient {
    pub fn new(base: &str, token: &str) -> Self {
        let mut base = base.trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            base = format!("http://{base}");
        }
        Self {
            base,
            token: token.to_string(),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }

    /// 从 daemon.json 构造（daemon 未运行时返回错误）。
    pub fn from_daemon_info() -> Result<Self> {
        let info = crate::config::read_daemon_info()?;
        Ok(Self::new(&info.addr, &info.token))
    }

    fn url(&self, path: &str) -> String {
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{}{sep}token={}", self.base.clone() + path, self.token)
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self.http.get(self.url(path)).send()?;
        check_status(resp)?
            .json::<T>()
            .map_err(Error::from)
    }

    fn post_json(&self, path: &str, body: serde_json::Value) -> Result<String> {
        let resp = self
            .http
            .post(self.url(path))
            .json(&body)
            .send()?;
        check_status(resp)?
            .text()
            .map_err(Error::from)
    }

    pub fn status(&self) -> Result<Vec<FolderStatus>> {
        Ok(self.get_json::<StatusResp>("/status")?.folders_or_empty())
    }

    pub fn conflicts(&self) -> Result<Vec<Conflict>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "conflicts", deserialize_with = "crate::protocol::null_to_vec", default)]
            conflicts: Vec<Conflict>,
        }
        Ok(self.get_json::<R>("/conflicts")?.conflicts)
    }

    pub fn pause(&self, folder: &str) -> Result<()> {
        self.post_json("/pause", json!({ "folder": folder })).map(|_| ())
    }

    pub fn resume(&self, folder: &str) -> Result<()> {
        self.post_json("/resume", json!({ "folder": folder })).map(|_| ())
    }

    pub fn trigger_sync(&self, folder: Option<&str>) -> Result<()> {
        self.post_json(
            "/sync",
            json!({ "folder": folder.unwrap_or_default() }),
        )
        .map(|_| ())
    }

    pub fn add_folder(
        &self,
        local_path: &str,
        name: &str,
        excludes: &[String],
        use_gitignore: bool,
    ) -> Result<()> {
        self.post_json(
            "/add",
            json!({
                "local_path": local_path,
                "name": name,
                "excludes": excludes,
                "use_gitignore": use_gitignore,
            }),
        )
        .map(|_| ())
    }

    pub fn remove_folder(&self, name: &str) -> Result<()> {
        self.post_json("/remove", json!({ "name": name })).map(|_| ())
    }

    /// choice: "local"（保留原名）| "copy"（采用副本）。
    pub fn resolve_conflict(&self, folder: &str, rel: &str, copy_rel: &str, choice: &str) -> Result<()> {
        self.post_json(
            "/resolve",
            json!({ "folder": folder, "rel": rel, "copy_rel": copy_rel, "choice": choice }),
        )
        .map(|_| ())
    }
}

fn check_status(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        let body = resp.text().unwrap_or_default();
        Err(Error::Msg(format!("HTTP {status}: {body}")))
    }
}
