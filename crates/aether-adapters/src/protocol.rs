//! D6 线协议常量与消息形状：JSON-RPC 2.0 over stdio / JSON-Lines v1.0。
//!
//! - 协议版本：`PROTOCOL_VERSION = "1.0"`（`hello` 携带，major 不匹配拒绝加载）；
//! - 方法表与超时：严格取自设计文档 D6（[`Method::timeout`]）；
//! - 错误码：JSON-RPC 标准码 + 应用码 1001–1005（[`code`]）；
//! - 通知（适配器→核心）：`hello` / `event` / `permission.request` / `log`。
//!
//! 本模块不含 I/O，全部为可单测的纯逻辑。

use std::time::Duration;

use aether_core::RuntimeStatus;
use serde::{Deserialize, Serialize};

/// 协议 major（D6：major 不匹配 → 拒绝加载）。
pub const PROTOCOL_MAJOR: u32 = 1;
/// 协议 minor（minor 兼容，未知字段忽略）。
pub const PROTOCOL_MINOR: u32 = 0;
/// 协议版本字符串（`hello.protocol`）。
pub const PROTOCOL_VERSION: &str = "1.0";
/// 握手超时（D6：进程启动 10s 内必须发 `hello`）。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// D6 失败场景：同进程连续 20 次无效帧（坏 JSON/帧校验失败）→ 视为不健康。
pub const INVALID_FRAME_UNHEALTHY_THRESHOLD: u32 = 20;

/// 核心 → 适配器方法表（D6）。方法集在 MVP 固定，未知方法回 `-32601` 不断连。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Initialize,
    SessionCreate,
    SessionSend,
    SessionInterrupt,
    SessionDispose,
    ToolsList,
    PermissionResolve,
    HealthPing,
    Shutdown,
}

impl Method {
    /// D6 方法表全集（9 个）。
    pub const ALL: [Self; 9] = [
        Self::Initialize,
        Self::SessionCreate,
        Self::SessionSend,
        Self::SessionInterrupt,
        Self::SessionDispose,
        Self::ToolsList,
        Self::PermissionResolve,
        Self::HealthPing,
        Self::Shutdown,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::SessionCreate => "session.create",
            Self::SessionSend => "session.send",
            Self::SessionInterrupt => "session.interrupt",
            Self::SessionDispose => "session.dispose",
            Self::ToolsList => "tools.list",
            Self::PermissionResolve => "permission.resolve",
            Self::HealthPing => "health.ping",
            Self::Shutdown => "shutdown",
        }
    }

    /// D6 方法表超时（核心侧请求超时，超时回错误码 1002）。
    pub const fn timeout(self) -> Duration {
        match self {
            Self::Initialize => Duration::from_secs(10),
            // `session.send` 30s（ack 快返回，不等模型）。
            Self::SessionCreate | Self::SessionSend => Duration::from_secs(30),
            Self::SessionInterrupt => Duration::from_secs(5),
            Self::SessionDispose => Duration::from_secs(15),
            Self::ToolsList => Duration::from_secs(10),
            Self::PermissionResolve => Duration::from_secs(5),
            Self::HealthPing => Duration::from_secs(5),
            Self::Shutdown => Duration::from_secs(5),
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|method| method.as_str() == name)
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 适配器 → 核心通知方法名（D6）。
pub mod notify {
    /// 握手：进程启动 10s 内必须发送。
    pub const HELLO: &str = "hello";
    /// 事件信封通知（附录 B 类型）。
    pub const EVENT: &str = "event";
    /// 权限请求（D9 回环的适配器侧入口）。
    pub const PERMISSION_REQUEST: &str = "permission.request";
    /// 日志/诊断通知（持久化样本走 stderr，协议 log 走本通知）。
    pub const LOG: &str = "log";
    /// 附件引用帧（D6：路径 + 元数据；数据体不进入线协议，存 artifacts 文件）。
    pub const ARTIFACT_REF: &str = "artifact_ref";
}

/// 错误码（D6：JSON-RPC 标准码 + 应用码 1001–1005）。
pub mod code {
    /// JSON-RPC 标准：JSON 解析失败。
    pub const PARSE_ERROR: i64 = -32700;
    /// JSON-RPC 标准：请求对象非法。
    pub const INVALID_REQUEST: i64 = -32600;
    /// JSON-RPC 标准：未知方法（**回错误但不断连**，D6 前向兼容）。
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// JSON-RPC 标准：参数非法。
    pub const INVALID_PARAMS: i64 = -32602;
    /// JSON-RPC 标准：内部错误。
    pub const INTERNAL_ERROR: i64 = -32603;

    /// 应用码：适配器崩溃。
    pub const ADAPTER_CRASHED: i64 = 1001;
    /// 应用码：请求超时（方法超时表，D6）。
    pub const REQUEST_TIMEOUT: i64 = 1002;
    /// 应用码：版本不匹配（`hello` major 校验失败）。
    pub const VERSION_MISMATCH: i64 = 1003;
    /// 应用码：能力缺失。
    pub const CAPABILITY_MISSING: i64 = 1004;
    /// 应用码：会话不存在。
    pub const SESSION_NOT_FOUND: i64 = 1005;
}

/// `hello` 通知的 `params` 形状（D6：`protocol` + runtime 信息）。
///
/// 前向兼容：未知字段忽略（不 `deny_unknown_fields`），minor 升级不阻断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// 协议版本字符串，形如 `"1.0"`。
    pub protocol: String,
    /// 适配器与 runtime 信息。
    pub runtime: RuntimeInfo,
}

