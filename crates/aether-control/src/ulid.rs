//! ULID 生成（M1-05）：核心侧生成事件 ID（delta 合并事件、降级 `error` 事件）。
//!
//! 规格（公开规范）：48 位毫秒时间戳 + 80 位随机数，Crockford Base32 编码为 26 字符；
//! 首字符 ≤ `7`（128 位值装进 130 位编码空间）。
//!
//! 熵源：`SystemTime` 纳秒 + 进程内单调计数器，经标准库 `RandomState`（OS 种子的 SipHash）
//! 散列出 80 位随机段。**不引入新依赖**（AGENTS §2.10）；本模块不承担密码学用途。
//!
//! `unsafe` 工作区禁用；本模块只用标准库安全接口。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Crockford Base32 字母表（ULID 规范）。
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 进程内单调计数器（保证同一时钟刻度内不同调用输入不同）。
static COUNTER: AtomicU64 = AtomicU64::new(1);

/// 进程级散列种子（OS 熵种子；进程重启后不同）。
static SEED: OnceLock<RandomState> = OnceLock::new();

/// 生成一个新的 ULID 字符串（26 字符，Crockford Base32，大写）。
pub(crate) fn generate() -> String {
    let state = SEED.get_or_init(RandomState::default);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    // 两路散列 → 128 位熵；取前 80 位作为随机段。
    let mut first = state.build_hasher();
    first.write_u64(nanos);
    first.write_u64(counter);
    let high = first.finish();

    let mut second = state.build_hasher();
    second.write_u64(!nanos);
    second.write_u64(counter.rotate_left(17));
    let low = second.finish();

    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
        .min(u128::from(u64::MAX));
    let millis = (millis as u64) & 0x0000_FFFF_FFFF_FFFF;

    let value: u128 = (u128::from(millis) << 80) | (u128::from(high) << 16) | u128::from(low >> 48);

    let mut out = [0u8; 26];
    let mut cursor = value;
    for index in (0..out.len()).rev() {
        out[index] = CROCKFORD[(cursor & 0x1F) as usize];
        cursor >>= 5;
    }
    // 字母表为 ASCII：逐字节转 char 恒成功，不使用 unwrap / 默认值吞错。
    let mut text = String::with_capacity(out.len());
    for byte in out {
        text.push(char::from(byte));
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn format_matches_ulid_spec() {
        for _ in 0..1_000 {
            let id = generate();
            assert_eq!(id.len(), 26, "ULID 必须为 26 字符: {id}");
            assert!(
                id.bytes().all(|byte| CROCKFORD.contains(&byte)),
                "仅允许 Crockford Base32: {id}"
            );
            assert!(
                id.bytes().next().is_some_and(|first| first <= b'7'),
                "首字符必须 ≤ 7（128 位装 130 位空间）: {id}"
            );
        }
    }

    #[test]
    fn ids_are_unique_and_time_prefixed() {
        let mut seen = HashSet::new();
        for _ in 0..50_000 {
            let id = generate();
            assert!(seen.insert(id.clone()), "ULID 必须唯一: {id}");
            // 时间前缀（前 10 字符）为 Crockford Base32，解码后应为 48 位毫秒。
            let prefix = &id[..10];
            assert!(
                prefix.bytes().all(|byte| CROCKFORD.contains(&byte)),
                "时间前缀必须为 Crockford Base32: {id}"
            );
        }
    }

    #[test]
    fn counter_is_monotonic() {
        let first = COUNTER.load(Ordering::Relaxed);
        let _ = generate();
        let second = COUNTER.load(Ordering::Relaxed);
        assert!(second > first, "计数器必须递增");
    }
}
