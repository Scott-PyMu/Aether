//! 时间工具（Unix epoch 毫秒口径，附录 E 映射规则 ①）。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前时间（Unix epoch 毫秒）；时钟早于 epoch 时返回 0。
pub(crate) fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}
