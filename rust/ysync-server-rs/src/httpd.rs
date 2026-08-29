//! 服务端 HTTP 层：手写 HTTP/1.1（thread-per-connection）+ WS 升级 + 全部端点路由。
//! 端点与语义与 Go internal/server/http.go 一致（含 ?token= 兜底认证、Range、
//! 分块上传、回收站/版本、分享/浏览页、只读 WebDAV、WS notify）。
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::hub::{read_frame_skip, Hub};
use crate::store::{NodeInfo, Store};
use crate::upload::UploadManager;
use crate::util::*;

pub struct ServerState {
    pub store: Store,
    pub uploads: UploadManager,
    pub hub: Hub,
    pub login_guard: LoginGuard,
    pub share_guard: ShareGuard,
    pub bytes_in: std::sync::atomic::AtomicU64,
    pub bytes_out: std::sync::atomic::AtomicU64,
    pub http_stats: HttpStats,
    pub started_at: std::time::Instant,
    pub audit_path: std::path::PathBuf,
}

/// 分享密码防爆破（P0-5）：按 IP+token 记失败次数，5 次后锁定 60s 起指数退避。
pub struct ShareGuard {
    failures: std::sync::Mutex<std::collections::HashMap<(String, String), (u32, i64)>>,
}

impl ShareGuard {
    pub fn new() -> Self {
        ShareGuard {
            failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
    pub fn check(&self, ip: &str, token: &str) -> Result<(), i64> {
        let now = now_secs();
        let mut g = self.failures.lock().unwrap();
        if g.len() > 10_000 {
            g.retain(|_, (_, until)| *until > now);
        }
        if let Some((_, until)) = g.get(&(ip.to_string(), token.to_string())) {
            if *until > now {
                return Err(until - now);
            }
        }
        Ok(())
    }
    pub fn record_failure(&self, ip: &str, token: &str) {
        let now = now_secs();
        let mut g = self.failures.lock().unwrap();
        let e = g.entry((ip.to_string(), token.to_string())).or_insert((0u32, 0i64));
        e.0 += 1;
        if e.0 >= 5 {
            e.1 = now + 60i64 * (1i64 << (e.0 - 5).min(7));
        }
    }
    pub fn record_success(&self, ip: &str, token: &str) {
        self.failures.lock().unwrap().remove(&(ip.to_string(), token.to_string()));
    }
}

/// 审计日志（P1-7）：JSONL 追加，16MB 轮转。
pub fn audit(state: &ServerState, user: &str, device: i64, event: &str, detail: &str) {
    use std::io::Write;
    let path = &state.audit_path;
    if path.metadata().map(|m| m.len()).unwrap_or(0) > 16 << 20 {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    let line = serde_json::json!({
        "ts": now_secs(), "user": user, "device": device,
        "event": event, "detail": detail
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// 登录暴力破解防护（P0-3）：按 (IP, 用户) 记失败次数，
/// 连续 5 次失败后指数退避锁定（60s 起，上限 12h）；成功登录清零。内存态，重启即清。
pub struct LoginGuard {
    failures: std::sync::Mutex<std::collections::HashMap<(String, String), (u32, i64)>>,
}

const LOCKOUT_THRESHOLD: u32 = 5;

impl LoginGuard {
    pub fn new() -> Self {
        LoginGuard {
            failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn key(ip: &str, user: &str) -> (String, String) {
        (ip.to_string(), user.to_string())
    }

    /// 返回 Err(重试秒数) 表示处于锁定窗口。
    pub fn check(&self, ip: &str, user: &str) -> Result<(), i64> {
        let now = now_secs();
        let mut g = self.failures.lock().unwrap();
        // 顺手清理过期条目，防膨胀
        if g.len() > 10_000 {
            g.retain(|_, (_, until)| *until > now);
        }
        if let Some((_, until)) = g.get(&Self::key(ip, user)) {
            if *until > now {
                return Err(until - now);
            }
        }
        Ok(())
    }

    pub fn record_failure(&self, ip: &str, user: &str) {
        let now = now_secs();
        let mut g = self.failures.lock().unwrap();
        let e = g.entry(Self::key(ip, user)).or_insert((0u32, 0i64));
        e.0 += 1;
        if e.0 >= LOCKOUT_THRESHOLD {
            let backoff = 60i64 * (1i64 << (e.0 - LOCKOUT_THRESHOLD).min(7)); // 上限 12h
            e.1 = now + backoff;
        }
    }

    pub fn record_success(&self, ip: &str, user: &str) {
        self.failures.lock().unwrap().remove(&Self::key(ip, user));
    }
}

/// HTTP 请求计数（/metrics 用）。
pub struct HttpStats {
    pub total: std::sync::Mutex<u64>,
    pub by_status: std::sync::Mutex<std::collections::HashMap<u16, u64>>,
}

impl HttpStats {
    pub fn new() -> Self {
        HttpStats {
            total: std::sync::Mutex::new(0),
            by_status: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
    pub fn record(&self, status: u16) {
        *self.total.lock().unwrap() += 1;
        *self.by_status.lock().unwrap().entry(status).or_insert(0) += 1;
    }
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
    fn q(&self, key: &str) -> Option<String> {
        query_get(&self.query, key).map(|s| s.to_string())
    }
}

fn auth_ok(state: &ServerState, req: &Request) -> Option<(i64, i64)> {
    let mut tok = req
        .header("Authorization")
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .to_string();
    if tok.is_empty() {
        tok = req.header("X-Ysync-Token").unwrap_or_default();
    }
    if tok.is_empty() {
        tok = req.q("token").unwrap_or_default();
    }
    state.store.auth_token(&tok).ok()
}

pub fn serve(addr: &str, state: Arc<ServerState>) -> Result<(), String> {
    let listener = TcpListener::bind(addr).map_err(|e| format!("{e}"))?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, &state);
        });
    }
    Ok(())
}

pub enum ReadErr {
    Closed,
    TooLarge,
    Io(std::io::Error),
}

impl From<std::io::Error> for ReadErr {
    fn from(e: std::io::Error) -> Self {
        ReadErr::Io(e)
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, ReadErr> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    // 读到 \r\n\r\n（简单起见逐字节，头部长度有限）
    loop {
        stream.read_exact(&mut byte).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                ReadErr::Closed
            } else {
                ReadErr::Io(e)
            }
        })?;
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 256 * 1024 {
            return Err(ReadErr::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers too large",
            )));
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.clone(), Vec::new()),
    };
    let chunked = headers
        .iter()
        .any(|(k, v)| {
            k.eq_ignore_ascii_case("transfer-encoding") && v.to_lowercase().contains("chunked")
        });
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    // 请求体上限（A4）：内容/分块上传 512MB，ops 64MB，其余 2MB
    let cap: usize = if path == "/api/v1/content" || path.starts_with("/api/v1/uploads/") {
        512 << 20
    } else if path == "/api/v1/ops" {
        64 << 20
    } else {
        2 << 20
    };
    let body = if chunked {
        let b = read_chunked(stream)?;
        if b.len() > cap {
            return Err(ReadErr::TooLarge);
        }
        b
    } else if content_length > 0 {
        if content_length > cap {
            return Err(ReadErr::TooLarge);
        }
        read_exact_n(stream, content_length)?
    } else {
        Vec::new()
    };
    Ok(Request {
        method,
        path: url_decode(&path),
        query,
        headers,
        body,
    })
}

/// 解码 chunked transfer 编码的 body。
fn read_chunked(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        // 读一行 chunk 大小（hex）
        let mut size_line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte)?;
            if byte[0] == b'\n' {
                break;
            }
            if byte[0] != b'\r' {
                size_line.push(byte[0]);
            }
        }
        let size_str = String::from_utf8_lossy(&size_line);
        let size_hex = size_str.split(';').next().unwrap_or("0").trim().to_string();
        let size = usize::from_str_radix(&size_hex, 16).unwrap_or(0);
        if size == 0 {
            // 读 trailer 结束的 \r\n（可能有 trailer 行，读到空行）
            loop {
                let mut line = Vec::new();
                loop {
                    stream.read_exact(&mut byte)?;
                    if byte[0] == b'\n' {
                        break;
                    }
                    if byte[0] != b'\r' {
                        line.push(byte[0]);
                    }
                }
                if line.is_empty() {
                    break;
                }
            }
            break;
        }
        let chunk = read_exact_n(stream, size)?;
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        stream.read_exact(&mut crlf)?;
    }
    Ok(body)
}

