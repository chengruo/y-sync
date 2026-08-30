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

<h2 id="setup-title" style="display:none">初始配置</h2>
<div class="card" id="setup-card" style="display:none">
 <div><label>服务端地址</label><input type="text" id="s-url" placeholder="https://ai-account.site/y-sync" style="width:60%"></div>
 <div style="margin-top:6px"><label>用户名</label><input type="text" id="s-user" style="width:30%">
 <label>密码</label><input type="password" id="s-pass" style="width:30%"></div>
 <div style="margin-top:8px"><button onclick="doSetup()">连接并保存</button></div>
</div>

<h2>同步文件夹</h2>
<table><thead><tr><th>名称</th><th>本地路径</th><th>文件</th><th>游标</th><th>最近同步</th><th>状态</th><th>操作</th></tr></thead>
<tbody id="rows"></tbody></table>
<p><button onclick="syncAll()">立即全部同步</button></p>

<h2>待处理冲突 <span id="ccount" style="font-weight:normal;color:#57606a"></span></h2>
<div id="conflicts"></div>

<h2>我的用量</h2>
<div id="usage" style="font-size:13px;color:#57606a"></div>

<h2>分享 <span id="sharecount" style="font-weight:normal;color:#57606a"></span></h2>
<div class="card">
 <label>文件夹</label><input type="text" id="sh-folder" placeholder="proj" style="width:18%">
 <label>路径</label><input type="text" id="sh-rel" placeholder="docs" style="width:28%">
 <label>有效期(h)</label><input type="text" id="sh-hours" placeholder="0=永久" style="width:12%">
 <label>密码</label><input type="text" id="sh-pass" style="width:14%">
 <button onclick="createShare()">创建分享</button>
</div>
<div id="shares"></div>

<h2>服务端回收站</h2>
<div id="strash"></div>

<h2>文件版本</h2>
<div class="card">
 <label>文件夹</label><input type="text" id="v-folder" placeholder="proj" style="width:20%">
 <label>相对路径</label><input type="text" id="v-rel" placeholder="src/main.rs" style="width:45%">
 <button onclick="loadVersions()">查看版本</button>
</div>
<div id="vlist"></div>

<h2>设备 <span id="devcount" style="font-weight:normal;color:#57606a"></span></h2>
<div id="devices"></div>

<h2>最近活动（审计）</h2>
<div id="auditbox"></div>

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
let TOKEN = new URLSearchParams(location.search).get("token") || __TOKEN__;
// P0-4：从地址栏移除 token（防浏览器历史/引用泄漏），改用 header 携带
if (location.search.includes("token=")) {
  history.replaceState(null, "", location.pathname);
}
const api = (p) => p;
const H = () => ({ "Content-Type": "application/json", "X-Ysync-Token": TOKEN });
const F = (p, opt) => fetch(p, Object.assign({}, opt || {}, { headers: { "X-Ysync-Token": TOKEN } }));

