//! Agent 预设命令后端（M7-09；ADR-026 §2.2；设计 D7/D14）。
//!
//! 覆盖 D7 命令面预设族：
//! - `presets_list`：预设清单（含内置标记与系统提示词；按内置在前、创建时间升序）；
//! - `preset_add` / `preset_update` / `preset_delete`：预设增改删（**内置不可删**：
//!   `builtin_preset_undeletable`；内置**可编辑**——与内置供应商语义对齐：仅删除受
//!   保护，编辑差异记入审计以便追溯基线漂移）。
//!
//! 单写者约束（AGENTS §2.4）：全部写路径经单写队列（[`WriteQueue::execute`]），
//! 事务实现见 `aether_store::ops`（本层不复刻 SQL）。
//!
//! 合成口径（ADR-026 §2.2-3）不在本模块：预设提示词块与工作区记忆块的合成由
//! `session_create` 调用 `aether_control::compose_system_prompt` 完成（禁止静默覆盖）。

use std::future::Future;
use std::time::Duration;

use aether_core::{Preset, PresetId};
use aether_store::{AuditLogRecord, ReadPool, StoreCommand, StoreOutcome, WriteQueue};
use serde_json::{json, Value};

use crate::ipc::dto::{
    PresetAddRequest, PresetDeleteRequest, PresetUpdateRequest, PresetsListRequest,
};
use crate::ipc::error::IpcError;

/// 预设命令硬超时（本地存储读写；无网络路径）。
pub const PRESET_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// 预设命令执行体（由 [`crate::session_backend::SessionBackend`] 持有并转发）。
pub struct PresetControl {
    reads: ReadPool,
    write: WriteQueue,
    handle: tokio::runtime::Handle,
    timeout: Duration,
}

impl PresetControl {
    pub fn new(reads: ReadPool, write: WriteQueue, handle: tokio::runtime::Handle) -> Self {
        Self {
            reads,
            write,
            handle,
            timeout: PRESET_COMMAND_TIMEOUT,
        }
    }

    /// 覆盖命令超时（测试/故障注入）。
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 同步桥接：spawn 到核心运行时并等待（与 `SessionBackend::call` 同口径）。
    fn call<T, F>(&self, future: F) -> Result<T, IpcError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, IpcError>> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver
            .recv_timeout(self.timeout)
            .map_err(|_| IpcError::internal(format!("预设命令超时（>{:?}，核心未在预算内返回）", self.timeout)))?
    }

    /// `presets_list`：预设清单（M7-09/ADR-026；内置在前、创建时间升序）。
    pub fn presets_list(&self, _request: &PresetsListRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        self.call(async move {
            let presets = reads
                .presets()
                .await
                .map_err(|error| IpcError::internal(format!("预设清单读取失败：{error}")))?;
            let items: Vec<Value> = presets.iter().map(preset_json).collect();
            Ok(json!({ "presets": items }))
        })
    }

    /// `preset_add`：新增自定义预设（`is_builtin=0`；名称/提示词上限由 DTO 校验）。
    pub fn preset_add(&self, request: &PresetAddRequest) -> Result<Value, IpcError> {
        let write = self.write.clone();
        let name = request.name.clone();
        let system_prompt = request.system_prompt.clone();
        self.call(async move {
            let now = now_ms();
            let preset = Preset {
                id: PresetId::new(ulid::Ulid::new().to_string()).map_err(|error| {
                    IpcError::internal(format!("preset_id 生成失败：{error}"))
                })?,
                name,
                system_prompt,
                is_builtin: false,
                created_at: now,
                updated_at: now,
            };
            let outcome = write
                .execute(StoreCommand::InsertPreset {
                    preset: preset.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("预设落库失败：{error}")))?;
            ensure_inserted(outcome)?;
            insert_preset_audit(
                &write,
                "preset.created",
                preset.id.as_str(),
                "created",
                json!({
                    "name": preset.name,
                    "system_prompt_bytes": preset.system_prompt.len(),
                }),
            )
            .await?;
            Ok(preset_json(&preset))
        })
    }

    /// `preset_update`：整体更新名称/提示词（至少一项；`preset_id` 不存在 →
    /// `preset_not_found`）；内置可编辑，差异记入审计（ADR-026 §2.2-2）。
    pub fn preset_update(&self, request: &PresetUpdateRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let preset_id = request.preset_id.clone();
        let name = request.name.clone();
        let system_prompt = request.system_prompt.clone();
        self.call(async move {
            let id = PresetId::new(preset_id.clone())
                .map_err(|error| IpcError::internal(format!("preset_id 非法：{error}")))?;
            let existing = reads
                .preset(&id)
                .await
                .map_err(|error| IpcError::internal(format!("预设读取失败：{error}")))?
                .ok_or_else(|| IpcError::preset_not_found(&preset_id))?;
            let updated = Preset {
                name: name.clone().unwrap_or(existing.name.clone()),
                system_prompt: system_prompt
                    .clone()
                    .unwrap_or(existing.system_prompt.clone()),
                updated_at: now_ms(),
                ..existing.clone()
            };
            let outcome = write
                .execute(StoreCommand::UpdatePreset {
                    preset: updated.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("预设更新失败：{error}")))?;
            // 并发删除窗口：影响 0 行 → 与查无同码（稳定业务码）。
            if ensure_applied(outcome)? == 0 {
                return Err(IpcError::preset_not_found(&preset_id));
            }
            // 编辑差异入审计（内置基线漂移可追溯）：只记录是否变化与变更后长度，
            // 不复制提示词正文（避免审计表体积失控）。
            insert_preset_audit(
                &write,
                "preset.updated",
                updated.id.as_str(),
                "updated",
                json!({
                    "is_builtin": updated.is_builtin,
                    "name_changed": updated.name != existing.name,
                    "system_prompt_changed": updated.system_prompt != existing.system_prompt,
                    "name": updated.name,
                    "system_prompt_bytes": updated.system_prompt.len(),
                }),
            )
            .await?;
            Ok(preset_json(&updated))
        })
    }

    /// `preset_delete`：删除预设（内置 → `builtin_preset_undeletable`；
    /// `preset_id` 不存在 → `preset_not_found`）。
    ///
    /// 内置判定在命令层先行（硬约束，不得以 UI 隐藏代替）；删除后
    /// `sessions.preset_id` 经 `ON DELETE SET NULL` 置空，既有会话提示词保持原样。
    pub fn preset_delete(&self, request: &PresetDeleteRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let preset_id = request.preset_id.clone();
        self.call(async move {
            let id = PresetId::new(preset_id.clone())
                .map_err(|error| IpcError::internal(format!("preset_id 非法：{error}")))?;
            let existing = reads
                .preset(&id)
                .await
                .map_err(|error| IpcError::internal(format!("预设读取失败：{error}")))?
                .ok_or_else(|| IpcError::preset_not_found(&preset_id))?;
            if existing.is_builtin {
                return Err(IpcError::builtin_preset_undeletable(&preset_id));
            }
            let outcome = write
                .execute(StoreCommand::DeletePreset { id: id.clone() })
                .await
                .map_err(|error| IpcError::internal(format!("预设删除失败：{error}")))?;
            // 并发删除窗口：影响 0 行 → 与查无同码（稳定业务码；不写删除审计）。
            if ensure_applied(outcome)? == 0 {
                return Err(IpcError::preset_not_found(&preset_id));
            }
            insert_preset_audit(
                &write,
                "preset.deleted",
                id.as_str(),
                "deleted",
                json!({ "name": existing.name }),
            )
            .await?;
            Ok(json!({ "preset_id": id.as_str(), "deleted": true }))
        })
    }
}

