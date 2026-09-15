//! Aether 适配器宿主与监督器（设计 D5 / D6）。
//!
//! 依赖方向（AGENTS.md §2.1）：仅依赖 `aether-core`，禁止依赖其他内部 crate。
//! 本里程碑（M1-01）仅建立 workspace 骨架；线协议与监督器自 M1-09 / M1-10 起落地。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(test)]
mod tests {
    #[test]
    fn adapters_depend_on_core_with_single_version_source() {
        assert_eq!(aether_core::version(), env!("CARGO_PKG_VERSION"));
    }
}
