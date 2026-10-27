//! One asynchronous update job per daemon; status reads never contact GitHub.
use crate::daemon::Daemon;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use ysync_core::update_install::{self, InstallPlan, UpdateJob};
use ysync_core::{updater, Error, Result};

pub struct UpdateService {
    job: Mutex<UpdateJob>,
}
static SERVICE: OnceLock<Arc<UpdateService>> = OnceLock::new();

pub fn service() -> Arc<UpdateService> {
    SERVICE
        .get_or_init(|| {
            let mut job = update_install::load_job();
            if job.active()
                && !job
                    .helper_pid
                    .map(update_install::process_alive)
                    .unwrap_or(false)
            {
                job.phase = "error".into();
                job.message = "上次更新未完成，请重试".into();
                let _ = update_install::save_job(&job);
            }
            Arc::new(UpdateService {
                job: Mutex::new(job),
            })
        })
        .clone()
}

pub fn build() -> &'static str {
    option_env!("Y_SYNC_VERSION").unwrap_or("dev")
}

impl UpdateService {
    fn change(&self, change: impl FnOnce(&mut UpdateJob)) -> Result<()> {
        let mut job = self.job.lock().unwrap();
        change(&mut job);
        update_install::save_job(&job)
    }

    fn phase(&self, phase: &str, message: &str) -> Result<()> {
        self.change(|job| {
            job.phase = phase.into();
            job.message = message.into();
        })
    }

    pub fn status(&self) -> serde_json::Value {
        let state = updater::load_update_state();
        let mut job = self.job.lock().unwrap();
        if job.phase == "restarting" {
            let saved = update_install::load_job();
            if !saved.active() {
                *job = saved;
            } else if !job
                .helper_pid
                .map(update_install::process_alive)
                .unwrap_or(false)
            {
                job.phase = "error".into();
                job.message = "更新助手已退出，请查看更新日志并重试".into();
                let _ = update_install::save_job(&job);
            }
        }
        let mut value = serde_json::to_value(updater::cached_result(build(), &state)).unwrap();
        value["auto_check"] = state.auto_check.into();
        value["job"] = serde_json::to_value(&*job).unwrap();
        value
    }

    pub fn check(self: &Arc<Self>, force: bool) -> Result<()> {
        if !force {
            let state = updater::load_update_state();
            let now = crate::api::now_millis() / 1000;
            if !state.auto_check
                || !updater::should_check(state.last_check_at, now, updater::CHECK_INTERVAL_SECS)
            {
                return Ok(());
            }
        }
        {
            let mut job = self.job.lock().unwrap();
            if job.active() {
                return Err(Error::Msg("已有更新任务正在运行".into()));
            }
            *job = UpdateJob {
                phase: "checking".into(),
                message: "正在检查更新…".into(),
                ..Default::default()
            };
            if let Err(error) = update_install::save_job(&job) {
                job.phase = "error".into();
                job.message = error.to_string();
                return Err(error);
            }
        }
        let me = self.clone();
        std::thread::spawn(move || match updater::check_update(build(), force) {
            Ok(check) => {
                let _ = me.phase(
                    if check.has_update {
                        "available"
                    } else {
                        "idle"
                    },
                    if check.has_update {
                        "发现可用更新"
                    } else {
                        "已是最新版本"
                    },
                );
            }
            Err(e) => {
                let _ = me.phase("error", &format!("检查更新失败：{e}"));
            }
        });
        Ok(())
    }

