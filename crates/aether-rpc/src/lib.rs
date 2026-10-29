//! # aether-rpc — JSON-RPC 2.0 over JSONL 叶子基础设施（ADR-029 D-029-3）
//!
//! 职责（只做传输/协议，不含业务）：
//! - JSON-RPC 2.0 报文类型：[`Request`] / [`Response`] / [`Notification`] /
//!   [`ErrorObject`] / [`ErrorData`] / [`Id`]；
//! - JSONL 编解码与 **2MiB 有界帧读取器**（[`FrameReader`]：超限立即拒绝，
//!   不整行读入内存；超限只失败该请求，连接可继续）；
//! - `initialize` 握手结构、`DaemonPhase`、能力协商与协议版本校验（[`initialize`]）；
//! - 连接状态机（[`Connection`]）：首个 `initialize`、重复 `initialize` / 重复
//!   `request id`、预初始化业务请求、`shutdown` 与关闭后请求；
//! - 传输/类别错误码命名空间（[`code`]）与 `aether_code`/`business_code` 数据形状。
//!
//! 约束（ADR-029 §3.3 / AGENTS §2.1）：
//! - 本 crate 是**叶子**：不依赖任何内部 crate，也不依赖 Tauri/Electron/窗口实现；
//! - 业务方法分发由调用方实现 [`Server`]（`aetherd` → `aether-app::dispatch`）；
//! - `aether_code` 只取 `IpcErrorCode` 32 枚举（由调用方映射，本 crate 只承载字符串）；
//!   `business_code` 只取 9 项业务码（调用方传 `BusinessCode::as_str()`）。
//!
//! 错误码命名空间（ADR-029 §4.3 / §16.2）：
//!
//! | `error.code` | 含义 |
//! |---|---|
//! | `-32700` | `parse_error`（帧不是合法 JSON） |
//! | `-32600` | `invalid_request`（非对象/`jsonrpc` 非 2.0/id 非法/重复 id/重复 initialize） |
//! | `-32601` | `method_not_found`（未知方法或壳本地命令误入 daemon） |
//! | `-32602` | `invalid_params`（内层 DTO 校验失败：`unknown_field`/`missing_field`/…） |
//! | `-32603` | `internal_error` |
//! | `-32000` | `application_error`（域/业务错误；读 `data.business_code` → `data.aether_code`） |
//! | `-32001` | `protocol_mismatch`（`data.reason`） |
//! | `-32002` | `core_not_ready`（业务后端未就绪 / 预 `initialize` 业务请求） |
//! | `-32003` | `too_large`（帧 > 2MiB；只失败本请求） |
//! | `-32004` | `shutting_down`（已接受 `shutdown`，拒绝新请求） |
//! | `-32005` | `bootstrap_failure`（预 `initialize` 致命启动检查失败首帧） |
//! | `-32006` | `capability_missing`（`requiredCapabilities` 未被 daemon 满足） |

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
// `RpcFailure` 携带 4 个字符串 + 可选 details（≈136B），超过 clippy 默认 128B 阈值。
// 错误对象每条请求最多构造一次且仅走失败路径；`Box<RpcFailure>` 会污染全部分发签名
// （`Server::call`/`initialize`/`negotiate`），收益为零——此处显式豁免并记录理由。
#![allow(clippy::result_large_err)]

pub mod codec;
pub mod connection;
pub mod initialize;
pub mod message;
pub mod server_request;

pub use codec::{encode_line, Frame, FrameError, FrameReader, MAX_FRAME_BYTES};
pub use connection::{Connection, Handled, RpcFailure, Server};
pub use initialize::{
    DaemonPhase, InitializeParams, InitializeResult, CLIENT_ONLY_CAPABILITIES, DAEMON_CAPABILITIES,
    PROTOCOL_MAJOR, PROTOCOL_VERSION,
};
pub use message::{ErrorData, ErrorObject, Id, Notification, Request, Response, JSONRPC_VERSION};
pub use server_request::{
    ClientResponse, DialogPickParams, DialogPickResult, PendingServerRequests, PickKind,
    RegistrationError, ResponseDisposition, ServerRequest, ServerRequestError,
    ServerRequestExpiry, DIALOG_PICK_METHOD, DIALOG_PICK_TIMEOUT_MS, SERVER_REQUEST_TIMEOUT_MS,
};

/// JSON-RPC 传输/类别错误码（ADR-029 §4.3；`error.code` 的唯一取值集合）。
pub mod code {
    /// 帧不是合法 JSON。
    pub const PARSE_ERROR: i32 = -32700;
    /// 报文不是合法请求（结构/`jsonrpc`/`id`）。
    pub const INVALID_REQUEST: i32 = -32600;
    /// 未知方法（或壳本地命令误入 daemon）。
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// 参数（内层 DTO）非法。
    pub const INVALID_PARAMS: i32 = -32602;
    /// 内部错误。
    pub const INTERNAL_ERROR: i32 = -32603;
    /// 应用/域错误（业务分支按 `data.business_code` → `data.aether_code`）。
    pub const APPLICATION_ERROR: i32 = -32000;
    /// 协议 major 不匹配（`data.reason`）。
    pub const PROTOCOL_MISMATCH: i32 = -32001;
    /// 核心未就绪（预 `initialize` 业务请求 / 后端尚未注入）。
    pub const CORE_NOT_READY: i32 = -32002;
    /// 帧超限（2MiB）。
    pub const TOO_LARGE: i32 = -32003;
    /// 已进入关闭序列。
    pub const SHUTTING_DOWN: i32 = -32004;
    /// 预 `initialize` 致命启动检查失败（首帧 error）。
    pub const BOOTSTRAP_FAILURE: i32 = -32005;
    /// 能力缺失（`requiredCapabilities` 未满足）。
    pub const CAPABILITY_MISSING: i32 = -32006;

    /// 全部传输/类别码（「传输码全覆盖」断言用）。
    pub const ALL: [i32; 12] = [
        PARSE_ERROR,
        INVALID_REQUEST,
        METHOD_NOT_FOUND,
        INVALID_PARAMS,
        INTERNAL_ERROR,
        APPLICATION_ERROR,
        PROTOCOL_MISMATCH,
        CORE_NOT_READY,
        TOO_LARGE,
        SHUTTING_DOWN,
        BOOTSTRAP_FAILURE,
        CAPABILITY_MISSING,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_codes_are_the_frozen_twelve() {
        let mut codes = code::ALL.to_vec();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 12, "传输码必须唯一");
        assert_eq!(MAX_FRAME_BYTES, 2 * 1024 * 1024, "帧上限 2MiB");
        assert!(MAX_FRAME_BYTES.is_power_of_two());
        // ADR-029 §4.3 逐个断言（防常量漂移）。
        assert_eq!(code::PARSE_ERROR, -32700);
        assert_eq!(code::INVALID_REQUEST, -32600);
        assert_eq!(code::METHOD_NOT_FOUND, -32601);
        assert_eq!(code::INVALID_PARAMS, -32602);
        assert_eq!(code::INTERNAL_ERROR, -32603);
        assert_eq!(code::APPLICATION_ERROR, -32000);
        assert_eq!(code::PROTOCOL_MISMATCH, -32001);
        assert_eq!(code::CORE_NOT_READY, -32002);
        assert_eq!(code::TOO_LARGE, -32003);
        assert_eq!(code::SHUTTING_DOWN, -32004);
        assert_eq!(code::BOOTSTRAP_FAILURE, -32005);
        assert_eq!(code::CAPABILITY_MISSING, -32006);
    }
}
