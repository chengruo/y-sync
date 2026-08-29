//! 协议 HTTP 客户端：与 Go internal/client/api.go 行为一致。
use std::io::{Read, Seek, Write};
use std::path::Path;

use ysync_core::protocol::*;
use ysync_core::{Error, Result};

pub struct Api {
    /// 连接参数内部可变（热重载；P1 并行化前提：去掉 Engine 外层互斥）
    pub base: std::sync::Mutex<String>,
    pub token: std::sync::Mutex<String>,
    /// 元数据请求：短超时（C5），避免挂起请求阻塞整轮同步
    http: reqwest::blocking::Client,
    /// 内容传输：长超时
    http_long: reqwest::blocking::Client,
    upload_limiter: Option<std::sync::Arc<RateLimiter>>,
    download_limiter: Option<std::sync::Arc<RateLimiter>>,
}

/// 令牌桶限速（FR-S12）：bytes/sec，突发上限 1 秒配额。
pub struct RateLimiter {
    state: std::sync::Mutex<(f64, f64, std::time::Instant)>, // (rate, tokens, last)
}

impl RateLimiter {
    pub fn new(kbs: i64) -> Self {
        let rate = (kbs.max(1) * 1024) as f64;
        RateLimiter {
            state: std::sync::Mutex::new((rate, rate, std::time::Instant::now())),
        }
    }
    pub fn take(&self, n0: i64) {
        // 分段发放：单次请求量可以大于 1 秒配额（如 1MB 分块 vs 256KB/s），
        // 每轮最多等 1 秒并发放 min(剩余, 突发上限)，保证有限时间内完成。
        let mut remaining = n0.max(1) as f64;
        while remaining > 0.0 {
            let wait;
            {
                let mut st = self.state.lock().unwrap();
                let now = std::time::Instant::now();
                st.1 += now.duration_since(st.2).as_secs_f64() * st.0;
                if st.1 > st.0 {
                    st.1 = st.0;
                }
                st.2 = now;
                if st.1 > 0.0 {
                    let grant = remaining.min(st.1);
                    st.1 -= grant;
                    remaining -= grant;
                }
                wait = if remaining > 0.0 {
                    (remaining.min(st.0) / st.0).max(0.01)
                } else {
                    0.0
                };
            }
            if wait > 0.0 {
                std::thread::sleep(std::time::Duration::from_secs_f64(wait));
            }
        }
    }
}

/// 上行限速读包装。
struct LimitingReader<R: Read> {
    inner: R,
    limiter: Option<std::sync::Arc<RateLimiter>>,
}
impl<R: Read> Read for LimitingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            if let Some(l) = &self.limiter {
                l.take(n as i64);
            }
        }
        Ok(n)
    }
}

fn check(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        let body = resp.text().unwrap_or_default();
        Err(Error::Msg(format!("HTTP {status}: {body}")))
    }
}

