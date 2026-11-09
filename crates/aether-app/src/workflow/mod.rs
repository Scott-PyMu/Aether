//! M10-01/ADR-031：工作流编排（定义校验、执行引擎、IPC 命令面）。
//!
//! 无画布（UI-04 仍属 P4）；本模块只提供 JSON 定义/运行视图与后台调度。

pub mod control;
pub mod definition;
pub mod engine;
pub mod session_executor;

pub use control::WorkflowControlBackend;
pub use definition::{parse_and_validate, DefinitionError, NodeType, WorkflowDefinition};
pub use engine::{
    AgentExecutor, AgentRequest, CancelOutcome, RetryError, UnavailableAgentExecutor,
    WorkflowEngine,
};
pub use session_executor::SessionAgentExecutor;
