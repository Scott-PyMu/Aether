//! ULID 生成与解析（M1-05）：核心生成事件 ID（delta 合并事件、降级 `error` 事件）。
//!
//! 实现恢复设计选型（《设计文档》§2.2 选型表：`ulid` crate；ADR-007 决策 3）：
//! 使用 `ulid::Ulid`（MIT）生成与解析——48 位毫秒时间戳 + 80 位随机数、
//! Crockford Base32 26 字符；不再维护 std-only 自研实现。
//!
//! 随机源由 `ulid` crate（`rand` → OS 熵）提供；本模块不承担密码学用途。
//! 本文件只保留最小包装，生成/解析语义以 crate 为准（单测锁定）。

use ulid::Ulid;

/// 生成新的 ULID 字符串（26 字符，Crockford Base32）。
pub(crate) fn generate() -> String {
    Ulid::new().to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::str::FromStr;

    use super::*;

    /// Crockford Base32 字母表（排除 I/L/O/U）。
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    #[test]
    fn generated_ulid_matches_spec_format() {
        for _ in 0..1_000 {
            let id = generate();
            assert_eq!(id.len(), 26, "ULID 必须为 26 字符: {id}");
            assert!(
                id.bytes().all(|byte| ALPHABET.contains(&byte)),
                "仅允许 Crockford Base32: {id}"
            );
            assert!(
                id.as_bytes().first().is_some_and(|first| *first <= b'7'),
                "首字符必须 ≤ 7（128 位装 130 位空间）: {id}"
            );
        }
    }

    /// ADR-007 增量修订 1 决策 3：生成侧属性测试——连续 10 万次首字符恒 ≤ 7。
    #[test]
    fn first_char_fits_128_bits_over_100k_generations() {
        for _ in 0..100_000 {
            let id = generate();
            assert!(
                id.as_bytes().first().is_some_and(|first| *first <= b'7'),
                "生成侧首字符必须恒 ≤ 7: {id}"
            );
            assert_eq!(id.len(), 26, "生成侧长度恒为 26: {id}");
        }
    }

    #[test]
    fn generated_ulids_are_unique() {
        let mut seen = HashSet::new();
        for _ in 0..50_000 {
            let id = generate();
            assert!(seen.insert(id.clone()), "ULID 必须唯一: {id}");
        }
    }

    #[test]
    fn generation_and_parsing_round_trip() {
        for _ in 0..1_000 {
            let id = generate();
            let parsed = Ulid::from_string(&id).expect("自产 ULID 必须可解析");
            assert_eq!(parsed.to_string(), id, "解析后序列化必须等价");
            assert_eq!(format!("{parsed}"), id, "Display 必须等价");
            let via_from_str = Ulid::from_str(&id).expect("FromStr 必须可解析");
            assert_eq!(via_from_str, parsed);
            // 时间前缀为 48 位毫秒（≥ 2020-01-01，≤ 2100）。
            let timestamp = parsed.timestamp_ms();
            assert!(
                (1_577_836_800_000..4_102_444_800_000).contains(&timestamp),
                "时间戳超出合理范围: {timestamp}"
            );
        }
    }

    #[test]
    fn parsing_rejects_invalid_input() {
        // 长度非法（空 / 25 / 27）。
        assert!(Ulid::from_string("").is_err());
        assert!(Ulid::from_string(&"0".repeat(25)).is_err());
        assert!(Ulid::from_string(&"0".repeat(27)).is_err());
        // 非法字符（I/L/O/U 不在 Crockford 字母表）。
        for invalid in ["I", "L", "O", "U", "-", " "] {
            let candidate = format!("{}{}", "0".repeat(25), invalid);
            assert!(
                Ulid::from_string(&candidate).is_err(),
                "非法字符 {invalid} 必须拒绝"
            );
        }
        // 规范上界（7ZZZ…）必须解析成功并原样往返。
        let max = "7ZZZZZZZZZZZZZZZZZZZZZZZZZ";
        let parsed_max = Ulid::from_string(max).expect("规范上界必须可解析");
        assert_eq!(parsed_max.to_string(), max);
        // ADR-007 增量修订 1 决策 3：首字符 > 7（130 位）的**已知非规范往返边界**——
        // `ulid 1.1.3` 丢弃溢出高位且不报错，显式断言该行为（升级 crate 时若语义变化，
        // 本用例必须失败，避免静默改变解析语义）：
        //   '8' = 0b01000 → 最高位落在 2^128（被丢弃）→ 解析结果为全零 ULID。
        let overflow = format!("8{}", "0".repeat(25));
        let parsed_overflow =
            Ulid::from_string(&overflow).expect("1.1.3 语义：丢弃高位、不报错（已知边界）");
        println!("ulid overflow boundary: {overflow} -> {parsed_overflow}");
        assert_eq!(
            parsed_overflow.to_string(),
            "0".repeat(26),
            "高位丢弃后的规范值必须为全零 ULID"
        );
        assert_ne!(
            parsed_overflow.to_string(),
            overflow,
            "溢出输入不得原样往返"
        );
        // 另一组：'9' = 0b01001 → 丢弃 2^128 后余 2^125 → 规范化首字符为 '1'。
        let overflow_nine = format!("9{}", "0".repeat(25));
        let parsed_nine = Ulid::from_string(&overflow_nine).expect("1.1.3 语义：丢弃高位");
        println!("ulid overflow boundary: {overflow_nine} -> {parsed_nine}");
        assert_eq!(parsed_nine.to_string(), format!("1{}", "0".repeat(25)));
        // 小写输入：crate 语义（部分实现按大小写不敏感接受）；仅断言解析结果的一致性与长度合法。
        let upper = generate();
        let lower = upper.to_ascii_lowercase();
        if let Ok(parsed) = Ulid::from_string(&lower) {
            assert_eq!(parsed.to_string(), upper, "小写若被接受必须归一到同一值");
        }
    }
}
