//! M10-07/ADR-037：调度器重试分类与退避（纯函数；无 I/O）。
//!
//! 口径（ADR-037 D-037-2）：
//! - 自动重试默认关闭（`max_attempts=1`）；仅来源显式设置 >1 时启用；
//! - 可重试：`timeout`、`run_timeout`、`run_stream_timeout`、`adapter_unavailable`、
//!   `provider_unreachable`、`storage_backpressure`；
//! - 不可重试：权限拒绝、`provider_auth_failed`、`invalid_value`、`cancelled`、
//!   `session_disposed`；未知 `internal` 默认不自动重试（UI 提供 `run_retry`）；
//! - 退避 1s/2s/4s/8s/16s/30s/60s，±20% 抖动（按 run id 确定性派生，测试可复现）。

use crate::gate::{
    MAX_ATTEMPTS_LIMIT, MAX_RUN_TIMEOUT_MS, MIN_RUN_TIMEOUT_MS, QUEUE_PRIORITY_MAX,
    QUEUE_PRIORITY_MIN, RETRY_BACKOFF_JITTER_PERCENT, RETRY_BACKOFF_MS,
};

/// 自动重试可重试错误码（ADR-037 D-037-2）。
pub const AUTO_RETRYABLE_CODES: &[&str] = &[
    "timeout",
    "run_timeout",
    "run_stream_timeout",
    "adapter_unavailable",
    "provider_unreachable",
    "storage_backpressure",
];

/// 人工 `run_retry` 明确拒绝的错误码（不可重试分类；ADR-037 D-037-2/D-037-5）。
pub const MANUAL_RETRY_BLOCKED_CODES: &[&str] = &[
    "permission_denied",
    "permission",
    "permission_error",
    "provider_auth_failed",
    "invalid_value",
    "session_disposed",
    "cancelled",
];

/// 自动重试是否允许该错误码。
pub fn is_auto_retryable(code: &str) -> bool {
    AUTO_RETRYABLE_CODES.contains(&code)
}

/// 人工重试是否被错误分类拒绝（`cancelled` 状态保持既有可重试语义）。
pub fn manual_retry_blocked(status: aether_core::RunStatus, error_code: Option<&str>) -> bool {
    if status == aether_core::RunStatus::Cancelled {
        return false;
    }
    error_code
        .map(|code| MANUAL_RETRY_BLOCKED_CODES.contains(&code))
        .unwrap_or(false)
}

/// 退避基数（`attempt` 从 1 起；超出序列按最后一档）。
pub fn backoff_delay_ms(attempt: u32) -> i64 {
    if attempt == 0 {
        return RETRY_BACKOFF_MS[0];
    }
    let index = usize::try_from(attempt.saturating_sub(1))
        .unwrap_or(usize::MAX)
        .min(RETRY_BACKOFF_MS.len() - 1);
    RETRY_BACKOFF_MS[index]
}

/// 确定性抖动：FNV-1a(seed) → `[−20%, +20%]`（测试可复现，不引入依赖）。
pub fn jitter_percent(seed: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let span = u64::try_from(RETRY_BACKOFF_JITTER_PERCENT * 2 + 1).unwrap_or(41);
    i64::try_from(hash % span).unwrap_or(0) - RETRY_BACKOFF_JITTER_PERCENT
}

/// 带抖动的退避（毫秒；不会低于 1ms）。
pub fn backoff_with_jitter_ms(attempt: u32, seed: &str) -> i64 {
    let base = backoff_delay_ms(attempt);
    let delta = base.saturating_mul(jitter_percent(seed)) / 100;
    base.saturating_add(delta).max(1)
}

/// 优先级校验（ADR-037：−100…100）。
pub fn validate_priority(priority: i32) -> Result<(), String> {
    if !(QUEUE_PRIORITY_MIN..=QUEUE_PRIORITY_MAX).contains(&priority) {
        return Err(format!(
            "priority 必须在 {QUEUE_PRIORITY_MIN}..={QUEUE_PRIORITY_MAX} 之间（实际 {priority}）"
        ));
    }
    Ok(())
}

/// 超时校验（ADR-037：1s–24h）。
pub fn validate_timeout_ms(timeout_ms: i64) -> Result<(), String> {
    if !(MIN_RUN_TIMEOUT_MS..=MAX_RUN_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!(
            "timeout_ms 必须在 {MIN_RUN_TIMEOUT_MS}..={MAX_RUN_TIMEOUT_MS} ms 之间（实际 {timeout_ms}）"
        ));
    }
    Ok(())
}

/// 最大尝试次数校验（ADR-037：1–5）。
pub fn validate_max_attempts(max_attempts: u32) -> Result<(), String> {
    if !(1..=MAX_ATTEMPTS_LIMIT).contains(&max_attempts) {
        return Err(format!(
            "max_attempts 必须在 1..={MAX_ATTEMPTS_LIMIT} 之间（实际 {max_attempts}）"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification_matches_adr_037() {
        for code in [
            "timeout",
            "run_timeout",
            "run_stream_timeout",
            "adapter_unavailable",
            "provider_unreachable",
            "storage_backpressure",
        ] {
            assert!(is_auto_retryable(code), "{code} 应可自动重试");
        }
        for code in [
            "permission_denied",
            "provider_auth_failed",
            "invalid_value",
            "cancelled",
            "session_disposed",
            "internal",
        ] {
            assert!(!is_auto_retryable(code), "{code} 不应自动重试");
        }
        assert!(!manual_retry_blocked(
            aether_core::RunStatus::Cancelled,
            Some("cancelled")
        ));
        assert!(manual_retry_blocked(
            aether_core::RunStatus::Failed,
            Some("provider_auth_failed")
        ));
        assert!(!manual_retry_blocked(
            aether_core::RunStatus::Failed,
            Some("adapter_unavailable")
        ));
    }

    #[test]
    fn backoff_follows_sequence_and_stays_within_jitter() {
        let expected = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 60_000];
        for (index, base) in expected.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let attempt = (index + 1) as u32;
            assert_eq!(backoff_delay_ms(attempt), *base);
            for seed in ["run-a", "run-b", "run-c"] {
                let jittered = backoff_with_jitter_ms(attempt, seed);
                let low = base - base / 5;
                let high = base + base / 5;
                assert!(
                    (low..=high).contains(&jittered),
                    "attempt {attempt} seed {seed}: {jittered} 越界 [{low}, {high}]"
                );
            }
        }
        assert_eq!(backoff_delay_ms(0), 1_000);
        assert_eq!(backoff_delay_ms(99), 60_000);
        // 抖动确定性：同 seed 同结果。
        assert_eq!(
            backoff_with_jitter_ms(3, "run-x"),
            backoff_with_jitter_ms(3, "run-x")
        );
    }

    #[test]
    fn validators_enforce_adr_ranges() {
        assert!(validate_priority(-100).is_ok());
        assert!(validate_priority(100).is_ok());
        assert!(validate_priority(-101).is_err());
        assert!(validate_priority(101).is_err());
        assert!(validate_timeout_ms(1_000).is_ok());
        assert!(validate_timeout_ms(86_400_000).is_ok());
        assert!(validate_timeout_ms(999).is_err());
        assert!(validate_timeout_ms(86_400_001).is_err());
        assert!(validate_max_attempts(1).is_ok());
        assert!(validate_max_attempts(5).is_ok());
        assert!(validate_max_attempts(0).is_err());
        assert!(validate_max_attempts(6).is_err());
    }
}