impl Api {
    pub fn new(base: &str, token: &str) -> Self {
        let mut base = base.trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            base = format!("http://{base}");
        }
        Api {
            base: std::sync::Mutex::new(base),
            token: std::sync::Mutex::new(token.to_string()),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            http_long: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30 * 60))
                .build()
                .expect("reqwest long client"),
            upload_limiter: None,
            download_limiter: None,
        }
    }

    pub fn set_limits(&mut self, upload_kbs: i64, download_kbs: i64) {
        self.upload_limiter = (upload_kbs > 0).then(|| std::sync::Arc::new(RateLimiter::new(upload_kbs)));
        self.download_limiter = (download_kbs > 0)
            .then(|| std::sync::Arc::new(RateLimiter::new(download_kbs)));
    }

    fn req(&self, method: &str, path: &str, body: Option<Vec<u8>>) -> reqwest::blocking::RequestBuilder {
        self.req_on(&self.http, method, path, body)
    }

    /// 长超时请求（内容传输类，C5）。
    fn req_long(&self, method: &str, path: &str, body: Option<Vec<u8>>) -> reqwest::blocking::RequestBuilder {
        self.req_on(&self.http_long, method, path, body)
    }

    pub fn get_base(&self) -> String {
        self.base.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn get_token(&self) -> String {
        self.token.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// 热重载连接参数（A7）。
    pub fn set_connection(&self, base: &str, token: &str) {
        *self.base.lock().unwrap_or_else(|p| p.into_inner()) = base.trim_end_matches('/').to_string();
        *self.token.lock().unwrap_or_else(|p| p.into_inner()) = token.to_string();
    }

    fn req_on(&self, client: &reqwest::blocking::Client, method: &str, path: &str, body: Option<Vec<u8>>) -> reqwest::blocking::RequestBuilder {
        let url = format!("{}{}", self.get_base(), path);
        let mut rb = client.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), &url);
        rb = rb.header("Authorization", format!("Bearer {}", self.get_token()));
        if let Some(b) = body {
            rb = rb.body(b);
        }
        rb
    }

    pub fn login(user: &str, password: &str, device: &str, base: &str) -> Result<LoginResp> {
        let mut base = base.trim_end_matches('/').to_string();
        if !base.starts_with("http") {
            base = format!("http://{base}");
        }
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let resp = http
            .post(format!("{base}/api/v1/auth/login"))
            .json(&LoginReq {
                user: user.to_string(),
                password: password.to_string(),
                device_name: device.to_string(),
            })
            .send()?;
        check(resp)?.json().map_err(Error::from)
    }

    pub fn nodes(&self) -> Result<Vec<NodeInfo>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(rename = "nodes", deserialize_with = "null_to_vec", default)]
            nodes: Vec<NodeInfo>,
        }
        let resp = self.req("GET", "/api/v1/nodes", None).send()?;
        Ok(check(resp)?.json::<R>()?.nodes)
    }

    /// 节点树分页列举（P0-1）。返回 (nodes, has_more)。
    pub fn nodes_paged(&self, after_id: i64, limit: i64) -> Result<(Vec<NodeInfo>, bool)> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(rename = "nodes", deserialize_with = "null_to_vec", default)]
            nodes: Vec<NodeInfo>,
            #[serde(rename = "has_more", default)]
            has_more: bool,
        }
        let resp = self.req(
            "GET",
            &format!("/api/v1/nodes?after={after_id}&limit={limit}"),
            None,
        )
        .send()?;
        let r: R = check(resp)?.json()?;
        Ok((r.nodes, r.has_more))
    }

    pub fn head(&self) -> Result<HeadResp> {
        let resp = self.req("GET", "/api/v1/sync/head", None).send()?;
        check(resp)?.json::<HeadResp>().map_err(Error::from)
    }

    pub fn changes(&self, cursor: i64, limit: i64, root_id: i64) -> Result<ChangesResp> {
        let resp = self
            .req(
                "GET",
                &format!("/api/v1/sync/changes?cursor={cursor}&limit={limit}&root={root_id}"),
                None,
            )
            .send()?;
        check(resp)?.json().map_err(Error::from)
    }

    /// 两阶段之一：上传内容（服务端按 SHA-256 去重）。返回 (hash, dedup)。
    pub fn put_content(&self, path: &Path, want_hash: &str) -> Result<(String, bool)> {
        use sha2::Digest;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = sha2::Sha256::new();
        let mut size = 0i64;
        let mut buf = [0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as i64;
        }
        let hash = hex::encode(hasher.finalize());
        if !want_hash.is_empty() && hash != want_hash {
            return Err(Error::Msg("本地文件哈希与预期不符".into()));
        }
        file.seek(std::io::SeekFrom::Start(0))?;
        let limiter = self.upload_limiter.clone();
        let reader = LimitingReader {
            inner: file,
            limiter,
        };
        let resp = self
            .http_long
            .put(format!("{}/api/v1/content", self.get_base()))
            .header("Authorization", format!("Bearer {}", self.get_token()))
            .header("X-Content-SHA256", &hash)
            .body(reqwest::blocking::Body::sized(reader, size as u64))
            .send()?;
        let out: DedupResp = check(resp)?.json()?;
        Ok((out.hash, out.dedup))
    }

    /// 两阶段之二：按哈希下载（临时文件 + 原子改名 + 哈希校验 + 回设 mtime）。
    pub fn get_content(&self, hash: &str, dest: &Path, mtime_milli: i64) -> Result<()> {
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let resp = self
            .req_long(
                "GET",
                &format!("/api/v1/content/{}", urlencoding::encode(hash)),
                None,
            )
            .send()?;
        let mut resp = check(resp)?;
        static DL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp_path = dest.with_file_name(format!(
            ".ysync-dl-{}-{}",
            std::process::id(),
            DL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        {
            let mut tmp = std::fs::File::create(&tmp_path)?;
            let limiter = self.download_limiter.clone();
            let mut reader = LimitingReader {
                inner: &mut resp,
                limiter,
            };
            std::io::copy(&mut reader, &mut tmp)?;
            tmp.sync_all()?;
        }
        // 下载内容哈希校验
        let got = file_hash(&tmp_path)?;
        if got != hash {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(Error::Msg(format!(
                "download hash mismatch: want {} got {}",
                &hash[..8],
                &got[..8]
            )));
        }
        std::fs::rename(&tmp_path, dest)?;
        if mtime_milli > 0 {
            set_mtime(dest, mtime_milli);
        }
        Ok(())
    }

    pub fn ops(&self, ops: &[Op]) -> Result<Vec<OpResult>> {
        let body = serde_json::to_vec(ops)?;
        let resp = self
            .req("POST", "/api/v1/ops", Some(body))
            .header("Content-Type", "application/json")
            .send()?;
        let out: OpsResp = check(resp)?.json()?;
        Ok(out.results)
    }

    // ---------- 分块上传（FR-S11） ----------

    pub fn upload_create(&self, size: i64, sha256: &str, chunk: i64) -> Result<UploadSessionResp> {
        let body = serde_json::to_vec(&serde_json::json!({
            "size": size, "sha256": sha256, "chunk_size": chunk
        }))?;
        let resp = self
            .req_long("POST", "/api/v1/uploads", Some(body))
            .header("Content-Type", "application/json")
            .send()?;
        check(resp)?.json().map_err(Error::from)
    }

    pub fn upload_status(&self, id: &str) -> Result<UploadSessionResp> {
        let resp = self.req("GET", &format!("/api/v1/uploads/{id}"), None).send()?;
        check(resp)?.json().map_err(Error::from)
    }

    pub fn upload_chunk(&self, id: &str, chunk_no: i64, data: &[u8]) -> Result<()> {
        let resp = self
            .req(
                "PUT",
                &format!("/api/v1/uploads/{id}?chunk={chunk_no}"),
                Some(data.to_vec()),
            )
            .send()?;
        if resp.status().as_u16() == 204 {
            return Ok(());
        }
        check(resp)?;
        Ok(())
    }

    pub fn upload_complete(&self, id: &str) -> Result<String> {
        let resp = self
            .req_long("POST", &format!("/api/v1/uploads/{id}/complete"), None)
            .send()?;
        let out: DedupResp = check(resp)?.json()?;
        Ok(out.hash)
    }

    /// 大文件分块上传 + 断点续传。
    /// 返回 (会话 ID, 结果)：完成时会话为 ""；失败时会话保留供下次续传（FR-S11）。
    pub fn put_content_chunked(
        &self,
        path: &Path,
        resume_id: Option<&str>,
        want_hash: &str,
        size: i64,
        chunk_size: i64,
    ) -> (String, Result<String>) {
        let (sess_id, mut received) = match resume_id {
            Some(id) if !id.is_empty() => match self.upload_status(id) {
                Ok(s) => (id.to_string(), s.received),
                Err(e) => return (String::new(), Err(e)),
            },
            _ => match self.upload_create(size, want_hash, chunk_size) {
                Ok(s) => (s.id, s.received),
                Err(e) => return (String::new(), Err(e)),
            },
        };
        let wrap = |e: Error| (sess_id.clone(), Err(e));
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => return wrap(e.into()),
        };
        let total = (size + chunk_size - 1) / chunk_size;
        let received_set: std::collections::HashSet<i64> = received.drain(..).collect();
        let mut buf = vec![0u8; chunk_size as usize];
        for i in 0..total {
            if received_set.contains(&i) {
                continue;
            }
            if let Err(e) = f.seek(std::io::SeekFrom::Start((i * chunk_size) as u64)) {
                return wrap(e.into());
            }
            let n = match read_full(&mut f, &mut buf) {
                Ok(n) => n,
                Err(e) => return wrap(e.into()),
            };
            if let Some(l) = &self.upload_limiter {
                l.take(n as i64);
            }
            if let Err(e) = self.upload_chunk(&sess_id, i, &buf[..n]) {
                return wrap(e);
            }
        }
        match self.upload_complete(&sess_id) {
            Ok(hash) => ("".into(), Ok(hash)),
            Err(e) => wrap(e),
        }
    }

    // ---------- 回收站 / 版本 / 分享 ----------

    pub fn trash_list(&self) -> Result<Vec<TrashItem>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(rename = "items", deserialize_with = "null_to_vec", default)]
            items: Vec<TrashItem>,
        }
        let resp = self.req("GET", "/api/v1/trash", None).send()?;
        Ok(check(resp)?.json::<R>()?.items)
    }

    pub fn trash_restore(&self, id: i64) -> Result<()> {
        let resp = self
            .req("POST", &format!("/api/v1/trash/{id}/restore"), None)
            .send()?;
        check(resp)?;
        Ok(())
    }

    pub fn trash_delete(&self, id: i64) -> Result<()> {
        let resp = self
            .req("DELETE", &format!("/api/v1/trash/{id}"), None)
            .send()?;
        check(resp)?;
        Ok(())
    }

    pub fn node_versions(&self, node_id: i64) -> Result<Vec<VersionItem>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(rename = "versions", deserialize_with = "null_to_vec", default)]
            versions: Vec<VersionItem>,
        }
        let resp = self
            .req("GET", &format!("/api/v1/nodes/{node_id}/versions"), None)
            .send()?;
        Ok(check(resp)?.json::<R>()?.versions)
    }

    pub fn download_version_to(&self, version_id: i64, dest: &Path, mtime_milli: i64) -> Result<()> {
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let resp = self
            .req_long(
                "GET",
                &format!("/api/v1/versions/{version_id}/content"),
                None,
            )
            .send()?;
        let mut resp = check(resp)?;
        static VER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = dest.with_file_name(format!(
            ".ysync-ver-{}-{}",
            std::process::id(),
            VER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        {
            let mut f = std::fs::File::create(&tmp)?;
            let limiter = self.download_limiter.clone();
            let mut reader = LimitingReader {
                inner: &mut resp,
                limiter,
            };
            std::io::copy(&mut reader, &mut f)?;
        }
        std::fs::rename(&tmp, dest)?;
        if mtime_milli > 0 {
            set_mtime(dest, mtime_milli);
        }
        Ok(())
    }

    pub fn create_share(&self, path: &str, hours: i64, password: &str) -> Result<ShareInfo> {
        let body = serde_json::to_vec(&serde_json::json!({
            "path": path, "hours": hours, "password": password
        }))?;
        let resp = self
            .req("POST", "/api/v1/shares", Some(body))
            .header("Content-Type", "application/json")
            .send()?;
        check(resp)?.json().map_err(Error::from)
    }

    pub fn list_shares(&self) -> Result<Vec<ShareInfo>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(rename = "shares", deserialize_with = "null_to_vec", default)]
            shares: Vec<ShareInfo>,
        }
        let resp = self.req("GET", "/api/v1/shares", None).send()?;
        Ok(check(resp)?.json::<R>()?.shares)
    }

    pub fn devices_list(&self) -> Result<Vec<serde_json::Value>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(rename = "devices", deserialize_with = "null_to_vec", default)]
            devices: Vec<serde_json::Value>,
        }
        let resp = self.req("GET", "/api/v1/devices", None).send()?;
        Ok(check(resp)?.json::<R>()?.devices)
    }

    pub fn device_revoke(&self, id: i64) -> Result<()> {
        let resp = self
            .req("DELETE", &format!("/api/v1/devices/{id}"), None)
            .send()?;
        check(resp)?;
        Ok(())
    }

    pub fn delete_share(&self, token: &str) -> Result<()> {
        let resp = self
            .req("DELETE", &format!("/api/v1/shares/{token}"), None)
            .send()?;
        check(resp)?;
        Ok(())
    }
}

fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

pub fn file_hash(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

pub fn hash_local_file_helper(root: &Path, rel: &str) -> Result<(String, i64)> {
    hash_and_size(&root.join(rel))
}

pub fn hash_and_size(path: &Path) -> Result<(String, i64)> {
    use sha2::Digest;
    let meta = std::fs::metadata(path)?;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok((hex::encode(h.finalize()), meta.len() as i64))
}

pub fn set_mtime(path: &Path, mtime_milli: i64) {
    use filetime::FileTime;
    let t = FileTime::from_unix_time(mtime_milli / 1000, (mtime_milli % 1000) as u32 * 1_000_000);
    let _ = filetime::set_file_times(path, t, t);
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// null 容忍（Go nil slice 序列化为 null）。
pub fn null_to_vec<'de, D, T>(de: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let opt: Option<Vec<T>> = serde::Deserialize::deserialize(de)?;
    Ok(opt.unwrap_or_default())
}
