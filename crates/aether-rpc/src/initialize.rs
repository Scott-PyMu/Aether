//! `initialize` 握手（ADR-029 §4.2）与 `DaemonPhase`。
//!
//! 请求/响应形状（逐字段冻结）：
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{
//!   "protocolVersion":"1.0","appVersion":"0.1.0","client":"electron",
//!   "clientCapabilities":["events","permissions","dialogs","browser-view"],
//!   "requiredCapabilities":["events","permissions"]}}
//! {"jsonrpc":"2.0","id":1,"result":{
//!   "protocolVersion":"1.0","daemonVersion":"0.1.0","coreVersion":"0.1.0",
//!   "dataDir":"C:\\Users\\...\\AppData\\Roaming\\Aether","phase":"warming",
//!   "daemonCapabilities":["events","permissions","dialogs"]}}
//! ```
//!
//! 口径：
//! - `initialize` 必须是连接上的第一个请求；重复 `initialize` → `invalid_request`；
//! - **不等待适配器预热**（P1-2 修正）；响应只表示「进程可用 + 数据目录/阶段可读」；
//! - major 不匹配 → `-32001 protocol_mismatch`（`data.reason`）；
//!   minor 允许增量字段（未知参数成员忽略）；
//! - 能力协商：`clientCapabilities ∩ daemonCapabilities` 为共享能力；未知项忽略；
//!   `requiredCapabilities` 未满足 → `-32006 capability_missing`；
//!   `browser-view` 是 client-only 能力，不要求 daemon 声明；
//! - `DaemonPhase` 8 个产出值；`migrating`/`blocked` 为保留值（M9 不产生）。

use serde::Serialize;
use serde_json::Value;

use crate::code::PROTOCOL_MISMATCH;
use crate::connection::RpcFailure;
use crate::message::ErrorData;

/// 协议版本（Aether 线协议 1.0；与适配器线协议版本解耦，见 ADR-029 §4.2）。
pub const PROTOCOL_VERSION: &str = "1.0";
/// 协议 major（不匹配即拒绝加载）。
pub const PROTOCOL_MAJOR: u32 = 1;

/// daemon 声明能力（`initialize` 响应的 `daemonCapabilities`）。
///
/// - `events`：`aether.event` 通知（M9-05 接线传输；能力自 M9-03 声明）；
/// - `permissions`：权限回环（`permission.pending` 通知 + 既有轮询语义）；
/// - `dialogs`：服务端发起对话框（M9 仅协议预留，产品路径仍走壳本地命令）。
pub const DAEMON_CAPABILITIES: &[&str] = &["events", "permissions", "dialogs"];

/// 仅客户端能力（daemon 不声明、`requiredCapabilities` 不校验）。
pub const CLIENT_ONLY_CAPABILITIES: &[&str] = &["browser-view"];

/// daemon 生命周期阶段（`initialize`/`daemon.status` 共用；ADR-029 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonPhase {
    /// 进程已启动（预检前）。
    Starting,
    /// 数据目录解析 / A4 检测中。
    Detecting,
    /// DB 打开 + 已应用迁移 checksum 校验中。
    Opening,
    /// 预热中（孤儿清理 / 适配器后台预热；不阻塞 `initialize` 应答）。
    Warming,
    /// 就绪（业务命令可执行）。
    Ready,
    /// 只读安全模式（`quick_check` 失败 → `persist_degraded`/S-08；**不退出**）。
    Degraded,
    /// 关闭序列执行中。
    ShuttingDown,
    /// 致命失败（首帧 error 后进程退出；壳层显示 S-12）。
    Failed,
    /// 保留（数据目录迁移；M9 不产生）。
    Migrating,
    /// 保留（启动阻断态；M9 的 A4 走 S-12，不产生）。
    Blocked,
}

