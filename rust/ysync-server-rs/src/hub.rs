//! WebSocket 通知 Hub（§4.2）：按用户分组，ops 提交后推送 cursor 事件。
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

pub struct WsConn {
    pub user_id: i64,
    write: Mutex<TcpStream>,
}

pub struct Hub {
    by_user: Mutex<HashMap<i64, Vec<Arc<WsConn>>>>,
}

impl Hub {
    pub fn new() -> Self {
        Hub {
            by_user: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, user_id: i64, stream: TcpStream) -> Arc<WsConn> {
        let conn = Arc::new(WsConn {
            user_id,
            write: Mutex::new(stream),
        });
        let mut g = self.by_user.lock().unwrap();
        g.entry(user_id).or_default().push(conn.clone());
        conn
    }

    pub fn unregister(&self, conn: &Arc<WsConn>) {
        let mut g = self.by_user.lock().unwrap();
        if let Some(list) = g.get_mut(&conn.user_id) {
            list.retain(|c| !Arc::ptr_eq(c, conn));
            if list.is_empty() {
                g.remove(&conn.user_id);
            }
        }
    }

    /// 推送文本帧（慢连接直接丢弃，与 Go 版一致）。
    pub fn notify(&self, user_id: i64, msg: &str) {
        let g = self.by_user.lock().unwrap();
        if let Some(list) = g.get(&user_id) {
            for c in list {
                if let Ok(mut w) = c.write.lock() {
                    let _ = w.write_all(&encode_text_frame(msg.as_bytes()));
                    let _ = w.flush();
                }
            }
        }
    }
}

/// 服务端 → 客户端文本帧（不掩码）。
fn encode_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x81u8]; // FIN + text
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= 0xFFFF {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// 读取并丢弃一个客户端帧（客户端帧必须掩码；仅用于保活检测）。
pub fn read_frame_skip(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr)?;
    let len = (hdr[1] & 0x7F) as usize;
    let len = if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b)?;
        u16::from_be_bytes(b) as usize
    } else if len == 127 {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b)?;
        u64::from_be_bytes(b) as usize
    } else {
        len
    };
    let mask = (hdr[1] & 0x80) != 0;
    let mut mask_key = [0u8; 4];
    if mask {
        stream.read_exact(&mut mask_key)?;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    if mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask_key[i % 4];
        }
    }
    Ok(())
}