    pub fn apply(self: &Arc<Self>, daemon: Daemon) -> Result<()> {
        {
            let mut job = self.job.lock().unwrap();
            if job.active() {
                return Err(Error::Msg("已有更新任务正在运行".into()));
            }
            *job = UpdateJob {
                phase: "checking".into(),
                message: "正在确认更新版本…".into(),
                ..Default::default()
            };
            if let Err(error) = update_install::save_job(&job) {
                job.phase = "error".into();
                job.message = error.to_string();
                return Err(error);
            }
        }
        let me = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = me.install(&daemon) {
                daemon.state.set_update_maintenance(false);
                let _ = me.phase("error", &e.to_string());
            }
        });
        Ok(())
    }

    fn install(&self, daemon: &Daemon) -> Result<()> {
        let release = updater::fetch_latest_release(updater::DEFAULT_REPO)?;
        if !updater::is_newer(&release.tag_name, build()) {
            return self.phase("idle", "已是最新版本");
        }
        updater::remember_release(&release)?;
        let exe = std::env::current_exe()?;
        #[allow(unused_mut)]
        let mut app: Option<PathBuf> = None;
        #[cfg(target_os = "macos")]
        {
            app = exe
                .ancestors()
                .find(|p| p.extension().and_then(|x| x.to_str()) == Some("app"))
                .map(Path::to_path_buf);
            if app.is_none() && Path::new("/Applications/y-sync.app").is_dir() {
                app = Some(PathBuf::from("/Applications/y-sync.app"));
            }
        }
        let asset = if app.is_some() {
            updater::find_macos_dmg_asset(&release.assets, std::env::consts::ARCH)
                .ok_or_else(|| Error::Msg("此版本没有适合当前架构的桌面更新包".into()))?
        } else {
            let name = updater::find_cli_asset_name(std::env::consts::OS, std::env::consts::ARCH)
                .ok_or_else(|| Error::Msg("此平台暂不支持自动安装".into()))?;
            release
                .assets
                .iter()
                .find(|a| a.name == name)
                .ok_or_else(|| Error::Msg(format!("此版本缺少 {name}")))?
        };
        let checksum = updater::asset_checksum(&release, asset)?;
        let dir = ysync_core::config_dir()?
            .join("updates")
            .join(update_install::nonce());
        std::fs::create_dir_all(&dir)?;
        let download = dir.join("download");
        self.change(|job| {
            job.phase = "downloading".into();
            job.target_version = release.tag_name.clone();
            job.total_bytes = asset.size;
            job.message = "正在下载更新…".into();
        })?;
        let mut last_saved = Instant::now();
        updater::download_verified(asset, &checksum, &download, |done, total| {
            let mut job = self.job.lock().unwrap();
            job.downloaded_bytes = done;
            job.total_bytes = total;
            if last_saved.elapsed() >= Duration::from_millis(500) {
                let _ = update_install::save_job(&job);
                last_saved = Instant::now();
            }
        })?;
        self.phase("verifying", "下载校验通过，正在验证安装包…")?;
        let target = app.clone().unwrap_or_else(|| exe.clone());
        let staged;
        #[cfg(target_os = "macos")]
        if app.is_some() {
            staged = update_install::prepare_macos_app(&download, &target)?;
        } else {
            staged = stage_binary(&download, &target)?;
        }
        #[cfg(not(target_os = "macos"))]
        {
            staged = stage_binary(&download, &target)?;
        }
        let mut staged_guard = StagedGuard {
            path: staged.clone(),
            handed_off: false,
        };
        let staged_exe = if app.is_some() {
            staged.join("Contents/MacOS/ysyncd")
        } else {
            staged.clone()
        };
        let output = std::process::Command::new(&staged_exe)
            .arg("version")
            .output()?;
        let expected = release.tag_name.trim_start_matches('v');
        let version_output = String::from_utf8_lossy(&output.stdout);
        if !output.status.success()
            || !version_output
                .split_whitespace()
                .any(|v| v.trim_start_matches('v') == expected)
        {
            return Err(Error::Msg("更新包中的客户端版本与 Release 不一致".into()));
        }
        self.phase(
            "waiting_for_sync",
            "等待当前同步完成，随后自动安装并重新启动…",
        )?;
        daemon.state.set_update_maintenance(true);
        let wait = Instant::now();
        while daemon.state.syncing_count() > 0 {
            if wait.elapsed() > Duration::from_secs(300) {
                return Err(Error::Msg("等待同步结束超时，请稍后重试更新".into()));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let backup = target.with_file_name(format!(
            ".y-sync-backup-{}{}",
            update_install::nonce(),
            if app.is_some() { ".app" } else { "" }
        ));
        #[cfg(unix)]
        {
            // The helper's own executable is copied before any running file is replaced.
            let address = ysync_core::read_daemon_info()?.addr;
            let plan = InstallPlan {
                daemon_exe: app
                    .as_ref()
                    .map(|p| p.join("Contents/MacOS/ysyncd"))
                    .unwrap_or_else(|| exe.clone()),
                target,
                staged,
                backup,
                running_daemon_exe: exe,
                daemon_args: restart_args(std::env::args().skip(1), &address),
                daemon_pid: std::process::id(),
                desktop_pids: app
                    .as_deref()
                    .map(update_install::desktop_pids)
                    .unwrap_or_default(),
                app,
                health_url: format!("http://{address}/healthz"),
                version: release.tag_name,
            };
            self.phase("installing", "准备安装并重新启动…")?;
            let pid = update_install::launch_helper(&plan)?;
            self.change(|job| {
                job.phase = "restarting".into();
                job.helper_pid = Some(pid);
                job.message = "正在安装并重新启动客户端…".into();
            })?;
            staged_guard.handed_off = true;
        }
        #[cfg(not(unix))]
        {
            update_install::atomic_replace(&staged, &target, &backup)?;
            daemon.state.set_update_maintenance(false);
            self.phase("completed", "更新已安装，请手动重新启动客户端")?;
        }
        Ok(())
    }
}

fn restart_args(args: impl IntoIterator<Item = String>, address: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "-http" || arg == "--http" {
            args.next();
        } else if !arg.starts_with("-http=") && !arg.starts_with("--http=") {
            result.push(arg);
        }
    }
    result.extend(["-http".into(), address.into()]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_keeps_folder_and_interval_but_reuses_actual_random_port() {
        let args = [
            "daemon",
            "-http",
            "127.0.0.1:0",
            "-only",
            "project",
            "-interval",
            "5s",
        ];
        assert_eq!(
            restart_args(args.map(String::from), "127.0.0.1:12345"),
            [
                "daemon",
                "-only",
                "project",
                "-interval",
                "5s",
                "-http",
                "127.0.0.1:12345"
            ]
        );
    }
}

struct StagedGuard {
    path: PathBuf,
    handed_off: bool,
}
impl Drop for StagedGuard {
    fn drop(&mut self) {
        if !self.handed_off && self.path.exists() {
            if self.path.is_dir() {
                let _ = std::fs::remove_dir_all(&self.path);
            } else {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

fn stage_binary(download: &Path, target: &Path) -> Result<PathBuf> {
    let staged = target.with_file_name(format!(".ysync-update-{}", update_install::nonce()));
    std::fs::copy(download, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(staged)
}

pub fn start_periodic_checks() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(10));
        loop {
            let _ = service().check(false);
            std::thread::sleep(Duration::from_secs(3600));
        }
    });
}