impl DaemonPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Detecting => "detecting",
            Self::Opening => "opening",
            Self::Warming => "warming",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::ShuttingDown => "shutting_down",
            Self::Failed => "failed",
            Self::Migrating => "migrating",
            Self::Blocked => "blocked",
        }
    }

    /// M9 产出值（8 个；`migrating`/`blocked` 为保留值）。
    pub const PRODUCED: [Self; 8] = [
        Self::Starting,
        Self::Detecting,
        Self::Opening,
        Self::Warming,
        Self::Ready,
        Self::Degraded,
        Self::ShuttingDown,
        Self::Failed,
    ];

    /// 保留值（M9 不产生；未来版本另立 ADR）。
    pub const fn is_reserved(self) -> bool {
        matches!(self, Self::Migrating | Self::Blocked)
    }

    /// `startup_get.phase` 映射（R-P2-1）：过渡态 → `ready`（`detail.phase` 给细值）。
    pub const fn startup_phase(self) -> &'static str {
        match self {
            Self::Starting | Self::Detecting | Self::Opening | Self::Warming => "ready",
            Self::Ready | Self::Degraded => "ready",
            Self::Failed => "blocked_error",
            Self::ShuttingDown => "ready",
            Self::Migrating | Self::Blocked => "blocked_sync_dir",
        }
    }
}

/// `initialize` 请求参数（内层 DTO；未知成员忽略 = minor 增量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializeParams {
    /// 客户端协议版本（`major.minor`）。
    pub protocol_version: String,
    pub app_version: Option<String>,
    pub client: Option<String>,
    pub client_capabilities: Vec<String>,
    pub required_capabilities: Vec<String>,
}

impl InitializeParams {
    /// 从 `params` 解析（`params` 必须为对象；`protocolVersion` 必填）。
    pub fn from_value(params: &Value) -> Result<Self, RpcFailure> {
        let Some(object) = params.as_object() else {
            return Err(RpcFailure::invalid_params(
                "invalid_value",
                "initialize 参数必须是 JSON 对象",
            ));
        };

        let protocol_version = match object.get("protocolVersion") {
            Some(Value::String(version)) if !version.trim().is_empty() => version.clone(),
            Some(Value::String(_)) => {
                return Err(RpcFailure::invalid_params(
                    "invalid_value",
                    "protocolVersion 不能为空",
                ))
            }
            Some(_) => {
                return Err(RpcFailure::invalid_params(
                    "invalid_type",
                    "protocolVersion 必须是字符串",
                ))
            }
            None => {
                return Err(RpcFailure::invalid_params(
                    "missing_field",
                    "initialize 缺少 protocolVersion",
                ))
            }
        };

        let app_version = optional_string(object.get("appVersion"), "appVersion")?;
        let client = optional_string(object.get("client"), "client")?;
        let client_capabilities =
            string_array(object.get("clientCapabilities"), "clientCapabilities")?;
        let required_capabilities =
            string_array(object.get("requiredCapabilities"), "requiredCapabilities")?;

        Ok(Self {
            protocol_version,
            app_version,
            client,
            client_capabilities,
            required_capabilities,
        })
    }

    /// 校验 major 与必需能力；返回共享能力（`client ∩ daemon`，未知项忽略）。
    pub fn negotiate(&self) -> Result<NegotiatedCapabilities, RpcFailure> {
        let (major, _minor) = parse_protocol_version(&self.protocol_version)?;
        if major != PROTOCOL_MAJOR {
            return Err(RpcFailure::transport(
                PROTOCOL_MISMATCH,
                "invalid_value",
                format!(
                    "协议 major 版本不一致（客户端 {major}，daemon {PROTOCOL_MAJOR}）：拒绝加载"
                ),
            )
            .with_reason("protocol_mismatch")
            .with_detail("client_major", major)
            .with_detail("daemon_major", PROTOCOL_MAJOR));
        }

        let missing: Vec<String> = self
            .required_capabilities
            .iter()
            .filter(|capability| !CLIENT_ONLY_CAPABILITIES.contains(&capability.as_str()))
            .filter(|capability| !DAEMON_CAPABILITIES.contains(&capability.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(RpcFailure::transport(
                crate::code::CAPABILITY_MISSING,
                "invalid_value",
                format!("daemon 不满足必需能力：{}", missing.join(", ")),
            )
            .with_reason("capability_missing")
            .with_details(Value::Array(
                missing.into_iter().map(Value::String).collect(),
            )));
        }

        let shared = self
            .client_capabilities
            .iter()
            .filter(|capability| DAEMON_CAPABILITIES.contains(&capability.as_str()))
            .cloned()
            .collect();
        Ok(NegotiatedCapabilities { shared })
    }
}

/// 能力协商结果（共享能力；未知能力已忽略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedCapabilities {
    pub shared: Vec<String>,
}

