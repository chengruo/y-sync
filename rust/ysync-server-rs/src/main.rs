//! y-sync-server-rs：服务端 Rust 实现（协议语义与 Go 版等价，e2e 全矩阵验证）。
//! 子命令：serve / adduser / passwd / list-users / gc / backup / version（SR1）。
mod blob;
mod httpd;
mod hub;
mod store;
mod upload;
mod util;

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use store::Store;

fn main() {

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let err: Result<(), String> = match args[0].as_str() {
        "serve" => cmd_serve(&args[1..]),
        "adduser" => cmd_adduser(&args[1..]),
        "passwd" => cmd_passwd(&args[1..]),
        "list-users" => cmd_list_users(),
        "gc" => cmd_gc(),
        "backup" => cmd_backup(&args[1..]),
        "version" => {
            let build = option_env!("Y_SYNC_VERSION").unwrap_or("dev");
            println!("y-sync-server-rs v{build} (Rust 服务端, 协议 v1 兼容)");
            Ok(())
        }
        _ => usage(),
    };
    if let Err(e) = err {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn usage() -> ! {
    eprintln!(
        r#"y-sync-server-rs — 轻量文件同步服务端（Rust 实现）

用法:
  y-sync-server-rs serve   [-addr ADDR] [-data DIR]
  y-sync-server-rs adduser <name> [--quota 字节数] [--password 密码]
  y-sync-server-rs passwd  <name> [--password 密码]
  y-sync-server-rs list-users
  y-sync-server-rs gc
  y-sync-server-rs backup -out <dir>
  y-sync-server-rs version

环境变量: YSYNC_ADDR / YSYNC_DATA / YSYNC_PASS（免交互密码）
密码输入: 交互终端下有提示且不回显；管道/重定向时读单行 stdin
管理命令: adduser/passwd/gc/backup 需以服务运行用户执行（生产 systemd 模板为 User=y-sync）:
          sudo -u y-sync YSYNC_DATA=/var/lib/y-sync y-sync-server-rs adduser <name>"#
    );
    std::process::exit(2)
}

fn cmd_list_users() -> Result<(), String> {
    let store = open_store()?;
    for u in store.list_users_with_usage() {
        let name = u["name"].as_str().unwrap_or("?");
        let quota = u["quota_bytes"].as_i64().unwrap_or(0);
        let used = u["used_bytes"].as_i64().unwrap_or(0);
        let quota_s = if quota > 0 {
            format!("{:.2} GB", quota as f64 / 1073741824.0)
        } else {
            "不限".into()
        };
        println!("{name:<20} 已用 {:.2} GB  配额 {quota_s}", used as f64 / 1073741824.0);
    }
    Ok(())
}

fn cmd_gc() -> Result<(), String> {
    let store = open_store()?;
    match store.gc() {
        Ok((purged, blobs)) => {
            println!("gc: purged {purged} trash entries, removed {blobs} unreferenced blobs");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn data_dir() -> PathBuf {
    match std::env::var("YSYNC_DATA") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from("./y-sync-data"),
    }
}

fn open_store() -> Result<Store, String> {
    Ok(Store::open(&data_dir()).map_err(|e| format!("open store: {e}"))?)
}

/// 密码来源：--password 旗标 / YSYNC_PASS 环境变量 / 终端交互。
/// 非终端 stdin（管道、heredoc）保持旧行为：读单行（e2e/脚本依赖）。
fn resolve_password(name: &str, flag: Option<String>) -> Result<String, String> {
    if let Some(p) = flag.or_else(|| std::env::var("YSYNC_PASS").ok().filter(|v| !v.is_empty())) {
        if p.is_empty() {
            return Err("密码不能为空".into());
        }
        return Ok(p);
    }
    let p1 = read_password(&format!("为用户 {name} 设置密码（输入不回显）: "));
    if std::io::stdin().is_terminal() {
        let p2 = read_password("再次输入确认: ");
        if p1 != p2 {
            return Err("两次输入不一致".into());
        }
    }
    if p1.is_empty() {
        return Err("密码不能为空".into());
    }
    Ok(p1)
}

fn read_password(prompt: &str) -> String {
    if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().lock().read_line(&mut s).unwrap_or(0);
        return s.trim().to_string();
    }
    // 先关回显再出提示，避免提前键入/粘贴的字符被回显；stty 不可用时退化为普通读入
    let echo_off = std::process::Command::new("stty")
        .arg("-echo")
        .stdin(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s).unwrap_or(0);
    if echo_off {
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        println!();
    }
    s.trim().to_string()
}

fn cmd_adduser(args: &[String]) -> Result<(), String> {
    let mut name: Option<String> = None;
    let mut quota: i64 = 0; // 0 = 不限
    let mut password: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--quota" => {
                i += 1;
                let v = args.get(i).ok_or("--quota 需要参数（字节数，如 10737418240）")?;
                quota = v.parse().map_err(|_| "--quota 需要字节数")?;
            }
            "--password" => {
                i += 1;
                password = Some(args.get(i).ok_or("--password 需要参数")?.clone());
            }
            other if !other.starts_with('-') => name = Some(other.to_string()),
            other => return Err(format!("未知参数 {other:?}")),
        }
        i += 1;
    }
    let Some(name) = name else {
        return Err("usage: y-sync-server-rs adduser <name> [--quota 字节数] [--password 密码]".into());
    };
    let store = open_store()?;
    let pw = resolve_password(&name, password)?;
    let id = store.create_user(&name, &pw, quota)?;
    println!("user {name:?} created (id={id}, quota={quota})");
    Ok(())
}