/// 预设响应形状（M7-09；`data-preset-id`/`data-builtin` 锚点数据源）。
fn preset_json(preset: &Preset) -> Value {
    json!({
        "id": preset.id.as_str(),
        "name": preset.name,
        "system_prompt": preset.system_prompt,
        "is_builtin": preset.is_builtin,
        "created_at": preset.created_at,
        "updated_at": preset.updated_at,
    })
}

/// 预设变更审计（ADR-026 附录 A-3：`preset.created/updated/deleted`，actor=`user`）。
async fn insert_preset_audit(
    write: &WriteQueue,
    action: &str,
    preset_id: &str,
    result: &str,
    detail: Value,
) -> Result<(), IpcError> {
    write
        .execute(StoreCommand::InsertAudit {
            record: AuditLogRecord {
                id: ulid::Ulid::new().to_string(),
                session_id: None,
                runtime_id: None,
                actor: "user".to_owned(),
                action: action.to_owned(),
                resource: Some(format!("preset:{preset_id}")),
                detail: Some(detail.to_string()),
                result: Some(result.to_owned()),
                ts: now_ms(),
            },
        })
        .await
        .map_err(|error| IpcError::internal(format!("审计落库失败：{error}")))?;
    Ok(())
}

/// 写结果校验：返回影响行数（`Applied` 之外的结果属不变量破坏）。
///
/// 调用方对 `affected == 0` 的语义分派：新增 → 内部错误（不变量破坏）；
/// 更新/删除 → `preset_not_found`（并发删除窗口，稳定业务码）。
fn ensure_applied(outcome: StoreOutcome) -> Result<usize, IpcError> {
    match outcome {
        StoreOutcome::Applied { affected } => Ok(affected),
        other => Err(IpcError::internal(format!(
            "预设写命令返回了非预期结果：{other:?}"
        ))),
    }
}

/// 新增路径的严格校验（影响 0 行属不变量破坏）。
fn ensure_inserted(outcome: StoreOutcome) -> Result<(), IpcError> {
    match ensure_applied(outcome)? {
        0 => Err(IpcError::internal(
            "预设新增未影响任何行（不变量破坏）",
        )),
        _ => Ok(()),
    }
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}
