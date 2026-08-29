//! 内容寻址 blob 存储（SR4）：SHA-256 命名、两层目录散列、临时文件 + 原子 rename。
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

use crate::util::{hex, valid_hash};

pub struct BlobStore {
    pub(crate) root: PathBuf, // 数据目录
}

pub struct PutResult {
    pub hash: String,
    pub dedup: bool,
    pub size: i64,
}

impl BlobStore {
    pub fn new(data_dir: &Path) -> Self {
        BlobStore {
            root: data_dir.to_path_buf(),
        }
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.root
            .join("blobs")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(hash)
    }

    /// 流式写入并校验哈希（want_hash 为空则不校验）；去重命中返回 dedup=true。
    pub fn put(&self, mut r: impl Read, want_hash: &str) -> std::io::Result<PutResult> {
        use sha2::Digest;
        let tmp_dir = self.root.join("tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        // 原子序号保证并发 PUT 的临时文件唯一（nanos 在快速循环里会撞名）
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = tmp_dir.join(format!(
            "upload-{}-{}-{}",
            std::process::id(),
            seq,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let mut tmp = std::fs::File::create(&tmp_path)?;
        tmp.write_all(b"")?; // 保持 Write trait 导入
        let mut hasher = sha2::Sha256::new();
        let mut size = 0i64;
        let mut buf = [0u8; 256 * 1024];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tmp.write_all(&buf[..n])?;
            size += n as i64;
        }
        tmp.sync_all()?;
        drop(tmp);
        let got = hex(&hasher.finalize());
        if !want_hash.is_empty() && got != want_hash {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                crate::util::status_reason(400),
            ));
        }
        let dst = self.blob_path(&got);
        if dst.exists() {
            let _ = std::fs::remove_file(&tmp_path);
            return Ok(PutResult {
                hash: got,
                dedup: true,
                size,
            });
        }
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::rename(&tmp_path, &dst)?;
        Ok(PutResult {
            hash: got,
            dedup: false,
            size,
        })
    }

    /// 块文件路径（重组装用）；不存在返回 None。
    pub fn blob_path_of(&self, hash: &str) -> Option<PathBuf> {
        if !valid_hash(hash) {
            return None;
        }
        let p = self.blob_path(hash);
        p.exists().then_some(p)
    }

    pub fn open(&self, hash: &str) -> std::io::Result<std::fs::File> {
        if !valid_hash(hash) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid hash",
            ));
        }
        std::fs::File::open(self.blob_path(hash))
    }

    pub fn exists(&self, hash: &str) -> bool {
        valid_hash(hash) && self.blob_path(hash).exists()
    }

    pub fn remove(&self, hash: &str) {
        if valid_hash(hash) {
            let _ = std::fs::remove_file(self.blob_path(hash));
        }
    }
}
