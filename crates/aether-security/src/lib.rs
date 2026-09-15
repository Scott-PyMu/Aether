//! Aether 权限网关（设计 D9）。
//!
//! 依赖方向（AGENTS.md §2.1）：仅依赖 `aether-core`，禁止依赖其他内部 crate。
//! 本里程碑（M1-01）仅建立 workspace 骨架；权限门自 M2-10 起落地。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(test)]
mod tests {
    #[test]
    fn security_depends_on_core_with_single_version_source() {
        assert_eq!(aether_core::version(), env!("CARGO_PKG_VERSION"));
    }
}