/// 按 Body 类型写出（File 流式，A2）。
fn send(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &Body,
    extra: Vec<(&'static str, String)>,
    keep: bool,
) -> std::io::Result<()> {
    let len = match body {
        Body::Bytes(b) => b.len() as u64,
        Body::File(_, l) => *l,
        Body::Manifest { len, .. } => *len,
    };
    let mut headers: Vec<(&str, String)> = extra;
    headers.push(("Content-Type", ctype.to_string()));
    crate::util::write_head(
        stream,
        status,
        status_reason(status),
        &headers,
        len,
        keep,
    )?;
    match body {
        Body::Bytes(b) => stream.write_all(b)?,
        Body::File(f, mut remaining) => {
            let mut buf = [0u8; 64 * 1024];
            let mut reader = f.take(remaining);
            while remaining > 0 {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                stream.write_all(&buf[..n])?;
                remaining -= n as u64;
            }
        }
        Body::Manifest { parts, skip, len } => {
            // 顺序拼接各块，skip 为起点
            let mut to_skip = *skip;
            let mut remaining = *len;
            for (path, plen) in parts {
                if remaining == 0 {
                    break;
                }
                if to_skip >= *plen {
                    to_skip -= plen;
                    continue;
                }
                let Ok(mut f) = std::fs::File::open(path) else { break };
                use std::io::Seek;
                let _ = f.seek(std::io::SeekFrom::Start(to_skip));
                let take_now = (*plen - to_skip).min(remaining);
                let mut reader = f.take(take_now);
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n])?;
                    remaining -= n as u64;
                }
                to_skip = 0;
            }
        }
    }
    stream.flush()
}

fn handle_conn(mut stream: TcpStream, state: &ServerState) -> std::io::Result<()> {
    // keep-alive（B1）：连接复用，省去每请求 TCP+TLS 握手；空闲 75s 由读超时回收
    stream.set_read_timeout(Some(std::time::Duration::from_secs(75)))?;
    loop {
        let req = match read_request(&mut stream) {
            Ok(r) => r,
            Err(ReadErr::Closed) => return Ok(()),
            Err(ReadErr::TooLarge) => {
                send(
                    &mut stream,
                    413,
                    "text/plain",
                    &Body::Bytes(b"request body too large".to_vec()),
                    vec![],
                    false,
                )?;
                return Ok(());
            }
            Err(ReadErr::Io(_)) => return Ok(()),
        };

        // WebSocket 升级（/api/v1/notify）——独占连接，不参与 keep-alive
        if req.path == "/api/v1/notify"
            && req
                .header("Upgrade")
                .map(|v| v.eq_ignore_ascii_case("websocket"))
                .unwrap_or(false)
        {
            return handle_ws(stream, state, &req);
        }

        let close = req
            .header("Connection")
            .map(|v| v.eq_ignore_ascii_case("close"))
            .unwrap_or(false);
        let (status, ctype, body, extra) = route(state, &req);
        state.http_stats.record(status);
        let keep = !close;
        let conn_header = if close { "close" } else { "keep-alive" };
        let mut extra = extra;
        extra.push(("Connection", conn_header.into()));
        send(&mut stream, status, &ctype, &body, extra, keep)?;
        if close {
            return Ok(());
        }
    }
}

fn handle_ws(mut stream: TcpStream, state: &ServerState, req: &Request) -> std::io::Result<()> {
    let Some(token) = req.q("token") else {
        return send(&mut stream, 401, "application/json", &Body::Bytes(b"{\"error\":\"unauthorized\"}".to_vec()), vec![], false);
    };
    let Ok((uid, _)) = state.store.auth_token(&token) else {
        return send(&mut stream, 401, "application/json", &Body::Bytes(b"{\"error\":\"unauthorized\"}".to_vec()), vec![], false);
    };
    let Some(key) = req.header("Sec-WebSocket-Key") else {
        return send(&mut stream, 400, "application/json", &Body::Bytes(b"{\"error\":\"bad request\"}".to_vec()), vec![], false);
    };
    let accept = base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    let conn = state.hub.register(uid, stream.try_clone()?);
    // 读侧保活；断开时注销
    let mut reader = stream;
    loop {
        match read_frame_skip(&mut reader) {
            Ok(()) => {}
            Err(_) => break,
        }
    }
    state.hub.unregister(&conn);
    Ok(())
}

