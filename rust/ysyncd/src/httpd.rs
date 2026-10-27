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
        let (server_url, user, device_name) =
            crate::ctx::with_cfg(|c| (c.server_url.clone(), c.user.clone(), c.device_name.clone()));
        return respond(
            request,
            200,
            &serde_json::to_string(&serde_json::json!({
                "server_url": server_url,
                "user": user,
                "device_name": device_name
            }))
            .unwrap(),
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
        (Method::Get, "/remote-folders") => {
            match daemon.remote_folders() {
                Ok(v) => respond(request, 200, &v.to_string(), "application/json"),
                Err(e) => respond(request, 502, &e, "text/plain"),
            }
        }
        (Method::Post, "/remote-folders/subscribe") => {
            let root_id = json_num("root_node_id", &body);
            let local_path = json_field_str("local_path");
            match daemon.subscribe_remote(root_id, &local_path) {
                Ok(()) => {
                    let d = daemon.clone();
                    std::thread::spawn(move || d.sync_all());
                    respond(request, 200, "{\"ok\":true}", "application/json")
                }
                Err(e) => respond(request, 400, &e, "text/plain"),
            }
        }
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
        (Method::Post, "/reveal") => {
            let p = json_field_str("path");
            if p.is_empty() {
                return respond(request, 400, "missing path", "text/plain");
            }
            std::thread::spawn(move || {
                reveal_path(&p);
            });
            respond(request, 200, "{\"ok\":true}", "application/json");
        }
        (Method::Post, "/choose-folder") => {
            match choose_folder_dialog() {
                Some(p) => {
                    let res = serde_json::json!({ "ok": true, "path": p }).to_string();
                    respond(request, 200, &res, "application/json");
                }
                None => {
                    respond(request, 200, "{\"ok\":false,\"cancelled\":true}", "application/json");
                }
            }
        }
        (Method::Get, "/update/status") => {
            respond(request, 200, &crate::update::service().status().to_string(), "application/json");
        }
        (Method::Post, "/update/check") | (Method::Post, "/update/apply") => {
            let updater = crate::update::service();
            let result = if path == "/update/check" { updater.check(true) } else { updater.apply(daemon.clone()) };
            match result {
                Ok(()) => respond(request, 202, "{\"ok\":true}", "application/json"),
                Err(e) => respond(request, 409, &serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(), "application/json"),
            }
        }
        (Method::Post, "/update/preferences") => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let auto_check = v.get("auto_check").and_then(|v| v.as_bool());
            let ignored = if v.get("skip_version").and_then(|v| v.as_bool()) == Some(true) {
                Some(ysync_core::updater::load_update_state().latest_version)
            } else { None };
            match ysync_core::updater::set_preferences(auto_check, ignored) {
                Ok(()) => respond(request, 200, "{\"ok\":true}", "application/json"),
                Err(e) => respond(request, 500, &serde_json::json!({ "error": e.to_string() }).to_string(), "application/json"),
            }
        }
        _ => respond(request, 404, "not found", "text/plain"),
    }
}

fn reveal_path(path: &str) {
    #[cfg(target_os = "macos")]
    {
        let p = std::path::Path::new(path);
        let mut cmd = std::process::Command::new("open");
        if p.is_file() {
            cmd.arg("-R").arg(path);
        } else {
            cmd.arg(path);
        }
        let _ = cmd.spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let p = std::path::Path::new(path);
        if p.is_file() {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,\"{}\"", path))
                .spawn();
        } else {
            let _ = std::process::Command::new("explorer").arg(path).spawn();
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn choose_folder_dialog() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("osascript")
            .arg("-e")
            .arg("POSIX path of (choose folder with prompt \"选择要同步的文件夹\")")
            .output()
            .ok()?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s.trim_end_matches('/').to_string());
            }
        }
    }
    None
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

const CONSOLE_PAGE: &str = include_str!("console.html");



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
