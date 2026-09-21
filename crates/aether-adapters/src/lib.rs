//! Aether 适配器宿主与监督器（设计 D5 / D6）。
//!
//! 依赖方向（AGENTS.md §2.1）：仅依赖 `aether-core`，禁止依赖其他内部 crate。
//! - M1-09：JSON-RPC 2.0 over stdio（JSON-Lines v1.0）线协议骨架、握手、超时表、
//!   错误码、大行策略；Mock 适配器（`packages/adapter-mock`）驱动一致性/健壮性测试；
//! - M1-10：进程监督（状态机、进程组/Job Object、心跳、退避/熔断、PID 台账、准入白名单、
//!   资源采样、`runtime_retry`/`runtime_enable`），见 [`supervisor`]。
//!
//! 硬约束：核心 crate 禁止 `unwrap()` / `expect()` / `panic!()`（测试代码显式豁免）。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod connection;
pub mod framing;
pub mod process;
pub mod protocol;
pub mod session_client;
pub mod supervisor;

pub use connection::{
    AdapterConnection, AdapterNotification, ConnectionState, DisconnectReason, RequestError,
    RpcError, NOTIFICATION_QUEUE_CAPACITY, OUTBOUND_QUEUE_CAPACITY, RECORDED_ERRORS_LIMIT,
};
pub use framing::{
    AetherLineCodec, ChunkLimitedReader, FrameError, RawLine, ARTIFACT_REF_LIMIT,
    ARTIFACT_REF_TYPE, MAX_FRAME_BYTES, READ_CHUNK_BYTES,
};
pub use process::{AdapterProcess, ProcessError, ProcessTerminationTarget, STDERR_TAIL_LINES};
pub use protocol::{
    code, notify, protocol_major, upgrade_hint, validate_hello, DisabledInfo, DisabledReason,
    Hello, Method, RuntimeInfo, HANDSHAKE_TIMEOUT, INVALID_FRAME_UNHEALTHY_THRESHOLD,
    PROTOCOL_MAJOR, PROTOCOL_MINOR, PROTOCOL_VERSION,
};
pub use session_client::{
    AdapterSessionClient, CreatedSession, RunOutcome, SendAck, SessionClientError, ToolDefinition,
    ADAPTER_DISCONNECTED_CODE, CLIENT_EVENT_LIMIT,
};

/// 适配器宿主版本号——取自单一版本来源（工作区 `Cargo.toml`）。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_depend_on_core_with_single_version_source() {
        assert_eq!(aether_core::version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn constants_match_d6() {
        assert_eq!(PROTOCOL_VERSION, "1.0");
        assert_eq!(PROTOCOL_MAJOR, 1);
        assert_eq!(HANDSHAKE_TIMEOUT.as_secs(), 10);
        assert_eq!(INVALID_FRAME_UNHEALTHY_THRESHOLD, 20);
        assert_eq!(MAX_FRAME_BYTES, 2 * 1024 * 1024);
        assert_eq!(ARTIFACT_REF_LIMIT, 1024 * 1024);
        assert_eq!(OUTBOUND_QUEUE_CAPACITY, 256);
    }
}
