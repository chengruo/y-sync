//! 自动更新模块：支持每日（24小时周期）检查、手动立即检查、GitHub Release 抓取与版本比对。
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{config_dir, Error, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::sync::Mutex;

static CHECK_LOCK: Mutex<()> = Mutex::new(());
fn enabled() -> bool { true }

pub const DEFAULT_REPO: &str = "chengruo/y-sync";
/// 24 小时检查周期（秒）
pub const CHECK_INTERVAL_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateState {
    #[serde(default = "enabled")]
    pub auto_check: bool,
    #[serde(default)]
    pub last_check_at: i64,
    #[serde(default)]
    pub latest_version: String,
    #[serde(default)]
    pub release_url: String,
    #[serde(default)]
    pub release_notes: String,
    #[serde(default)]
    pub ignored_version: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self { auto_check: true, last_check_at: 0, latest_version: String::new(),
            release_url: String::new(), release_notes: String::new(), ignored_version: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReleaseAsset {
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReleaseInfo {
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub release_notes: String,
    pub last_check_at: i64,
}

/// 解析类似 "0.8.2", "v0.8.2", "0.8.10" 的版本号为纯数值段向量 [0, 8, 2]
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches('v').trim_start_matches('V');
    if s.is_empty() {
        return None;
    }
    // 截取 prerelease/build 标识（如 0.8.2-beta.1 -> 0.8.2）
    let main_part = s.split(['-', '+']).next()?;
    let parts: std::result::Result<Vec<u64>, _> = main_part.split('.').map(|p| p.parse::<u64>()).collect();
    parts.ok()
}

/// 语义化版本比对：返回 a 与 b 的大小关系
pub fn compare_version(a: &str, b: &str) -> Ordering {
    if let (Ok(a), Ok(b)) = (
        semver::Version::parse(a.trim().trim_start_matches(['v', 'V'])),
        semver::Version::parse(b.trim().trim_start_matches(['v', 'V'])),
    ) {
        return a.cmp_precedence(&b);
    }
    match (parse_version(a), parse_version(b)) {
        (Some(v1), Some(v2)) => {
            let max_len = v1.len().max(v2.len());
            for i in 0..max_len {
                let p1 = v1.get(i).copied().unwrap_or(0);
                let p2 = v2.get(i).copied().unwrap_or(0);
                match p1.cmp(&p2) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            Ordering::Equal
        }
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

/// 判断 remote 版本是否严格大于 current 版本
pub fn is_newer(remote: &str, current: &str) -> bool {
    let current = current.trim();
    if current.is_empty() || current == "dev" {
        // 开发模式下不自动判定为有更新（避免开发阶段被频繁打扰）
        return false;
    }
    compare_version(remote, current) == Ordering::Greater
}

/// 判断是否达到 24 小时检查周期
pub fn should_check(last_check_at: i64, now: i64, interval_secs: i64) -> bool {
    if last_check_at <= 0 {
        return true;
    }
    now.saturating_sub(last_check_at) >= interval_secs
}

/// 获取更新状态保存路径（如 ~/.config/y-sync/updater.json 或 Application Support/y-sync/updater.json）
pub fn updater_state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("updater.json"))
}

/// 加载本地更新状态，若不存在或损坏则返回默认空状态
pub fn load_update_state() -> UpdateState {
    let p = match updater_state_path() {
        Ok(p) => p,
        Err(_) => return UpdateState { auto_check: true, ..Default::default() },
    };
    if let Ok(b) = std::fs::read(&p) {
        if let Ok(s) = serde_json::from_slice(&b) {
            return s;
        }
    }
    UpdateState { auto_check: true, ..Default::default() }
}

/// 保存本地更新状态
pub fn save_update_state(state: &UpdateState) -> Result<()> {
    let p = updater_state_path()?;
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let b = serde_json::to_vec_pretty(state)?;
    let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, b)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// 请求 GitHub Latest Release API（超时 8 秒，包含 User-Agent 防拒）
pub fn fetch_latest_release(repo: &str) -> Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent("ysync-updater")
        .build()?;
    let resp = client.get(&url).send()?;
    if !resp.status().is_success() {
        return Err(Error::Msg(format!(
            "GitHub API 请求失败 (HTTP {})",
            resp.status()
        )));
    }
    let rel: ReleaseInfo = resp.json()?;
    Ok(rel)
}

/// 检查更新核心逻辑：
/// - 若 `force == false`，则检查是否满足 24 小时间隔；若不满足，直接复用已缓存状态，不发网络请求。
/// - 若满足间隔或 `force == true`，发起网络请求并更新持久化时间戳。
pub fn check_update(current_version: &str, force: bool) -> Result<UpdateCheckResult> {
    let _guard = CHECK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_update_state();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // 24小时节流过滤（非强制检查且未满 24 小时且已有缓存）
    if !force && (!state.auto_check || !should_check(state.last_check_at, now, CHECK_INTERVAL_SECS)) {
        return Ok(cached_result(current_version, &state));
    }

    // 触发网络拉取
    let release = fetch_latest_release(DEFAULT_REPO)?;
    if force { state.ignored_version = None; }
    state.last_check_at = now;
    state.latest_version = release.tag_name.clone();
    state.release_url = release.html_url.clone();
    state.release_notes = release.body.clone().unwrap_or_default();
    save_update_state(&state)?;

    let has_update = is_newer(&release.tag_name, current_version)
        && (force || state.ignored_version.as_deref() != Some(release.tag_name.as_str()));
    Ok(UpdateCheckResult {
        has_update,
        current_version: current_version.to_string(),
        latest_version: release.tag_name,
        release_url: release.html_url,
        release_notes: release.body.unwrap_or_default(),
        last_check_at: now,
    })
}

pub fn cached_result(current: &str, state: &UpdateState) -> UpdateCheckResult {
    UpdateCheckResult {
        has_update: is_newer(&state.latest_version, current)
            && state.ignored_version.as_deref() != Some(state.latest_version.as_str()),
        current_version: current.into(), latest_version: state.latest_version.clone(),
        release_url: state.release_url.clone(), release_notes: state.release_notes.clone(),
        last_check_at: state.last_check_at,
    }
}

pub fn remember_release(release: &ReleaseInfo) -> Result<()> {
    let _guard = CHECK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_update_state();
    state.last_check_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    state.latest_version = release.tag_name.clone();
    state.release_url = release.html_url.clone();
    state.release_notes = release.body.clone().unwrap_or_default();
    save_update_state(&state)
}

pub fn set_preferences(auto_check: Option<bool>, ignored: Option<String>) -> Result<()> {
    let _guard = CHECK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_update_state();
    if let Some(value) = auto_check { state.auto_check = value; }
    if let Some(value) = ignored { state.ignored_version = Some(value); }
    save_update_state(&state)
}

/// Prefer the digest returned by GitHub; older releases may supply SHA256SUMS.
pub fn asset_checksum(release: &ReleaseInfo, asset: &ReleaseAsset) -> Result<String> {
    if let Some(digest) = asset.digest.as_deref().and_then(|s| s.strip_prefix("sha256:")) {
        if valid_checksum(digest) { return Ok(digest.to_lowercase()); }
    }
    let manifest = release.assets.iter().find(|a| a.name == "SHA256SUMS")
        .ok_or_else(|| Error::Msg("此版本没有下载校验信息，请通过发布页手动安装".into()))?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(20))
        .user_agent("ysync-updater").build()?;
    let body = client.get(&manifest.browser_download_url).send()?.error_for_status()?.text()?;
    checksum_from_manifest(&body, &asset.name)
        .ok_or_else(|| Error::Msg(format!("SHA256SUMS 未包含 {} 的有效校验值", asset.name)))
}

fn valid_checksum(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn checksum_from_manifest(body: &str, filename: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        (valid_checksum(hash) && name == filename).then(|| hash.to_lowercase())
    })
}

pub fn download_verified(
    asset: &ReleaseAsset, expected: &str, target: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    if !valid_checksum(expected) { return Err(Error::Msg("无效的 SHA256 校验值".into())); }
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(600))
        .user_agent("ysync-updater").build()?;
    let mut response = client.get(&asset.browser_download_url).send()?.error_for_status()?;
    let total = response.content_length().unwrap_or(asset.size);
    let parent = target.parent().ok_or_else(|| Error::Msg("无效的下载目录".into()))?;
    std::fs::create_dir_all(parent)?;
    let temp = target.with_extension("part");
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        let mut hasher = Sha256::new();
        let mut downloaded = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = response.read(&mut buf)?;
            if n == 0 { break; }
            file.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            downloaded += n as u64;
            progress(downloaded, total);
        }
        if (asset.size > 0 && downloaded != asset.size) || (total > 0 && downloaded != total) {
            return Err(Error::Msg("下载文件长度不匹配".into()));
        }
        let got = format!("{:x}", hasher.finalize());
        if !got.eq_ignore_ascii_case(expected) { return Err(Error::Msg("下载文件 SHA256 校验失败".into())); }
        file.sync_all()?;
        std::fs::rename(&temp, target)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temp); }
    result
}

