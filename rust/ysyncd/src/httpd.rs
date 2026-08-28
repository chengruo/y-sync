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
    for h in request.headers() {
        if h.field.as_str().as_str().eq_ignore_ascii_case("authorization") {
            bearer = h.value.as_str().trim_start_matches("Bearer ").to_string();
        }
    }
    let authed = qtoken == token || bearer == token;
    if path == "/healthz" {
        return respond(request, 200, "{\"status\":\"ok\"}", "application/json");
    }
    if method == Method::Get && (path == "/" || path.is_empty()) {
        let html = page(&token);
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

/// 内置管理页（与 Go 版一致；token 注入）。
fn page(token: &str) -> String {
    GO_PAGE.replace("__TOKEN__", &format!("{:?}", token))
}

const GO_PAGE: &str = r##"<!doctype html>
<html lang="zh"><head><meta charset="utf-8"><title>y-sync 管理台</title>
<style>
 body{font-family:-apple-system,"PingFang SC",sans-serif;max-width:860px;margin:36px auto;padding:0 16px;color:#1f2328}
 h1{font-size:20px} h2{font-size:16px;margin-top:28px}
 table{width:100%;border-collapse:collapse;margin-top:8px}
 th,td{padding:7px 10px;border-bottom:1px solid #eaeef2;text-align:left;font-size:13.5px}
 .ok{color:#1a7f37}.warn{color:#9a6700}.err{color:#cf222e}
 button{padding:3px 10px;font-size:12.5px;cursor:pointer;border:1px solid #d0d7de;border-radius:6px;background:#f6f8fa}
 button.danger{color:#cf222e}
 .card{border:1px solid #eaeef2;border-radius:8px;padding:12px 14px;margin-top:10px}
 input[type=text]{padding:5px 8px;border:1px solid #d0d7de;border-radius:6px;font-size:13px}
 label{font-size:13px;margin-right:8px}
 #msg{font-size:13px;margin:8px 0;color:#57606a;min-height:1.2em}
 code{background:#f6f8fa;padding:1px 5px;border-radius:4px;font-size:12px}
</style></head><body>
<h1>y-sync 管理台</h1><div id="msg"></div>

<h2>同步文件夹</h2>
<table><thead><tr><th>名称</th><th>本地路径</th><th>文件</th><th>游标</th><th>最近同步</th><th>状态</th><th>操作</th></tr></thead>
<tbody id="rows"></tbody></table>
<p><button onclick="syncAll()">立即全部同步</button></p>

<h2>待处理冲突 <span id="ccount" style="font-weight:normal;color:#57606a"></span></h2>
<div id="conflicts"></div>

<h2>接入新文件夹</h2>
<div class="card">
 <div><label>本地路径</label><input type="text" id="f-path" placeholder="/Users/me/code/my-project" style="width:70%"></div>
 <div style="margin-top:6px"><label>名称</label><input type="text" id="f-name" placeholder="默认取目录名">
 <label><input type="checkbox" id="f-gi"> 沿用 .gitignore</label>
 <label>排除 <input type="text" id="f-ex" placeholder="node_modules,dist" style="width:30%"></label></div>
 <div style="margin-top:8px"><button onclick="addFolder()">接入并同步</button></div>
</div>
<p style="font-size:12px;color:#8b949e">冲突处理说明：「保留当前」= 保留原名文件并删除冲突副本；「采用副本」= 用副本内容覆盖原名文件。结果会同步到所有设备。</p>
<script>
const TOKEN = __TOKEN__;
const api = (p) => p + (p.includes("?") ? "&" : "?") + "token=" + TOKEN;

async function refresh() {
  try {
    const s = await (await fetch(api("/status"))).json();
    const rows = document.getElementById("rows");
    rows.innerHTML = "";
    for (const f of [...s.folders].sort((a,b)=>a.name.localeCompare(b.name))) {
      let state = "空闲", cls = "ok";
      if (f.paused) { state = "已暂停"; cls = "warn"; }
      else if (f.last_error) { state = "错误: " + f.last_error; cls = "err"; }
      else if (f.conflicts_total > 0) { state = "有 " + f.conflicts_total + " 个冲突待处理"; cls = "warn"; }
      else if (f.last_sync) { state = "正常 · " + (f.last_stats || ""); }
      const last = f.last_sync ? new Date(f.last_sync).toLocaleTimeString() : "-";
      rows.insertAdjacentHTML("beforeend",
        "<tr><td><b>" + esc(f.name) + "</b></td><td>" + esc(f.local_path) + "</td><td>" + f.files +
        "</td><td>" + f.cursor + "</td><td>" + last + '</td><td class="' + cls + '">' + esc(state) + "</td>" +
        "<td>" + (f.paused
          ? btn("resume", f.name, "恢复")
          : btn("pause", f.name, "暂停")) +
        ' <button class="danger" onclick=\'removeFolder("' + esc(f.name) + '")\'>移除</button></td></tr>');
    }
  } catch (e) { if (String(e).indexOf("401") >= 0) msg("token 无效，请通过 ysync ui 重新打开"); }
  try {
    const c = await (await fetch(api("/conflicts"))).json();
    const box = document.getElementById("conflicts");
    const list = c.conflicts || [];
    document.getElementById("ccount").textContent = "(" + list.length + ")";
    box.innerHTML = list.length ? "" : '<div style="color:#1a7f37;font-size:13px">没有待处理的冲突 ✓</div>';
    for (const it of list) {
      box.insertAdjacentHTML("beforeend", '<div class="card"><b>' + esc(it.folder) + "</b> / " + esc(it.rel) +
        " <code>副本: " + esc(it.copy_rel) + "</code> (" + (it.size/1024).toFixed(1) + " KB)" +
        ' <button onclick=\'resolve("' + esc(it.folder) + '","' + esc(it.rel) + '","' + esc(it.copy_rel) + '","local")\'>保留当前</button>' +
        ' <button onclick=\'resolve("' + esc(it.folder) + '","' + esc(it.rel) + '","' + esc(it.copy_rel) + '","copy")\'>采用副本</button></div>');
    }
  } catch (e) {}
}
function btn(path, folder, label) {
  return '<button onclick=\'op("' + path + '","' + esc(folder) + '")\'>' + label + "</button>";
}
async function op(path, folder) {
  const r = await fetch(api(path), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder})});
  if (!r.ok) msg("操作失败: " + await r.text()); else msg("已执行 " + path + (folder ? " " + folder : ""));
  refresh();
}
async function syncAll() { await op("/sync", ""); }
async function removeFolder(name) {
  if (!confirm("解除跟踪 " + name + "？（本地文件与服务端副本都保留）")) return;
  const r = await fetch(api("/remove"), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({name})});
  if (!r.ok) msg("失败: " + await r.text());
  refresh();
}
async function addFolder() {
  const body = {
    local_path: document.getElementById("f-path").value.trim(),
    name: document.getElementById("f-name").value.trim(),
    use_gitignore: document.getElementById("f-gi").checked,
    excludes: document.getElementById("f-ex").value.split(",").map(s=>s.trim()).filter(Boolean)
  };
  if (!body.local_path) { msg("请填写本地路径"); return; }
  const r = await fetch(api("/add"), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body)});
  if (!r.ok) msg("接入失败: " + await r.text());
  else { msg("已接入 " + body.local_path); document.getElementById("f-path").value=""; document.getElementById("f-name").value=""; }
  refresh();
}
async function resolve(folder, rel, copyRel, choice) {
  const r = await fetch(api("/resolve"), {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, copy_rel: copyRel, choice})});
  if (!r.ok) msg("失败: " + await r.text()); else msg("冲突已处理，同步传播中");
  refresh();
}
function esc(s) { return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/"/g,"&quot;"); }
function msg(t) { document.getElementById("msg").textContent = t; }
refresh();
setInterval(refresh, 3000);
</script></body></html>"##;
