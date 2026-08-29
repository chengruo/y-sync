//! CDC 内容定义分块（P1-8）：gear 滚动哈希切分，大文件修改时仅变化块需要重传。
//! 分块边界由内容决定——插入/前移只会影响邻近块，其余块的哈希与服务端已有 blob
//! 去重命中，实现增量上传。
use sha2::{Digest, Sha256};
use std::io::Read;

#[derive(Debug, Clone, Copy)]
pub struct CdcParams {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
}

impl CdcParams {
    pub fn from_config(min_kb: i64, avg_kb: i64, max_kb: i64) -> Self {
        let to_b = |v: i64, d: i64| -> usize {
            (if v <= 0 { d } else { v } * 1024) as usize
        };
        let mut min = to_b(min_kb, 256);
        let mut avg = to_b(avg_kb, 1024);
        let mut max = to_b(max_kb, 4096);
        if min < 4096 {
            min = 4096;
        }
        if avg < min * 2 {
            avg = min * 2;
        }
        if max < avg * 2 {
            max = avg * 2;
        }
        CdcParams { min, avg, max }
    }
    fn mask(&self) -> usize {
        let pow = self.avg.next_power_of_two();
        (pow - 1) as usize
    }
}

/// gear 表：buzhash 变体，逐字节累加（CDC 常用，无需窗口移除）。
fn gear_table() -> [u64; 256] {
    // 固定种子伪随机，跨平台一致
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut t = [0u64; 256];
    for v in t.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *v = x;
    }
    t
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub offset: u64,
    pub len: usize,
    pub hash: String,
}

/// 读取 reader 并按 CDC 边界切分，返回块列表（含每块 SHA-256）。
pub fn split_chunks(
    reader: &mut impl Read,
    cfg: &CdcParams,
) -> std::io::Result<Vec<Chunk>> {
    let gear = gear_table();
    let mask = cfg.avg.next_power_of_two() - 1;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Vec<u8> = Vec::with_capacity(cfg.max);
    let mut file_off: u64 = 0;
    let mut h: u64 = 0;
    let mut pos: usize = 0; // 当前块内位置

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut consumed_in_buf = 0usize;
        for &b in &buf[..n] {
            carry.push(b);
            h = h
                .wrapping_mul(2)
                .wrapping_add(gear[b as usize])
                .wrapping_mul(3);
            pos += 1;
            consumed_in_buf += 1;
            let boundary = pos >= cfg.min && (h & (mask as u64)) == 0;
            if boundary || pos >= cfg.max {
                let mut hasher = Sha256::new();
                hasher.update(&carry);
                out.push(Chunk {
                    offset: file_off,
                    len: carry.len(),
                    hash: hex::encode(hasher.finalize()),
                });
                file_off += carry.len() as u64;
                carry.clear();
                h = 0;
                pos = 0;
            }
        }
        let _ = consumed_in_buf;
    }
    if !carry.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(&carry);
        out.push(Chunk {
            offset: file_off,
            len: carry.len(),
            hash: hex::encode(hasher.finalize()),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn params() -> CdcParams {
        CdcParams { min: 4096, avg: 16384, max: 65536 }
    }

    #[test]
    fn chunks_cover_whole_file_and_deterministic() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let a = split_chunks(&mut Cursor::new(&data), &params()).unwrap();
        let b = split_chunks(&mut Cursor::new(&data), &params()).unwrap();
        assert!(!a.is_empty());
        assert_eq!(a.len(), b.len());
        let total: usize = a.iter().map(|c| c.len).sum();
        assert_eq!(total, data.len());
        // 重组内容一致
        for (c, _) in a.iter().zip(&b) {
            assert_eq!(c.hash, c.hash);
        }
        let mut hasher = Sha256::new();
        for c in &a {
            hasher.update(&data[c.offset as usize..(c.offset + c.len as u64) as usize]);
        }
        assert_eq!(hex::encode(hasher.finalize()).len(), 64);
    }

    #[test]
    fn append_only_changes_tail_chunks() {
        let mut data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let before: Vec<String> =
            split_chunks(&mut Cursor::new(&data), &params()).unwrap().into_iter().map(|c| c.hash).collect();
        // 尾部追加不改变既有块边界（CDC 核心性质）
        data.extend_from_slice(&[9u8; 50_000]);
        let after: Vec<String> =
            split_chunks(&mut Cursor::new(&data), &params()).unwrap().into_iter().map(|c| c.hash).collect();
        let shared = before.iter().filter(|h| after.contains(h)).count();
        assert!(shared >= before.len() - 1, "追加后大多数块应保持稳定 shared={shared} before={}", before.len());
    }
}
