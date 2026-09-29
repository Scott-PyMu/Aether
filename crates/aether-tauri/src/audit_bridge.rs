//! 适配器监督审计落库桥接（M3-07；D9/SE-03 P0 最小审计集）。
//!
//! 背景（`docs/M1-10-证据.md` §6）：监督器经 [`SupervisorObserver`] 上报
//! `runtime.status_changed` 与审计记录（准入拒绝/熔断/台账处置/终止），实现方
//! （命令层）负责把二者落库。组合根装配顺序为「先监督器（`health.runtimes` 摘要源）
//! → 后存储」，因此审计出口经 [`DeferredObserver`] 延迟注入：存储就绪前监督器
//! 不产生任何回调，就绪后（启动序列尾段/预热/控制命令）全部审计经单写队列追加
//! `audit_log`（应用层仅 INSERT，无 UPDATE/DELETE）。
//!
//! 审计动作命名沿用既有约定（与 `permission.*` / `backup.restore` 一致）：
//! - `runtime.status_changed`：每次合法状态转移（`result` = `from→to`）；
//! - `runtime.<audit_kind>`：监督器审计记录（`admission_rejected` / `circuit_broken` /
//!   `terminated` / `ledger_reclaimed` / `ledger_stale_record` / `ledger_skipped`）。

use std::sync::{Arc, RwLock};

use aether_adapters::supervisor::{AuditRecord, ResourceEvent, StatusChange, SupervisorObserver};
use aether_store::{AuditLogRecord, StoreCommand, WriteQueue};
use serde_json::json;

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
/// 回调为同步接口，写入经核心运行时 spawn（fire-and-forget，不阻塞监督器任务）；
/// 写入失败仅记 tracing 告警（审计为追加日志，不反向阻断适配器监督）。
pub struct StoreAuditObserver {
    write: WriteQueue,
    handle: tokio::runtime::Handle,
}

impl StoreAuditObserver {
    pub fn new(write: WriteQueue, handle: tokio::runtime::Handle) -> Self {
        Self { write, handle }
    }

    fn enqueue(&self, record: AuditLogRecord) {
        let write = self.write.clone();
        self.handle.spawn(async move {
            if let Err(error) = write.execute(StoreCommand::InsertAudit { record }).await {
                tracing::warn!(error = %error, "适配器审计写入失败（M3-07）");
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
