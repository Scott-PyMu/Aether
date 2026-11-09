//! M10-02/ADR-032：子 Agent 派生、隔离、结果回注（PF-01 / UI-03）。
//!
//! - `control`：5 条 IPC 命令 + 适配器入口 + 后台结果收集/交付；
//! - `envelope`：compact envelope（N→1、去重、冲突、折叠）；
//! - `result`：结构化结果解析与 schema 上限。

pub mod bridge;
pub mod control;
pub mod envelope;
pub mod result;

pub use bridge::SubagentBridge;
pub use control::SubagentControlBackend;