/// 流式下载 URL 内容并写入目标文件
pub fn download_file(url: &str, target_path: &Path) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent("ysync-updater")
        .build()?;
    let mut resp = client.get(url).send()?;
    if !resp.status().is_success() {
        return Err(Error::Msg(format!("下载失败: HTTP {}", resp.status())));
    }
    let tmp_path = target_path.with_extension("download_tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    std::io::copy(&mut resp, &mut file)?;
    file.sync_all()?;
    std::fs::rename(tmp_path, target_path)?;
    Ok(())
}

/// 根据操作系统与架构寻找对应资产
pub fn find_cli_asset_name(os: &str, arch: &str) -> Option<String> {
    let a = match arch {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        _ => return None,
    };
    match os {
        "macos" | "darwin" => Some(format!("ysyncd-darwin-{a}")),
        "linux" => Some(format!("ysyncd-linux-{a}")),
        "windows" => Some(format!("ysyncd-windows-{a}.exe")),
        _ => None,
    }
}

/// 匹配 macOS 桌面端的 DMG 资产（例如 y-sync_0.8.2_aarch64.dmg）
pub fn find_macos_dmg_asset<'a>(assets: &'a [ReleaseAsset], arch: &str) -> Option<&'a ReleaseAsset> {
    let a = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => "aarch64",
    };
    assets.iter().find(|it| it.name.ends_with(".dmg") && it.name.contains(a))
        .or_else(|| assets.iter().find(|it| it.name.ends_with(".dmg") && it.name.contains("universal")))
}

