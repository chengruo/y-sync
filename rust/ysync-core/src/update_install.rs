//! Staged update installation and detached restart/rollback helper.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateJob {
    pub phase: String,
    pub target_version: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub message: String,
    pub helper_pid: Option<u32>,
}

impl UpdateJob {
    pub fn active(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "checking"
                | "downloading"
                | "verifying"
                | "waiting_for_sync"
                | "installing"
                | "restarting"
        )
    }
}

pub fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub fn job_path() -> Result<PathBuf> {
    Ok(crate::config_dir()?.join("update-job.json"))
}

pub fn load_job() -> UpdateJob {
    job_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_job(job: &UpdateJob) -> Result<()> {
    let path = job_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let temp = path.with_extension(format!("{}.tmp", nonce()));
    std::fs::write(&temp, serde_json::to_vec(job)?)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallPlan {
    pub target: PathBuf,
    pub staged: PathBuf,
    pub backup: PathBuf,
    pub daemon_exe: PathBuf,
    pub running_daemon_exe: PathBuf,
    pub daemon_args: Vec<String>,
    pub daemon_pid: u32,
    pub desktop_pids: Vec<u32>,
    pub app: Option<PathBuf>,
    pub health_url: String,
    pub version: String,
}

/// Both paths must be siblings: the promotion and rollback are filesystem renames.
pub fn atomic_replace(staged: &Path, target: &Path, backup: &Path) -> Result<()> {
    if staged.parent() != target.parent() || backup.parent() != target.parent() {
        return Err(Error::Msg("更新暂存目录必须与安装目标位于同一目录".into()));
    }
    if backup.exists() {
        return Err(Error::Msg("更新备份路径已存在".into()));
    }
    std::fs::symlink_metadata(staged)?;
    std::fs::rename(target, backup)?;
    if let Err(e) = std::fs::rename(staged, target) {
        std::fs::rename(backup, target)
            .map_err(|rollback| Error::Msg(format!("安装失败：{e}；恢复旧版本失败：{rollback}")))?;
        return Err(e.into());
    }
    Ok(())
}

pub fn rollback(target: &Path, backup: &Path) -> Result<()> {
    if backup.parent() != target.parent() || !backup.exists() {
        return Err(Error::Msg("无可用的更新备份".into()));
    }
    let failed = target.with_file_name(format!(".y-sync-failed-{}", nonce()));
    if target.exists() {
        std::fs::rename(target, &failed)?;
    }
    if let Err(e) = std::fs::rename(backup, target) {
        if failed.exists() {
            let _ = std::fs::rename(&failed, target);
        }
        return Err(e.into());
    }
    // Keep failed content for diagnosis; never delete the only working backup.
    Ok(())
}

pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid <= 1 || unsafe { libc::kill(pid as i32, 0) != 0 } {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            return std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit_once(") ").map(|(_, r)| !r.starts_with('Z')))
                .unwrap_or(false);
        }
        #[cfg(not(target_os = "linux"))]
        {
            return Command::new("/bin/ps")
                .args(["-p", &pid.to_string(), "-o", "stat="])
                .output()
                .map(|o| {
                    o.status.success()
                        && !String::from_utf8_lossy(&o.stdout).trim().starts_with('Z')
                })
                .unwrap_or(false);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(unix)]
fn process_path(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("/bin/ps")
            .args(["-ww", "-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
    }
}

#[cfg(unix)]
pub fn desktop_pids(app: &Path) -> Vec<u32> {
    let expected = app.join("Contents/MacOS/ysync-desktop");
    let Ok(output) = Command::new("/bin/ps")
        .args(["-ww", "-axo", "pid=,comm="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, path) = line.trim().split_once(char::is_whitespace)?;
            (Path::new(path.trim()) == expected)
                .then(|| pid.parse::<u32>().ok())
                .flatten()
        })
        .collect()
}

#[cfg(unix)]
fn stop_matching(pid: u32, expected: &Path) -> Result<()> {
    if !process_alive(pid) {
        return Ok(());
    }
    if pid == std::process::id() || process_path(pid).as_deref() != Some(expected) {
        return Err(Error::Msg(format!("进程 {pid} 路径不匹配，已停止更新")));
    }
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    for _ in 0..200 {
        if !process_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(Error::Msg(format!("等待进程 {pid} 退出超时")))
}

pub fn launch_helper(plan: &InstallPlan) -> Result<u32> {
    let dir = crate::config_dir()?.join("updates").join(nonce());
    std::fs::create_dir_all(&dir)?;
    let helper = dir.join(if cfg!(windows) {
        "update-helper.exe"
    } else {
        "update-helper"
    });
    std::fs::copy(std::env::current_exe()?, &helper)?;
    let plan_file = dir.join("plan.json");
    std::fs::write(&plan_file, serde_json::to_vec(plan)?)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::config_dir()?.join("update.log"))?;
    let mut command = Command::new(&helper);
    command
        .arg("__update-helper")
        .arg(&plan_file)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    Ok(command.spawn()?.id())
}

fn start_daemon(plan: &InstallPlan) -> Result<std::process::Child> {
    start_daemon_exe(plan, &plan.daemon_exe)
}

fn start_daemon_exe(plan: &InstallPlan, executable: &Path) -> Result<std::process::Child> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::config_dir()?.join("update.log"))?;
    Ok(Command::new(executable)
        .args(&plan.daemon_args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?)
}

fn restart_previous(plan: &InstallPlan, error: &dyn std::fmt::Display) -> Error {
    let result = (|| -> Result<()> {
        let mut child = start_daemon_exe(plan, &plan.running_daemon_exe)?;
        wait_for_health(&plan.health_url, &mut child)?;
        open_app(plan)
    })();
    match result {
        Ok(()) => Error::Msg(format!("{error}；已恢复旧版本并重新启动")),
        Err(recovery) => Error::Msg(format!("{error}；旧版本重新启动失败：{recovery}")),
    }
}

