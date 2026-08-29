//! ysyncd：Rust 客户端 CLI（Go cmd/ysync 的移植，协议与配置文件完全兼容）。
mod api;
mod chunk;
mod conflicts;
mod ctx;
mod daemon;
mod daemon_state;
mod engine;
mod httpd;
mod ignore;
mod state;
mod watcher;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use ysync_core::protocol::TYPE_DIR;
use ysync_core::Result;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = |msg: String| eprintln!("{msg}");
    if args.is_empty() {
        usage();
    }
    let err: Result<()> = match args[0].as_str() {
        "init" => cmd_init(&args[1..]),
        "add" => cmd_add(&args[1..]),
        "sync" => cmd_sync(&args[1..]),
        "daemon" => cmd_daemon(log, &args[1..]),
        "status" => cmd_status(),
        "trash" => cmd_trash(&args[1..]),
        "versions" => cmd_versions(&args[1..]),
        "share" => cmd_share(&args[1..]),
        "shares" => cmd_shares(),
        "unshare" => cmd_unshare(&args[1..]),
        "devices" => cmd_devices(),
        "revoke" => cmd_revoke(&args[1..]),
        "remove" => cmd_remove(&args[1..]),
        "ui" => cmd_ui(),
        "version" => {
            println!("ysyncd v0.1.0 (Rust 客户端, 协议 v1 兼容)");
            Ok(())
        }
        _ => {
            usage();
            unreachable!()
        }
    };
    if let Err(e) = err {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn usage() -> ! {
    eprintln!(
        r#"ysyncd — Rust 客户端（协议 v1 兼容，配置与 ysync 互通）

用法:
  ysyncd init   -server URL -user NAME            首次登录（交互输入密码）
  ysyncd add    <本地目录> [-as 服务端名] [--exclude 子树]... [--use-gitignore]
  ysyncd sync   [-only NAME]                     同步一次
  ysyncd daemon [-http ADDR] [-interval 3s]       常驻（管理台/事件/WS/轮询）
  ysyncd status                                  查看各文件夹状态
  ysyncd trash   list | restore <id> | rm <id>    回收站（FR-V2）
  ysyncd versions list|restore <folder> <path>    文件版本（FR-V1）
  ysyncd share   <folder> <path> [-hours N] [-password pw]
  ysyncd shares / ysyncd unshare <token>        分享管理
  ysyncd devices / ysyncd revoke <id>          设备管理（吊销丢失设备）
  ysyncd remove  <name>                          解除跟踪文件夹
  ysyncd ui                                      打开本地管理台
  ysyncd version
"#
    );
    std::process::exit(2)
}

// ---------- 手写参数解析（与 Go 端一致：支持 flag-after-arg 与带值 flag） ----------

fn split_args(args: &[String], value_flags: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut positional = Vec::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let bare = a.trim_start_matches('-');
        if value_flags.contains(&bare) && a.starts_with('-') {
            if i + 1 >= args.len() {
                eprintln!("error: {a} 需要参数");
                std::process::exit(2);
            }
            flags.push(format!("{bare}={}", args[i + 1]));
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            flags.push(bare.to_string());
            i += 1;
            continue;
        }
        positional.push(a.clone());
        i += 1;
    }
    (positional, flags)
}

fn flag_value(flags: &[String], name: &str) -> Option<String> {
    flags
        .iter()
        .find(|f| f.split('=').next() == Some(name))
        .and_then(|f| f.split_once('=').map(|(_, v)| v.to_string()))
}

fn flag_bool(flags: &[String], name: &str) -> bool {
    flags.iter().any(|f| f.split('=').next() == Some(name))
}

fn read_line() -> String {
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s).unwrap_or(0);
    s.trim().to_string()
}

fn read_password() -> String {
    read_line()
}

fn parse_duration(s: &str) -> std::time::Duration {
    let s = s.trim();
    let idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let v: f64 = s[..idx].parse().unwrap_or(3.0);
    match &s[idx..] {
        "ms" => std::time::Duration::from_millis(v as u64),
        "m" => std::time::Duration::from_secs_f64(v * 60.0),
        "h" => std::time::Duration::from_secs_f64(v * 3600.0),
        _ => std::time::Duration::from_secs_f64(v),
    }
}

// ---------- 子命令 ----------

