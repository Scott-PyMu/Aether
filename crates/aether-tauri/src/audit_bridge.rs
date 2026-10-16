//! 适配器监督审计落库桥接（M3-07；D9/SE-03 P0 最小审计集）。
//!
//! 背景（`docs/M1-10-证据.md` §6）：监督器经 [`SupervisorObserver`] 上报
//! `runtime.status_changed` 与审计记录（准入拒绝/熔断/台账处置/终止），实现方
//! （命令层）负责把二者落库。组合根装配顺序为「先监督器（`health.runtimes` 摘要源）
//! → 后存储」，因此审计出口经 [`DeferredObserver`] 延迟注入：存储就绪前监督器
//! 不产生任何回调，就绪后（启动序列尾段/预热/控制命令）全部审计经单写队列追加
//! `audit_log`（应用层仅 INSERT，无 UPDATE/DELETE）。
//!
//! M7-01/ADR-015（DoD0）：注入事件管线后，**每次合法转移同时发射
//! `runtime.status_changed` 事件**（信封 9 字段；先日志后广播，D4），会话归段为
//! 全局合成段 `session_id = "runtime:<runtime_id>"`（不建 `sessions` 行、不进
//! `session_list`/`messages_page`/补读；审计记录保留不变——事件与审计并存）。
//!
//! 审计动作命名沿用既有约定（与 `permission.*` / `backup.restore` 一致）：
//! - `runtime.status_changed`：每次合法状态转移（`result` = `from→to`）；
//! - `runtime.<audit_kind>`：监督器审计记录（`admission_rejected` / `circuit_broken` /
//!   `terminated` / `ledger_reclaimed` / `ledger_stale_record` / `ledger_skipped` /
//!   `registry_rejected`）。

use std::sync::{Arc, RwLock};

use aether_adapters::supervisor::{AuditRecord, ResourceEvent, StatusChange, SupervisorObserver};
use aether_control::EventPipeline;
use aether_store::{AuditLogRecord, StoreCommand, WriteQueue};
use serde_json::json;

/// 全局事件的合成会话归段前缀（ADR-015 附录 C-8：`session_id = "runtime:<id>"`）。
pub const RUNTIME_EVENT_SESSION_PREFIX: &str = "runtime:";

/// 延迟接线观察者：监督器构造期无存储句柄时的占位出口。
///
/// 存储就绪后 [`DeferredObserver::set`] 注入真实出口；未注入期间回调为 no-op
/// （组合根保证监督器在注入前无状态转移/审计活动）。
#[derive(Default)]
pub struct DeferredObserver {
    delegate: RwLock<Option<Arc<dyn SupervisorObserver>>>,
}