/// `initialize` 响应结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// 实际协议版本（`"1.0"`）。
    pub protocol_version: &'static str,
    /// daemon 版本（workspace 单一版本源）。
    pub daemon_version: String,
    /// 核心库版本（`aether-core`）。
    pub core_version: String,
    /// 解析后的数据目录（canonical 展示形态）。
    pub data_dir: String,
    /// 握手时的阶段（不等待预热）。
    pub phase: DaemonPhase,
    /// daemon 能力清单。
    pub daemon_capabilities: Vec<String>,
}

impl InitializeResult {
    pub fn new(
        daemon_version: impl Into<String>,
        core_version: impl Into<String>,
        data_dir: impl Into<String>,
        phase: DaemonPhase,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: daemon_version.into(),
            core_version: core_version.into(),
            data_dir: data_dir.into(),
            phase,
            daemon_capabilities: DAEMON_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// 解析 `major.minor`（非法 → `invalid_params`/`invalid_format`）。
pub fn parse_protocol_version(version: &str) -> Result<(u32, u32), RpcFailure> {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    match (major, minor, parts.next()) {
        (Some(major), Some(minor), None) => Ok((major, minor)),
        _ => Err(RpcFailure::invalid_params(
            "invalid_format",
            format!("protocolVersion 必须是 \"major.minor\"（收到 {version:?}）"),
        )),
    }
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, RpcFailure> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(RpcFailure::invalid_params(
            "invalid_type",
            format!("{field} 必须是字符串"),
        )),
    }
}

fn string_array(value: Option<&Value>, field: &str) -> Result<Vec<String>, RpcFailure> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(text) => result.push(text.clone()),
                    _ => {
                        return Err(RpcFailure::invalid_params(
                            "invalid_type",
                            format!("{field} 只能包含字符串"),
                        ))
                    }
                }
            }
            Ok(result)
        }
        Some(_) => Err(RpcFailure::invalid_params(
            "invalid_type",
            format!("{field} 必须是字符串数组"),
        )),
    }
}