/// 响应体：Bytes 常规；File 流式（零拷贝落 socket，大文件不再整载内存，A2）。
pub enum Body {
    Bytes(Vec<u8>),
    File(std::fs::File, u64),
    /// CDC 清单重组装：按序拼接各块文件，skip 为 Range 起点
    Manifest { parts: Vec<(std::path::PathBuf, u64)>, skip: u64, len: u64 },
}

type RouteResult = (u16, String, Body, Vec<(&'static str, String)>);

fn bytes_body(v: Vec<u8>) -> Body {
    Body::Bytes(v)
}

fn ok_json(v: impl serde::Serialize) -> RouteResult {
    (
        200,
        "application/json".into(),
        bytes_body(serde_json::to_vec(&v).unwrap_or_default()),
        vec![],
    )
}

fn err_json(status: u16, msg: &str) -> RouteResult {
    (
        status,
        "application/json".into(),
        bytes_body(
            serde_json::to_vec(&serde_json::json!({ "error": msg })).unwrap_or_default(),
        ),
        vec![],
    )
}

fn route(state: &ServerState, req: &Request) -> RouteResult {
    let m = req.method.as_str();
    let p = req.path.as_str();

    if m == "GET" && p == "/metrics" {
        // 服务端口仅绑定 loopback，/metrics 不经 nginx 暴露，故无需认证
        let c = state.store.counts().unwrap_or_default();
        let uptime = state.started_at.elapsed().as_secs();
        let statuses = state.http_stats.by_status.lock().unwrap().clone();
        let total = *state.http_stats.total.lock().unwrap();
        let mut out = String::new();
        out.push_str("# TYPE ysync_uptime_seconds gauge\n");
        out.push_str(&format!("ysync_uptime_seconds {uptime}\n"));
        for (name, val) in [
            ("ysync_users", c.users),
            ("ysync_devices", c.devices),
            ("ysync_files", c.files),
            ("ysync_dirs", c.dirs),
            ("ysync_blobs", c.blobs),
            ("ysync_blob_bytes", c.blob_bytes),
            ("ysync_shares", c.shares),
            ("ysync_trash_items", c.trash),
        ] {
            out.push_str(&format!("# TYPE {name} gauge\n{name} {val}\n"));
        }
        let bin = state.bytes_in.load(std::sync::atomic::Ordering::Relaxed);
        let bout = state.bytes_out.load(std::sync::atomic::Ordering::Relaxed);
        out.push_str("# TYPE ysync_bytes_in_total counter\n");
        out.push_str(&format!("ysync_bytes_in_total {bin}\n"));
        out.push_str("# TYPE ysync_bytes_out_total counter\n");
        out.push_str(&format!("ysync_bytes_out_total {bout}\n"));
        out.push_str("# TYPE ysync_http_requests_total counter\n");
        out.push_str(&format!("ysync_http_requests_total {total}\n"));
        for (code, n) in &statuses {
            out.push_str(&format!(
                "# TYPE ysync_http_requests_status{{code=\"{code}\"}} counter\n"
            ));
            out.push_str(&format!(
                "ysync_http_requests_status{{code=\"{code}\"}} {n}\n"
            ));
        }
        return (200, "text/plain; version=0.0.4".into(), Body::Bytes(out.into_bytes()), vec![]);
    }
    if m == "GET" && p == "/healthz" {
        const VERSION: &str = match option_env!("Y_SYNC_VERSION") {
            Some(v) => v,
            None => env!("CARGO_PKG_VERSION"),
        };
        return ok_json(serde_json::json!({"status": "ok", "version": VERSION}));
    }
    if m == "POST" && p == "/api/v1/auth/login" {
        #[derive(serde::Deserialize)]
        struct LoginReq {
            #[serde(rename = "user", default)]
            user: String,
            #[serde(rename = "password", default)]
            password: String,
            #[serde(rename = "device_name", default)]
            device_name: String,
        }
        let Ok(lr) = serde_json::from_slice::<LoginReq>(&req.body) else {
            return err_json(400, "bad request");
        };
        // 暴力破解防护：X-Real-IP（nginx 设置）+ 用户名 维度
        let ip = req.header("X-Real-IP").unwrap_or_else(|| "unknown".into());
        if let Err(retry_after) = state.login_guard.check(&ip, &lr.user) {
            let mut h = vec![("Retry-After", retry_after.to_string())];
            h.push(("Content-Type", "application/json".into()));
            return (
                429,
                "application/json".into(),
                Body::Bytes(serde_json::to_vec(
                    &serde_json::json!({"error": format!("尝试过于频繁，请 {retry_after} 秒后重试")}),
                ).unwrap_or_default()),
                h,
            );
        }
        let Ok(uid) = state.store.authenticate(&lr.user, &lr.password) else {
            state.login_guard.record_failure(&ip, &lr.user);
            audit(state, &lr.user, 0, "login_failed", &format!("ip={ip}"));
            return err_json(401, "invalid credentials");
        };
        state.login_guard.record_success(&ip, &lr.user);
        audit(state, &lr.user, 0, "login", &format!("ip={ip} device={}", lr.device_name));
        let name = if lr.device_name.is_empty() {
            "unnamed-device".to_string()
        } else {
            lr.device_name.clone()
        };
        let Ok((dev_id, token)) = state.store.create_device(uid, &name) else {
            return err_json(500, "create device failed");
        };
        return ok_json(serde_json::json!({
            "token": token, "user_id": uid, "device_id": dev_id, "device_name": name
        }));
    }

    // ---------- 认证区 ----------
    if p.starts_with("/api/") {
        let Some((uid, device_id)) = auth_ok(state, req) else {
            return err_json(401, "unauthorized");
        };

        match (m, p) {
            ("GET", "/api/v1/nodes") => {
                // P0-1：带 limit 参数走分页（has_more + after 游标）；不带则全量（兼容）
                let limit: i64 = match req.q("limit") {
                    Some(v) => v.parse().unwrap_or(-1),
                    None => -1,
                };
                if limit > 0 {
                    let after: i64 = req.q("after").and_then(|v| v.parse().ok()).unwrap_or(0);
                    return match state.store.nodes_paged(uid, after, limit) {
                        Ok((nodes, has_more)) => ok_json(
                            serde_json::json!({ "nodes": nodes, "has_more": has_more }),
                        ),
                        Err(e) => err_json(500, &e),
                    };
                }
                return match state.store.nodes(uid) {
                    Ok(nodes) => ok_json(serde_json::json!({ "nodes": nodes })),
                    Err(e) => err_json(500, &e),
                };
            }
            ("GET", "/api/v1/sync/head") => {
                return match state.store.head_cursor(uid) {
                    Ok(c) => ok_json(serde_json::json!({
                        "cursor": c, "watermark": state.store.journal_watermark()
                    })),
                    Err(e) => err_json(500, &e),
                };
            }
            ("GET", "/api/v1/sync/changes") => {
                let since: i64 = req.q("cursor").and_then(|v| v.parse().ok()).unwrap_or(0);
                let root: i64 = req.q("root").and_then(|v| v.parse().ok()).unwrap_or(0);
                let limit: i64 = req.q("limit").and_then(|v| v.parse().ok()).unwrap_or(1000);
                return match state.store.changes(uid, since, limit, root) {
                    Ok((changes, head, watermark)) => ok_json(
                        serde_json::json!({
                            "cursor": head, "changes": changes, "watermark": watermark
                        }),
                    ),
                    Err(e) => err_json(500, &e),
                };
            }
            ("PUT", "/api/v1/content") => {
                let want = req
                    .header("X-Content-SHA256")
                    .unwrap_or_default();
                match state.store.blobs.put(req.body.as_slice(), &want) {
                    Ok(r) => {
                        state
                            .bytes_in
                            .fetch_add(r.size as u64, std::sync::atomic::Ordering::Relaxed);
                        if let Err(e) = state.store.ensure_blob_row(&r.hash, r.size) {
                            return err_json(500, &e);
                        }
                        return ok_json(
                            serde_json::json!({ "hash": r.hash, "dedup": r.dedup }),
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        return err_json(400, "hash mismatch");
                    }
                    Err(e) => return err_json(500, &e.to_string()),
                }
            }
            ("GET", "/api/v1/content") if !req.path.is_empty() => {}
            _ => {}
        }

        if m == "GET" && p.starts_with("/api/v1/content/") {
            let hash = p.trim_start_matches("/api/v1/content/");
            // CDC 清单内容：重组装（Body::Manifest 流式）
            if state.store.is_manifest(hash) {
                let Some((chunks, total)) = state.store.manifest_info(hash) else {
                    return err_json(500, "manifest broken");
                };
                let mut parts: Vec<(std::path::PathBuf, u64)> = Vec::with_capacity(chunks.len());
                let mut missing = Vec::new();
                for ch in &chunks {
                    match state.store.blobs.blob_path_of(ch) {
                        Some(pb) => {
                            let l = std::fs::metadata(&pb).map(|m| m.len()).unwrap_or(0);
                            parts.push((pb, l));
                        }
                        None => missing.push(ch.clone()),
                    }
                }
                if !missing.is_empty() {
                    return err_json(500, &format!("missing chunks: {}", missing.join(",")));
                }
                let mut headers: Vec<(&'static str, String)> =
                    vec![("X-Content-SHA256", hash.to_string())];
                // Range 支持（单区间）
                if let Some(range) = req.header("Range") {
                    if let Some((start, end)) = parse_range(&range, total as i64) {
                        let end = end.min(total as i64 - 1);
                        let len = (end - start + 1).max(0) as u64;
                        headers.push((
                            "Content-Range",
                            format!("bytes {start}-{end}/{}", total),
                        ));
                        return (
                            206,
                            "application/octet-stream".into(),
                            Body::Manifest { parts, skip: start as u64, len: len as u64 },
                            headers,
                        );
                    }
                }
                return (
                    200,
                    "application/octet-stream".into(),
                    Body::Manifest { parts, skip: 0, len: total as u64 },
                    headers,
                );
            }
            let mut file = match state.store.blobs.open(hash) {
                Ok(f) => f,
                Err(_) => return err_json(404, "content not found"),
            };
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let mut headers: Vec<(&'static str, String)> =
                vec![("X-Content-SHA256", hash.to_string())];
            // Range 支持（单区间）；File+偏移流式，避免大文件整载内存（A2）
            if let Some(range) = req.header("Range") {
                if let Some((start, end)) = parse_range(&range, size as i64) {
                    let end = end.min(size as i64 - 1);
                    let len = (end - start + 1).max(0) as u64;
                    use std::io::Seek;
                    let _ = file.seek(std::io::SeekFrom::Start(start as u64));
                    headers.push((
                        "Content-Range",
                        format!("bytes {start}-{end}/{}", size),
                    ));
                    state
                        .bytes_out
                        .fetch_add(len, std::sync::atomic::Ordering::Relaxed);
                    return (
                        206,
                        "application/octet-stream".into(),
                        Body::File(file, len),
                        headers,
                    );
                }
            }
            state
                .bytes_out
                .fetch_add(size, std::sync::atomic::Ordering::Relaxed);
            return (
                200,
                "application/octet-stream".into(),
                Body::File(file, size),
                headers,
            );
        }

        if m == "POST" && p == "/api/v1/ops" {
            let Ok(ops) = serde_json::from_slice::<Vec<ysync_core::protocol::Op>>(&req.body) else {
                return err_json(400, "bad request");
            };
            return match state.store.apply_ops(uid, device_id, &ops) {
                Ok(results) => {
                    for (op, r) in ops.iter().zip(results.iter()) {
                        let detail = if op.content_hash.is_empty() {
                            op.name.clone()
                        } else {
                            format!("{} {}", op.name, &op.content_hash[..8.min(op.content_hash.len())])
                        };
                        if r.ok {
                            audit(state, &uid.to_string(), device_id, &format!("op_{}", op.op), &detail);
                        } else {
                            audit(
                                state,
                                &uid.to_string(),
                                device_id,
                                "op_failed",
                                &format!("{} {} {}", op.op, detail, r.error),
                            );
                        }
                    }
                    if !ops.is_empty() {
                        if let Ok(head) = state.store.head_cursor(uid) {
                            state.hub.notify(
                                uid,
                                &serde_json::to_string(
                                    &serde_json::json!({"user_id": uid, "device_id": device_id, "cursor": head}),
                                )
                                .unwrap_or_default(),
                            );
                        }
                    }
                    ok_json(serde_json::json!({ "results": results }))
                }
                Err(e) => err_json(500, &e),
            };
        }

        if m == "GET" && p == "/api/v1/devices" {
            let Ok(mut list) = state.store.list_devices(uid) else {
                return err_json(500, "query failed");
            };
            for v in &mut list {
                if v["id"].as_i64() == Some(device_id) {
                    v["current"] = serde_json::Value::Bool(true);
                }
            }
            return ok_json(serde_json::json!({ "devices": list }));
        }
        if m == "DELETE" && p.starts_with("/api/v1/devices/") {
            let id: i64 = p.trim_start_matches("/api/v1/devices/").parse().unwrap_or(-1);
            return match state.store.revoke_device(uid, id) {
                Ok(()) => {
                    audit(state, &uid.to_string(), device_id, "device_revoked", &id.to_string());
                    ok_json(serde_json::json!({ "ok": true }))
                }
                Err(e) if e == crate::store::ERR_NOT_FOUND => err_json(404, "device not found"),
                Err(e) => err_json(500, &e),
            };
        }
        if m == "GET" && p == "/api/v1/trash" {
            return match state.store.list_trash(uid) {
                Ok(items) => ok_json(serde_json::json!({ "items": items })),
                Err(e) => err_json(500, &e),
            };
        }
        if m == "POST" && p.starts_with("/api/v1/trash/") && p.ends_with("/restore") {
            let id: i64 = p
                .trim_start_matches("/api/v1/trash/")
                .trim_end_matches("/restore")
                .parse()
                .unwrap_or(-1);
            return match state.store.restore_trash(uid, id) {
                Ok(n) => {
                    audit(state, &uid.to_string(), device_id, "trash_restore", &n.path);
                    ok_json(n)
                }
                Err(e) if e == crate::store::ERR_NOT_FOUND => err_json(404, "trash item not found"),
                Err(e) => err_json(500, &e),
            };
        }
        if m == "DELETE" && p.starts_with("/api/v1/trash/") {
            let id: i64 = p.trim_start_matches("/api/v1/trash/").parse().unwrap_or(-1);
            return match state.store.delete_trash(uid, id) {
                Ok(()) => {
                    audit(state, &uid.to_string(), device_id, "trash_delete", &id.to_string());
                    ok_json(serde_json::json!({ "ok": true }))
                }
                Err(_) => err_json(404, "trash item not found"),
            };
        }
        if m == "GET" && p.starts_with("/api/v1/nodes/") && p.ends_with("/versions") {
            let id: i64 = p
                .trim_start_matches("/api/v1/nodes/")
                .trim_end_matches("/versions")
                .parse()
                .unwrap_or(-1);
            return match state.store.list_versions(uid, id) {
                Ok(versions) => ok_json(serde_json::json!({ "versions": versions })),
                Err(e) if e == crate::store::ERR_NOT_FOUND => err_json(404, "node not found"),
                Err(e) => err_json(500, &e),
            };
        }
        if m == "GET" && p.starts_with("/api/v1/versions/") && p.ends_with("/content") {
            let id: i64 = p
                .trim_start_matches("/api/v1/versions/")
                .trim_end_matches("/content")
                .parse()
                .unwrap_or(-1);
            let Ok(hash) = state.store.version_content(uid, id) else {
                return err_json(404, "version not found");
            };
            let file = match state.store.blobs.open(&hash) {
                Ok(f) => f,
                Err(_) => return err_json(404, "content not found"),
            };
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            return (
                200,
                "application/octet-stream".into(),
                Body::File(file, len),
                vec![],
            );
        }

        // ---------- CDC 清单（P1-8） ----------
        if m == "POST" && p == "/api/v1/manifests" {
            #[derive(serde::Deserialize)]
            struct MReq {
                #[serde(rename = "size")]
                size: i64,
                #[serde(rename = "file_hash")]
                file_hash: String,
                #[serde(rename = "chunks")]
                chunks: Vec<String>,
            }
            let Ok(mr) = serde_json::from_slice::<MReq>(&req.body) else {
                return err_json(400, "bad manifest");
            };
            return match state.store.create_manifest(&mr.file_hash, mr.size, &mr.chunks) {
                Ok(missing) if missing.is_empty() => ok_json(
                    serde_json::json!({ "hash": mr.file_hash }),
                ),
                Ok(missing) => (
                    409,
                    "application/json".into(),
                    Body::Bytes(
                        serde_json::to_vec(&serde_json::json!({ "missing": missing }))
                            .unwrap_or_default(),
                    ),
                    vec![],
                ),
                Err(e) => err_json(400, &e),
            };
        }

        // ---------- 分块上传（FR-S11） ----------
        if m == "POST" && p == "/api/v1/uploads" {
            #[derive(serde::Deserialize)]
            struct UReq {
                #[serde(rename = "size", default)]
                size: i64,
                #[serde(rename = "sha256", default)]
                sha256: String,
                #[serde(rename = "chunk_size", default)]
                chunk_size: i64,
            }
            let Ok(ur) = serde_json::from_slice::<UReq>(&req.body) else {
                return err_json(400, "bad request");
            };
            let chunk = if ur.chunk_size <= 0 { 8 << 20 } else { ur.chunk_size };
            return match state.uploads.create(ur.size, &ur.sha256, chunk) {
                Ok(s) => ok_json(serde_json::json!({ "id": s.id, "received": s.received })),
                Err(e) => err_json(400, &e),
            };
        }
        if m == "GET" && p.starts_with("/api/v1/uploads/") {
            let id = p.trim_start_matches("/api/v1/uploads/");
            return match state.uploads.get(id) {
                Some(s) => {
                    let received: Vec<i64> = s.lock().unwrap().received.iter().copied().collect();
                    ok_json(serde_json::json!({ "id": id, "received": received }))
                }
                None => err_json(404, "session not found"),
            };
        }
        if m == "PUT" && p.starts_with("/api/v1/uploads/") {
            let id = p.trim_start_matches("/api/v1/uploads/");
            let Some(s) = state.uploads.get(id) else {
                return err_json(404, "session not found");
            };
            let Ok(chunk_no) = req
                .q("chunk")
                .and_then(|v| v.parse::<i64>().ok())
                .ok_or_else(|| "bad chunk".to_string())
            else {
                return err_json(400, "bad chunk");
            };
            let mut s = s.lock().unwrap();
            let off = chunk_no * s.chunk;
            if off < 0 || off >= s.size || req.body.len() as i64 > s.chunk {
                return err_json(400, "offset out of range");
            }
            if let Err(e) = write_chunk_at(&s.path, off, &req.body) {
                return err_json(500, &e);
            }
            s.received.insert(chunk_no);
            return (204, "text/plain".into(), Body::Bytes(Vec::new()), vec![]);
        }
        if m == "POST" && p.starts_with("/api/v1/uploads/") && p.ends_with("/complete") {
            let id = p
                .trim_start_matches("/api/v1/uploads/")
                .trim_end_matches("/complete");
            let Some(s) = state.uploads.get(id) else {
                return err_json(404, "session not found");
            };
            let complete = {
                let mut s = s.lock().unwrap();
                let total = s.total_chunks();
                let missing = (0..total).any(|i| !s.received.contains(&i));
                if missing {
                    Err("missing chunk".to_string())
                } else {
                    use sha2::Digest;
                    let Ok(mut f) = std::fs::File::open(&s.path) else {
                        return err_json(500, "open session file failed");
                    };
                    let mut h: sha2::Sha256 = sha2::Digest::new();
                    let mut size = 0i64;
                    let mut buf = [0u8; 256 * 1024];
                    loop {
                        match f.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                h.update(&buf[..n]);
                                size += n as i64;
                            }
                            Err(e) => return err_json(500, &e.to_string()),
                        }
                    }
                    let got = crate::util::hex(&h.finalize());
                    if !s.sha256.is_empty() && got != s.sha256 {
                        Err("hash mismatch".to_string())
                    } else {
                        Ok((got, size))
                    }
                }
            };
            return match complete {
                Ok((hash, size)) => {
                    if let Err(e) = state.store.ensure_blob_row(&hash, size) {
                        return err_json(500, &e);
                    }
                    // 落为 blob（复用 put 的改名逻辑：先复制 tmp 到 blobs 路径）
                    let blob_result = match state.uploads.get(id).unwrap().lock() {
                        Ok(s) => match std::fs::File::open(&s.path) {
                            Ok(mut f) => state
                                .store
                                .blobs
                                .put(&mut f, &hash)
                                .map(|_| ())
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(e.to_string()),
                        },
                        Err(_) => Err("lock poisoned".to_string()),
                    };
                    match blob_result {
                        Ok(_) => {
                            state.uploads.drop_session(id);
                            ok_json(serde_json::json!({ "hash": hash }))
                        }
                        Err(e) => err_json(500, &e),
                    }
                }
                Err(e) => err_json(400, &e),
            };
        }

        // ---------- 分享管理（FR-H1） ----------
        if m == "POST" && p == "/api/v1/shares" {
            #[derive(serde::Deserialize)]
            struct SReq {
                #[serde(rename = "path", default)]
                path: String,
                #[serde(rename = "hours", default)]
                hours: i64,
                #[serde(rename = "password", default)]
                password: String,
            }
            let Ok(sr) = serde_json::from_slice::<SReq>(&req.body) else {
                return err_json(400, "bad request");
            };
            return match state.store.create_share(uid, &sr.path, sr.hours, &sr.password) {
                Ok(info) => {
                    audit(state, &uid.to_string(), device_id, "share_create", &sr.path);
                    ok_json(info)
                }
                Err(e) => err_json(400, &e),
            };
        }
        if m == "GET" && p == "/api/v1/shares" {
            return match state.store.list_shares(uid) {
                Ok(shares) => ok_json(serde_json::json!({ "shares": shares })),
                Err(e) => err_json(500, &e),
            };
        }
        if m == "DELETE" && p.starts_with("/api/v1/shares/") {
            let token = p.trim_start_matches("/api/v1/shares/");
            return match state.store.delete_share(uid, token) {
                Ok(()) => {
                    audit(state, &uid.to_string(), device_id, "share_delete", token);
                    ok_json(serde_json::json!({ "ok": true }))
                }
                Err(_) => err_json(404, "share not found"),
            };
        }
    }

    // ---------- 公开分享 ----------
    if m == "GET" && p.starts_with("/s/") {
        return handle_public_share(state, req, p);
    }
    // ---------- 浏览页 ----------
    if m == "GET" && p == "/browse" {
        return handle_browse(state, req);
    }
    // ---------- 只读 WebDAV ----------
    if p == "/dav" || p.starts_with("/dav/") {
        return handle_dav(state, req);
    }

    err_json(404, "not found")
}

fn parse_range(range: &str, size: i64) -> Option<(i64, i64)> {
    let spec = range.strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    let start: i64 = start_s.parse().ok()?;
    let end: i64 = if end_s.is_empty() {
        size - 1
    } else {
        end_s.parse().ok()?
    };
    if start < 0 || start > end || start >= size {
        return None;
    }
    Some((start, end))
}

fn write_chunk_at(path: &std::path::Path, offset: i64, data: &[u8]) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(offset as u64))
        .map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())
}