/// 自更新当前运行的可执行文件（CLI / ysync）
pub fn update_current_binary(asset: &ReleaseAsset, checksum: &str) -> Result<()> {
    let current = std::env::current_exe()?;
    let staged = current.with_file_name(format!(".ysync-new-{}", crate::update_install::nonce()));
    download_verified(asset, checksum, &staged, |_, _| {})?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let backup = current.with_file_name(format!(".ysync-old-{}", crate::update_install::nonce()));
    crate::update_install::atomic_replace(&staged, &current, &backup)
}

#[cfg(target_os = "macos")]
pub fn install_macos_desktop_dmg(dmg_path: &Path) -> Result<()> {
    let target = Path::new("/Applications/y-sync.app");
    let staged = crate::update_install::prepare_macos_app(dmg_path, target)?;
    let backup = target.with_file_name(format!(".y-sync-backup-{}.app", crate::update_install::nonce()));
    crate::update_install::atomic_replace(&staged, target, &backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_compare() {
        assert_eq!(compare_version("v0.8.2", "v0.8.1"), Ordering::Greater);
        assert_eq!(compare_version("v0.8.2", "0.8.2"), Ordering::Equal);
        assert_eq!(compare_version("0.8.10", "0.8.2"), Ordering::Greater);
        assert_eq!(compare_version("v1.0.0", "v0.9.9"), Ordering::Greater);
        assert_eq!(compare_version("v0.7.8", "v0.8.0"), Ordering::Less);
        assert_eq!(compare_version("0.8.0-beta.1", "0.8.0"), Ordering::Less);
    }

    #[test]
    fn test_is_newer() {
        assert!(is_newer("v0.8.3", "v0.8.2"));
        assert!(!is_newer("v0.8.2", "v0.8.2"));
        assert!(!is_newer("v0.8.1", "v0.8.2"));
        // dev 模式默认不提示更新
        assert!(!is_newer("v0.8.3", "dev"));
    }

    #[test]
    fn test_should_check_throttling() {
        let now = 100000;
        assert!(should_check(0, now, 86400));
        assert!(!should_check(now - 1000, now, 86400));
        assert!(should_check(now - 86400, now, 86400));
        assert!(should_check(now - 90000, now, 86400));
    }

    #[test]
    fn prerelease_build_and_skip_preferences() {
        assert_eq!(compare_version("1.0.0-beta.2", "1.0.0-beta.10"), Ordering::Less);
        assert_eq!(compare_version("1.0.0+build.2", "1.0.0+build.1"), Ordering::Equal);
        let mut state: UpdateState = serde_json::from_str(r#"{"latest_version":"v2.0.0"}"#).unwrap();
        assert!(state.auto_check);
        assert!(cached_result("1.0.0", &state).has_update);
        state.ignored_version = Some("v2.0.0".into());
        assert!(!cached_result("1.0.0", &state).has_update);
        state.latest_version = "v2.1.0".into();
        assert!(cached_result("1.0.0", &state).has_update);
        let arm_only = vec![ReleaseAsset { name: "y-sync_1.0.0_aarch64.dmg".into(), ..Default::default() }];
        assert!(find_macos_dmg_asset(&arm_only, "x86_64").is_none());
    }

    #[test]
    fn test_asset_name_match() {
        assert_eq!(
            find_cli_asset_name("macos", "arm64"),
            Some("ysyncd-darwin-arm64".to_string())
        );
        assert_eq!(
            find_cli_asset_name("linux", "x86_64"),
            Some("ysyncd-linux-amd64".to_string())
        );
    }
}