fn cmd_init(args: &[String]) -> Result<()> {
    let (_, flags) = split_args(args, &["server", "user", "device"]);
    let server = flag_value(&flags, "server").unwrap_or_else(|| "http://127.0.0.1:8720".into());
    let user = flag_value(&flags, "user").unwrap_or_default();
    let device = flag_value(&flags, "device").unwrap_or_else(ysync_core::default_device_name);
    if user.is_empty() {
        return Err(ysync_core::Error::Msg("需要 -user".into()));
    }
    print!("password: ");
    std::io::stdout().flush().ok();
    let pw = read_password();
    let resp = api::Api::login(&user, &pw, &device, &server)?;
    let mut cfg = ysync_core::Config {
        server_url: server.trim_end_matches('/').to_string(),
        user,
        token: resp.token,
        device_name: device,
        device_id: resp.device_id,
        ..Default::default()
    };
    cfg.defaults();
    ysync_core::save_config(&cfg)?;
    println!("已登录为 {}（设备 {}）", cfg.user, cfg.device_name);
    Ok(())
}

fn cmd_add(args: &[String]) -> Result<()> {
    let (pos, flags) = split_args(args, &["as", "exclude"]);
    if pos.len() != 1 {
        return Err(ysync_core::Error::Msg(
            "usage: ysyncd add <本地目录> [-as 名字] [--exclude 子树]...".into(),
        ));
    }
    let local = Path::new(&pos[0]);
    let local = local.canonicalize().unwrap_or_else(|_| local.to_path_buf());
    if !local.exists() {
        std::fs::create_dir_all(&local)?;
    } else if !local.is_dir() {
        return Err(ysync_core::Error::Msg(format!("{} 不是目录", local.display())));
    }
    let name = flag_value(&flags, "as").unwrap_or_else(|| {
        local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(ysync_core::Error::Msg(format!("非法的文件夹名 {name:?}")));
    }
    let excludes: Vec<String> = flags
        .iter()
        .filter(|f| f.starts_with("exclude="))
        .map(|f| f["exclude=".len()..].trim_matches('/').to_string())
        .collect();
    let use_gitignore = flag_bool(&flags, "use-gitignore");
    let mut cfg = ysync_core::load_config()?;
    let abs_str = local.to_string_lossy().to_string();
    for f in &cfg.folders {
        if f.name == name {
            return Err(ysync_core::Error::Msg(format!("文件夹 {name:?} 已存在")));
        }
        if ysync_core::is_sub_path(&f.local_path, &abs_str)
            || ysync_core::is_sub_path(&abs_str, &f.local_path)
        {
            return Err(ysync_core::Error::Msg(format!(
                "文件夹不得嵌套或重叠（FR-S15）：{} 与 {}",
                f.local_path,
                local.display()
            )));
        }
    }
    cfg.folders.push(ysync_core::Folder {
        name,
        local_path: abs_str,
        root_node_id: 0,
        cursor: 0,
        excludes,
        use_gitignore,
    });
    ysync_core::save_config(&cfg)?;
    println!(
        "已接入 {:?} → 服务端子树 {:?}，执行 sync 开始同步",
        local.display(),
        cfg.folders.last().unwrap().name
    );
    Ok(())
}

fn make_engine(cfg: &ysync_core::Config) -> std::sync::Arc<engine::Engine> {
    let mut api = api::Api::new(&cfg.server_url, &cfg.token);
    api.set_limits(cfg.upload_limit_kbs, cfg.download_limit_kbs);
    std::sync::Arc::new(engine::Engine {
        api: std::sync::Arc::new(api),
        device_name: cfg.device_name.clone(),
    })
}

fn cmd_sync(args: &[String]) -> Result<()> {
    let (_, flags) = split_args(args, &["only"]);
    let only = flag_value(&flags, "only").unwrap_or_default();
    let mut cfg = ysync_core::load_config()?;
    cfg.defaults();
    ctx::install(cfg.clone(), cfg.device_id);
    let engine = make_engine(&cfg);
    let mut had_err = false;
    for f in &mut cfg.folders {
        if !only.is_empty() && f.name != only {
            continue;
        }
        let mut fc = engine::FolderCfg {
            name: f.name.clone(),
            local_path: PathBuf::from(&f.local_path),
            root_node_id: f.root_node_id,
            cursor: f.cursor,
            excludes: f.excludes.clone(),
            use_gitignore: f.use_gitignore,
        };
        match engine.sync_folder(&mut fc) {
            Ok(s) => {
                if s.uploaded + s.downloaded + s.moved + s.deleted + s.conflicts > 0 {
                    eprintln!(
                        "level=INFO msg=synced folder={:?} up={} down={} moved={} deleted={} conflicts={}",
                        f.name, s.uploaded, s.downloaded, s.moved, s.deleted, s.conflicts
                    );
                }
            }
            Err(ysync_core::Error::SyncBusy) => {
                // daemon 或另一 CLI 正在同步该文件夹：静默跳过（不是失败）
                eprintln!("level=INFO msg=\"sync skipped (busy)\" folder={:?}", f.name);
            }
            Err(e) => {
                eprintln!("level=ERROR msg=\"sync failed\" folder={:?} err={e:?}", f.name);
                had_err = true;
            }
        }
    }
    if had_err {
        return Err(ysync_core::Error::Msg("部分文件夹同步失败".into()));
    }
    Ok(())
}

fn cmd_daemon(log: impl Fn(String) + Send + Sync + 'static, args: &[String]) -> Result<()> {
    let (_, flags) = split_args(args, &["http", "interval", "only", "reconcile"]);
    let interval =
        parse_duration(&flag_value(&flags, "interval").unwrap_or_else(|| "3s".into()));
    let http_addr = flag_value(&flags, "http").unwrap_or_else(|| "127.0.0.1:8730".into());
    let only = flag_value(&flags, "only").unwrap_or_default();
    let reconcile =
        parse_duration(&flag_value(&flags, "reconcile").unwrap_or_else(|| "5m".into()));

    // setup 模式（UI 配置访问）：config.json 缺失时以空配置启动，
    // 用户经浏览器管理台完成服务器/账号配置后进入正常同步
    let (cfg, setup_mode) = match ysync_core::load_config() {
        Ok(mut c) => {
            c.defaults();
            (c, false)
        }
        Err(_) => {
            eprintln!("level=INFO msg=\"未初始化：进入 setup 模式（请在管理台完成配置）\"");
            let mut c = ysync_core::Config::default();
            c.device_name = ysync_core::default_device_name();
            (c, true)
        }
    };
    ctx::install(cfg.clone(), cfg.device_id);

    let engine = make_engine(&cfg);
    let state = std::sync::Arc::new(daemon_state::DaemonState::new());
    // P1-10：日志落盘 + 轮转（5MB → .1），stderr 同步保留
    let log_dir = ysync_core::config_dir()?;
    std::fs::create_dir_all(&log_dir)?; // setup 模式下目录可能尚不存在
    let log_path = log_dir.join("daemon.log");
    let logfile: std::sync::Arc<std::sync::Mutex<std::fs::File>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::fs::OpenOptions::new()
            .create(true).append(true).open(&log_path)?));
    let log = std::sync::Arc::new(move |msg: String| {
        eprintln!("{msg}");
        let mut f = logfile.lock().unwrap_or_else(|p| p.into_inner());
        if f.metadata().map(|m| m.len()).unwrap_or(0) > 5 << 20 {
            if let Some(p1) = log_path.with_extension("log.1").to_str() {
                let _ = std::fs::rename(&log_path, p1);
            }
            *f = std::fs::OpenOptions::new().create(true).append(true)
                .open(&log_path).unwrap_or_else(|_| std::fs::File::open(&log_path).unwrap());
        }
        let _ = writeln!(f, "{msg}");
    });

    // daemon.json（供 ysync ui / Tauri 壳发现）
    let token: String = {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..24).map(|_| rng.gen::<u8>()).collect::<Vec<u8>>().iter().map(|b| format!("{b:02x}")).collect()
    };
    ysync_core::write_daemon_info(&ysync_core::DaemonInfo {
        pid: std::process::id() as i32,
        addr: http_addr.clone(),
        token: token.clone(),
        started: api::now_millis(),
    })?;

    let d = daemon::Daemon {
        engine,
        state,
        log,
        only,
        http_addr: http_addr.clone(),
        token,
        stop_tx: std::sync::Arc::new(std::sync::Mutex::new(None)),
        setup_mode,
        setup_done: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        watched: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        watcher_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
        watch_tx: std::sync::Arc::new(std::sync::Mutex::new(None)),
    };
    d.run(interval, reconcile);
    Ok(())
}

