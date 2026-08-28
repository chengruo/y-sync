//! 分块上传会话（FR-S11）：内存态 + tmp 稀疏文件；重启后会话失效（客户端重建）。
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::blob::BlobStore;
use crate::util::{hex, now_millis};

pub struct Session {
    pub size: i64,
    pub sha256: String,
    pub chunk: i64,
    pub path: PathBuf,
    pub received: HashSet<i64>,
}

impl Session {
    pub fn total_chunks(&self) -> i64 {
        let mut n = self.size / self.chunk;
        if self.size % self.chunk != 0 {
            n += 1;
        }
        n
    }
}

pub struct UploadManager {
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    tmp_dir: PathBuf,
}

pub struct UploadCreate {
    pub id: String,
    pub received: Vec<i64>,
}

impl UploadManager {
    pub fn new(tmp_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&tmp_dir);
        UploadManager {
            sessions: Mutex::new(HashMap::new()),
            tmp_dir,
        }
    }

    pub fn create(&self, size: i64, sha256: &str, chunk: i64) -> Result<UploadCreate, String> {
        if size <= 0 || chunk <= 0 || size > 512i64 << 30 {
            return Err("bad size/chunk".into());
        }
        let raw: Vec<u8> = (0..16).map(|_| rand_byte()).collect();
        let id = hex(&raw);
        let path = self.tmp_dir.join(format!("upload-{id}"));
        let file = std::fs::File::create(&path).map_err(|e| format!("{e}"))?;
        file.set_len(size as u64).map_err(|e| format!("{e}"))?;
        let received_empty: Vec<i64> = Vec::new();
        let s = Arc::new(Mutex::new(Session {
            size,
            sha256: sha256.to_string(),
            chunk,
            path,
            received: HashSet::new(),
        }));
        self.sessions.lock().unwrap().insert(id.clone(), s.clone());
        Ok(UploadCreate {
            id,
            received: received_empty,
        })
    }

    pub fn get(&self, id: &str) -> Option<Arc<Mutex<Session>>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn drop_session(&self, id: &str) {
        if let Some(s) = self.sessions.lock().unwrap().remove(id) {
            let path = s.lock().unwrap().path.clone();
            let _ = std::fs::remove_file(path);
        }
    }
}

fn rand_byte() -> u8 {
    use std::io::Read;
    let mut b = [0u8; 1];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut b).is_ok() {
            return b[0];
        }
    }
    (now_millis() % 256) as u8
}