async function doSetup() {
  const body = {
    server_url: document.getElementById("s-url").value.trim(),
    user: document.getElementById("s-user").value.trim(),
    password: document.getElementById("s-pass").value
  };
  if (!body.server_url || !body.user) { msg("请填写服务端地址与用户名"); return; }
  const r = await F("/setup", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body)});
  if (!r.ok) { msg("配置失败: " + await r.text()); return; }
  msg("配置成功，开始同步");
  refresh();
}
async function refresh() {
  try {
    const st = await (await F("/setup-status")).json();
    const initd = st.initialized;
    document.getElementById("setup-title").style.display = initd ? "none" : "block";
    document.getElementById("setup-card").style.display = initd ? "none" : "block";
    document.getElementById("conflicts").parentElement.querySelectorAll("h2")[1].style.display = initd ? "" : "none";
  } catch (e) {}
  try {
    const s = await (await F("/status")).json();
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
    const c = await (await F("/conflicts")).json();
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
  try {
    const d = await (await F("/devices")).json();
    const box = document.getElementById("devices");
    const list = d.devices || [];
    document.getElementById("devcount").textContent = "(" + list.length + ")";
    box.innerHTML = "";
    for (const it of list) {
      const cur = it.current ? ' <span style="color:#1a7f37">← 当前设备</span>' : "";
      box.insertAdjacentHTML("beforeend",
        '<div class="card"><b>' + esc(it.name) + "</b>" + cur +
        ' <code>最近活跃 ' + new Date(it.last_seen * 1000).toLocaleString() + "</code>" +
        ' <button class="danger" onclick=\'revoke("' + it.id + '")\'>吊销</button></div>');
    }
  } catch (e) {}
}
async function trashOp(op, id) {
  const r = await F("/trash-" + op, {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({id})});
  if (!r.ok) msg("失败: " + await r.text()); else msg("回收站操作完成");
  refresh();
}
async function createShare() {
  const folder = document.getElementById("sh-folder").value.trim();
  const rel = document.getElementById("sh-rel").value.trim();
  const hours = parseInt(document.getElementById("sh-hours").value || "0");
  const password = document.getElementById("sh-pass").value;
  if (!folder || !rel) { msg("请填写文件夹与路径"); return; }
  const r = await F("/share-create", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, hours, password})});
  if (!r.ok) { msg("创建失败: " + await r.text()); return; }
  const info = await r.json();
  msg("分享已创建: " + info.token + (password ? "（密码 " + password + "）" : ""));
  refresh();
}
async function unshare(token) {
  const r = await F("/share-delete", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({token})});
  if (!r.ok) msg("撤销失败: " + await r.text()); else msg("分享已撤销");
  refresh();
}
async function revoke(id) {
  if (!confirm("吊销设备 #" + id + "？（其 token 立即失效）")) return;
  const r = await F("/devices/" + id, {method: "DELETE"});
  if (!r.ok) msg("失败: " + await r.text());
  refresh();
}
function btn(path, folder, label) {
  return '<button onclick=\'op("' + path + '","' + esc(folder) + '")\'>' + label + "</button>";
}
async function op(path, folder) {
  const r = await F(path, {method:"POST", headers:H(), body: JSON.stringify({folder})});
  if (!r.ok) msg("操作失败: " + await r.text()); else msg("已执行 " + path + (folder ? " " + folder : ""));
  refresh();
}
async function syncAll() { await op("/sync", ""); }
async function removeFolder(name) {
  if (!confirm("解除跟踪 " + name + "？（本地文件与服务端副本都保留）")) return;
  const r = await F("/remove", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({name})});
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
  const r = await F("/add", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body)});
  if (!r.ok) msg("接入失败: " + await r.text());
  else { msg("已接入 " + body.local_path); document.getElementById("f-path").value=""; document.getElementById("f-name").value=""; }
  refresh();
}
async function resolve(folder, rel, copyRel, choice) {
  const r = await F("/resolve", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, copy_rel: copyRel, choice})});
  if (!r.ok) msg("失败: " + await r.text()); else msg("冲突已处理，同步传播中");
  refresh();
}
async function loadVersions() {
  const folder = document.getElementById("v-folder").value.trim();
  const rel = document.getElementById("v-rel").value.trim();
  if (!folder || !rel) { msg("请填写文件夹与路径"); return; }
  try {
    const v = await (await F("/versions?folder=" + encodeURIComponent(folder) + "&rel=" + encodeURIComponent(rel))).json();
    const box = document.getElementById("vlist");
    box.innerHTML = (v.versions||[]).length ? "" : '<div style="font-size:13px">无历史版本</div>';
    for (const it of v.versions||[]) {
      box.insertAdjacentHTML("beforeend",
        '<div class="card">#' + it.id + " " + (it.size/1024).toFixed(1) + " KB · " +
        new Date(it.created * 1000).toLocaleString() +
        ' <button onclick=\'restoreVersion("' + esc(folder) + '","' + esc(rel) + '",' + it.id + ')\'>回写此版本</button></div>');
    }
  } catch (e) { msg("版本查询失败: " + e); }
}
async function restoreVersion(folder, rel, vid) {
  const r = await F("/version-restore", {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify({folder, rel, version_id: vid})});
  if (!r.ok) msg("回写失败: " + await r.text()); else msg("版本已回写本地，同步后上传");
}
function esc(s) { return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/"/g,"&quot;"); }
function msg(t) { document.getElementById("msg").textContent = t; }
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