fn cmd_status() -> Result<()> {
    let cfg = ysync_core::load_config()?;
    println!(
        "server: {}  user: {}  device: {}",
        cfg.server_url, cfg.user, cfg.device_name
    );
    // daemon 运行状态（易用性：一条命令看全局健康度）
    match ysync_core::read_daemon_info() {
        Ok(info) => {
            let cc = ysync_core::control::ControlClient::new(&info.addr, &info.token);
            match cc.status() {
                Ok(folders) => {
                    let conflicts: i64 = folders.iter().map(|f| f.conflicts_total).sum();
                    let paused = folders.iter().filter(|f| f.paused).count();
                    let errors = folders.iter().filter(|f| !f.last_error.is_empty()).count();
                    println!(
                        "daemon: running @ {}（{} 个文件夹，{} 冲突，{} 暂停，{} 错误）",
                        info.addr,
                        folders.len(),
                        conflicts,
                        paused,
                        errors
                    );
                }
                Err(e) => println!("daemon: running @ {}（通信失败: {e}）", info.addr),
            }
        }
        Err(_) => println!("daemon: 未运行"),
    }
    if cfg.folders.is_empty() {
        println!("(无同步文件夹，使用 add 接入)");
    }
    for f in &cfg.folders {
        let n = state::State::open(Path::new(&f.local_path))
            .and_then(|s| s.all().map(|m| m.len()))
            .unwrap_or(0);
        println!(
            "  {:<20} {:<40} cursor={} files={}",
            f.name, f.local_path, f.cursor, n
        );
    }
    Ok(())
}

