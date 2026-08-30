// y-sync 桌面壳：托盘状态灯 + 原生菜单 + 管理台窗口。
// 架构（FR-C3 薄壳原则）：同步引擎在 Go daemon 中运行，本应用只是控制 API 的
// 另一个客户端——不内嵌引擎，GUI 崩溃不影响同步。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::{Duration, Instant};

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::WebviewWindowBuilder,
    AppHandle, Manager, WebviewUrl,
};
use ysync_core::config::read_daemon_info;
use ysync_core::control::ControlClient;

mod tray_assets {
    pub const GRAY: &[u8] = include_bytes!("../tray/gray.png");
    pub const GREEN: &[u8] = include_bytes!("../tray/green.png");
    pub const BLUE: &[u8] = include_bytes!("../tray/blue.png");
    pub const YELLOW: &[u8] = include_bytes!("../tray/yellow.png");
    pub const RED: &[u8] = include_bytes!("../tray/red.png");
}

struct DaemonHandle {
    addr: String,
    token: String,
    client: ControlClient,
}

/// 读取 daemon.json（每次轮询刷新，容忍 daemon 重启换 token）。
fn connect() -> Option<DaemonHandle> {
    let info = read_daemon_info().ok()?;
    let client = ControlClient::new(&info.addr, &info.token);
    Some(DaemonHandle {
        addr: info.addr,
        token: info.token,
        client,
    })
}