/// `hello.runtime` 信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// 适配器名称（如 `mock`）。
    pub name: String,
    /// 适配器版本（semver）。
    pub version: String,
    /// 能力清单（RA-04 握手上报，MVP 不用于路由）。
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl RuntimeInfo {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|item| item == capability)
    }
}

/// 解析协议版本字符串的 major 段（`"1.0"` → `1`）。
pub fn protocol_major(protocol: &str) -> Option<u32> {
    protocol
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
}

/// D5/ADR-002 `status_reason` 词典（`runtimes.status_reason` TEXT，无 CHECK）。
///
/// 覆盖 `disabled` 与 `degraded` 两类转移原因：
/// - `disabled`：`handshake_timeout` / `version_mismatch` / `start_failed` /
///   `protocol_error` / `crash_loop` / `untrusted`（D5、ADR-002/评审 #1）；
/// - `degraded`：`heartbeat_failed`（10s×3 连续失败）/ `crashed`（运行中崩溃）/
///   `storage_backpressure`（ADR-003，D8 熔断隔离，M1-05 触发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledReason {
    /// 启动 10s 内未收到 `hello`。
    HandshakeTimeout,
    /// `hello` major 不匹配（D6/ADR-002 口径一致）。
    VersionMismatch,
    /// 启动即崩或启动期退出（含 `initialize` 失败）。
    StartFailed,
    /// 首帧不是合法 `hello` 或帧违反协议约束。
    ProtocolError,
    /// 60s 内 ≥5 次崩溃（D5，M1-10 监督器置位）。
    CrashLoop,
    /// 非官方 manifest（D5/评审 #1，M1-10 监督器置位）。
    Untrusted,
    /// 心跳连续 3 次失败（D5：ready→degraded，监督器自动重启）。
    HeartbeatFailed,
    /// 运行中崩溃/意外退出（D5 失败表「运行中崩溃」：ready→degraded）。
    Crashed,
    /// 存储背压熔断隔离（ADR-003/ADR-004；D8，`persist_degraded` 与临时背压的边界）。
    StorageBackpressure,
}