// ---------- 公开分享（FR-H1） ----------

fn handle_public_share(state: &ServerState, req: &Request, p: &str) -> RouteResult {
    let rest = p.trim_start_matches("/s/");
    let (token, rel) = match rest.split_once('/') {
        Some((t, r)) => (t.to_string(), r.to_string()),
        None => (rest.to_string(), String::new()),
    };
    let Ok((user_id, node_id, pwd_hash, _)) = state.store.get_share(&token) else {
        return (404, "text/html".into(), Body::Bytes("链接不存在或已过期".as_bytes().to_vec()), vec![]);
    };
    if !pwd_hash.is_empty() {
        let ip = req
            .header("X-Real-IP")
            .unwrap_or_else(|| "unknown".into());
        if let Err(retry) = state.share_guard.check(&ip, &token) {
            let mut h = vec![("Retry-After", retry.to_string())];
            h.push(("Content-Type", "text/html".into()));
            return (
                429,
                "text/html".into(),
                Body::Bytes(format!("尝试过于频繁，请 {retry} 秒后重试").into_bytes()),
                h,
            );
        }
        let pwd = req.q("p").unwrap_or_default();
        if sha256_hex(format!("ysync-share:{pwd}").as_bytes()) != pwd_hash {
            state.share_guard.record_failure(&ip, &token);
            return (401, "text/html".into(), Body::Bytes("需要密码（?p=）".as_bytes().to_vec()), vec![]);
        }
        state.share_guard.record_success(&ip, &token);
    }
    let Ok(root) = node_by_id(state, user_id, node_id) else {
        return (404, "text/html".into(), Body::Bytes("内容不存在".as_bytes().to_vec()), vec![]);
    };
    if !rel.is_empty() {
        if root.kind != "dir" {
            return (404, "text/html".into(), Body::Bytes("not found".as_bytes().to_vec()), vec![]);
        }
        if rel.contains("..") {
            return (400, "text/html".into(), Body::Bytes("bad path".as_bytes().to_vec()), vec![]);
        }
        let Ok(n) = store_node_by_path(state, user_id, &format!("{}/{}", root.path, rel)) else {
            return (404, "text/html".into(), Body::Bytes("not found".as_bytes().to_vec()), vec![]);
        };
        return serve_file_or_list(state, user_id, req, &token, n, &rel);
    }
    serve_file_or_list(state, user_id, req, &token, root, "")
}

