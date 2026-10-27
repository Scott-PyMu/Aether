//! 业务码常量（**新增字段**，不改变 `IpcErrorCode` 32 枚举）。
//!
//! 来源与口径（ADR-029 v0.4 §4.3 / §16.2；ADR-018 附录 A-7）：
//! - JSON-RPC 错误的 `error.data.business_code` **只取本表 9 项**；
//! - `error.data.aether_code` **只取** [`super::error::IpcErrorCode`] 的 32 枚举
//!   （`IpcErrorCode::as_str()` 是唯一来源）；
//! - 前端判定顺序：`business_code`（若存在）→ `aether_code`；`message` 只用于展示；
//! - 内部 outcome 字符串（如 `already_running`）**不得**直接作为 `aether_code`
//!   或 `business_code`（ADR-029 §4.3）。
//!
//! 生成物一致性：`packages/protocol/src/error_codes.json` 的 `business` 集合必须与
//! [`BusinessCode::ALL`] 逐项一致（`pnpm verify:error-codes --business-set` 校验；
//! M9-03 起启用）。**本文件是该集合在 Rust 侧的唯一事实来源**，新增业务码必须
//! 同批更新 `scripts/ci/error-codes.mjs` 的 `BUSINESS_CODES` 登记表并走 ADR。

/// 业务码（9 项，冻结；新增须走 ADR 并同步生成器/JSON）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusinessCode {
    /// 会话在途/运行中，命令被拒（ADR-018 A-7；M5-01 `session_delete`/`session_workspace_assign`）。
    SessionBusy,
    /// 会话已暂停（ADR-018 A-7）。
    SessionPaused,
    /// 存储背压（D8；写队列 L2 阈值）。
    StorageBackpressure,
    /// 记忆冲突（ADR-018 A-7）。
    MemoryConflict,
    /// 权限票据不在待审批集合（`permission_resolve`）。
    PermissionNotPending,
    /// 数据目录 OS 独占锁获取失败（**M9-03**；ADR-029 §3.5/§4.3，退出码 20）。
    DataDirLocked,
    /// A4 同步盘/云目录拒绝启动（ADR-018 A-7；ADR-029 起为**致命**，退出码 21）。
    SyncDirBlocked,
    /// DB 无法打开（**M9-03**；ADR-029 §3.5；仅「无法打开」，不含 `quick_check` 失败）。
    DbOpenFailed,
    /// 已应用迁移 checksum 与内嵌文件不符（**M9-03**；ADR-029 §3.5/R11，致命 22）。
    SchemaChecksumMismatch,
}

impl BusinessCode {
    /// 全部业务码（顺序 = `error_codes.json` 的 `business` 顺序 = ADR-018 A-7 登记顺序）。
    pub const ALL: [Self; 9] = [
        Self::SessionBusy,
        Self::SessionPaused,
        Self::StorageBackpressure,
        Self::MemoryConflict,
        Self::PermissionNotPending,
        Self::DataDirLocked,
        Self::SyncDirBlocked,
        Self::DbOpenFailed,
        Self::SchemaChecksumMismatch,
    ];

    /// 线上取值（snake_case；`data.business_code` 唯一来源）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionBusy => "session_busy",
            Self::SessionPaused => "session_paused",
            Self::StorageBackpressure => "storage_backpressure",
            Self::MemoryConflict => "memory_conflict",
            Self::PermissionNotPending => "permission_not_pending",
            Self::DataDirLocked => "data_dir_locked",
            Self::SyncDirBlocked => "sync_dir_blocked",
            Self::DbOpenFailed => "db_open_failed",
            Self::SchemaChecksumMismatch => "schema_checksum_mismatch",
        }
    }

    /// 由线上取值反解（诊断/测试；未知值返回 `None`）。
    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|code| code.as_str() == value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn business_codes_are_frozen_at_nine_unique_snake_case_values() {
        assert_eq!(BusinessCode::ALL.len(), 9, "ADR-018 A-7 业务码 9 项");
        let mut values: Vec<&str> = BusinessCode::ALL.iter().map(|code| code.as_str()).collect();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), 9, "业务码取值必须唯一");
        for code in BusinessCode::ALL {
            let text = code.as_str();
            assert!(
                text.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                "业务码必须是 snake_case：{text}"
            );
            assert_eq!(BusinessCode::from_wire(text), Some(code));
        }
        assert_eq!(BusinessCode::from_wire("already_running"), None);
    }

    #[test]
    fn m9_03_bootstrap_codes_are_registered() {
        // M9-03 DoD-5：四类致命启动检查必须有稳定业务码。
        for expected in [
            "data_dir_locked",
            "sync_dir_blocked",
            "db_open_failed",
            "schema_checksum_mismatch",
        ] {
            assert!(
                BusinessCode::from_wire(expected).is_some(),
                "缺少业务码 {expected}"
            );
        }
    }
}