fn wait_for_health(url: &str, child: &mut std::process::Child) -> Result<()> {
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(1))
        .no_proxy()
        .build()?;
    for _ in 0..40 {
        if child.try_wait()?.is_some() {
            return Err(Error::Msg("新版本守护进程启动失败".into()));
        }
        if http
            .get(url)
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
            && crate::read_daemon_info()
                .map(|info| info.pid == child.id() as i32)
                .unwrap_or(false)
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(Error::Msg("新版本健康检查超时".into()))
}

fn open_app(plan: &InstallPlan) -> Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(app) = &plan.app {
        if !Command::new("/usr/bin/open").arg(app).status()?.success() {
            return Err(Error::Msg("重新启动桌面应用失败".into()));
        }
    }
    let _ = plan;
    Ok(())
}

/// Invoked only by the detached internal CLI command, never by a UI request thread.
#[cfg(unix)]
pub fn run_helper(plan_file: &Path) -> Result<()> {
    let plan: InstallPlan = serde_json::from_slice(&std::fs::read(plan_file)?)?;
    // Let the initiating HTTP response reach the UI before stopping the old process.
    std::thread::sleep(Duration::from_millis(700));
    let mut job = load_job();
    if job.phase != "restarting" || job.helper_pid != Some(std::process::id()) {
        return Err(Error::Msg("更新交接未完成，已保留当前版本".into()));
    }
    let result = (|| -> Result<()> {
        if let Some(app) = &plan.app {
            let desktop = app.join("Contents/MacOS/ysync-desktop");
            for pid in &plan.desktop_pids {
                stop_matching(*pid, &desktop)?;
            }
        }
        stop_matching(plan.daemon_pid, &plan.running_daemon_exe)?;
        if let Err(e) = atomic_replace(&plan.staged, &plan.target, &plan.backup) {
            return Err(restart_previous(&plan, &e));
        }
        let mut child = match start_daemon(&plan) {
            Ok(child) => child,
            Err(e) => {
                rollback(&plan.target, &plan.backup)?;
                return Err(restart_previous(&plan, &e));
            }
        };
        if let Err(e) = wait_for_health(&plan.health_url, &mut child).and_then(|_| open_app(&plan))
        {
            let _ = child.kill();
            let _ = child.wait();
            rollback(&plan.target, &plan.backup)?;
            return Err(restart_previous(&plan, &e));
        }
        Ok(())
    })();
    job.phase = if result.is_ok() { "completed" } else { "error" }.into();
    job.helper_pid = None;
    job.message = match &result {
        Ok(()) => format!("已更新至 {}", plan.version),
        Err(e) => e.to_string(),
    };
    save_job(&job)?;
    result
}

#[cfg(not(unix))]
pub fn run_helper(_plan_file: &Path) -> Result<()> {
    Err(Error::Msg("此平台请在下载完成后手动重启客户端".into()))
}

#[cfg(target_os = "macos")]
pub fn prepare_macos_app(dmg: &Path, target: &Path) -> Result<PathBuf> {
    let mount = std::env::temp_dir().join(format!("ysync-mount-{}", nonce()));
    std::fs::create_dir(&mount)?;
    struct Mount(PathBuf);
    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = Command::new("hdiutil").arg("detach").arg(&self.0).output();
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let output = Command::new("hdiutil")
        .arg("attach")
        .arg(dmg)
        .args(["-readonly", "-nobrowse", "-mountpoint"])
        .arg(&mount)
        .output()?;
    if !output.status.success() {
        let _ = std::fs::remove_dir(&mount);
        return Err(Error::Msg(format!(
            "挂载更新包失败：{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let _mount = Mount(mount.clone());
    let source = mount.join("y-sync.app");
    let identifier = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleIdentifier"])
        .arg(source.join("Contents/Info.plist"))
        .output()?;
    if !identifier.status.success()
        || String::from_utf8_lossy(&identifier.stdout).trim() != "app.ysync.desktop"
    {
        return Err(Error::Msg("更新包的应用标识不匹配".into()));
    }
    verify_app(&source)?;
    let staged = target.with_file_name(format!(".y-sync-update-{}.app", nonce()));
    let result = (|| -> Result<()> {
        if !Command::new("/usr/bin/ditto")
            .arg(&source)
            .arg(&staged)
            .status()?
            .success()
        {
            return Err(Error::Msg("无法写入应用目录，请检查安装目录权限".into()));
        }
        verify_app(&staged)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }
    Ok(staged)
}

#[cfg(target_os = "macos")]
fn verify_app(app: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .output()?;
    if !output.status.success() {
        return Err(Error::Msg(format!(
            "应用签名校验失败：{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn promotion_and_rollback_preserve_working_version() {
        let dir = std::env::temp_dir().join(nonce());
        std::fs::create_dir(&dir).unwrap();
        let target = dir.join("app");
        let staged = dir.join("new");
        let backup = dir.join("backup");
        std::fs::write(&target, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        atomic_replace(&staged, &target, &backup).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "old");
        rollback(&target, &backup).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_stage_and_existing_backup_do_not_remove_target() {
        let dir = std::env::temp_dir().join(nonce());
        std::fs::create_dir(&dir).unwrap();
        let target = dir.join("app");
        let backup = dir.join("backup");
        std::fs::write(&target, "old").unwrap();
        assert!(atomic_replace(&dir.join("missing"), &target, &backup).is_err());
        assert!(target.exists());
        std::fs::write(&backup, "earlier").unwrap();
        std::fs::write(dir.join("new"), "new").unwrap();
        assert!(atomic_replace(&dir.join("new"), &target, &backup).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