impl DisabledReason {
    /// 词典全集（D5「如」清单 + ADR-003/ADR-004 补充值）。
    pub const ALL: [Self; 9] = [
        Self::HandshakeTimeout,
        Self::VersionMismatch,
        Self::StartFailed,
        Self::ProtocolError,
        Self::CrashLoop,
        Self::Untrusted,
        Self::HeartbeatFailed,
        Self::Crashed,
        Self::StorageBackpressure,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HandshakeTimeout => "handshake_timeout",
            Self::VersionMismatch => "version_mismatch",
            Self::StartFailed => "start_failed",
            Self::ProtocolError => "protocol_error",
            Self::CrashLoop => "crash_loop",
            Self::Untrusted => "untrusted",
            Self::HeartbeatFailed => "heartbeat_failed",
            Self::Crashed => "crashed",
            Self::StorageBackpressure => "storage_backpressure",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
    }
}

impl std::fmt::Display for DisabledReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 适配器被禁用（D5：`disabled + status_reason` + UI/升级提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisabledInfo {
    /// 恒为 [`RuntimeStatus::Disabled`]（与 `runtimes.status` 一一对应）。
    pub status: RuntimeStatus,
    /// `runtimes.status_reason` 取值。
    pub status_reason: DisabledReason,
    /// 面向用户的升级/修复提示（版本不匹配时必须非空）。
    pub upgrade_hint: Option<String>,
    /// 诊断细节（日志用，不面向用户）。
    pub detail: String,
}

impl DisabledInfo {
    /// 版本不匹配（D6：`Disabled` + 升级提示）。
    pub fn version_mismatch(found: &str) -> Self {
        Self {
            status: RuntimeStatus::Disabled,
            status_reason: DisabledReason::VersionMismatch,
            upgrade_hint: Some(upgrade_hint(found)),
            detail: format!("hello.protocol={found}，核心需要 major {PROTOCOL_MAJOR}"),
        }
    }

    /// 启动 10s 内未收到 `hello`。
    pub fn handshake_timeout(timeout: Duration) -> Self {
        Self {
            status: RuntimeStatus::Disabled,
            status_reason: DisabledReason::HandshakeTimeout,
            upgrade_hint: None,
            detail: format!("{timeout:?} 内未收到 hello（D6 握手约束）"),
        }
    }

    /// 首帧非法或协议违例。
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        Self {
            status: RuntimeStatus::Disabled,
            status_reason: DisabledReason::ProtocolError,
            upgrade_hint: None,
            detail: detail.into(),
        }
    }
}

/// 版本不匹配时面向用户的升级提示（DoD4 断言其非空且含 actionable 信息）。
pub fn upgrade_hint(found: &str) -> String {
    format!(
        "适配器协议版本 {found} 与核心不兼容：需要 major {PROTOCOL_MAJOR}.x（当前核心 {PROTOCOL_VERSION}）。\
         请升级或更换兼容该 major 的适配器版本后重试。"
    )
}