fn serve_file_or_list(
    state: &ServerState,
    user_id: i64,
    req: &Request,
    token: &str,
    n: NodeInfo,
    rel: &str,
) -> RouteResult {
    if n.kind == "dir" {
        // 子路径部署友好：目录统一 301 到带尾斜杠的自身（相对 Location，
        // 按当前 URL 的父目录解析，外部前缀如 /y-sync 由浏览器自行保留）
        if !rel.is_empty() && !req.path.ends_with('/') {
            let pwd = req.q("p").unwrap_or_default();
            let loc = if pwd.is_empty() {
                format!("{rel}/")
            } else {
                format!("{rel}/?p={pwd}")
            };
            return (301, "text/html".into(), Body::Bytes(Vec::new()), vec![("Location", loc)]);
        }
        let nodes = state.store.nodes(user_id).unwrap_or_default();
        let prefix = if n.path.is_empty() {
            String::new()
        } else {
            format!("{}/", n.path)
        };
        let pwd_q = req
            .q("p")
            .map(|p| format!("?p={p}"))
            .unwrap_or_default();
        let mut html = format!(
            "<!doctype html><meta charset=utf-8><title>{}</title><h3>{}</h3><ul>",
            html_escape(&n.path),
            html_escape(&n.path)
        );
        if !rel.is_empty() {
            html.push_str(&format!(r#"<li><a href="../{pwd_q}">../</a></li>"#));
        }
        for k in &nodes {
            if !prefix.is_empty() {
                if !k.path.starts_with(&prefix) {
                    continue;
                }
            } else if k.path.contains('/') {
                continue;
            }
            let child_rel = k.path.trim_start_matches(&prefix).to_string();
            if child_rel.contains('/') {
                continue; // 仅当前层
            }
            let href = format!("{child_rel}{pwd_q}");
            let label = if k.kind == "dir" {
                format!("{}/", html_escape(&child_rel))
            } else {
                format!("{} ({:.1} KB)", html_escape(&child_rel), k.size as f64 / 1024.0)
            };
            html.push_str(&format!(r#"<li><a href="{href}">{label}</a></li>"#));
        }
        html.push_str("</ul>");
        return (200, "text/html; charset=utf-8".into(), Body::Bytes(html.into_bytes()), vec![]);
    }
    let file = match state.store.blobs.open(&n.content_hash) {
        Ok(f) => f,
        Err(_) => return (404, "text/html".into(), Body::Bytes("content missing".as_bytes().to_vec()), vec![]),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let headers: Vec<(&'static str, String)> = vec![(
        "Content-Disposition",
        format!(
            "attachment; filename*=UTF-8''{}",
            urlencoding::encode(&n.name)
        ),
    )];
    (
        200,
        "application/octet-stream".into(),
        Body::File(file, len),
        headers,
    )
}

fn node_by_id(state: &ServerState, user_id: i64, id: i64) -> Result<NodeInfo, String> {
    state.store.nodes(user_id).map_err(|e| e)?.into_iter().find(|n| n.id == id).ok_or_else(|| "not found".to_string())
}

fn store_node_by_path(state: &ServerState, user_id: i64, path: &str) -> Result<NodeInfo, String> {
    state
        .store
        .nodes(user_id)
        .map_err(|e| e)?
        .into_iter()
        .find(|n| n.path == path)
        .ok_or_else(|| "not found".to_string())
}

// ---------- 浏览页 ----------

fn handle_browse(state: &ServerState, req: &Request) -> RouteResult {
    let Some(token) = req.q("token") else {
        return (401, "text/html".into(), Body::Bytes("需要 token".as_bytes().to_vec()), vec![]);
    };
    let Ok((uid, _)) = state.store.auth_token(&token) else {
        return (401, "text/html".into(), Body::Bytes("unauthorized".as_bytes().to_vec()), vec![]);
    };
    let path = req.q("path").unwrap_or_default();
    let path = path.trim_matches('/').to_string();
    let nodes = state.store.nodes(uid).unwrap_or_default();
    // 相对链接（子路径部署友好）：子目录走 path 查询参数，文件直接相对引用
    let mut html = format!(
        "<!doctype html><meta charset=utf-8><title>y-sync 浏览</title><h3>/{}§</h3><ul>",
        html_escape(&path)
    );
    html = html.replace('\u{a7}', "");
    let parent_q = match path.rfind('/') {
        Some(i) => format!("?token={token}&path={}", &path[..i]),
        None => format!("?token={token}"),
    };
    if !path.is_empty() {
        html.push_str(&format!(r#"<li><a href="{parent_q}">../</a></li>"#));
    }
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    for n in &nodes {
        if prefix.is_empty() {
            if n.path.contains('/') {
                continue;
            }
        } else if !n.path.starts_with(&prefix)
            || n.path.trim_start_matches(&prefix).contains('/')
        {
            continue;
        }
        let rel = n.path.trim_start_matches(&prefix).to_string();
        if n.kind == "dir" {
            html.push_str(&format!(
                r#"<li><a href="{}/">{}/</a></li>"#,
                urlencoding::encode(&n.path),
                html_escape(&rel)
            ));
        } else {
            html.push_str(&format!(
                r#"<li><a href="{}?token={}">{} ({:.1} KB)</a></li>"#,
                urlencoding::encode(&n.path),
                token,
                html_escape(&rel),
                n.size as f64 / 1024.0
            ));
        }
    }
    html.push_str("</ul>");
    (200, "text/html; charset=utf-8".into(), Body::Bytes(html.into_bytes()), vec![])
}

// ---------- 只读 WebDAV（M4 最小实现：OPTIONS/PROPFIND/GET + Basic Auth） ----------

fn handle_dav(state: &ServerState, req: &Request) -> RouteResult {
    let Some(auth) = req.header("Authorization") else {
        return (
            401,
            "text/plain".into(),
            Body::Bytes("unauthorized".as_bytes().to_vec()),
            vec![("WWW-Authenticate", "Basic realm=\"y-sync\"".to_string())],
        );
    };
    let Some(b64) = auth.strip_prefix("Basic ") else {
        return (401, "text/plain".into(), Body::Bytes("unauthorized".as_bytes().to_vec()), vec![]);
    };
    let decoded = base64_decode(b64);
    let Some((user, pass)) = String::from_utf8(decoded).ok().and_then(|s| s.split_once(':').map(|(a, b)| (a.to_string(), b.to_string()))) else {
        return (401, "text/plain".into(), Body::Bytes("unauthorized".as_bytes().to_vec()), vec![]);
    };
    let Ok(uid) = state.store.authenticate(&user, &pass) else {
        return (
            401,
            "text/plain".into(),
            Body::Bytes("unauthorized".as_bytes().to_vec()),
            vec![("WWW-Authenticate", "Basic realm=\"y-sync\"".to_string())],
        );
    };
    let dav_path = req.path.trim_start_matches("/dav").trim_matches('/').to_string();
    let nodes = state.store.nodes(uid).unwrap_or_default();

    let find_node = |p: &str| -> Option<NodeInfo> {
        if p.is_empty() {
            return Some(NodeInfo {
                kind: "dir".into(),
                ..Default::default()
            });
        }
        nodes.iter().find(|n| n.path == p).cloned()
    };
    let children_of = |dir: &str| -> Vec<NodeInfo> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        nodes
            .iter()
            .filter(|n| {
                if prefix.is_empty() {
                    !n.path.contains('/')
                } else {
                    n.path.starts_with(&prefix)
                        && !n.path.trim_start_matches(&prefix).contains('/')
                }
            })
            .cloned()
            .collect()
    };

    match req.method.as_str() {
        "OPTIONS" => (
            200,
            "text/plain".into(),
            Body::Bytes(Vec::new()),
            vec![("DAV", "1".to_string()), ("Allow", "OPTIONS, GET, PROPFIND".to_string())],
        ),
        "GET" => {
            let Some(n) = find_node(&dav_path) else {
                return (404, "text/plain".into(), Body::Bytes("not found".as_bytes().to_vec()), vec![]);
            };
            if n.kind == "dir" {
                return (200, "text/plain".into(), Body::Bytes(Vec::new()), vec![]);
            }
            let file = match state.store.blobs.open(&n.content_hash) {
                Ok(f) => f,
                Err(_) => return (404, "text/plain".into(), Body::Bytes("missing".as_bytes().to_vec()), vec![]),
            };
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            (200, "application/octet-stream".into(), Body::File(file, len), vec![])
        }
        "PROPFIND" => {
            let Some(n) = find_node(&dav_path) else {
                return (404, "text/plain".into(), Body::Bytes("not found".as_bytes().to_vec()), vec![]);
            };
            let depth = req.header("Depth").unwrap_or_else(|| "0".into());
            let href_base = format!("/dav/{}", dav_path);
            let mut xml = String::from(
                r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">"#,
            );
            let entry = |href: &str, is_dir: bool, size: i64, mtime: i64| -> String {
                let rt = if is_dir {
                    r#"<D:resourcetype><D:collection/></D:resourcetype>"#.to_string()
                } else {
                    format!(
                        r#"<D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength>"#
                    )
                };
                format!(
                    r#"<D:response><D:href>{href}</D:href><D:propstat><D:prop>{rt}<D:getlastmodified>{}</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"#,
                    http_date(mtime)
                )
            };
            let self_href = if dav_path.is_empty() {
                "/dav/".to_string()
            } else if n.kind == "dir" {
                format!("{href_base}/")
            } else {
                href_base.clone()
            };
            xml.push_str(&entry(&self_href, n.kind == "dir", n.size, n.mtime));
            if depth == "1" && n.kind == "dir" {
                let prefix = if dav_path.is_empty() {
                    String::new()
                } else {
                    format!("{dav_path}/")
                };
                for k in children_of(&dav_path) {
                    let child_rel = k.path.trim_start_matches(&prefix).to_string();
                    let href = if k.kind == "dir" {
                        format!("/dav/{}{}/", dav_path, child_rel)
                    } else {
                        format!("/dav/{}{}", dav_path, child_rel)
                    };
                    xml.push_str(&entry(&href, k.kind == "dir", k.size, k.mtime));
                }
            }
            xml.push_str("</D:multistatus>");
            (
                207,
                "application/xml; charset=utf-8".into(),
                Body::Bytes(xml.into_bytes()),
                vec![],
            )
        }
        _ => (405, "text/plain".into(), Body::Bytes("method not allowed".as_bytes().to_vec()), vec![]),
    }
}

fn http_date(mtime_milli: i64) -> String {
    // RFC1123 风格（GMT）；mtime 用途展示
    let secs = (mtime_milli / 1000).max(0) as u64;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let wd = ((days + 4) % 7) as usize; // 1970-01-01 是周四
    let weekdays = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
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
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    while d >= mdays[m] {
        d -= mdays[m];
        m += 1;
    }
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {:02} {} {y:04} {h:02}:{mi:02}:{s:02} GMT",
        weekdays[wd],
        d + 1,
        months[m]
    )
}

fn base64_decode(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s = s.trim_end_matches('=');
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let Some(v) = T.iter().position(|t| *t == c) else { break };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}