/// 定位 ysyncd 可执行文件：环境变量 → 应用同目录（sidecar）→ 常见安装路径。
fn find_daemon_bin() -> Option<std::path::PathBuf> {
    const NAME: &str = if cfg!(windows) { "ysyncd.exe" } else { "ysyncd" };
    if let Ok(p) = std::env::var("YSYNC_DAEMON_BIN") {
        if !p.is_empty() {
            return Some(p.into());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent()?.join(NAME);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    for p in [
        "/usr/local/bin/ysyncd",
        "/opt/homebrew/bin/ysyncd",
        "/usr/local/bin/ysync",
        "/opt/homebrew/bin/ysync",
    ] {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// daemon 未运行时尝试拉起（分离进程，壳退出不影响 daemon，FR-C3）。
/// 成功与否以 daemon.json 出现/复活为准（read_daemon_info 含 PID 存活检查）。
fn spawn_daemon() -> bool {
    let Some(bin) = find_daemon_bin() else {
        eprintln!("daemon 未运行且未找到 ysyncd 可执行文件");
        return false;
    };
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("daemon").stdin(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 新进程组：壳退出/重启不影响 daemon 存活（FR-C3 薄壳原则）
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    match cmd.spawn() {
        Ok(child) => {
            eprintln!("daemon 已拉起: {} (pid {})", bin.display(), child.id());
            true
        }
        Err(e) => {
            eprintln!("daemon 拉起失败: {e}");
            false
        }
    }
}

/// 确保 daemon 可用：已运行直接成功；否则拉起并轮询 daemon.json 就绪。
fn ensure_daemon() -> bool {
    if connect().is_some() {
        return true;
    }
    spawn_daemon();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(500));
        if connect().is_some() {
            return true;
        }
    }
    false
}

/// 状态 → 托盘图标：灰=未运行 蓝=同步中 黄=冲突 红=错误 绿=正常
fn pick_icon(status: Option<&[ysync_core::protocol::FolderStatus]>) -> &'static [u8] {
    let Some(list) = status else {
        return tray_assets::GRAY;
    };
    if list.iter().any(|f| !f.last_error.is_empty()) {
        tray_assets::RED
    } else if list.iter().any(|f| f.conflicts_total > 0) {
        tray_assets::YELLOW
    } else if list
        .iter()
        .any(|f| f.last_stats.contains('↑') || f.last_stats.contains('↓'))
    {
        tray_assets::BLUE
    } else {
        tray_assets::GREEN
    }
}

fn show_manager(app: &AppHandle) -> tauri::Result<()> {
    if connect().is_none() {
        ensure_daemon();
    }
    let Some(d) = connect() else {
        // daemon 拉起失败：打开说明页（daemon 就绪后轮询线程会自动重导航到管理台）
        if let Some(w) = app.get_webview_window("manager") {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
            return Ok(());
        }
        WebviewWindowBuilder::new(
            app,
            "manager",
            WebviewUrl::App("offline.html".into()),
        )
        .title("y-sync daemon 未运行")
        .inner_size(720.0, 480.0)
        .build()?;
        return Ok(());
    };
    let url = manager_url(&d)
        .parse::<tauri::Url>()
        .expect("daemon url");
    if let Some(w) = app.get_webview_window("manager") {
        // macOS：最小化的窗口仅 set_focus 不会还原，需先解除最小化
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, "manager", WebviewUrl::External(url))
        .title("y-sync 管理台")
        .inner_size(920.0, 680.0)
        .build()?;
    Ok(())
}

fn manager_url(handle: &DaemonHandle) -> String {
    format!("http://{}/?token={}", handle.addr, handle.token)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // 托盘应用：不占 Dock（macOS）——先于 handle 借用
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let handle = app.handle();

            let open_i = MenuItem::with_id(handle, "open", "打开管理台", true, None::<&str>)?;
            let sync_i = MenuItem::with_id(handle, "sync", "立即全部同步", true, None::<&str>)?;
            let pause_i = MenuItem::with_id(handle, "pause", "暂停全部", true, None::<&str>)?;
            let resume_i = MenuItem::with_id(handle, "resume", "恢复全部", true, None::<&str>)?;
            let status_i =
                MenuItem::with_id(handle, "status", "状态：检查中…", false, None::<&str>)?;
            let quit_i = MenuItem::with_id(handle, "quit", "退出 y-sync 壳", true, None::<&str>)?;
            let menu = Menu::with_items(
                handle,
                &[
                    &status_i,
                    &PredefinedMenuItem::separator(handle)?,
                    &open_i,
                    &sync_i,
                    &pause_i,
                    &resume_i,
                    &PredefinedMenuItem::separator(handle)?,
                    &quit_i,
                ],
            )?;

            let tray = TrayIconBuilder::with_id("main-tray")
                .icon(tauri::image::Image::from_bytes(tray_assets::GRAY)?.to_owned())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("y-sync")
                .on_menu_event(move |app, event| {
                    if event.id().as_ref() == "quit" {
                        app.exit(0);
                        return;
                    }
                    if connect().is_none() {
                        ensure_daemon();
                    }
                    let Some(d) = connect() else { return };
                    let r = match event.id().as_ref() {
                        "open" => show_manager(app).map_err(|e| e.to_string()),
                        "sync" => d.client.trigger_sync(None).map_err(|e| e.to_string()),
                        "pause" => d.client.pause("").map_err(|e| e.to_string()),
                        "resume" => d.client.resume("").map_err(|e| e.to_string()),
                        _ => Ok(()),
                    };
                    if let Err(e) = r {
                        eprintln!("menu action failed: {e}");
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let _ = show_manager(tray.app_handle());
                    }
                })
                .build(app)?;
            tray.set_visible(true)?;

            // 状态轮询：更新托盘图标与状态文案（3s）；C3：token 变化 → 管理台窗口重导航（401 自愈）
            let status_item = status_i.clone();
            let tray = tray.clone();
            let app_for_poll = handle.clone();
            std::thread::spawn(move || {
                let mut last_url: Option<String> = None;
                // daemon 看门狗：启动即拉起；之后掉线每 30s 重试一次
                let mut last_spawn = Instant::now() - Duration::from_secs(60);
                loop {
                let (icon_bytes, text): (&'static [u8], String) = match connect() {
                    None => {
                        if last_spawn.elapsed() >= Duration::from_secs(30) {
                            last_spawn = Instant::now();
                            spawn_daemon();
                        }
                        (tray_assets::GRAY, "状态：daemon 未运行".into())
                    }
                    Some(d) => match d.client.status() {
                        Ok(list) => {
                            let icon: &'static [u8] = pick_icon(Some(&list));
                            let mut desc = String::from("正常");
                            if list.iter().any(|f| f.conflicts_total > 0) {
                                let n: i64 = list.iter().map(|f| f.conflicts_total).sum();
                                desc = format!("{n} 个冲突待处理");
                            }
                            if list.iter().any(|f| f.paused) {
                                desc += "（部分已暂停）";
                            }
                            for f in &list {
                                if !f.last_error.is_empty() {
                                    desc = format!("错误: {}", f.last_error);
                                    break;
                                }
                            }
                            (
                                icon,
                                format!("状态：{desc}（{} 个文件夹）", list.len()),
                            )
                        }
                        Err(_) => (
                            tray_assets::RED,
                            "状态：daemon 通信失败".to_string(),
                        ),
                    },
                };
                if let Ok(img) = tauri::image::Image::from_bytes(icon_bytes) {
                    let _ = tray.set_icon(Some(img.to_owned()));
                }
                let _ = status_item.set_text(&text);
                // daemon.json 中的 addr/token 变化（重启/重新配置）→ 已开窗口重导航
                if let Some(d) = connect() {
                    let url = format!("http://{}?token={}", d.addr, d.token);
                    if last_url.as_deref() != Some(url.as_str()) {
                        if let Some(w) = app_for_poll.get_webview_window("manager") {
                            if let Ok(u) = url.parse::<tauri::Url>() {
                                let _ = w.navigate(u);
                            }
                        }
                        last_url = Some(url);
                    }
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running y-sync desktop");
}