fn cmd_trash(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(ysync_core::Error::Msg(
            "usage: ysyncd trash list | restore <id> | rm <id>".into(),
        ));
    }
    let cfg = ysync_core::load_config()?;
    let api = api::Api::new(&cfg.server_url, &cfg.token);
    match args[0].as_str() {
        "list" => {
            let items = api.trash_list()?;
            if items.is_empty() {
                println!("(回收站为空)");
                return Ok(());
            }
            for it in &items {
                let kind = if it.kind == TYPE_DIR { "dir " } else { "file" };
                println!(
                    "  {:<8} {} {:<60} {:>8.1}KB  删除于 {}",
                    it.id,
                    kind,
                    it.orig_path,
                    it.size as f64 / 1024.0,
                    fmt_unix(it.deleted_at)
                );
            }
        }
        "restore" => {
            let id: i64 = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| ysync_core::Error::Msg("usage: ysyncd trash restore <id>".into()))?;
            api.trash_restore(id)?;
            println!("已恢复 #{id}（各设备将在下次同步取回）");
        }
        "rm" => {
            let id: i64 = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| ysync_core::Error::Msg("usage: ysyncd trash rm <id>".into()))?;
            api.trash_delete(id)?;
            println!("已彻底删除 #{id}");
        }
        other => {
            return Err(ysync_core::Error::Msg(format!("未知子命令 {other:?}")));
        }
    }
    Ok(())
}

fn cmd_versions(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        return Err(ysync_core::Error::Msg(
            "usage: ysyncd versions list <folder> <relpath> | restore <folder> <relpath> <version-id>"
                .into(),
        ));
    }
    let cfg = ysync_core::load_config()?;
    let api = api::Api::new(&cfg.server_url, &cfg.token);
    let folder = cfg
        .folders
        .iter()
        .find(|f| f.name == args[1])
        .ok_or_else(|| ysync_core::Error::Msg(format!("文件夹 {:?} 不存在", args[1])))?;
    if args.len() < 3 {
        return Err(ysync_core::Error::Msg("缺少相对路径".into()));
    }
    let rel = &args[2];
    let node_id = lookup_node(&api, folder, rel)?;
    match args[0].as_str() {
        "list" => {
            let versions = api.node_versions(node_id)?;
            if versions.is_empty() {
                println!("(无历史版本)");
                return Ok(());
            }
            for v in &versions {
                println!(
                    "  {:<8} {:<64} {:>8.1}KB  {}",
                    v.id,
                    format!("{}…", &v.content_hash[..16.min(v.content_hash.len())]),
                    v.size as f64 / 1024.0,
                    fmt_unix(v.created)
                );
            }
        }
        "restore" => {
            let vid: i64 = args.get(3).and_then(|s| s.parse().ok()).ok_or_else(|| {
                ysync_core::Error::Msg(
                    "usage: ysyncd versions restore <folder> <relpath> <version-id>".into(),
                )
            })?;
            let dest = Path::new(&folder.local_path)
                .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            api.download_version_to(vid, &dest, 0)?;
            println!("版本 #{vid} 已写回 {rel}（下次 sync 上行）");
        }
        other => {
            return Err(ysync_core::Error::Msg(format!("未知子命令 {other:?}")));
        }
    }
    Ok(())
}

fn lookup_node(api: &api::Api, f: &ysync_core::Folder, rel: &str) -> Result<i64> {
    let target = format!("{}/{}", f.name, rel);
    for n in api.nodes()? {
        if n.path == target {
            return Ok(n.id);
        }
    }
    Err(ysync_core::Error::Msg(format!("服务端不存在 {rel:?}")))
}