impl DeferredObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入真实审计出口（存储就绪后调用；重复注入以最后一次为准）。
    pub fn set(&self, observer: Arc<dyn SupervisorObserver>) {
        match self.delegate.write() {
            Ok(mut guard) => *guard = Some(observer),
            Err(poisoned) => *poisoned.into_inner() = Some(observer),
        }
    }

    fn current(&self) -> Option<Arc<dyn SupervisorObserver>> {
        match self.delegate.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl SupervisorObserver for DeferredObserver {
    fn on_status_changed(&self, change: &StatusChange) {
        if let Some(observer) = self.current() {
            observer.on_status_changed(change);
        }
    }

    fn on_audit(&self, record: &AuditRecord) {
        if let Some(observer) = self.current() {
            observer.on_audit(record);
        }
    }

    fn on_resource_alert(&self, alert: &ResourceEvent) {
        if let Some(observer) = self.current() {
            observer.on_resource_alert(alert);
        }
    }
}

/// 存储审计出口：监督器回调 → `audit_log` 追加（经单写队列，D3）。
///
/// M7-01/ADR-015（DoD0）：注入事件管线后，状态转移同时发射 `runtime.status_changed`
/// 事件（合成归段 `runtime:<id>`；经管线归一化 → 先日志后广播 → `aether://event`）。
///
/// 回调为同步接口，写入/事件提交经核心运行时 spawn（fire-and-forget，不阻塞监督器
/// 任务）；失败仅记 tracing 告警（审计为追加日志，不反向阻断适配器监督）。
pub struct StoreAuditObserver {
    write: WriteQueue,
    handle: tokio::runtime::Handle,
    pipeline: Option<EventPipeline>,
}

impl StoreAuditObserver {
    pub fn new(write: WriteQueue, handle: tokio::runtime::Handle) -> Self {
        Self {
            write,
            handle,
            pipeline: None,
        }
    }

    /// 注入事件管线（M7-01 DoD0：状态转移 → 事件发射链路；未注入仅审计）。
    #[must_use]
    pub fn with_pipeline(mut self, pipeline: EventPipeline) -> Self {
        self.pipeline = Some(pipeline);
        self
    }

    fn enqueue(&self, record: AuditLogRecord) {
        let write = self.write.clone();
        self.handle.spawn(async move {
            if let Err(error) = write.execute(StoreCommand::InsertAudit { record }).await {
                tracing::warn!(error = %error, "适配器审计写入失败（M3-07）");
            }
        });
    }

    /// 发射 `runtime.status_changed`（附录 B 信封 9 字段；合成会话归段）。
    ///
    /// 走管线 `submit` 与适配器事件同一路径（Normalizer 严格校验 → 会话 sequencer →
    /// 先日志后广播）；提交失败仅告警（状态审计仍在，缺口可经补读/前端轮询兜底）。
    fn submit_status_event(&self, change: &StatusChange) {
        let Some(pipeline) = self.pipeline.clone() else {
            return;
        };
        let raw = json!({
            "v": aether_core::EVENT_ENVELOPE_VERSION,
            "id": ulid::Ulid::new().to_string(),
            "session_id": format!("{RUNTIME_EVENT_SESSION_PREFIX}{}", change.runtime_id.as_str()),
            "run_id": null,
            "runtime_id": change.runtime_id.as_str(),
            "seq": null,
            "ts": change.at_ms,
            "type": "runtime.status_changed",
            "payload": {
                "runtime_id": change.runtime_id.as_str(),
                "from": change.from.as_str(),
                "to": change.to.as_str(),
                "reason": change.reason.map(|reason| reason.as_str()),
            },
        });
        self.handle.spawn(async move {
            if let Err(error) = pipeline.submit(raw).await {
                tracing::warn!(
                    error = %error,
                    "runtime.status_changed 事件提交管线失败（审计保留；事件缺口不阻断监督器）"
                );
            }
        });
    }
}

impl SupervisorObserver for StoreAuditObserver {
    fn on_status_changed(&self, change: &StatusChange) {
        self.enqueue(AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: None,
            runtime_id: Some(change.runtime_id.as_str().to_owned()),
            actor: "system".to_owned(),
            action: "runtime.status_changed".to_owned(),
            resource: Some(format!("runtime:{}", change.runtime_id.as_str())),
            detail: Some(
                json!({
                    "from": change.from.as_str(),
                    "to": change.to.as_str(),
                    "reason": change.reason.map(|reason| reason.as_str()),
                    "detail": change.detail,
                    "at_ms": change.at_ms,
                })
                .to_string(),
            ),
            result: Some(format!("{}→{}", change.from.as_str(), change.to.as_str())),
            ts: change.at_ms,
        });
        self.submit_status_event(change);
    }

    fn on_audit(&self, record: &AuditRecord) {
        self.enqueue(AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: None,
            runtime_id: Some(record.runtime_id().as_str().to_owned()),
            actor: "system".to_owned(),
            action: format!("runtime.{}", record.kind().as_str()),
            resource: Some(format!("runtime:{}", record.runtime_id().as_str())),
            detail: Some(
                json!({
                    "kind": record.kind().as_str(),
                    "detail": record.detail(),
                    "at_ms": record.at_ms,
                })
                .to_string(),
            ),
            result: Some(record.kind().as_str().to_owned()),
            ts: record.at_ms,
        });
    }
}