/// 预初始化致命失败首帧（ADR-029 §4.2/§4.3）：`id: null` + `-32005` + 业务码/阶段/日志路径。
#[must_use]
pub fn bootstrap_failure(
    aether_code: &str,
    business_code: Option<&str>,
    message: impl Into<String>,
    phase: DaemonPhase,
    log_path: &str,
) -> crate::message::ErrorObject {
    let mut data = ErrorData::new(aether_code)
        .with_phase(phase)
        .with_log_path(log_path);
    if let Some(business_code) = business_code {
        data = data.with_business(business_code);
    }
    crate::message::ErrorObject::new(crate::code::BOOTSTRAP_FAILURE, data, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn phases_are_frozen_and_reserved_values_are_never_produced_in_m9() {
        assert_eq!(DaemonPhase::PRODUCED.len(), 8);
        assert!(DaemonPhase::Migrating.is_reserved());
        assert!(DaemonPhase::Blocked.is_reserved());
        for phase in DaemonPhase::PRODUCED {
            assert!(!phase.is_reserved());
            assert!(!phase.as_str().is_empty());
            if phase != DaemonPhase::Failed {
                assert_eq!(phase.startup_phase(), "ready", "{}", phase.as_str());
            }
        }
        assert_eq!(DaemonPhase::Failed.startup_phase(), "blocked_error");
        let mut codes: Vec<&str> = DaemonPhase::PRODUCED.iter().map(|p| p.as_str()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 8);
    }

    #[test]
    fn initialize_params_accept_inner_shape_and_ignore_unknown_members() {
        let params = InitializeParams::from_value(&json!({
            "protocolVersion": "1.0",
            "appVersion": "0.1.0",
            "client": "electron",
            "clientCapabilities": ["events", "permissions", "dialogs", "browser-view", "future"],
            "requiredCapabilities": ["events", "permissions"],
            "futureField": {"ignored": true}
        }))
        .expect("合法 initialize");
        assert_eq!(params.protocol_version, "1.0");
        let negotiated = params.negotiate().expect("能力满足");
        assert_eq!(
            negotiated.shared,
            vec![
                "events".to_owned(),
                "permissions".to_owned(),
                "dialogs".to_owned()
            ],
            "共享能力 = client ∩ daemon（未知项/客户端专属项忽略）"
        );
    }

    #[test]
    fn initialize_requires_protocol_version_and_object_params() {
        let error = InitializeParams::from_value(&Value::Null).expect_err("null 非法");
        assert_eq!(error.transport_code(), crate::code::INVALID_PARAMS);
        assert_eq!(error.aether_code(), "invalid_value");

        let error = InitializeParams::from_value(&json!({"client": "electron"}))
            .expect_err("缺 protocolVersion");
        assert_eq!(error.aether_code(), "missing_field");
    }

    #[test]
    fn major_mismatch_is_protocol_mismatch() {
        let params =
            InitializeParams::from_value(&json!({"protocolVersion": "2.0"})).expect("解析");
        let error = params.negotiate().expect_err("major 不匹配");
        assert_eq!(error.transport_code(), crate::code::PROTOCOL_MISMATCH);
        assert_eq!(error.reason(), Some("protocol_mismatch"));
    }

    #[test]
    fn required_capability_missing_is_capability_missing() {
        let params = InitializeParams::from_value(&json!({
            "protocolVersion": "1.0",
            "requiredCapabilities": ["events", "mcp"]
        }))
        .expect("解析");
        let error = params.negotiate().expect_err("缺能力");
        assert_eq!(error.transport_code(), crate::code::CAPABILITY_MISSING);
        assert_eq!(
            error.details(),
            Some(&json!(["mcp"])),
            "details 列出缺失能力"
        );
    }

    #[test]
    fn client_only_capability_does_not_require_daemon_support() {
        let params = InitializeParams::from_value(&json!({
            "protocolVersion": "1.0",
            "requiredCapabilities": ["browser-view"]
        }))
        .expect("解析");
        assert!(params.negotiate().is_ok(), "browser-view 是 client-only");
    }

    #[test]
    fn malformed_version_is_invalid_format() {
        for version in ["1", "1.0.0", "a.b"] {
            let params =
                InitializeParams::from_value(&json!({"protocolVersion": version})).expect("解析");
            let error = params.negotiate().expect_err("非法版本");
            assert_eq!(error.aether_code(), "invalid_format", "version={version}");
        }
        // 空串在参数解析阶段即拒绝（invalid_value）。
        let error =
            InitializeParams::from_value(&json!({"protocolVersion": ""})).expect_err("空版本");
        assert_eq!(error.aether_code(), "invalid_value");
    }

    #[test]
    fn initialize_result_has_frozen_field_set() {
        let result = InitializeResult::new("0.1.0", "0.1.0", r"C:\Aether", DaemonPhase::Warming);
        let value = result.to_value();
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "coreVersion",
                "daemonCapabilities",
                "daemonVersion",
                "dataDir",
                "phase",
                "protocolVersion"
            ]
        );
        assert_eq!(value["protocolVersion"], json!("1.0"));
        assert_eq!(value["phase"], json!("warming"));
        assert_eq!(
            value["daemonCapabilities"],
            json!(["events", "permissions", "dialogs"])
        );
    }

    #[test]
    fn bootstrap_failure_frame_carries_business_code_phase_and_log_path() {
        let error = bootstrap_failure(
            "startup_blocked",
            Some("sync_dir_blocked"),
            "A4 同步盘",
            DaemonPhase::Failed,
            r"C:\Aether\logs\aetherd.log",
        );
        let value = error.to_value();
        assert_eq!(value["code"], json!(-32005));
        assert_eq!(value["data"]["aether_code"], json!("startup_blocked"));
        assert_eq!(value["data"]["business_code"], json!("sync_dir_blocked"));
        assert_eq!(value["data"]["phase"], json!("failed"));
        assert_eq!(
            value["data"]["log_path"],
            json!(r"C:\Aether\logs\aetherd.log")
        );
    }
}