/// 校验 `hello` 的协议 major 是否兼容。
pub fn validate_hello(hello: &Hello) -> Result<(), DisabledInfo> {
    match protocol_major(&hello.protocol) {
        Some(major) if major == PROTOCOL_MAJOR => Ok(()),
        _ => Err(DisabledInfo::version_mismatch(&hello.protocol)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_table_matches_d6_and_timeouts_are_frozen() {
        let expected: [(&str, u64); 9] = [
            ("initialize", 10),
            ("session.create", 30),
            ("session.send", 30),
            ("session.interrupt", 5),
            ("session.dispose", 15),
            ("tools.list", 10),
            ("permission.resolve", 5),
            ("health.ping", 5),
            ("shutdown", 5),
        ];
        assert_eq!(Method::ALL.len(), expected.len());
        for (method, (name, secs)) in Method::ALL.into_iter().zip(expected) {
            assert_eq!(method.as_str(), name);
            assert_eq!(method.timeout(), Duration::from_secs(secs), "{name}");
            assert_eq!(Method::parse(name), Some(method));
        }
        assert_eq!(Method::parse("unknown.method"), None);
    }

    #[test]
    fn error_codes_cover_standard_and_application_set() {
        assert_eq!(code::PARSE_ERROR, -32700);
        assert_eq!(code::INVALID_REQUEST, -32600);
        assert_eq!(code::METHOD_NOT_FOUND, -32601);
        assert_eq!(code::INVALID_PARAMS, -32602);
        assert_eq!(code::INTERNAL_ERROR, -32603);
        assert_eq!(code::ADAPTER_CRASHED, 1001);
        assert_eq!(code::REQUEST_TIMEOUT, 1002);
        assert_eq!(code::VERSION_MISMATCH, 1003);
        assert_eq!(code::CAPABILITY_MISSING, 1004);
        assert_eq!(code::SESSION_NOT_FOUND, 1005);
    }

    #[test]
    fn hello_accepts_same_major_and_ignores_unknown_fields() {
        let hello: Hello = serde_json::from_str(
            r#"{"protocol":"1.0","runtime":{"name":"mock","version":"0.1.0","future":true}}"#,
        )
        .unwrap();
        assert!(validate_hello(&hello).is_ok());
        assert_eq!(hello.runtime.name, "mock");
        assert!(validate_hello(&Hello {
            protocol: "1.99".to_owned(),
            runtime: hello.runtime,
        })
        .is_ok());
    }

    #[test]
    fn version_mismatch_is_disabled_with_reason_and_upgrade_hint() {
        let hello = Hello {
            protocol: "2.0".to_owned(),
            runtime: RuntimeInfo {
                name: "mock".to_owned(),
                version: "9.9.9".to_owned(),
                capabilities: vec![],
            },
        };
        let disabled = validate_hello(&hello).unwrap_err();
        assert_eq!(disabled.status, RuntimeStatus::Disabled);
        assert_eq!(disabled.status_reason.as_str(), "version_mismatch");
        let hint = disabled.upgrade_hint.unwrap_or_default();
        assert!(hint.contains("2.0"), "提示需包含实际版本：{hint}");
        assert!(hint.contains("升级"), "提示需给出升级动作：{hint}");
        assert!(disabled.detail.contains("2.0"));
    }

    #[test]
    fn unparsable_protocol_is_version_mismatch() {
        for found in ["", "abc", "x.y", "1"] {
            let hello = Hello {
                protocol: found.to_owned(),
                runtime: RuntimeInfo {
                    name: "mock".to_owned(),
                    version: "0.1.0".to_owned(),
                    capabilities: vec![],
                },
            };
            // "1" 缺少 minor：major 可解析为 1，按兼容处理（minor 不阻断）。
            if found == "1" {
                assert!(validate_hello(&hello).is_ok());
            } else {
                let disabled = validate_hello(&hello).unwrap_err();
                assert_eq!(
                    disabled.status_reason,
                    DisabledReason::VersionMismatch,
                    "{found}"
                );
            }
        }
    }

    #[test]
    fn disabled_reason_dictionary_round_trips() {
        for reason in DisabledReason::ALL {
            assert_eq!(DisabledReason::parse(reason.as_str()), Some(reason));
        }
        for value in [
            "handshake_timeout",
            "version_mismatch",
            "start_failed",
            "protocol_error",
            "crash_loop",
            "untrusted",
            "heartbeat_failed",
            "crashed",
            "storage_backpressure",
        ] {
            let reason = DisabledReason::parse(value).unwrap();
            assert_eq!(reason.as_str(), value);
        }
        assert_eq!(DisabledReason::parse("nope"), None);
        assert_eq!(
            DisabledReason::HandshakeTimeout.to_string(),
            "handshake_timeout"
        );
    }

    #[test]
    fn handshake_timeout_constructs_disabled_without_hint() {
        let info = DisabledInfo::handshake_timeout(HANDSHAKE_TIMEOUT);
        assert_eq!(info.status, RuntimeStatus::Disabled);
        assert_eq!(info.status_reason, DisabledReason::HandshakeTimeout);
        assert!(info.upgrade_hint.is_none());
        assert!(info.detail.contains("10s"));
    }

    #[test]
    fn runtime_capability_lookup() {
        let info = RuntimeInfo {
            name: "mock".to_owned(),
            version: "0.1.0".to_owned(),
            capabilities: vec!["tools.list".to_owned()],
        };
        assert!(info.has_capability("tools.list"));
        assert!(!info.has_capability("browser"));
    }
}