fn cmd_passwd(args: &[String]) -> Result<(), String> {
    let mut name: Option<String> = None;
    let mut password: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--password" => {
                i += 1;
                password = Some(args.get(i).ok_or("--password 需要参数")?.clone());
            }
            other if !other.starts_with('-') => name = Some(other.to_string()),
            other => return Err(format!("未知参数 {other:?}")),
        }
        i += 1;
    }
    let Some(name) = name else {
        return Err("usage: y-sync-server-rs passwd <name> [--password 密码]".into());
    };
    let store = open_store()?;
    let pw = resolve_password(&name, password)?;
    store.reset_password(&name, &pw)?;
    println!("password of {name:?} updated");
    Ok(())
}

fn cmd_serve(args: &[String]) -> Result<(), String> {
    let mut addr = std::env::var("YSYNC_ADDR").unwrap_or_else(|_| "127.0.0.1:8720".into());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-addr" => {
                i += 1;
                addr = args.get(i).cloned().ok_or("-addr 需要参数")?;
            }
            "-data" => {
                i += 1;
                let d = args.get(i).cloned().ok_or("-data 需要参数")?;
                std::env::set_var("YSYNC_DATA", d);
            }
            other => return Err(format!("未知参数 {other:?}")),
        }
        i += 1;
    }

    let store = open_store()?;
    let state = std::sync::Arc::new(httpd::ServerState {
        uploads: upload::UploadManager::new(data_dir().join("tmp")),
        hub: hub::Hub::new(),
        store,
        login_guard: httpd::LoginGuard::new(),
        share_guard: httpd::ShareGuard::new(),
        bytes_in: std::sync::atomic::AtomicU64::new(0),
        bytes_out: std::sync::atomic::AtomicU64::new(0),
        http_stats: httpd::HttpStats::new(),
        started_at: std::time::Instant::now(),
        audit_path: data_dir().join("audit.log"),
    });

    // 优雅退出（SR6）：SIGTERM 直接退出（SQLite WAL 保证一致性）。
    // signal-hook 的 flag/iterator 不支持 Windows，Windows 依赖服务管理器终止。
    #[cfg(unix)]
    {
        let term_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, term_flag.clone())
            .map_err(|e| e.to_string())?;
        let flag_for_thread = term_flag.clone();
        std::thread::spawn(move || loop {
            if flag_for_thread.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("level=INFO msg=\"shutting down (SR6: 优雅退出)\"");
                std::process::exit(0);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
    }

    eprintln!(
        "level=INFO msg=\"y-sync-server-rs listening\" addr={addr} data={}",
        data_dir().display()
    );
    httpd::serve(&addr, state)
}

fn cmd_backup(args: &[String]) -> Result<(), String> {
    let mut out = String::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-out" {
            i += 1;
            out = args.get(i).cloned().ok_or("-out 需要参数")?;
        }
        i += 1;
    }
    if out.is_empty() {
        return Err("usage: y-sync-server-rs backup -out <dir>".into());
    }
    let data = data_dir();
    std::fs::create_dir_all(std::path::Path::new(&out).join("blobs"))
        .map_err(|e| format!("{e}"))?;
    let store = open_store()?;
    {
        let conn = store.db.lock().unwrap();
        let snapshot = std::path::Path::new(&out)
            .join("y-sync.db")
            .to_string_lossy()
            .replace('\'', "''");
        conn.execute(&format!("VACUUM INTO '{snapshot}'"), [])
            .map_err(|e| format!("vacuum into: {e}"))?;
    }
    let blobs_dir = data.join("blobs");
    let mut copied = 0i64;
    copy_dir(&blobs_dir, &std::path::Path::new(&out).join("blobs"), &mut copied)?;
    let manifest = serde_json::json!({
        "created": util::now_secs(),
        "blobs": copied,
        "note": "恢复：将 y-sync.db 与 blobs/ 放回数据目录"
    });
    std::fs::write(
        std::path::Path::new(&out).join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|e| format!("{e}"))?;
    println!("backup 完成: {out}（blobs={copied}）");
    Ok(())
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path, copied: &mut i64) -> Result<(), String> {
    if !src.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src).map_err(|e| format!("{e}"))? {
        let entry = entry.map_err(|e| format!("{e}"))?;
        let ty = entry.file_type().map_err(|e| format!("{e}"))?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&to).map_err(|e| format!("{e}"))?;
            copy_dir(&entry.path(), &to, copied)?;
        } else {
            std::fs::copy(entry.path(), &to).map_err(|e| format!("{e}"))?;
            *copied += 1;
        }
    }
    Ok(())
}