fn cmd_share(args: &[String]) -> Result<()> {
    let (pos, flags) = split_args(args, &["hours", "password"]);
    if pos.len() != 2 {
        return Err(ysync_core::Error::Msg(
            "usage: ysyncd share <folder> <相对路径> [-hours N] [-password pw]".into(),
        ));
    }
    let cfg = ysync_core::load_config()?;
    let api = api::Api::new(&cfg.server_url, &cfg.token);
    let hours: i64 = flag_value(&flags, "hours")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let password = flag_value(&flags, "password").unwrap_or_default();
    let server_path = format!("{}/{}", pos[0], pos[1]);
    let info = api.create_share(&server_path, hours, &password)?;
    println!("分享链接: {}/s/{}", cfg.server_url, info.token);
    if !password.is_empty() {
        println!("访问密码: {password}");
    }
    if info.expires_at > 0 {
        println!("过期时间: {}", fmt_unix(info.expires_at));
    }
    Ok(())
}

fn cmd_shares() -> Result<()> {
    let cfg = ysync_core::load_config()?;
    let api = api::Api::new(&cfg.server_url, &cfg.token);
    let shares = api.list_shares()?;
    if shares.is_empty() {
        println!("(无分享)");
        return Ok(());
    }
    for s in &shares {
        let exp = if s.expires_at > 0 {
            fmt_unix(s.expires_at)
        } else {
            "永久".into()
        };
        let pwd = if s.has_password { " [密码]" } else { "" };
        println!("  {}  {:<50}{} 过期: {}", s.token, s.path, pwd, exp);
    }
    Ok(())
}

fn cmd_unshare(args: &[String]) -> Result<()> {
    let Some(token) = args.first() else {
        return Err(ysync_core::Error::Msg("usage: ysyncd unshare <token>".into()));
    };
    let cfg = ysync_core::load_config()?;
    let api = api::Api::new(&cfg.server_url, &cfg.token);
    api.delete_share(token)?;
    println!("已撤销分享");
    Ok(())
}

fn cmd_devices() -> Result<()> {
    let cfg = ysync_core::load_config()?;
    let api = api::Api::new(&cfg.server_url, &cfg.token);
    let devices = api.devices_list()?;
    if devices.is_empty() {
        println!("(无设备)");
        return Ok(());
    }
    for d in &devices {
        let id = d["id"].as_i64().unwrap_or(0);
        let name = d["name"].as_str().unwrap_or("?");
        let seen = d["last_seen"].as_i64().unwrap_or(0);
        let cur = d["current"].as_bool().unwrap_or(false);
        println!(
            "  {:<6} {:<30} 最近活跃 {}{}",
            id,
            name,
            fmt_unix(seen),
            if cur { "  ← 当前设备" } else { "" }
        );
    }
    Ok(())
}

fn cmd_revoke(args: &[String]) -> Result<()> {
    let Some(id) = args.first() else {
        return Err(ysync_core::Error::Msg("usage: ysyncd revoke <id>".into()));
    };
    let id: i64 = id.parse().map_err(|_| ysync_core::Error::Msg("id 需为数字".into()))?;
    let cfg = ysync_core::load_config()?;
    let api = api::Api::new(&cfg.server_url, &cfg.token);
    api.device_revoke(id)?;
    println!("设备 #{id} 已吊销（其 token 立即失效）");
    Ok(())
}

fn cmd_remove(args: &[String]) -> Result<()> {
    let Some(name) = args.first() else {
        return Err(ysync_core::Error::Msg("usage: ysyncd remove <name>".into()));
    };
    let mut cfg = ysync_core::load_config()?;
    let before = cfg.folders.len();
    cfg.folders.retain(|f| f.name != *name);
    if cfg.folders.len() == before {
        return Err(ysync_core::Error::Msg(format!("文件夹 {name:?} 不存在")));
    }
    ysync_core::save_config(&cfg)?;
    println!("已解除跟踪 {name:?}（本地文件与服务端副本保留；如需恢复请重新 add）");
    Ok(())
}

fn cmd_ui() -> Result<()> {
    let info = ysync_core::read_daemon_info()?;
    let url = format!("http://{}?token={}", info.addr, info.token);
    let opener = match std::env::consts::OS {
        "macos" => "open",
        "windows" => "rundll32",
        _ => "xdg-open",
    };
    match std::process::Command::new(opener).arg(&url).spawn() {
        Ok(_) => {
            println!("已在浏览器打开管理台: {url}");
            Ok(())
        }
        Err(_) => {
            println!("{url}");
            Ok(())
        }
    }
}

fn fmt_unix(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
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
    format!("{y:04}-{:02}-{:02} {h:02}:{mi:02}", m + 1, d + 1)
}
