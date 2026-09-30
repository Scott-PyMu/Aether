//! tauri-specta 类型绑定（M3-01/T14；设计 D7）。
//!
//! - 生成物 `packages/protocol/src/bindings.ts` 由 `tauri-specta` 生成，**禁止手改**
//!   （AGENTS §2.8）；生成入口见 `tests/export_bindings.rs`（`--ignored`），
//!   CI 以「重新生成 + `git diff --exit-code`」校验（T14）；
//! - 命令覆盖 D7 P0 全集 29 个可调用命令（含 ADR-004 七命令、ADR-006 四命令、
//!   ADR-007 `health` 与 ADR-010 文件引用四命令；供应商七命令随 M3-11 落地）；
//!   运行期注册仍走 [`crate::ipc::commands::handler`]，
//!   本模块只服务于类型导出，不改变 M1-08 严格校验契约；
//! - 事件通道（D7）：单通道 [`EVENT_CHANNEL`]，信封含 `session_id`，UI 侧过滤；
//!   运行期转发见 [`crate::event_bridge`]。

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::{collect_events, Builder};

use crate::ipc::dto::{
    AppExitRequest, AppRestartRequest, ArtifactAddRequest, ArtifactRemoveRequest,
    ArtifactsListRequest, BackupCreateRequest, BackupListRequest, BackupRestoreRequest,
    BackupSource, ExportDiagnosticsRequest, HealthRequest, MessagesPageRequest, PermissionDecision,
    PermissionResolveRequest, PermissionsPendingRequest, RefPickKind, RefPickRequest,
    RunRetryRequest, RuntimeEnableRequest, RuntimeRetryRequest, SessionCreateRequest,
    SessionIdRequest, SessionListRequest, SessionSendRequest, SessionStatus, SettingsGetRequest,
    SettingsSetRequest, StartupGetRequest, StartupMigrateRequest, StartupPickTargetRequest,
    WorkspaceSetRequest,
};
use crate::ipc::error::IpcError;
use crate::json_payload::JsonPayload;
use crate::session_backend::SessionSummary;

/// 事件通道（D7：单通道，UI 侧按 `session_id` 过滤）。
pub const EVENT_CHANNEL: &str = "aether://event";

/// `aether://event` 事件载荷：与 `aether_core::EventEnvelope` 的 JSON 形状一致
/// （9 字段：`v/id/session_id/run_id/runtime_id/seq/ts/type/payload`）。
///
/// 等价性由 `tests/event_bridge.rs` 的序列化对照断言锁定（字段增删必须同版本完成）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type, tauri_specta::Event)]
#[tauri_specta(event_name = "aether://event")]
pub struct AetherEvent {
    pub v: u32,
    pub id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub runtime_id: String,
    pub seq: u64,
    pub ts: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: JsonPayload,
}

impl AetherEvent {
    /// 从核心事件信封构造（字段一一对应；`payload` 为 payload 本体 JSON）。
    pub fn from_envelope(envelope: &aether_core::EventEnvelope) -> Result<Self, serde_json::Error> {
        Ok(Self {
            v: envelope.v,
            id: envelope.id.as_str().to_owned(),
            session_id: envelope.session_id.as_str().to_owned(),
            run_id: envelope.run_id.as_ref().map(|id| id.as_str().to_owned()),
            runtime_id: envelope.runtime_id.as_str().to_owned(),
            seq: envelope.seq,
            ts: envelope.ts,
            event_type: envelope.event_type().as_str().to_owned(),
            payload: JsonPayload(envelope.payload.to_value()?),
        })
    }
}

/// 绑定构建器：命令（D7 全集）+ `aether://event` 事件 + 请求/错误类型。
///
/// `.typ::<T>()` 导出的 DTO 与命令运行期严格解析的 DTO 为同一类型（`ipc::dto`），
/// 前端可直接以生成类型构造 payload。
pub fn builder<R: tauri::Runtime>() -> Builder<R> {
    Builder::<R>::new()
        // `seq`/`ts`（`u64`/`i64`）在线协议上为 JSON number（IPC 经 serde_json），
        // 且实际取值远小于 2^53（seq 为事件计数、ts 为 epoch 毫秒）：导出为 TS `number`
        // 与真实解析行为一致。该开关为 tauri-specta 对本仓库 BigInt 字段的官方适配。
        .dangerously_cast_bigints_to_number()
        .commands(crate::ipc::commands::collected())
        .events(collect_events![AetherEvent])
        .typ::<IpcError>()
        .typ::<SessionCreateRequest>()
        .typ::<SessionListRequest>()
        .typ::<SessionSendRequest>()
        .typ::<SessionIdRequest>()
        .typ::<MessagesPageRequest>()
        .typ::<PermissionsPendingRequest>()
        .typ::<PermissionResolveRequest>()
        .typ::<PermissionDecision>()
        .typ::<SessionStatus>()
        .typ::<SettingsGetRequest>()
        .typ::<SettingsSetRequest>()
        .typ::<BackupCreateRequest>()
        .typ::<BackupListRequest>()
        .typ::<BackupRestoreRequest>()
        .typ::<BackupSource>()
        .typ::<AppRestartRequest>()
        .typ::<AppExitRequest>()
        .typ::<RunRetryRequest>()
        .typ::<RuntimeRetryRequest>()
        .typ::<RuntimeEnableRequest>()
        .typ::<WorkspaceSetRequest>()
        .typ::<RefPickRequest>()
        .typ::<RefPickKind>()
        .typ::<ArtifactsListRequest>()
        .typ::<ArtifactAddRequest>()
        .typ::<ArtifactRemoveRequest>()
        .typ::<SessionSummary>()
        .typ::<ExportDiagnosticsRequest>()
        .typ::<HealthRequest>()
        .typ::<StartupGetRequest>()
        .typ::<StartupMigrateRequest>()
        .typ::<StartupPickTargetRequest>()
}

/// 默认输出路径：`<repo>/packages/protocol/src/bindings.ts`（相对本 crate 清单目录）。
pub fn default_output_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packages")
        .join("protocol")
        .join("src")
        .join("bindings.ts")
}
