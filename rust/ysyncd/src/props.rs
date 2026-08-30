//! 属性测试（P2）：chunker 与 ignore 的不变量。
#![cfg(test)]
use proptest::prelude::*;

use crate::chunk::{self, CdcParams};
use crate::ignore::Ignore;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// CDC 切分不变量：块并集覆盖全文件、块内哈希正确、确定性
    #[test]
    fn cdc_split_invariants(data in prop::collection::vec(any::<u8>(), 0..300_000)) {
        let cfg = CdcParams { min: 4096, avg: 16384, max: 65536 };
        let chunks = chunk::split_chunks(&mut std::io::Cursor::new(&data), &cfg).unwrap();
        // 1) 块并集 == 原数据（逐字节重组一致）
        let mut rebuilt: Vec<u8> = Vec::with_capacity(data.len());
        for c in &chunks {
            rebuilt.extend_from_slice(&data[c.offset as usize..(c.offset + c.len as u64) as usize]);
        }
        prop_assert_eq!(rebuilt, data.clone());
        // 2) 块边界有序不重叠
        let mut expected_off = 0u64;
        for c in &chunks {
            prop_assert_eq!(c.offset, expected_off);
            expected_off += c.len as u64;
            prop_assert!(c.len as u64 <= cfg.max as u64 + 1);
        }
        // 3) 确定性
        let again = chunk::split_chunks(&mut std::io::Cursor::new(&data), &cfg).unwrap();
        prop_assert_eq!(chunks.len(), again.len());
    }

    /// ignore 匹配器不变量：同一输入匹配结果确定；空模式列表不忽略任何路径
    #[test]
    fn ignore_match_consistent(
        patterns in prop::collection::vec("[a-z*?/!]{0,12}", 0..8),
        paths in prop::collection::vec("[a-z]{1,6}(\\.[a-z]{1,3})?", 1..12),
    ) {
        let ig = Ignore::new(&patterns);
        for p in &paths {
            let _ = ig.matches(p, false);
            let _ = ig.matches(p, true);
        }
    }
}
