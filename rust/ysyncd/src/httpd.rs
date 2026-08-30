//! 本地控制服务（tiny_http）：与管理页端点和 Go daemon 完全一致（含 token 认证）。
use std::io::Read;
use std::sync::Arc;

use tiny_http::{Header, Method, Response, Server};

use crate::daemon::Daemon;

pub fn serve(addr: &str, token: String, daemon: Daemon) -> Result<String, String> {
    let server = Server::http(addr).map_err(|e| format!("{e}"))?;
    let actual = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a.to_string(),
        _ => addr.to_string(),
    };
    let server = Arc::new(server);
    for _ in 0..4 {
        let server = server.clone();
        let daemon = daemon.clone();
        let token = token.clone();
        std::thread::spawn(move || loop {
            let Ok(request) = server.recv() else { return };
            handle(request, &token, &daemon);
        });
    }
    Ok(actual)
}

fn handle(mut request: tiny_http::Request, token: &str, daemon: &Daemon) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };
    let qtoken = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .unwrap_or("")
        .to_string();

    // 认证：query token 或 Bearer
    let mut bearer = String::new();
    let mut htoken = String::new();
    for h in request.headers() {
        if h.field.as_str().as_str().eq_ignore_ascii_case("authorization") {
            bearer = h.value.as_str().trim_start_matches("Bearer ").to_string();
        }
        if h.field.as_str().as_str().eq_ignore_ascii_case("x-ysync-token") {
            htoken = h.value.as_str().to_string();
        }
    }
    // X-Ysync-Token 优先（P0-4：管理台改用 header，token 不进 URL）
    let authed = htoken == token || qtoken == token || bearer == token;
    if path == "/setup-status" {
        return respond(
            request,
            200,
            &serde_json::to_string(&serde_json::json!({
                "initialized": daemon.is_initialized()
            }))
            .unwrap(),
            "application/json",
        );
    }
    if path == "/server-info" {
        let server_url = crate::ctx::with_cfg(|c| c.server_url.clone());
        return respond(
            request,
            200,
            &serde_json::to_string(&serde_json::json!({ "server_url": server_url })).unwrap(),
            "application/json",
        );
    }
    if path == "/healthz" {
        return respond(request, 200, "{\"status\":\"ok\"}", "application/json");
    }
    if method == Method::Get && (path == "/" || path.is_empty()) {
        // P0-4：token 不匹配时不回显管理页 token（防本机其他进程枚举）
        let qt = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .unwrap_or("");
        let html = if qt == token {
            page(&token)
        } else {
            "<!doctype html><meta charset=\"utf-8\"><p>token 无效或缺失，请通过 <code>ysync ui</code> 重新打开管理台。</p>".to_string()
        };
        return respond(request, 200, &html, "text/html; charset=utf-8");
    }
    if !authed {
        return respond(request, 401, "unauthorized", "text/plain");
    }

    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    let json_field = |field: &str| -> String {
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get(field)
                    .map(|x| x.as_str().unwrap_or_default().to_string())
            })
            .unwrap_or_default()
    };
    let json_field_str = |field: &str| -> String { json_field(field) };

    match (&method, path.as_str()) {
        (Method::Get, "/status") => {
            let folders = daemon.state.snapshot();
            respond(
                request,
                200,
                &serde_json::to_string(&serde_json::json!({ "folders": folders })).unwrap(),
                "application/json",
            );
        }
        (Method::Get, "/conflicts") => {
            let conflicts = daemon.conflicts();
            respond(
                request,
                200,
                &serde_json::to_string(&serde_json::json!({ "conflicts": conflicts })).unwrap(),
                "application/json",
            );
        }
        (Method::Post, "/sync") => {
            let folder = json_field_str("folder");
            let d = daemon.clone();
            if folder.is_empty() {
                std::thread::spawn(move || d.sync_all());
            } else {
                std::thread::spawn(move || {
                    let f = ctx_folder(&folder);
                    if let Some(f) = f {
                        d.sync_folder(&f);
                    }
                });
            }
            respond(request, 200, "{\"ok\":true}", "application/json");
        }
        (Method::Post, "/pause") | (Method::Post, "/resume") => {
            let mut folder = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("folder="))
                .unwrap_or("")
                .to_string();
            if folder.is_empty() {
                folder = json_field_str("folder");
            }
            let folder = urlencoding::decode(&folder).unwrap_or_default().to_string();
            if method == Method::Post && path == "/pause" {
                daemon.state.pause(&folder);
            } else {
                daemon.state.resume(&folder);
            }
            respond(request, 200, "{\"ok\":true}", "application/json");
        }
        (Method::Post, "/add") => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let local_path = v
                .get("local_path")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let use_gitignore = v
                .get("use_gitignore")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let excludes: Vec<String> = v
                .get("excludes")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            match daemon.add_folder(&local_path, &name, &excludes, use_gitignore) {
                Ok(()) => {
                    let d = daemon.clone();
                    std::thread::spawn(move || d.sync_all());
                    respond(request, 200, "{\"ok\":true}", "application/json");
                }
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Post, "/setup") => {
            let v: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let server_url = v.get("server_url").and_then(|x| x.as_str()).unwrap_or("");
            let user = v.get("user").and_then(|x| x.as_str()).unwrap_or("");
            let password = v.get("password").and_then(|x| x.as_str()).unwrap_or("");
            let device_name = v.get("device_name").and_then(|x| x.as_str()).unwrap_or("");
            match daemon.setup(server_url, user, password, device_name) {
                Ok(()) => {
                    let d = daemon.clone();
                    std::thread::spawn(move || d.sync_all());
                    respond(request, 200, "{\"ok\":true,\"initialized\":true}", "application/json")
                }
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Get, "/server-trash") => {
            let items = daemon.server_trash_list().unwrap_or_default();
            respond(request, 200, &serde_json::to_string(&serde_json::json!({ "items": items })).unwrap(), "application/json")
        }
        (Method::Post, "/trash-restore") => {
            let id = json_num("id", &body);
            match daemon.server_trash_restore(id) {
                Ok(()) => { let d = daemon.clone(); std::thread::spawn(move || d.sync_all()); respond(request, 200, "{\"ok\":true}", "application/json") }
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Post, "/trash-delete") => {
            let id = json_num("id", &body);
            match daemon.server_trash_delete(id) {
                Ok(()) => respond(request, 200, "{\"ok\":true}", "application/json"),
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Get, "/versions") => {
            let folder = query.split('&').find_map(|kv| kv.strip_prefix("folder=")).unwrap_or("");
            let folder = urlencoding::decode(folder).unwrap_or_default().to_string();
            let rel = query.split('&').find_map(|kv| kv.strip_prefix("rel=")).unwrap_or("");
            let rel = urlencoding::decode(rel).unwrap_or_default().to_string();
            match daemon.server_versions(&folder, &rel) {
                Ok((_, versions)) => respond(request, 200, &serde_json::to_string(&serde_json::json!({ "versions": versions })).unwrap(), "application/json"),
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Post, "/version-restore") => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let folder = v.get("folder").and_then(|x| x.as_str()).unwrap_or("");
            let rel = v.get("rel").and_then(|x| x.as_str()).unwrap_or("");
            let vid = v.get("version_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            match daemon.server_version_restore(folder, rel, vid) {
                Ok(()) => { let d = daemon.clone(); std::thread::spawn(move || d.sync_all()); respond(request, 200, "{\"ok\":true}", "application/json") }
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Get, "/server-shares") => {
            let shares = daemon.server_shares().unwrap_or_default();
            respond(request, 200, &serde_json::to_string(&serde_json::json!({ "shares": shares })).unwrap(), "application/json")
        }
        (Method::Post, "/share-create") => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let folder = v.get("folder").and_then(|x| x.as_str()).unwrap_or("");
            let rel = v.get("rel").and_then(|x| x.as_str()).unwrap_or("");
            let hours = v.get("hours").and_then(|x| x.as_i64()).unwrap_or(0);
            let password = v.get("password").and_then(|x| x.as_str()).unwrap_or("");
            match daemon.server_share_create(folder, rel, hours, password) {
                Ok(info) => respond(request, 200, &serde_json::to_string(&info).unwrap(), "application/json"),
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Post, "/share-delete") => {
            let token = json_field_str("token");
            match daemon.server_share_delete(&token) {
                Ok(()) => respond(request, 200, "{\"ok\":true}", "application/json"),
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Get, "/usage") => match daemon.my_usage() {
            Ok(v) => respond(request, 200, &v.to_string(), "application/json"),
            Err(e) => respond(request, 400, &e, "text/plain"),
        },
        (Method::Get, "/devices") => {
            let items = daemon.devices_list().unwrap_or_default();
            respond(
                request,
                200,
                &serde_json::to_string(&serde_json::json!({ "devices": items })).unwrap(),
                "application/json",
            )
        }
        (Method::Delete, p) if p.starts_with("/devices/") => {
            let id = p.trim_start_matches("/devices/").parse::<i64>().unwrap_or(-1);
            match daemon.device_revoke(id) {
                Ok(()) => respond(request, 200, "{\"ok\":true}", "application/json"),
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Get, "/audit") => {
            let entries = daemon.server_audit(200).unwrap_or_default();
            respond(request, 200, &serde_json::to_string(&serde_json::json!({ "entries": entries })).unwrap(), "application/json")
        }
        (Method::Post, "/remove") => {
            let name = json_field_str("name");
            match daemon.remove_folder(&name) {
                Ok(()) => respond(request, 200, "{\"ok\":true}", "application/json"),
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        (Method::Post, "/resolve") => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let folder = v.get("folder").and_then(|x| x.as_str()).unwrap_or_default();
            let rel = v.get("rel").and_then(|x| x.as_str()).unwrap_or_default();
            let copy_rel = v.get("copy_rel").and_then(|x| x.as_str()).unwrap_or_default();
            let choice = v.get("choice").and_then(|x| x.as_str()).unwrap_or_default();
            match daemon.resolve_conflict(folder, rel, copy_rel, choice) {
                Ok(()) => {
                    let d = daemon.clone();
                    std::thread::spawn(move || d.sync_all());
                    respond(request, 200, "{\"ok\":true}", "application/json");
                }
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
        _ => respond(request, 404, "not found", "text/plain"),
    }
}

fn ctx_folder(name: &str) -> Option<ysync_core::Folder> {
    crate::ctx::with_cfg(|c| c.folders.iter().find(|f| f.name == name).cloned())
}

fn respond(request: tiny_http::Request, code: u16, body: &str, ctype: &str) {
    let response = Response::from_string(body)
        .with_status_code(code)
        .with_header(Header::from_bytes("Content-Type", ctype).unwrap());
    let _ = request.respond(response);
}

/// 内置管理页（token 注入）。
fn page(token: &str) -> String {
    CONSOLE_PAGE.replace("__TOKEN__", &format!("{:?}", token))
}

const CONSOLE_PAGE: &str = r##"<!doctype html>
<html lang="zh"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>y-sync 管理台</title>
<style>
:root{
 --bg:#f4f6fb;--card:#ffffff;--text:#0f172a;--muted:#64748b;--border:#e5e9f2;
 --accent:#4c6ef5;--accent-2:#3b82f6;--accent-soft:rgba(76,110,245,.10);
 --ok:#16a34a;--ok-soft:rgba(22,163,74,.11);
 --warn:#d97706;--warn-soft:rgba(217,119,6,.13);
 --err:#dc2626;--err-soft:rgba(220,38,38,.11);
 --shadow:0 1px 2px rgba(15,23,42,.05),0 10px 28px -14px rgba(15,23,42,.16);
 --mono:ui-monospace,"SF Mono",Menlo,Consolas,monospace;
}
@media(prefers-color-scheme:dark){:root{
 --bg:#0c1220;--card:#151d31;--text:#e5eaf3;--muted:#8b98ad;--border:#253150;
 --accent:#7d95ff;--accent-2:#60a5fa;--accent-soft:rgba(125,149,255,.14);
 --ok:#4ade80;--ok-soft:rgba(74,222,128,.14);
 --warn:#fbbf24;--warn-soft:rgba(251,191,36,.14);
 --err:#f87171;--err-soft:rgba(248,113,113,.14);
 --shadow:0 1px 2px rgba(0,0,0,.35),0 14px 36px -18px rgba(0,0,0,.55);
}}
*{box-sizing:border-box}
body{font-family:system-ui,-apple-system,"PingFang SC","Segoe UI",sans-serif;background:var(--bg);color:var(--text);
 margin:0;padding:0 20px 60px;font-size:14px;-webkit-font-smoothing:antialiased}
.wrap{max-width:980px;margin:0 auto}
/* ── 顶栏 ── */
.top{display:flex;align-items:center;gap:14px;padding:26px 2px 6px}
.logo{width:44px;height:44px;flex:none}
.top h1{font-size:19px;margin:0;letter-spacing:-.2px}
.sub{margin:2px 0 0;font-size:12.5px;color:var(--muted)}
.spacer{flex:1}
.conn{display:inline-flex;align-items:center;gap:7px;font-size:12.5px;color:var(--muted);
 background:var(--card);border:1px solid var(--border);border-radius:99px;padding:6px 14px}
.conn .dot{width:8px;height:8px;border-radius:50%;background:var(--muted)}
.conn.ok{color:var(--ok)} .conn.ok .dot{background:var(--ok);box-shadow:0 0 0 3px var(--ok-soft)}
.conn.err{color:var(--err)} .conn.err .dot{background:var(--err);box-shadow:0 0 0 3px var(--err-soft)}
.conn.warn{color:var(--warn)} .conn.warn .dot{background:var(--warn);box-shadow:0 0 0 3px var(--warn-soft)}
/* ── 卡片 ── */
.card{background:var(--card);border:1px solid var(--border);border-radius:16px;box-shadow:var(--shadow);
 padding:18px 22px;margin-top:16px}
.card-head{display:flex;align-items:center;gap:11px;margin-bottom:4px}
.card-head h2{font-size:15px;font-weight:600;margin:0;letter-spacing:-.1px}
.ico{width:30px;height:30px;border-radius:9px;background:var(--accent-soft);color:var(--accent);
 display:inline-flex;align-items:center;justify-content:center;flex:none}
.ico svg{width:16px;height:16px}
.count{font-size:12px;color:var(--muted);background:var(--bg);border:1px solid var(--border);
 padding:1px 9px;border-radius:99px}
.card-note{font-size:12.5px;color:var(--muted);margin:8px 0 0}
/* ── 表格 ── */
.tbl-wrap{overflow-x:auto;margin-top:10px}
table{width:100%;border-collapse:collapse}
th{font-size:11.5px;font-weight:600;color:var(--muted);text-transform:uppercase;letter-spacing:.4px;
 text-align:left;padding:8px 10px;border-bottom:1px solid var(--border)}
td{padding:9px 10px;border-bottom:1px solid var(--border);font-size:13.5px;vertical-align:middle}
tr:last-child td{border-bottom:none}
tbody tr{transition:background .12s} tbody tr:hover{background:var(--accent-soft)}
.num{font-family:var(--mono);font-size:12.5px}
.path{color:var(--muted);font-size:12.5px}
/* ── 控件 ── */
button{font-family:inherit}
.btn{padding:6px 14px;font-size:13px;cursor:pointer;border:1px solid var(--border);border-radius:9px;
 background:var(--card);color:var(--text);transition:all .15s;font-weight:500}
.btn:hover{border-color:var(--accent);color:var(--accent)}
.btn.primary{background:linear-gradient(135deg,#5b5fef,#3b82f6);color:#fff;border:none;
 box-shadow:0 4px 14px -4px rgba(79,110,245,.55)}
.btn.primary:hover{filter:brightness(1.07);color:#fff}
.btn.danger{color:var(--err)} .btn.danger:hover{background:var(--err-soft);border-color:var(--err);color:var(--err)}
.btn.sm{padding:3px 10px;font-size:12px;border-radius:7px}
input[type=text],input[type=password]{padding:7px 11px;border:1px solid var(--border);border-radius:9px;
 background:var(--card);color:var(--text);font-size:13px;transition:all .15s;outline:none;font-family:inherit}
input[type=text]:focus,input[type=password]:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}
label{font-size:13px;color:var(--muted);margin-right:6px}
code{background:var(--bg);border:1px solid var(--border);padding:1px 6px;border-radius:6px;
 font-size:12px;font-family:var(--mono)}
/* ── 徽章/状态 ── */
.badge{display:inline-flex;align-items:center;gap:6px;padding:3px 11px;border-radius:99px;
 font-size:12px;font-weight:500;white-space:nowrap}
.badge::before{content:"";width:6px;height:6px;border-radius:50%;background:currentColor;flex:none}
.badge.ok{color:var(--ok);background:var(--ok-soft)}
.badge.warn{color:var(--warn);background:var(--warn-soft)}
.badge.err{color:var(--err);background:var(--err-soft)}
.badge.muted{color:var(--muted);background:var(--bg)}
/* ── 空态 ── */
.empty{text-align:center;color:var(--muted);font-size:13px;padding:22px 0}
.empty .ok-ico{color:var(--ok);font-size:17px;display:block;margin-bottom:6px}
/* ── 用量 ── */
.stats{display:flex;gap:34px;margin:12px 0 4px}
.stat .num{font-size:21px;font-weight:650;letter-spacing:-.3px}
.stat .lab{font-size:12px;color:var(--muted);margin-top:3px}
.bar{height:8px;background:var(--bg);border:1px solid var(--border);border-radius:99px;overflow:hidden;margin:10px 0 6px}
.bar .fill{height:100%;border-radius:99px;background:linear-gradient(90deg,#5b5fef,#38bdf8);transition:width .4s}
/* ── 列表行 ── */
.row{display:flex;align-items:center;gap:10px;padding:10px 2px;border-bottom:1px solid var(--border);flex-wrap:wrap}
.row:last-child{border-bottom:none}
.row .grow{flex:1;min-width:140px}
.row b{font-size:13.5px}
.meta{font-size:12px;color:var(--muted);font-family:var(--mono)}
/* ── 初始配置 ── */
.hero{margin-top:22px;padding:30px 34px;text-align:left}
.hero h2{font-size:17px;margin:0 0 6px}
.hero p{color:var(--muted);font-size:13.5px;margin:0 0 18px}
.field{margin-bottom:12px}
.field label{display:block;margin-bottom:6px}
.field input{width:min(420px,100%)}
.field-row{display:flex;gap:16px;flex-wrap:wrap}
.field-row .field{flex:1;min-width:180px}
/* ── Toast ── */
#msg{position:fixed;left:50%;bottom:30px;transform:translateX(-50%) translateY(16px);opacity:0;
 background:var(--text);color:var(--bg);padding:11px 20px;border-radius:12px;font-size:13px;
 transition:all .25s;pointer-events:none;max-width:82%;box-shadow:var(--shadow);z-index:99}
#msg.show{opacity:1;transform:translateX(-50%) translateY(0)}
#msg.ok{background:var(--ok);color:#fff} #msg.err{background:var(--err);color:#fff}
h2[id$="-title"]{display:none}
@media(max-width:640px){.stats{gap:20px}.card{padding:14px 16px}}
</style></head><body><div class="wrap">
<header class="top">
 <svg class="logo" viewBox="0 0 48 48" aria-hidden="true">
  <defs><linearGradient id="lg" x1="0" y1="0" x2="1" y2="1">
   <stop offset="0" stop-color="#5b5fef"/><stop offset="1" stop-color="#38bdf8"/></linearGradient></defs>
  <rect width="48" height="48" rx="12" fill="url(#lg)"/>
  <g stroke="#fff" stroke-width="3.4" fill="none" stroke-linecap="round">
   <path d="M 15.2 20.4 A 10 10 0 0 1 31.9 17.9"/>
   <path d="M 32.8 27.6 A 10 10 0 0 1 16.1 30.1"/>
  </g>
  <polygon points="34.9,22.6 33.2,15.9 28.9,19.9" fill="#fff"/>
  <polygon points="13.1,25.4 14.8,32.1 19.1,28.1" fill="#fff"/>
 </svg>
 <div><h1>y-sync 管理台</h1><p class="sub">轻量文件同步 · 本机控制</p></div>
 <span class="spacer"></span>
 <span class="conn" id="conn-pill"><span class="dot"></span><span id="conn-text">连接中…</span></span>
</header>
<div id="msg"></div>

<section class="card hero" id="setup-card" style="display:none">
 <h2 id="setup-title">初始配置</h2>
 <p>首次使用：填写服务端地址与账号，完成配置后即可开始同步。</p>
 <div class="field"><label>服务端地址</label>
  <input type="text" id="s-url" placeholder="https://sync.example.com/y-sync"></div>
 <div class="field-row">
  <div class="field"><label>用户名</label><input type="text" id="s-user" placeholder="alice"></div>
  <div class="field"><label>密码</label><input type="password" id="s-pass" placeholder="••••••••"></div>
 </div>
 <button class="btn primary" onclick="doSetup()">连接并保存</button>
</section>

<main id="main-ui">
<section class="card">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg></span>
  <h2>同步文件夹</h2><span class="count" id="folder-count">0</span>
  <span class="spacer"></span><button class="btn primary sm" onclick="syncAll()">立即全部同步</button></div>
 <div class="tbl-wrap"><table><thead><tr><th>名称</th><th>本地路径</th><th>文件</th><th>游标</th><th>最近同步</th><th>状态</th><th></th></tr></thead>
 <tbody id="rows"></tbody></table></div>
</section>

<div class="grid2" style="display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:16px">
<section class="card" style="margin-top:0">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21.2 15.9A10 10 0 1 1 19 5.3"/><path d="M22 4 12 14.01l-3-3"/></svg></span>
  <h2>我的用量</h2></div>
 <div id="usage"></div>
</section>
<section class="card" style="margin-top:0">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="5" y="2" width="14" height="20" rx="2"/><path d="M12 18h.01"/></svg></span>
  <h2>设备</h2><span class="count" id="devcount">0</span></div>
 <div id="devices"></div>
</section>
</div>

<section class="card">
 <div class="card-head"><span class="ico" style="background:var(--warn-soft);color:var(--warn)"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><path d="M12 9v4"/><path d="M12 17h.01"/></svg></span>
  <h2>待处理冲突</h2><span class="count" id="ccount">0</span></div>
 <div id="conflicts"></div>
 <p class="card-note">「保留当前」= 保留原名文件并删除冲突副本；「采用副本」= 用副本内容覆盖原名文件。结果会同步到所有设备。</p>
</section>

<section class="card">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><path d="m8.6 13.5 6.8 4M15.4 6.5l-6.8 4"/></svg></span>
  <h2>分享</h2><span class="count" id="sharecount">0</span></div>
 <div class="row" style="border-bottom:1px solid var(--border);padding-bottom:14px;margin-bottom:6px">
  <input type="text" id="sh-folder" placeholder="文件夹" style="width:110px">
  <input type="text" id="sh-rel" placeholder="路径，如 docs" style="width:170px">
  <input type="text" id="sh-hours" placeholder="有效期 h（0=永久）" style="width:130px">
  <input type="text" id="sh-pass" placeholder="密码（可空）" style="width:110px">
  <span class="spacer"></span>
  <button class="btn primary sm" onclick="createShare()">创建分享</button>
 </div>
 <div id="shares"></div>
</section>

<section class="card">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg></span>
  <h2>服务端回收站</h2><span class="count" id="trashcount">0</span></div>
 <div id="strash"></div>
</section>

<section class="card">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/></svg></span>
  <h2>文件版本</h2></div>
 <div class="row" style="border-bottom:1px solid var(--border);padding-bottom:14px;margin-bottom:6px">
  <input type="text" id="v-folder" placeholder="文件夹" style="width:130px">
  <input type="text" id="v-rel" placeholder="相对路径，如 src/main.rs" style="width:250px">
  <span class="spacer"></span>
  <button class="btn sm" onclick="loadVersions()">查看版本</button>
 </div>
 <div id="vlist"></div>
</section>

<section class="card">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14"/><path d="m5 12 7 7 7-7"/></svg></span>
  <h2>接入新文件夹</h2></div>
 <div class="field"><label>本地路径</label><input type="text" id="f-path" placeholder="/Users/me/code/my-project" style="width:min(460px,100%)"></div>
 <div class="field-row">
  <div class="field"><label>名称（默认取目录名）</label><input type="text" id="f-name"></div>
  <div class="field"><label>排除（逗号分隔）</label><input type="text" id="f-ex" placeholder="node_modules,dist"></div>
 </div>
 <label style="display:inline-flex;align-items:center;gap:6px;cursor:pointer;margin:2px 0 14px">
  <input type="checkbox" id="f-gi" style="accent-color:var(--accent)"> 沿用 .gitignore</label><br>
 <button class="btn primary" onclick="addFolder()">接入并同步</button>
</section>

<section class="card">
 <div class="card-head"><span class="ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 12h-4l-3 9L9 3l-3 9H2"/></svg></span>
  <h2>最近活动</h2></div>
 <div id="auditbox"></div>
</section>
</main>
</div>

<script>
let TOKEN = new URLSearchParams(location.search).get("token") || __TOKEN__;
// P0-4：从地址栏移除 token（防浏览器历史/引用泄漏），改用 header 携带
if (location.search.includes("token=")) {
  history.replaceState(null, "", location.pathname);
}
const H = () => ({ "Content-Type": "application/json", "X-Ysync-Token": TOKEN });
const F = (p, opt) => fetch(p, Object.assign({}, opt || {}, { headers: { "X-Ysync-Token": TOKEN } }));
let SERVER_BASE = "";

function fmtBytes(n) {
  if (!n && n !== 0) return "-";
  const u = ["B", "KB", "MB", "GB", "TB"]; let i = 0, v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return (i ? v.toFixed(1) : v) + " " + u[i];
}
function fmtTime(sec) {
  return sec > 0 ? new Date(sec * 1000).toLocaleString() : "-";
}
let msgTimer;
function msg(t, type) {
  const el = document.getElementById("msg");
  el.textContent = t; el.className = "show " + (type || "");
  clearTimeout(msgTimer);
  msgTimer = setTimeout(() => { el.className = ""; }, 4000);
}
function setConn(cls, text) {
  const el = document.getElementById("conn-pill");
  el.className = "conn " + cls;
  document.getElementById("conn-text").textContent = text;
}
function emptyBox(ok, text) {
  return '<div class="empty">' + (ok ? '<span class="ok-ico">✓</span>' : "") + text + "</div>";
}
async function copyText(t) {
  try { await navigator.clipboard.writeText(t); msg("已复制到剪贴板", "ok"); }
  catch (e) {
    const ta = document.createElement("textarea"); ta.value = t; document.body.appendChild(ta);
    ta.select(); document.execCommand("copy"); ta.remove(); msg("已复制到剪贴板", "ok");
  }
}

async function doSetup() {
  const body = {
    server_url: document.getElementById("s-url").value.trim(),
    user: document.getElementById("s-user").value.trim(),
    password: document.getElementById("s-pass").value
  };
  if (!body.server_url || !body.user) { msg("请填写服务端地址与用户名", "err"); return; }
  const r = await F("/setup", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body)});
  if (!r.ok) { msg("配置失败: " + await r.text(), "err"); return; }
  msg("配置成功，开始同步", "ok");
  refresh();
}
async function refresh() {
  let initd = true;
  try {
    const st = await (await F("/setup-status")).json();
    initd = st.initialized;
    setConn(initd ? "ok" : "warn", initd ? "已连接" : "待配置");
  } catch (e) { setConn("err", "daemon 通信失败"); }
  document.getElementById("setup-card").style.display = initd ? "none" : "";
  document.getElementById("main-ui").style.display = initd ? "" : "none";
  if (!initd) return;
  loadInfo();
  try {
    const s = await (await F("/status")).json();
    const rows = document.getElementById("rows");
    rows.innerHTML = "";
    document.getElementById("folder-count").textContent = s.folders.length;
    if (!s.folders.length) { rows.innerHTML = '<tr><td colspan="7">' + emptyBox(false, "还没有接入文件夹，可在下方「接入新文件夹」添加") + "</td></tr>"; }
    for (const f of [...s.folders].sort((a,b)=>a.name.localeCompare(b.name))) {
      let state = "正常", cls = "ok";
      if (f.paused) { state = "已暂停"; cls = "muted"; }
      else if (f.last_error) { state = "错误: " + f.last_error; cls = "err"; }
      else if (f.conflicts_total > 0) { state = f.conflicts_total + " 个冲突待处理"; cls = "warn"; }
      const last = f.last_sync ? new Date(f.last_sync).toLocaleTimeString() : "-";
      const stats = f.last_stats ? '<div class="meta">' + esc(f.last_stats) + "</div>" : "";
      rows.insertAdjacentHTML("beforeend",
        "<tr><td><b>" + esc(f.name) + "</b></td><td>" + esc(f.local_path) + '</td><td class="num">' + f.files +
        '</td><td class="num">' + f.cursor + "</td><td>" + last + '</td><td><span class="badge ' + cls + '">' + esc(state) + "</span>" + stats + "</td>" +
        "<td>" + (f.paused
          ? btn("resume", f.name, "恢复")
          : btn("pause", f.name, "暂停")) +
        ' <button class="btn danger sm" onclick=\'removeFolder("' + esc(f.name) + '")\'>移除</button></td></tr>');
    }
  } catch (e) { if (String(e).indexOf("401") >= 0) msg("token 无效，请通过 ysyncd ui 重新打开", "err"); }
  try {
    const c = await (await F("/conflicts")).json();
    const box = document.getElementById("conflicts");
    const list = c.conflicts || [];
    document.getElementById("ccount").textContent = list.length;
    box.innerHTML = list.length ? "" : emptyBox(true, "没有待处理的冲突");
    for (const it of list) {
      box.insertAdjacentHTML("beforeend", '<div class="row"><span class="grow"><b>' + esc(it.folder) + "</b> / " + esc(it.rel) +
        '<div class="meta">副本: ' + esc(it.copy_rel) + " · " + fmtBytes(it.size) + "</div></span>" +
        ' <button class="btn sm" onclick=\'resolve("' + esc(it.folder) + '","' + esc(it.rel) + '","' + esc(it.copy_rel) + '","local")\'>保留当前</button>' +
        ' <button class="btn sm" onclick=\'resolve("' + esc(it.folder) + '","' + esc(it.rel) + '","' + esc(it.copy_rel) + '","copy")\'>采用副本</button></div>');
    }
  } catch (e) {}
  try {
    const d = await (await F("/devices")).json();
    const box = document.getElementById("devices");
    const list = d.devices || [];
    document.getElementById("devcount").textContent = list.length;
    box.innerHTML = list.length ? "" : emptyBox(false, "暂无其他设备");
    for (const it of list) {
      box.insertAdjacentHTML("beforeend",
        '<div class="row"><span class="grow"><b>' + esc(it.name) + "</b>" +
        (it.current ? ' <span class="badge ok">当前设备</span>' : "") +
        '<div class="meta">最近活跃 ' + new Date(it.last_seen * 1000).toLocaleString() + "</div></span>" +
        ' <button class="btn danger sm" onclick=\'revoke("' + it.id + '")\'>吊销</button></div>');
    }
  } catch (e) {}
  try {
    const u = await (await F("/usage")).json();
    const used = u.used_bytes || 0, quota = u.quota_bytes || 0;
    const pct = quota > 0 ? Math.min(100, used / quota * 100) : 0;
    document.getElementById("usage").innerHTML =
      '<div class="stats"><div class="stat"><div class="num">' + fmtBytes(used) + '</div><div class="lab">已用空间</div></div>' +
      '<div class="stat"><div class="num">' + (quota > 0 ? fmtBytes(quota) : "不限") + '</div><div class="lab">配额</div></div></div>' +
      (quota > 0 ? '<div class="bar"><div class="fill" style="width:' + pct + '%"></div></div><div class="meta">已用 ' + pct.toFixed(1) + "%</div>" : "");
  } catch (e) {}
  try {
    const t = await (await F("/server-trash")).json();
    const box = document.getElementById("strash");
    const list = t.items || [];
    document.getElementById("trashcount").textContent = list.length;
    box.innerHTML = list.length ? "" : emptyBox(true, "回收站是空的");
    for (const it of list) {
      box.insertAdjacentHTML("beforeend", '<div class="row"><span class="grow"><b>' + esc(it.orig_path || it.name) + "</b>" +
        '<div class="meta">' + fmtBytes(it.size) + " · 删除于 " + fmtTime(it.deleted_at) + "</div></span>" +
        ' <button class="btn sm" onclick=\'trashOp("restore",' + it.id + ')\'>恢复</button>' +
        ' <button class="btn danger sm" onclick=\'trashOp("delete",' + it.id + ')\'>彻底删除</button></div>');
    }
  } catch (e) {}
  try {
    const sh = await (await F("/server-shares")).json();
    const box = document.getElementById("shares");
    const list = sh.shares || [];
    document.getElementById("sharecount").textContent = list.length;
    if (list.length) box.innerHTML = "";
    for (const it of list) {
      const link = SERVER_BASE ? SERVER_BASE + "/s/" + esc(it.token) : esc(it.token);
      box.insertAdjacentHTML("beforeend", '<div class="row"><span class="grow"><b>' + esc(it.path) + "</b>" +
        (it.has_password ? ' <span class="badge warn">密码保护</span>' : "") +
        '<div class="meta">有效期至 ' + (it.expires_at > 0 ? fmtTime(it.expires_at) : "永久") + "</div></span>" +
        " <code>" + link + "</code>" +
        ' <button class="btn sm" onclick=\'copyText("' + link + '")\'>复制</button>' +
        ' <button class="btn danger sm" onclick=\'unshare("' + esc(it.token) + '")\'>撤销</button></div>');
    }
  } catch (e) {}
  try {
    const a = await (await F("/audit")).json();
    const box = document.getElementById("auditbox");
    const list = a.entries || [];
    box.innerHTML = list.length ? "" : emptyBox(false, "暂无活动记录");
    for (const it of list.slice(0, 30)) {
      box.insertAdjacentHTML("beforeend", '<div class="row"><span class="badge muted">' + esc(it.event || "") + "</span>" +
        '<span class="grow meta">' + esc(it.detail || "") + "</span>" +
        '<span class="meta">' + (it.ts ? new Date(it.ts * 1000).toLocaleString() : "") + "</span></div>");
    }
  } catch (e) {}
}
async function loadInfo() {
  try { const i = await (await F("/server-info")).json(); SERVER_BASE = (i.server_url || "").replace(/\/+$/, ""); } catch (e) {}
}
async function trashOp(op, id) {
  const r = await F("/trash-" + op, {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({id})});
  if (!r.ok) msg("失败: " + await r.text(), "err"); else msg(op === "restore" ? "已恢复，同步中" : "已彻底删除", "ok");
  refresh();
}
async function createShare() {
  const folder = document.getElementById("sh-folder").value.trim();
  const rel = document.getElementById("sh-rel").value.trim();
  const hours = parseInt(document.getElementById("sh-hours").value || "0");
  const password = document.getElementById("sh-pass").value;
  if (!folder || !rel) { msg("请填写文件夹与路径", "err"); return; }
  const r = await F("/share-create", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, hours, password})});
  if (!r.ok) { msg("创建失败: " + await r.text(), "err"); return; }
  msg("分享已创建", "ok");
  refresh();
}
async function unshare(token) {
  const r = await F("/share-delete", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({token})});
  if (!r.ok) msg("撤销失败: " + await r.text(), "err"); else msg("分享已撤销", "ok");
  refresh();
}
async function revoke(id) {
  if (!confirm("吊销设备 #" + id + "？（其 token 立即失效）")) return;
  const r = await F("/devices/" + id, {method: "DELETE"});
  if (!r.ok) msg("失败: " + await r.text(), "err");
  refresh();
}
function btn(path, folder, label) {
  return '<button class="btn sm" onclick=\'op("' + path + '","' + esc(folder) + '")\'>' + label + "</button>";
}
async function op(path, folder) {
  const r = await F(path, {method:"POST", headers:H(), body: JSON.stringify({folder})});
  if (!r.ok) msg("操作失败: " + await r.text(), "err"); else msg("已执行 " + path + (folder ? " " + folder : ""), "ok");
  refresh();
}
async function syncAll() { await op("/sync", ""); }
async function removeFolder(name) {
  if (!confirm("解除跟踪 " + name + "？（本地文件与服务端副本都保留）")) return;
  const r = await F("/remove", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({name})});
  if (!r.ok) msg("失败: " + await r.text(), "err");
  refresh();
}
async function addFolder() {
  const body = {
    local_path: document.getElementById("f-path").value.trim(),
    name: document.getElementById("f-name").value.trim(),
    use_gitignore: document.getElementById("f-gi").checked,
    excludes: document.getElementById("f-ex").value.split(",").map(s=>s.trim()).filter(Boolean)
  };
  if (!body.local_path) { msg("请填写本地路径", "err"); return; }
  const r = await F("/add", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body)});
  if (!r.ok) msg("接入失败: " + await r.text(), "err");
  else { msg("已接入 " + body.local_path, "ok"); document.getElementById("f-path").value=""; document.getElementById("f-name").value=""; }
  refresh();
}
async function resolve(folder, rel, copyRel, choice) {
  const r = await F("/resolve", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, copy_rel: copyRel, choice})});
  if (!r.ok) msg("失败: " + await r.text(), "err"); else msg("冲突已处理，同步传播中", "ok");
  refresh();
}
async function loadVersions() {
  const folder = document.getElementById("v-folder").value.trim();
  const rel = document.getElementById("v-rel").value.trim();
  if (!folder || !rel) { msg("请填写文件夹与路径", "err"); return; }
  try {
    const v = await (await F("/versions?folder=" + encodeURIComponent(folder) + "&rel=" + encodeURIComponent(rel))).json();
    const box = document.getElementById("vlist");
    const list = v.versions || [];
    box.innerHTML = list.length ? "" : emptyBox(false, "无历史版本");
    for (const it of list) {
      box.insertAdjacentHTML("beforeend", '<div class="row"><span class="badge muted">#' + it.id + "</span>" +
        '<span class="grow"><b>' + fmtBytes(it.size) + '</b><div class="meta">' + fmtTime(it.mtime) + "</div></span>" +
        ' <button class="btn sm" onclick=\'restoreVersion("' + esc(folder) + '","' + esc(rel) + '",' + it.id + ')\'>回写此版本</button></div>');
    }
  } catch (e) { msg("版本查询失败: " + e, "err"); }
}
async function restoreVersion(folder, rel, vid) {
  const r = await F("/version-restore", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, version_id: vid})});
  if (!r.ok) msg("回写失败: " + await r.text(), "err"); else msg("版本已回写本地，同步后上传", "ok");
}
function esc(s) { return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/"/g,"&quot;"); }
refresh();
setInterval(refresh, 3000);
</script></body></html>"##;


/// 从 JSON body 取数值字段（兼容数字/数字字符串）。
fn json_num(field: &str, body: &str) -> i64 {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| match v.get(field) {
            Some(serde_json::Value::Number(n)) => n.as_i64(),
            Some(serde_json::Value::String(s)) => s.parse().ok(),
            _ => None,
        })
        .unwrap_or(-1)
}
