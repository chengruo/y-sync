use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use ysync_core::updater::{self, ReleaseAsset, ReleaseInfo};

fn fixture(body: Vec<u8>, claimed_size: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {claimed_size}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    (format!("http://{addr}/asset"), handle)
}
fn temp() -> PathBuf {
    std::env::temp_dir().join(format!(
        "ysync-download-{}",
        ysync_core::update_install::nonce()
    ))
}

#[test]
fn verified_download_reports_progress_and_promotes_only_matching_content() {
    let body = vec![42; 180_000];
    let hash = format!("{:x}", Sha256::digest(&body));
    let (url, server) = fixture(body.clone(), body.len());
    let target = temp().join("client");
    let asset = ReleaseAsset {
        name: "client".into(),
        browser_download_url: url,
        size: body.len() as u64,
        ..Default::default()
    };
    let mut progress = Vec::new();
    updater::download_verified(&asset, &hash, &target, |done, total| {
        progress.push((done, total))
    })
    .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), body);
    assert_eq!(progress.last(), Some(&(180_000, 180_000)));
    assert!(progress.windows(2).all(|w| w[1].0 > w[0].0));
    assert!(!target.with_extension("part").exists());
    server.join().unwrap();
    std::fs::remove_dir_all(target.parent().unwrap()).unwrap();
}

#[test]
fn corrupt_or_truncated_download_cannot_replace_existing_content() {
    for truncated in [false, true] {
        let body = b"new content".to_vec();
        let (url, server) = fixture(body.clone(), if truncated { 200 } else { body.len() });
        let target = temp().join("client");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "working version").unwrap();
        let asset = ReleaseAsset {
            name: "client".into(),
            browser_download_url: url,
            size: body.len() as u64,
            ..Default::default()
        };
        assert!(updater::download_verified(&asset, &"0".repeat(64), &target, |_, _| {}).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "working version");
        assert!(!target.with_extension("part").exists());
        server.join().unwrap();
        std::fs::remove_dir_all(target.parent().unwrap()).unwrap();
    }
}

#[test]
fn checksum_resolution_accepts_digest_or_exact_manifest_entry() {
    let hash = "a".repeat(64);
    let asset = ReleaseAsset {
        name: "client file.dmg".into(),
        digest: Some(format!("sha256:{hash}")),
        ..Default::default()
    };
    assert_eq!(
        updater::asset_checksum(&ReleaseInfo::default(), &asset).unwrap(),
        hash
    );
    let manifest = format!("{}  other\n{hash}  client file.dmg\n", "b".repeat(64));
    let (url, server) = fixture(manifest.as_bytes().to_vec(), manifest.len());
    let release = ReleaseInfo {
        assets: vec![ReleaseAsset {
            name: "SHA256SUMS".into(),
            browser_download_url: url,
            ..Default::default()
        }],
        ..Default::default()
    };
    let asset = ReleaseAsset {
        name: asset.name,
        ..Default::default()
    };
    assert_eq!(updater::asset_checksum(&release, &asset).unwrap(), hash);
    server.join().unwrap();
    assert!(updater::asset_checksum(&ReleaseInfo::default(), &asset).is_err());
    assert!(updater::checksum_from_manifest(&manifest, "client").is_none());
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "set YSYNC_TEST_DMG to a locally built signed DMG"]
fn desktop_dmg_is_staged_verified_and_reversible() {
    use ysync_core::update_install::{atomic_replace, prepare_macos_app, rollback};
    let dmg =
        PathBuf::from(std::env::var_os("YSYNC_TEST_DMG").expect("YSYNC_TEST_DMG is required"));
    let dir = temp();
    let target = dir.join("y-sync.app");
    let backup = dir.join("old.app");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("old-marker"), "keep").unwrap();
    let staged = prepare_macos_app(&dmg, &target).unwrap();
    assert!(
        target.join("old-marker").exists(),
        "staging must not remove installed content"
    );
    assert!(staged.join("Contents/MacOS/ysyncd").is_file());
    atomic_replace(&staged, &target, &backup).unwrap();
    assert!(target.join("Contents/MacOS/ysyncd").is_file());
    assert!(backup.join("old-marker").exists());
    rollback(&target, &backup).unwrap();
    assert!(target.join("old-marker").exists());
    std::fs::remove_dir_all(dir).unwrap();
}
