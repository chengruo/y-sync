// y-sync 桌面壳：托盘状态灯 + 原生菜单 + 管理台窗口。
// 架构（FR-C3 薄壳原则）：同步引擎在 Go daemon 中运行，本应用只是控制 API 的
// 另一个客户端——不内嵌引擎，GUI 崩溃不影响同步。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;

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
    let Some(d) = connect() else {
        return Ok(()); // daemon 未运行：托盘已显示灰色，无 URL 可开
    };
    let url = manager_url(&d)
        .parse::<tauri::Url>()
        .expect("daemon url");
    if let Some(w) = app.get_webview_window("manager") {
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
                    let Some(d) = connect() else { return };
                    let r = match event.id().as_ref() {
                        "open" => show_manager(app).map_err(|e| e.to_string()),
                        "sync" => d.client.trigger_sync(None).map_err(|e| e.to_string()),
                        "pause" => d.client.pause("").map_err(|e| e.to_string()),
                        "resume" => d.client.resume("").map_err(|e| e.to_string()),
                        "quit" => {
                            app.exit(0);
                            Ok::<(), String>(())
                        }
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

            // 托盘应用：不占 Dock（macOS）
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // 状态轮询：更新托盘图标与状态文案（3s）
            let status_item = status_i.clone();
            let tray = tray.clone();
            std::thread::spawn(move || loop {
                let (icon_bytes, text): (&'static [u8], String) = match connect() {
                    None => (tray_assets::GRAY, "状态：daemon 未运行".into()),
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
                std::thread::sleep(Duration::from_secs(3));
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running y-sync desktop");
}
