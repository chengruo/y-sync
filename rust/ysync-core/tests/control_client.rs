//! ControlClient 集成测试：拉起真实 Go 服务端与 Go daemon，验证 Rust 控制客户端
//! 与 Go 控制服务端的互通（协议一致性的一部分）。
//! 需要环境变量：YSYNC_E2E_SERVER_BIN / YSYNC_E2E_CLIENT_BIN（Go 二进制路径），否则跳过。

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ysync_core::control::ControlClient;
use ysync_core::protocol::LoginReq;

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_and_wait(bin: &str, port: u16, data_dir: &Path) -> Option<KillOnDrop> {
    let child = Command::new(bin)
        .args([
            "serve",
            "-addr",
            &format!("127.0.0.1:{port}"),
            "-data",
            data_dir.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
        .map(KillOnDrop)?;
    let mut ok = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if ok {
        Some(child)
    } else {
        None
    }
}

#[test]
fn control_client_against_go_daemon() {
    let server_bin = match std::env::var("YSYNC_E2E_SERVER_BIN") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skip: YSYNC_E2E_SERVER_BIN 未设置");
            return;
        }
    };
    let client_bin = match std::env::var("YSYNC_E2E_CLIENT_BIN") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skip: YSYNC_E2E_CLIENT_BIN 未设置");
            return;
        }
    };

    let dir = std::env::temp_dir().join(format!("ysync-core-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = dir.join("data");
    let cfg_a = dir.join("cfgA");
    std::fs::create_dir_all(&cfg_a).unwrap();
    let port = free_port();
    let server = spawn_and_wait(&server_bin, port, &data).expect("server 启动");
    let _server_keep = server;

    // adduser
    let st = Command::new(&server_bin)
        .env("YSYNC_DATA", &data)
        .arg("adduser")
        .arg("alice")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut st = st;
    st.stdin
        .as_mut()
        .unwrap()
        .write_all(b"secret\n")
        .unwrap();
    st.wait().unwrap();

    // client init + add + sync
    let base = format!("http://127.0.0.1:{port}");
    let run_client = |args: &[&str], envs: Vec<(&str, String)>| {
        let mut child = Command::new(&client_bin)
            .args(args)
            .envs(envs)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(b"secret\n");
        }
        let out = child.wait_with_output().unwrap();
        if !out.status.success() {
            eprintln!("client {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
        }
        out.status.success()
    };
    let e = vec![
        ("YSYNC_CONFIG_DIR", cfg_a.to_string_lossy().to_string()),
        ("YSYNC_DATA", data.to_string_lossy().to_string()),
    ];
    assert!(run_client(
        &["init", "-server", &base, "-user", "alice", "-device", "devA"],
        e.clone()
    ));
    let folder = dir.join("proj");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a.txt"), b"hello").unwrap();
    assert!(run_client(
        &["add", folder.to_str().unwrap(), "--as", "proj"],
        e.clone()
    ));
    assert!(run_client(&["sync"], e.clone()));

    // 启动 Go daemon（控制 API）
    let dport = free_port();
    let daddr = format!("127.0.0.1:{dport}");
    let daemon = Command::new(&client_bin)
        .env("YSYNC_CONFIG_DIR", cfg_a.to_string_lossy().to_string())
        .args(["daemon", "-http", &daddr, "-interval", "60s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(KillOnDrop)
        .expect("daemon 启动");
    let _daemon = daemon;

    let info_path = cfg_a.join("daemon.json");
    let mut info = None;
    for _ in 0..100 {
        if let Ok(b) = std::fs::read(&info_path) {
            info = Some(serde_json::from_slice::<ysync_core::config::DaemonInfo>(&b).unwrap());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let info = info.expect("daemon.json 出现");

    // Rust 控制客户端 ↔ Go daemon 控制服务
    let cc = ControlClient::new(&info.addr, &info.token);
    eprintln!("daemon addr={} token={}", info.addr, info.token);
    let raw = reqwest::blocking::Client::new()
        .get(format!("http://{}/status?token={}", info.addr, info.token))
        .send()
        .unwrap()
        .text()
        .unwrap();
    eprintln!("raw status body: {raw}");
    let folders = cc.status().unwrap_or_else(|e| panic!("status 失败: {e:?}"));
    assert!(folders.iter().any(|f| f.name == "proj"));

    // 无 token 应被拒
    let anon = ControlClient::new(&info.addr, "bad-token");
    assert!(anon.status().is_err(), "错误 token 应 401");

    // conflicts（无冲突为空）
    assert!(cc.conflicts().unwrap().is_empty());

    // add folder via API
    let folder2 = dir.join("uiadd");
    std::fs::create_dir_all(&folder2).unwrap();
    std::fs::write(folder2.join("b.txt"), b"ui").unwrap();
    cc.add_folder(folder2.to_str().unwrap(), "uiadd", &[], false)
        .expect("add folder");
    std::thread::sleep(Duration::from_secs(3));
    let raw2 = reqwest::blocking::Client::new()
        .get(format!("http://{}/status?token={}", info.addr, info.token))
        .send()
        .unwrap()
        .text()
        .unwrap();
    eprintln!("raw status2 body: {raw2}");
    let folders = cc.status().unwrap();
    assert!(folders.iter().any(|f| f.name == "uiadd"), "uiadd 应出现在状态中");

    // pause/resume
    cc.pause("notes-unknown").ok(); // 不存在的文件夹也应成功（暂停集合按名记录）
    cc.pause("proj").unwrap();
    assert!(folders.iter().any(|f| f.name == "proj"));
    cc.resume("proj").unwrap();
    cc.trigger_sync(Some("proj")).unwrap();

    // 模拟一条冲突副本 → /conflicts 应列出，resolve 可执行
    let proj_local = folders
        .iter()
        .find(|f| f.name == "proj")
        .unwrap()
        .local_path
        .clone();
    let copy = std::path::Path::new(&proj_local).join("a (conflict from devX).txt");
    std::fs::write(&copy, b"conflict").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let conflicts = cc.conflicts().unwrap();
    let c = conflicts.iter().find(|c| c.folder == "proj").expect("应发现冲突");
    cc.resolve_conflict("proj", &c.rel, &c.copy_rel, "local")
        .expect("resolve");
    assert!(!copy.exists(), "保留当前 = 删除副本");

    // 登录协议互通（ysync-core 直接调 Go 服务端）
    let http = reqwest::blocking::Client::new();
    let resp = http
        .post(format!("{base}/api/v1/auth/login"))
        .json(&LoginReq {
            user: "alice".into(),
            password: "secret".into(),
            device_name: "rust-probe".into(),
        })
        .send()
        .unwrap();
    assert!(resp.status().is_success());
    let lr: ysync_core::protocol::LoginResp = resp.json().unwrap();
    assert!(!lr.token.is_empty());

    let _ = folder2;
}
