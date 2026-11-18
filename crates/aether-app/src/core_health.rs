//! `health` 命令的真实数据源（ADR-007 决策 1；增量修订 1：不再推迟到 M2-07）。
//!
//! 分层：
//! - [`PipelineHealthSource`]：`health.storage_state` / `write_queue_depth` 的最小句柄；
//!   生产实现直接映射 [`aether_control::EventPipeline::health`]（D4 唯一降级事实源）；
//! - [`RuntimeSummarySource`]：`runtimes` 摘要来源（监督器状态；当前壳层未启动适配器，
//!   以 [`StaticRuntimeSummaries`] 占位，接线点见 ADR-007 附录 A）；
//! - [`HealthProvider`]：组合为 [`HealthReport`]（只读；不落库、不产生事件）；
//! - [`CoreHealthBackend`]：实现 [`IpcBackend`] 的 `health` 分支并持有存储运行时；
//! - [`boot_core_health`]：生产启动构造（D2 启动序列：库打开 + `quick_check` → 管线）。
//!
//! 启动失败（安全模式等）由 [`degraded_backend`] 呈现为 `persist_degraded`（D3 只读语义），
//! 不再回退 `not_implemented`。

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use aether_adapters::supervisor::Supervisor;
use aether_control::{
    DegradeTrigger, EventPipeline, PipelineConfig, PipelineError, StartupSelfCheckReport,
    StoreEventSource, StoreJournal, SPACE_GUARD_MIN_FREE_BYTES,
};
use aether_store::{
    integrity_check_path, plan_startup_checks, quick_check_path, StorageLayout, StoreError,
    StoreRuntime, WriteQueue, WriteQueueConfig,
};
use serde::Serialize;
use serde_json::Value;

use crate::ipc::backend::IpcBackend;
use crate::ipc::error::IpcError;
use crate::ipc::MaintenanceCell;
use crate::shutdown::StorageSlot;

/// 存储健康快照（`EventPipeline::health()` 的最小投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageHealthSnapshot {
    /// `normal` / `persist_degraded`（`EventPipeline::health().storage_state_code()`）。
    pub storage_state: String,
    /// 写队列深度（D3）。
    pub write_queue_depth: usize,
    /// 降级触发源（可选；`write_failure` / `space_guard` / `integrity_failure`）。
    pub degrade_trigger: Option<String>,
    /// 降级进入时间（Unix epoch 毫秒；可选）。
    pub degraded_since_ms: Option<i64>,
    /// 启动/降级原因（可选；面向诊断展示，不含密钥）。
    pub detail: Option<String>,
}

/// `health.storage_state` / `write_queue_depth` 的最小句柄。
pub trait PipelineHealthSource: Send + Sync + 'static {
    fn snapshot(&self) -> StorageHealthSnapshot;
}

impl PipelineHealthSource for EventPipeline {
    fn snapshot(&self) -> StorageHealthSnapshot {
        let health = self.health();
        StorageHealthSnapshot {
            storage_state: health.storage_state_code().to_owned(),
            write_queue_depth: health.journal_queue_depth,
            degrade_trigger: health.degrade_trigger.as_ref().map(|t| t.code().to_owned()),
            degraded_since_ms: health.degraded_since_ms,
            detail: None,
        }
    }
}

/// 固定快照源（启动失败降级呈现；也便于测试/骨架注入）。
#[derive(Debug, Clone)]
pub struct StaticHealthSource {
    snapshot: StorageHealthSnapshot,
}

impl StaticHealthSource {
    pub fn new(snapshot: StorageHealthSnapshot) -> Self {
        Self { snapshot }
    }
}

impl PipelineHealthSource for StaticHealthSource {
    fn snapshot(&self) -> StorageHealthSnapshot {
        self.snapshot.clone()
    }
}

/// runtime 摘要（监督器状态投影；与 `runtimes.status/status_reason` 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeSummary {
    pub id: String,
    /// `cold` / `starting` / `ready` / `degraded` / `disabled`（D5）。
    pub status: String,
    /// `status_reason` 词典取值（可空）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

/// `runtimes` 摘要来源（监督器接线点）。
pub trait RuntimeSummarySource: Send + Sync + 'static {
    /// `None` = 监督器**未接线**（`health.runtimes = null`，不可与「已接线但无 runtime」混淆）；
    /// `Some(vec)` = 已接线快照（`[]` 表示已接线且当前无 runtime）。
    fn summaries(&self) -> Option<Vec<RuntimeSummary>>;
}

/// 静态摘要源（ADR-007 增量 2）：显式区分「未接线」与「已接线空列表」。
#[derive(Debug, Clone, Default)]
pub struct StaticRuntimeSummaries {
    summaries: Option<Vec<RuntimeSummary>>,
}

impl StaticRuntimeSummaries {
    /// 监督器未接线（`health.runtimes = null`）。
    pub fn unwired() -> Self {
        Self { summaries: None }
    }

    /// 监督器已接线的静态快照（`[]` 表示已接线但无 runtime）。
    pub fn wired(summaries: Vec<RuntimeSummary>) -> Self {
        Self {
            summaries: Some(summaries),
        }
    }
}

impl RuntimeSummarySource for StaticRuntimeSummaries {
    fn summaries(&self) -> Option<Vec<RuntimeSummary>> {
        self.summaries.clone()
    }
}

/// 生产摘要源（M2-07；ADR-007 §5-2 字段映射冻结）：
/// `RuntimeSupervisor` 的 `status` / `status_reason` 一一映射为 `RuntimeSummary`。
///
/// 监督器未接线时使用 [`StaticRuntimeSummaries::unwired`]（`runtimes = null`）；
/// 已接线但无 runtime → `Some(Vec::new())`（`runtimes = []`）。摘要顺序按 `id` 排序，
/// 保证命令返回稳定（不依赖注册表遍历顺序）。
pub struct SupervisorRuntimeSummaries {
    supervisor: Arc<Supervisor>,
}

impl SupervisorRuntimeSummaries {
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { supervisor }
    }
}

impl RuntimeSummarySource for SupervisorRuntimeSummaries {
    fn summaries(&self) -> Option<Vec<RuntimeSummary>> {
        let mut summaries: Vec<RuntimeSummary> = self
            .supervisor
            .runtime_ids()
            .into_iter()
            .filter_map(|id| {
                self.supervisor.get(&id).map(|runtime| {
                    let (status, status_reason) = runtime.summary_snapshot();
                    RuntimeSummary {
                        id,
                        status: status.as_str().to_owned(),
                        status_reason: status_reason.map(|reason| reason.as_str().to_owned()),
                    }
                })
            })
            .collect();
        summaries.sort_by(|left, right| left.id.cmp(&right.id));
        Some(summaries)
    }
}

/// `health.storage_check`（ADR-039 D-039-5 §1；ADR-040 D-040-4 §1 v2 字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageCheckSnapshot {
    /// 2 = ADR-040 语义；缺失/1 = ADR-039（旧 daemon）。
    pub contract_version: u32,
    /// `checking` / `ok` / `failed`（v2 中 `ok` 表示本轮计划完成，不必然全量覆盖）。
    pub state: String,
    /// `startup` / `background`。
    pub mode: String,
    /// `checking` / `open`（启动准入；不是降级状态）。
    pub write_gate: String,
    /// 主库 `quick_check` 是否完成。
    pub main_checked: bool,
    /// 本轮实际完成的分片检查数（不是计划数/总数）。
    pub checked_shards: usize,
    pub total_shards: usize,
    /// 本轮计划检查的分片数（0..8）。
    pub planned_shards: usize,
    /// 本轮计划跳过的分片数 = `total_shards - planned_shards`。
    pub skipped_shards: usize,
    /// 本轮计划是否覆盖主库 + 全部分片。
    pub full_coverage: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StorageCheckSnapshot {
    fn base(
        state: &str,
        mode: &str,
        write_gate: &str,
        total_shards: usize,
        planned_shards: usize,
        full_coverage: bool,
    ) -> Self {
        let planned_shards = planned_shards.min(total_shards);
        Self {
            contract_version: crate::storage_check::STORAGE_CHECK_CONTRACT_VERSION,
            state: state.to_owned(),
            mode: mode.to_owned(),
            write_gate: write_gate.to_owned(),
            main_checked: false,
            checked_shards: 0,
            total_shards,
            planned_shards,
            skipped_shards: total_shards.saturating_sub(planned_shards),
            full_coverage,
            started_at: None,
            completed_at: None,
            error: None,
        }
    }

    /// 全量回退启动检查进行中（写门关闭）。
    pub fn checking(total_shards: usize, started_at: i64) -> Self {
        let mut snapshot = Self::base(
            "checking",
            "startup",
            "checking",
            total_shards,
            total_shards,
            true,
        );
        snapshot.started_at = Some(started_at);
        snapshot
    }

    /// 选择性/仅主库启动检查进行中（`planned_shards` 为计划分片数）。
    pub fn startup_checking(
        total_shards: usize,
        planned_shards: usize,
        full_coverage: bool,
        started_at: i64,
    ) -> Self {
        let mut snapshot = Self::base(
            "checking",
            "startup",
            "checking",
            total_shards,
            planned_shards,
            full_coverage,
        );
        snapshot.started_at = Some(started_at);
        snapshot
    }

    /// 严格跳过（本轮未检查主库/分片；写门 open；不声称全量覆盖）。
    pub fn strict_skipped(total_shards: usize, completed_at: i64) -> Self {
        let mut snapshot = Self::base("ok", "startup", "open", total_shards, 0, false);
        snapshot.started_at = Some(completed_at);
        snapshot.completed_at = Some(completed_at);
        snapshot
    }

    /// 选择性/全量回退启动检查完成（`checked` = 实际完成分片数）。
    pub fn startup_ok(
        total_shards: usize,
        main_checked: bool,
        planned_shards: usize,
        checked_shards: usize,
        full_coverage: bool,
        started_at: i64,
        completed_at: i64,
    ) -> Self {
        let mut snapshot = Self::base(
            "ok",
            "startup",
            "open",
            total_shards,
            planned_shards,
            full_coverage,
        );
        snapshot.main_checked = main_checked;
        snapshot.checked_shards = checked_shards.min(planned_shards).min(total_shards);
        snapshot.started_at = Some(started_at);
        snapshot.completed_at = Some(completed_at);
        snapshot
    }

    /// 周检/20 次启动/基线后台完整检查（写门保持 open）。
    pub fn background_checking(total_shards: usize, started_at: i64) -> Self {
        let mut snapshot = Self::base(
            "checking",
            "background",
            "open",
            total_shards,
            total_shards,
            true,
        );
        snapshot.started_at = Some(started_at);
        snapshot
    }

    /// 启动同步检查完成（ADR-039 兼容：主库 + 8 分片）。
    pub fn ok(total_shards: usize, completed_at: i64) -> Self {
        Self::startup_ok(
            total_shards,
            true,
            total_shards,
            total_shards,
            true,
            completed_at,
            completed_at,
        )
    }

    /// 检查失败（主库/分片）。
    pub fn failed(total_shards: usize, checked_shards: usize, error: String) -> Self {
        let mut snapshot = Self::base(
            "failed",
            "startup",
            "open",
            total_shards,
            total_shards,
            true,
        );
        snapshot.checked_shards = checked_shards.min(total_shards);
        snapshot.error = Some(error);
        snapshot
    }

    /// 检查失败（v2：保留本轮计划/覆盖与主库完成状态）。
    pub fn failed_with(
        total_shards: usize,
        main_checked: bool,
        checked_shards: usize,
        planned_shards: usize,
        full_coverage: bool,
        error: String,
    ) -> Self {
        let mut snapshot = Self::base(
            "failed",
            "startup",
            "checking",
            total_shards,
            planned_shards,
            full_coverage,
        );
        snapshot.main_checked = main_checked;
        snapshot.checked_shards = checked_shards.min(planned_shards).min(total_shards);
        snapshot.error = Some(error);
        snapshot
    }
}

pub use crate::storage_check::{
    detect_storage_device, startup_check_profile, ShardCheckDiagnostic, StartupCheckProfile,
    StorageCheckDiagnostics, StorageDeviceClass, DEFAULT_STARTUP_CHECK_CONCURRENCY,
    STARTUP_CHECK_CONCURRENCY,
};

/// 动态 `health.storage_check` 单元（M10-10 后台并行检查；M10-14 选择性/运行期协调）。
pub struct StorageCheckCell {
    snapshot: Mutex<StorageCheckSnapshot>,
    diagnostics: Mutex<StorageCheckDiagnostics>,
    /// 首个 `health` 请求触发后台检查（避免 5GB quick_check 抢占首屏 I/O）；
    /// 10s 兜底用于无健康轮的测试/无 UI 场景。
    start: tokio::sync::Notify,
    /// 启动路径检查完成（运行期调度器在此之后才开始评估）。
    startup_done: tokio::sync::Notify,
    startup_completed: AtomicBool,
    /// 启动/后台/周期检查共用互斥（0 = 空闲；非 0 = 当前检查 epoch）。
    in_progress_epoch: AtomicU64,
    /// 检查 epoch（超时/取消/迟到结果丢弃）。
    check_epoch: AtomicU64,
    /// 全局取消（shutdown/app_exit/降级）。
    cancelled: AtomicBool,
    cancel_notify: tokio::sync::Notify,
    /// 导入维护窗口（只读；由 `import_control.rs` 持有/复位）。
    import_activity: Mutex<Option<Arc<dyn crate::storage_check::ImportActivitySource>>>,
    /// 备份/恢复维护态（只读；与 `health.maintenance` 同源）。
    maintenance: Mutex<Option<Arc<dyn crate::storage_check::MaintenanceActivitySource>>>,
    /// 串行化 `begin_full_check` 的 epoch 分配与 in-progress 抢占。
    check_lock: Mutex<()>,
}

impl std::fmt::Debug for StorageCheckCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageCheckCell")
            .field("snapshot", &self.snapshot())
            .field("check_in_progress", &self.check_in_progress())
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl StorageCheckCell {
    pub fn new(snapshot: StorageCheckSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
            diagnostics: Mutex::new(StorageCheckDiagnostics::default()),
            start: tokio::sync::Notify::new(),
            startup_done: tokio::sync::Notify::new(),
            startup_completed: AtomicBool::new(false),
            in_progress_epoch: AtomicU64::new(0),
            check_epoch: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            cancel_notify: tokio::sync::Notify::new(),
            import_activity: Mutex::new(None),
            maintenance: Mutex::new(None),
            check_lock: Mutex::new(()),
        }
    }

    /// 设置本轮检查的并发/设备诊断起点（启动或周期检查开始时）。
    pub fn begin_diagnostics(&self, profile: StartupCheckProfile, mode: &str) {
        let next = StorageCheckDiagnostics {
            device: profile.device.code().to_owned(),
            concurrency: profile.concurrency,
            mode: mode.to_owned(),
            ..StorageCheckDiagnostics::default()
        };
        match self.diagnostics.lock() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// 记录一项检查结果（主库/分片耗时与失败数）。
    pub fn record_check(&self, is_main: bool, shard_id: u8, ms: f64, ok: bool) {
        if is_main {
            self.mark_main_checked();
        } else {
            self.mark_shard_checked();
        }
        let update = |diagnostics: &mut StorageCheckDiagnostics| {
            if is_main {
                diagnostics.main_ms = Some(ms);
                diagnostics.main_ok = Some(ok);
            } else {
                diagnostics
                    .shards
                    .push(ShardCheckDiagnostic { shard_id, ms, ok });
            }
            if !ok {
                diagnostics.failure_count = diagnostics.failure_count.saturating_add(1);
            }
        };
        match self.diagnostics.lock() {
            Ok(mut guard) => update(&mut guard),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// 当前诊断快照（证据/测试；不影响 IPC 契约）。
    pub fn diagnostics(&self) -> StorageCheckDiagnostics {
        match self.diagnostics.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 首次 `health` 到达/测试显式触发后台检查。
    pub fn request_start(&self) {
        self.start.notify_one();
    }

    async fn wait_for_start(&self) {
        tokio::select! {
            _ = self.start.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
        }
    }

    pub fn snapshot(&self) -> StorageCheckSnapshot {
        match self.snapshot.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn set(&self, snapshot: StorageCheckSnapshot) {
        match self.snapshot.lock() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    pub fn mark_main_checked(&self) {
        match self.snapshot.lock() {
            Ok(mut guard) => guard.main_checked = true,
            Err(poisoned) => poisoned.into_inner().main_checked = true,
        }
    }

    pub fn mark_shard_checked(&self) {
        match self.snapshot.lock() {
            Ok(mut guard) => {
                guard.checked_shards = guard
                    .checked_shards
                    .saturating_add(1)
                    .min(guard.planned_shards)
                    .min(guard.total_shards);
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.checked_shards = guard
                    .checked_shards
                    .saturating_add(1)
                    .min(guard.planned_shards)
                    .min(guard.total_shards);
            }
        }
    }

    /// ADR-039 兼容完成（全量覆盖）。
    pub fn complete_ok(&self) {
        let completed_at = now_ms();
        match self.snapshot.lock() {
            Ok(mut guard) => {
                guard.state = "ok".to_owned();
                guard.write_gate = "open".to_owned();
                guard.main_checked = true;
                guard.checked_shards = guard.total_shards;
                guard.planned_shards = guard.total_shards;
                guard.skipped_shards = 0;
                guard.full_coverage = true;
                guard.completed_at = Some(completed_at);
                guard.error = None;
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.state = "ok".to_owned();
                guard.write_gate = "open".to_owned();
                guard.main_checked = true;
                guard.checked_shards = guard.total_shards;
                guard.planned_shards = guard.total_shards;
                guard.skipped_shards = 0;
                guard.full_coverage = true;
                guard.completed_at = Some(completed_at);
                guard.error = None;
            }
        }
    }

    /// 启动计划完成（选择性/仅主库/全量回退；不得伪造 8/8）。
    pub fn complete_startup_ok(
        &self,
        main_checked: bool,
        planned_shards: usize,
        checked_shards: usize,
        full_coverage: bool,
        started_at: i64,
    ) {
        let completed_at = now_ms();
        let update = |guard: &mut StorageCheckSnapshot| {
            guard.state = "ok".to_owned();
            guard.mode = "startup".to_owned();
            guard.write_gate = "open".to_owned();
            guard.main_checked = main_checked;
            guard.planned_shards = planned_shards.min(guard.total_shards);
            guard.skipped_shards = guard.total_shards.saturating_sub(guard.planned_shards);
            guard.checked_shards = checked_shards
                .min(guard.planned_shards)
                .min(guard.total_shards);
            guard.full_coverage = full_coverage;
            guard.started_at = Some(started_at);
            guard.completed_at = Some(completed_at);
            guard.error = None;
        };
        match self.snapshot.lock() {
            Ok(mut guard) => update(&mut guard),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// 后台全量/周期完整检查完成（`full_coverage=true`）。
    pub fn complete_background_ok(&self) {
        let completed_at = now_ms();
        let update = |guard: &mut StorageCheckSnapshot| {
            guard.state = "ok".to_owned();
            guard.mode = "background".to_owned();
            guard.write_gate = "open".to_owned();
            guard.main_checked = true;
            guard.planned_shards = guard.total_shards;
            guard.skipped_shards = 0;
            guard.checked_shards = guard.total_shards;
            guard.full_coverage = true;
            guard.completed_at = Some(completed_at);
            guard.error = None;
        };
        match self.snapshot.lock() {
            Ok(mut guard) => update(&mut guard),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// 检查失败（启动/后台；保留本轮计划/覆盖与已完成的项）。
    pub fn fail(&self, error: String) {
        let completed_at = now_ms();
        let update = |guard: &mut StorageCheckSnapshot| {
            guard.state = "failed".to_owned();
            if guard.mode == "startup" {
                guard.write_gate = "checking".to_owned();
            }
            guard.completed_at = Some(completed_at);
            guard.error = Some(error);
        };
        match self.snapshot.lock() {
            Ok(mut guard) => update(&mut guard),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// 检查失败（显式提供 v2 计划/完成字段；后台失败保留 `write_gate=open`）。
    pub fn fail_with(
        &self,
        mode: &str,
        main_checked: bool,
        checked_shards: usize,
        planned_shards: usize,
        full_coverage: bool,
        error: String,
    ) {
        let update = |guard: &mut StorageCheckSnapshot| {
            guard.state = "failed".to_owned();
            guard.mode = mode.to_owned();
            guard.main_checked = main_checked;
            guard.planned_shards = planned_shards.min(guard.total_shards);
            guard.skipped_shards = guard.total_shards.saturating_sub(guard.planned_shards);
            guard.checked_shards = checked_shards
                .min(guard.planned_shards)
                .min(guard.total_shards);
            guard.full_coverage = full_coverage;
            guard.completed_at = Some(now_ms());
            guard.error = Some(error);
        };
        match self.snapshot.lock() {
            Ok(mut guard) => update(&mut guard),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// 启动路径检查完成（唤醒运行期调度器；任何启动分支只通知一次）。
    pub fn notify_startup_done(&self) {
        self.startup_completed.store(true, Ordering::Release);
        self.startup_done.notify_one();
    }

    /// 等待启动路径检查完成（运行期调度器在此之前不评估）。
    pub async fn wait_startup_done(&self) {
        if self.startup_completed.load(Ordering::Acquire) {
            return;
        }
        self.startup_done.notified().await;
    }

    /// 抢占一次完整检查（启动/后台/周期共用；返回本轮 epoch）。
    pub fn begin_full_check(&self) -> Option<u64> {
        if self.cancelled.load(Ordering::Acquire) {
            return None;
        }
        let Ok(_guard) = self.check_lock.lock() else {
            return None;
        };
        if self.in_progress_epoch.load(Ordering::Acquire) != 0 {
            return None;
        }
        let epoch = self
            .check_epoch
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        self.in_progress_epoch.store(epoch, Ordering::Release);
        Some(epoch)
    }

    /// 结束本轮检查（仅当 in-progress 仍属于该 epoch；迟到结果不得清除新检查）。
    pub fn finish_full_check(&self, epoch: u64) {
        let _ =
            self.in_progress_epoch
                .compare_exchange(epoch, 0, Ordering::AcqRel, Ordering::Acquire);
    }

    /// 当前 epoch 是否仍有效（取消/超时/新检查开始后为 false）。
    pub fn is_current(&self, epoch: u64) -> bool {
        !self.cancelled.load(Ordering::Acquire) && self.check_epoch.load(Ordering::Acquire) == epoch
    }

    /// 使当前 epoch 失效（超时/取消但保留调度器继续运行；迟到结果丢弃）。
    pub fn invalidate_epoch(&self, epoch: u64) {
        let _ = self.check_epoch.compare_exchange(
            epoch,
            epoch.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// 当前是否有检查占位（诊断/测试）。
    pub fn check_in_progress(&self) -> bool {
        self.in_progress_epoch.load(Ordering::Acquire) != 0
    }

    /// 取消所有检查并递增 epoch（shutdown/app_exit/降级/超时）。
    pub fn cancel_checks(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.check_epoch.fetch_add(1, Ordering::AcqRel);
        self.cancel_notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// 等待取消通知（运行期调度器用；`Notify` 保存一次许可避免竞态）。
    pub async fn wait_cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.cancel_notify.notified().await;
    }

    /// shutdown/app_exit：取消检查并等待 `grace`；返回检查是否已收口。
    pub async fn shutdown_cancel(&self, grace: std::time::Duration) -> bool {
        self.cancel_checks();
        let started = Instant::now();
        while self.check_in_progress() {
            if started.elapsed() >= grace {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        true
    }

    /// 放弃 in-progress（shutdown grace 超时后；仅用于进程退出前释放诊断状态）。
    pub fn abandon_in_progress(&self) {
        self.in_progress_epoch.store(0, Ordering::Release);
    }

    /// 安装导入维护窗口来源（只读；`import_control.rs` 持有/复位）。
    pub fn install_import_activity(
        &self,
        activity: Arc<dyn crate::storage_check::ImportActivitySource>,
    ) {
        match self.import_activity.lock() {
            Ok(mut guard) => *guard = Some(activity),
            Err(poisoned) => *poisoned.into_inner() = Some(activity),
        }
    }

    pub fn import_in_progress(&self) -> bool {
        match self.import_activity.lock() {
            Ok(guard) => guard
                .as_ref()
                .is_some_and(|activity| activity.import_in_progress()),
            Err(poisoned) => poisoned
                .into_inner()
                .as_ref()
                .is_some_and(|activity| activity.import_in_progress()),
        }
    }

    /// 安装备份/恢复维护态来源（与 `health.maintenance` 同源）。
    pub fn install_maintenance(
        &self,
        activity: Arc<dyn crate::storage_check::MaintenanceActivitySource>,
    ) {
        match self.maintenance.lock() {
            Ok(mut guard) => *guard = Some(activity),
            Err(poisoned) => *poisoned.into_inner() = Some(activity),
        }
    }

    pub fn maintenance_active(&self) -> bool {
        match self.maintenance.lock() {
            Ok(guard) => guard
                .as_ref()
                .is_some_and(|activity| activity.maintenance_active()),
            Err(poisoned) => poisoned
                .into_inner()
                .as_ref()
                .is_some_and(|activity| activity.maintenance_active()),
        }
    }
}

/// 小型 generation（测试/新库）在启动路径内直接完成检查，避免测试与首启竞态；
/// 大型 generation 仍走后台并行检查。
pub const STARTUP_INLINE_CHECK_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// 7 天周期（ADR-040 D-040-3 §3）。
pub const FULL_CHECK_INTERVAL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 墙钟后退判定阈值（ADR-040 D-040-3 §5：>5 分钟保守视为 due 一次）。
const CLOCK_BACKWARD_JUMP_MS: i64 = 5 * 60 * 1000;
/// 墙钟前进判定阈值（ADR-040 D-040-3 §5：>1 天保守视为 due 一次）。
const CLOCK_FORWARD_JUMP_MS: i64 = 24 * 60 * 60 * 1000;

/// 运行期调度器配置（内部常量/测试注入；不新增设置键）。
#[derive(Clone)]
pub struct SchedulerConfig {
    pub enabled: bool,
    /// 轮询兜底（默认 1 小时）。
    pub poll_interval: std::time::Duration,
    /// 周期完整检查超时（默认 30 分钟；DoD-6 按 P99 调整）。
    pub periodic_timeout: std::time::Duration,
    /// 监督重启退避（默认 1s/5s/30s）。
    pub restart_backoff: [std::time::Duration; 3],
    /// 连续失败判定窗口（信息性；当前按连续 3 次重启失败降级）。
    pub failure_window: std::time::Duration,
    /// 运行期检查并发上限（默认 2）。
    pub runtime_concurrency: usize,
    /// 后台全量 `quick_check` 的空闲延迟（默认 10s；测试可注入 0）。
    pub background_delay: std::time::Duration,
    /// 时钟源（测试注入；默认系统墙钟）。
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval: crate::storage_check::PERIODIC_CHECK_POLL,
            periodic_timeout: crate::storage_check::PERIODIC_CHECK_TIMEOUT,
            restart_backoff: crate::storage_check::SCHEDULER_RESTART_BACKOFF,
            failure_window: crate::storage_check::SCHEDULER_FAILURE_WINDOW,
            runtime_concurrency: crate::storage_check::RUNTIME_CHECK_MAX_CONCURRENCY,
            background_delay: std::time::Duration::from_secs(10),
            now: Arc::new(now_ms),
        }
    }
}

/// 调度器测试/故障注入钩子（不改变生产默认行为）。
#[derive(Clone, Default)]
pub struct SchedulerHooks {
    /// 接下来 N 次调度迭代注入“任务异常退出”（覆盖 panic/退出同一监督路径）。
    pub fail_iterations: Arc<AtomicU32>,
    /// 持续注入异常退出（连续 3 次 → 调度器监督失败降级）。
    pub always_fail: Arc<AtomicBool>,
    /// 周期检查启动前延迟（超时/迟到结果注入）。
    pub periodic_delay_ms: Arc<AtomicU64>,
    /// 周期检查实际启动次数（不重复触发断言）。
    pub periodic_runs: Arc<AtomicU32>,
    /// 调度器循环次数（诊断/测试：确认调度器存活）。
    pub scheduler_ticks: Arc<AtomicU32>,
    /// 真实 panic 注入（测试提供闭包；生产默认 `None`）。
    ///
    /// 闭包在调度迭代内执行，panic 由 `tokio::spawn` 捕获为 `JoinError::is_panic`，
    /// 走与真实 panic 相同的监督/重启路径。
    pub panic_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl SchedulerHooks {
    fn maybe_fail(&self) -> Result<(), SchedulerExit> {
        if self.always_fail.load(Ordering::Acquire) {
            return Err(SchedulerExit("injected_scheduler_exit"));
        }
        let mut current = self.fail_iterations.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return Ok(());
            }
            match self.fail_iterations.compare_exchange(
                current,
                current.saturating_sub(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Err(SchedulerExit("injected_scheduler_exit")),
                Err(next) => current = next,
            }
        }
    }
}

/// 调度器任务异常退出（含注入；与真实 panic 共用监督/重启路径）。
#[derive(Debug)]
pub struct SchedulerExit(&'static str);

/// 一次周期/后台检查结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    Completed,
    Failed,
    Cancelled,
    Busy,
}

#[allow(clippy::too_many_arguments)]
async fn run_periodic_full_check(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    last_full: Arc<AtomicI64>,
    clean_count: Arc<AtomicU32>,
    profile: StartupCheckProfile,
    config: SchedulerConfig,
    hooks: SchedulerHooks,
) -> CheckOutcome {
    let Some(epoch) = cell.begin_full_check() else {
        return CheckOutcome::Busy;
    };
    let delay = hooks.periodic_delay_ms.load(Ordering::Acquire);
    if !cell.is_current(epoch) {
        cell.finish_full_check(epoch);
        return CheckOutcome::Cancelled;
    }
    hooks.periodic_runs.fetch_add(1, Ordering::AcqRel);
    let total = layout.shard_paths().len();
    let concurrency = profile.concurrency.min(config.runtime_concurrency).max(1);
    cell.begin_diagnostics(profile, "background");
    cell.set(StorageCheckSnapshot::background_checking(total, now_ms()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    for (index, path) in layout.all_db_paths().into_iter().enumerate() {
        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            cell.finish_full_check(epoch);
            return CheckOutcome::Failed;
        };
        tasks.spawn_blocking(move || {
            if delay > 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            let shard_id = u8::try_from(index.saturating_sub(1)).unwrap_or(0);
            let started = Instant::now();
            let result = integrity_check_path(&path);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            drop(permit);
            (index == 0, shard_id, ms, result)
        });
    }

    let joined = tokio::time::timeout(config.periodic_timeout, async {
        let mut results = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            results.push(joined);
        }
        results
    })
    .await;

    let results = match joined {
        Ok(results) => results,
        Err(_) => {
            if cell.is_current(epoch) {
                cell.invalidate_epoch(epoch);
                cell.finish_full_check(epoch);
                background_flag.store(false, Ordering::Release);
                cell.fail_with(
                    "background",
                    false,
                    0,
                    total,
                    true,
                    "storage_check_timeout".to_owned(),
                );
                write.pause_writes();
                let _ = pipeline
                    .signal_degraded(DegradeTrigger::IntegrityFailure {
                        detail: "storage_check_timeout".to_owned(),
                    })
                    .await;
            } else {
                cell.finish_full_check(epoch);
            }
            return CheckOutcome::Failed;
        }
    };

    let mut failure: Option<String> = None;
    let mut main_checked = false;
    let mut checked = 0usize;
    for joined_result in results {
        match joined_result {
            Ok((is_main, shard_id, ms, result)) => {
                let ok = matches!(&result, Ok(report) if report.ok);
                if cell.is_current(epoch) {
                    cell.record_check(is_main, shard_id, ms, ok);
                }
                if is_main && ok {
                    main_checked = true;
                } else if !is_main && ok {
                    checked = checked.saturating_add(1);
                }
                match result {
                    Ok(report) if report.ok => {}
                    Ok(report) => {
                        failure.get_or_insert_with(|| {
                            format!("integrity_check 失败：{}", report.summary())
                        });
                    }
                    Err(error) => {
                        failure.get_or_insert_with(|| format!("integrity_check 打开失败：{error}"));
                    }
                }
            }
            Err(_) => {
                failure.get_or_insert_with(|| "周期完整检查任务异常退出".to_owned());
            }
        }
    }

    if !cell.is_current(epoch) {
        cell.finish_full_check(epoch);
        return CheckOutcome::Cancelled;
    }
    cell.finish_full_check(epoch);
    if let Some(error) = failure {
        background_flag.store(false, Ordering::Release);
        cell.fail_with(
            "background",
            main_checked,
            checked,
            total,
            true,
            error.clone(),
        );
        write.pause_writes();
        let _ = pipeline
            .signal_degraded(DegradeTrigger::IntegrityFailure { detail: error })
            .await;
        CheckOutcome::Failed
    } else {
        cell.complete_background_ok();
        background_flag.store(true, Ordering::Release);
        last_full.store((config.now)(), Ordering::Release);
        clean_count.store(0, Ordering::Release);
        CheckOutcome::Completed
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_periodic_full_check(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    handle: &tokio::runtime::Handle,
    last_full: Arc<AtomicI64>,
    clean_count: Arc<AtomicU32>,
    profile: StartupCheckProfile,
    config: SchedulerConfig,
    hooks: SchedulerHooks,
) {
    handle.spawn(async move {
        let _ = run_periodic_full_check(
            layout,
            cell,
            write,
            background_flag,
            pipeline,
            last_full,
            clean_count,
            profile,
            config,
            hooks,
        )
        .await;
    });
}

/// 干净/选择性启动后的后台全量 `quick_check`（ADR-040 D-040-3 §1：不关闭写门）。
#[allow(clippy::too_many_arguments)]
async fn run_background_quick_check(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    profile: StartupCheckProfile,
    config: SchedulerConfig,
) -> CheckOutcome {
    cell.wait_for_start().await;
    let Some(epoch) = cell.begin_full_check() else {
        return CheckOutcome::Busy;
    };
    if !config.background_delay.is_zero() {
        tokio::time::sleep(config.background_delay).await;
    }
    if !cell.is_current(epoch) {
        cell.finish_full_check(epoch);
        return CheckOutcome::Cancelled;
    }
    let total = layout.shard_paths().len();
    let concurrency = profile.concurrency.max(1);
    cell.begin_diagnostics(profile, "background");
    cell.set(StorageCheckSnapshot::background_checking(total, now_ms()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    for (index, path) in layout.all_db_paths().into_iter().enumerate() {
        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            cell.finish_full_check(epoch);
            return CheckOutcome::Failed;
        };
        tasks.spawn_blocking(move || {
            let shard_id = u8::try_from(index.saturating_sub(1)).unwrap_or(0);
            let started = Instant::now();
            let result = quick_check_path(&path);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            drop(permit);
            (index == 0, shard_id, ms, result)
        });
    }

    let joined = tokio::time::timeout(config.periodic_timeout, async {
        let mut results = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            results.push(joined);
        }
        results
    })
    .await;

    let results = match joined {
        Ok(results) => results,
        Err(_) => {
            if cell.is_current(epoch) {
                cell.invalidate_epoch(epoch);
                cell.finish_full_check(epoch);
                background_flag.store(false, Ordering::Release);
                cell.fail_with(
                    "background",
                    false,
                    0,
                    total,
                    true,
                    "storage_check_timeout".to_owned(),
                );
                write.pause_writes();
                let _ = pipeline
                    .signal_degraded(DegradeTrigger::IntegrityFailure {
                        detail: "storage_check_timeout".to_owned(),
                    })
                    .await;
            } else {
                cell.finish_full_check(epoch);
            }
            return CheckOutcome::Failed;
        }
    };

    let mut failure: Option<String> = None;
    let mut main_checked = false;
    let mut checked = 0usize;
    for joined_result in results {
        match joined_result {
            Ok((is_main, shard_id, ms, result)) => {
                let ok = matches!(&result, Ok(report) if report.ok);
                if cell.is_current(epoch) {
                    cell.record_check(is_main, shard_id, ms, ok);
                }
                if is_main && ok {
                    main_checked = true;
                } else if !is_main && ok {
                    checked = checked.saturating_add(1);
                }
                match result {
                    Ok(report) if report.ok => {}
                    Ok(report) => {
                        failure.get_or_insert_with(|| {
                            format!("quick_check 失败：{}", report.summary())
                        });
                    }
                    Err(error) => {
                        failure.get_or_insert_with(|| format!("quick_check 打开失败：{error}"));
                    }
                }
            }
            Err(_) => {
                failure.get_or_insert_with(|| "后台检查任务异常退出".to_owned());
            }
        }
    }

    if !cell.is_current(epoch) {
        cell.finish_full_check(epoch);
        return CheckOutcome::Cancelled;
    }
    cell.finish_full_check(epoch);
    if let Some(error) = failure {
        background_flag.store(false, Ordering::Release);
        cell.fail_with(
            "background",
            main_checked,
            checked,
            total,
            true,
            error.clone(),
        );
        write.pause_writes();
        let _ = pipeline
            .signal_degraded(DegradeTrigger::IntegrityFailure { detail: error })
            .await;
        CheckOutcome::Failed
    } else {
        cell.complete_background_ok();
        background_flag.store(true, Ordering::Release);
        CheckOutcome::Completed
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_background_quick_check(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    handle: &tokio::runtime::Handle,
    profile: StartupCheckProfile,
    config: SchedulerConfig,
) {
    handle.spawn(async move {
        let _ = run_background_quick_check(
            layout,
            cell,
            write,
            background_flag,
            pipeline,
            profile,
            config,
        )
        .await;
    });
}

/// 运行期调度器循环（到期点触发；维护/导入/检查中延后；不并发）。
#[allow(clippy::too_many_arguments)]
async fn scheduler_loop(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    last_full: Arc<AtomicI64>,
    clean_count: Arc<AtomicU32>,
    profile: StartupCheckProfile,
    config: SchedulerConfig,
    hooks: SchedulerHooks,
) -> Result<(), SchedulerExit> {
    let mut last_wall = (config.now)();
    let mut last_mono = Instant::now();
    loop {
        hooks.scheduler_ticks.fetch_add(1, Ordering::AcqRel);
        if cell.is_cancelled() {
            return Ok(());
        }
        let now = (config.now)();
        let monotonic_delta = i64::try_from(last_mono.elapsed().as_millis()).unwrap_or(i64::MAX);
        let wall_delta = now.saturating_sub(last_wall);
        let clock_jump = wall_delta < -CLOCK_BACKWARD_JUMP_MS
            || wall_delta > monotonic_delta.saturating_add(CLOCK_FORWARD_JUMP_MS);
        last_wall = now;
        last_mono = Instant::now();

        let last = last_full.load(Ordering::Acquire);
        let due = clean_count.load(Ordering::Acquire) >= 20
            || last <= 0
            || now.saturating_sub(last) >= FULL_CHECK_INTERVAL_MS
            || clock_jump;
        let storage_normal = pipeline.health().storage_state_code() != "persist_degraded";
        if due
            && storage_normal
            && !cell.maintenance_active()
            && !cell.import_in_progress()
            && !cell.check_in_progress()
        {
            if let Some(hook) = hooks.panic_hook.as_ref() {
                hook();
            }
            hooks.maybe_fail()?;
            let outcome = run_periodic_full_check(
                layout.clone(),
                Arc::clone(&cell),
                write.clone(),
                Arc::clone(&background_flag),
                Arc::clone(&pipeline),
                Arc::clone(&last_full),
                Arc::clone(&clean_count),
                profile,
                config.clone(),
                hooks.clone(),
            )
            .await;
            match outcome {
                CheckOutcome::Completed => {}
                CheckOutcome::Failed => return Ok(()),
                CheckOutcome::Cancelled => return Ok(()),
                CheckOutcome::Busy => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            }
        }

        let sleep = if due || clock_jump {
            config.poll_interval
        } else {
            let remaining = last
                .saturating_add(FULL_CHECK_INTERVAL_MS)
                .saturating_sub(now);
            let remaining = u64::try_from(remaining).unwrap_or(0);
            config
                .poll_interval
                .min(std::time::Duration::from_millis(remaining))
        };
        let sleep = sleep.max(std::time::Duration::from_millis(10));
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = cell.wait_cancelled() => return Ok(()),
        }
    }
}

/// 运行期 7 天调度器（受监督：异常退出 → 退避重启；连续 3 次 → 降级）。
#[allow(clippy::too_many_arguments)]
fn spawn_runtime_scheduler(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    handle: &tokio::runtime::Handle,
    last_full: Arc<AtomicI64>,
    clean_count: Arc<AtomicU32>,
    profile: StartupCheckProfile,
    config: SchedulerConfig,
    hooks: SchedulerHooks,
) -> tokio::task::JoinHandle<()> {
    if !config.enabled {
        return handle.spawn(async {});
    }
    handle.spawn(async move {
        cell.wait_startup_done().await;
        let mut failures = 0u32;
        loop {
            if cell.is_cancelled() {
                break;
            }
            let worker = tokio::spawn(scheduler_loop(
                layout.clone(),
                Arc::clone(&cell),
                write.clone(),
                Arc::clone(&background_flag),
                Arc::clone(&pipeline),
                Arc::clone(&last_full),
                Arc::clone(&clean_count),
                profile,
                config.clone(),
                hooks.clone(),
            ));
            let failed = match worker.await {
                Ok(Ok(())) => false,
                Ok(Err(error)) => {
                    tracing::warn!(
                        reason = error.0,
                        attempt = failures + 1,
                        "运行期周期检查调度器异常退出（监督重启）"
                    );
                    true
                }
                Err(error) if error.is_panic() => {
                    tracing::warn!(
                        attempt = failures + 1,
                        "运行期周期检查调度器 panic（监督重启）"
                    );
                    true
                }
                Err(_) => true,
            };
            if !failed {
                break;
            }
            failures = failures.saturating_add(1);
            if failures >= 3 {
                let detail = "storage_check_scheduler_supervision_failed".to_owned();
                background_flag.store(false, Ordering::Release);
                cell.fail_with(
                    "background",
                    false,
                    0,
                    layout.shard_paths().len(),
                    true,
                    detail.clone(),
                );
                cell.cancel_checks();
                write.pause_writes();
                let _ = pipeline
                    .signal_degraded(DegradeTrigger::IntegrityFailure { detail })
                    .await;
                break;
            }
            let backoff = config.restart_backoff[usize::try_from(failures.saturating_sub(1))
                .unwrap_or(0)
                .min(2)];
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = cell.wait_cancelled() => break,
            }
        }
    })
}

/// 启动检查完成后的后台兜底调度（ADR-040 D-040-3 §1）：
/// - 全量回退通过 = 本轮已全量覆盖（`background_check_completed=true`，不重复 `quick_check`）；
/// - 严格跳过/选择性通过 = 后台全量 `quick_check`（`full_check_due` 时由周期完整检查替代）；
/// - 选择性通过不置位 `background_check_completed`。
#[allow(clippy::too_many_arguments)]
fn schedule_post_startup_checks(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    handle: &tokio::runtime::Handle,
    last_full: Arc<AtomicI64>,
    clean_count: Arc<AtomicU32>,
    profile: StartupCheckProfile,
    full_check_due: bool,
    full_coverage: bool,
    config: SchedulerConfig,
    hooks: SchedulerHooks,
) {
    if full_check_due {
        spawn_periodic_full_check(
            layout,
            cell,
            write,
            background_flag,
            pipeline,
            handle,
            last_full,
            clean_count,
            profile,
            config,
            hooks,
        );
    } else if !full_coverage {
        spawn_background_quick_check(
            layout,
            cell,
            write,
            background_flag,
            pipeline,
            handle,
            profile,
            config,
        );
    }
}

/// ADR-040 D-040-1：启动检查（严格跳过 / 选择性 / 全量回退）。
#[allow(clippy::too_many_arguments)]
pub fn start_storage_check(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    handle: &tokio::runtime::Handle,
    full_check_due: bool,
    last_full: Arc<AtomicI64>,
    clean_count: Arc<AtomicU32>,
    scheduler: SchedulerConfig,
    hooks: SchedulerHooks,
) -> tokio::task::JoinHandle<()> {
    let total_shards = layout.shard_paths().len();
    let profile = startup_check_profile(layout.data_dir());
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    let started_at = now_ms();
    let Some(startup_epoch) = cell.begin_full_check() else {
        cell.fail_with(
            "startup",
            false,
            0,
            total_shards,
            true,
            "启动检查状态已被占用".to_owned(),
        );
        cell.notify_startup_done();
        return handle.spawn(async {});
    };
    cell.begin_diagnostics(profile, "startup");

    if plan.strict_ok {
        cell.set(StorageCheckSnapshot::strict_skipped(
            total_shards,
            started_at,
        ));
        write.resume_writes();
        cell.finish_full_check(startup_epoch);
        cell.notify_startup_done();
        schedule_post_startup_checks(
            layout,
            cell,
            write,
            background_flag,
            pipeline,
            handle,
            last_full,
            clean_count,
            profile,
            full_check_due,
            false,
            scheduler,
            hooks,
        );
        return handle.spawn(async {});
    }

    cell.set(StorageCheckSnapshot::startup_checking(
        total_shards,
        plan.planned_shards(),
        plan.full_coverage(),
        started_at,
    ));
    let planned = plan.planned_shards();
    let full_coverage = plan.full_coverage();

    if aether_store::generation_live_bytes(&layout) <= STARTUP_INLINE_CHECK_MAX_BYTES {
        let (main_checked, checked, failure) = run_startup_checks_inline(&layout, &cell, &plan);
        finish_startup_check(
            &layout,
            &cell,
            &write,
            &background_flag,
            &pipeline,
            handle,
            &last_full,
            &clean_count,
            profile,
            scheduler,
            hooks,
            full_check_due,
            startup_epoch,
            main_checked,
            checked,
            planned,
            full_coverage,
            failure,
            started_at,
        );
        return handle.spawn(async {});
    }

    let async_handle = handle.clone();
    async_handle.clone().spawn(async move {
        cell.wait_for_start().await;
        // 让首个 health 响应/壳层 ready 先完成，避免启动 I/O 与首屏争抢。
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        if !cell.is_current(startup_epoch) {
            cell.finish_full_check(startup_epoch);
            return;
        }
        let (main_checked, checked, failure) =
            run_startup_checks_async(&layout, &cell, &plan, startup_epoch).await;
        if !cell.is_current(startup_epoch) {
            cell.finish_full_check(startup_epoch);
            return;
        }
        finish_startup_check(
            &layout,
            &cell,
            &write,
            &background_flag,
            &pipeline,
            &async_handle,
            &last_full,
            &clean_count,
            profile,
            scheduler,
            hooks,
            full_check_due,
            startup_epoch,
            main_checked,
            checked,
            planned,
            full_coverage,
            failure,
            started_at,
        );
    })
}

fn startup_paths(
    layout: &StorageLayout,
    plan: &aether_store::StartupCheckPlan,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if plan.main {
        paths.push(layout.main_db().to_path_buf());
    }
    for shard_id in &plan.shards {
        if let Some(path) = layout.shard_paths().get(usize::from(*shard_id)) {
            paths.push(path.clone());
        }
    }
    paths
}

fn shard_id_for_path(layout: &StorageLayout, path: &std::path::Path) -> u8 {
    layout
        .shard_paths()
        .iter()
        .position(|candidate| candidate == path)
        .and_then(|index| u8::try_from(index).ok())
        .unwrap_or(0)
}

fn run_startup_checks_inline(
    layout: &StorageLayout,
    cell: &StorageCheckCell,
    plan: &aether_store::StartupCheckPlan,
) -> (bool, usize, Option<String>) {
    let mut main_checked = false;
    let mut checked = 0usize;
    let mut failure: Option<String> = None;
    for path in startup_paths(layout, plan) {
        let is_main = path == layout.main_db();
        let shard_id = shard_id_for_path(layout, &path);
        let started = Instant::now();
        let result = quick_check_path(&path);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let ok = matches!(&result, Ok(report) if report.ok);
        cell.record_check(is_main, shard_id, ms, ok);
        if is_main && ok {
            main_checked = true;
        } else if !is_main && ok {
            checked = checked.saturating_add(1);
        }
        match result {
            Ok(report) if report.ok => {}
            Ok(report) => {
                failure.get_or_insert_with(|| format!("quick_check 失败：{}", report.summary()));
            }
            Err(error) => {
                failure.get_or_insert_with(|| format!("quick_check 打开失败：{error}"));
            }
        }
    }
    (main_checked, checked, failure)
}

async fn run_startup_checks_async(
    layout: &StorageLayout,
    cell: &StorageCheckCell,
    plan: &aether_store::StartupCheckPlan,
    epoch: u64,
) -> (bool, usize, Option<String>) {
    let profile = startup_check_profile(layout.data_dir());
    let semaphore = Arc::new(tokio::sync::Semaphore::new(profile.concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for path in startup_paths(layout, plan) {
        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            return (false, 0, Some("启动检查并发闸门已关闭".to_owned()));
        };
        let is_main = path == layout.main_db();
        let shard_id = shard_id_for_path(layout, &path);
        tasks.spawn_blocking(move || {
            let started = Instant::now();
            let result = quick_check_path(&path);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            drop(permit);
            (is_main, shard_id, ms, result)
        });
    }
    let mut main_checked = false;
    let mut checked = 0usize;
    let mut failure: Option<String> = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((is_main, shard_id, ms, result)) => {
                let ok = matches!(&result, Ok(report) if report.ok);
                if cell.is_current(epoch) {
                    cell.record_check(is_main, shard_id, ms, ok);
                }
                if is_main && ok {
                    main_checked = true;
                } else if !is_main && ok {
                    checked = checked.saturating_add(1);
                }
                match result {
                    Ok(report) if report.ok => {}
                    Ok(report) => {
                        failure.get_or_insert_with(|| {
                            format!("quick_check 失败：{}", report.summary())
                        });
                    }
                    Err(error) => {
                        failure.get_or_insert_with(|| format!("quick_check 打开失败：{error}"));
                    }
                }
            }
            Err(_) => {
                failure.get_or_insert_with(|| "启动检查任务异常退出".to_owned());
            }
        }
    }
    (main_checked, checked, failure)
}

#[allow(clippy::too_many_arguments)]
fn finish_startup_check(
    layout: &StorageLayout,
    cell: &Arc<StorageCheckCell>,
    write: &WriteQueue,
    background_flag: &Arc<AtomicBool>,
    pipeline: &Arc<EventPipeline>,
    handle: &tokio::runtime::Handle,
    last_full: &Arc<AtomicI64>,
    clean_count: &Arc<AtomicU32>,
    profile: StartupCheckProfile,
    scheduler: SchedulerConfig,
    hooks: SchedulerHooks,
    full_check_due: bool,
    epoch: u64,
    main_checked: bool,
    checked: usize,
    planned: usize,
    full_coverage: bool,
    failure: Option<String>,
    started_at: i64,
) {
    if !cell.is_current(epoch) {
        cell.finish_full_check(epoch);
        return;
    }
    if let Some(error) = failure {
        let checked_final = checked;
        cell.finish_full_check(epoch);
        cell.fail_with(
            "startup",
            main_checked,
            checked_final,
            planned,
            full_coverage,
            error.clone(),
        );
        let pipeline = Arc::clone(pipeline);
        handle.spawn(async move {
            let _ = pipeline
                .signal_degraded(DegradeTrigger::IntegrityFailure { detail: error })
                .await;
        });
        cell.notify_startup_done();
        return;
    }

    cell.finish_full_check(epoch);
    cell.complete_startup_ok(main_checked, planned, checked, full_coverage, started_at);
    write.resume_writes();
    if full_coverage {
        // 全量回退启动检查 = 一次完整覆盖（ADR-040 D-040-3 §2 setter #1）。
        background_flag.store(true, Ordering::Release);
    }
    cell.notify_startup_done();
    schedule_post_startup_checks(
        layout.clone(),
        Arc::clone(cell),
        write.clone(),
        Arc::clone(background_flag),
        Arc::clone(pipeline),
        handle,
        Arc::clone(last_full),
        Arc::clone(clean_count),
        profile,
        full_check_due,
        full_coverage,
        scheduler,
        hooks,
    );
}

/// `health.maintenance`（ADR-039 D-039-5 §1；M10-09 只保留契约形状，M10-10 接线）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaintenanceSnapshot {
    /// `backup` / `restore`。
    pub kind: String,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<serde_json::Value>,
}

/// `health` 命令返回（ADR-007 附录 A；字段语义见该附录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthReport {
    pub storage_state: String,
    pub write_queue_depth: usize,
    /// `array | null`：`null` = 监督器未接线；`[]` = 已接线且无 runtime；`[...]` = 快照。
    pub runtimes: Option<Vec<RuntimeSummary>>,
    /// `concurrency_gate: { limit, active, queued }`（M7-02/ADR-022 附录 A-5；UI 轮询 5s）。
    /// 未接线（降级启动/测试构造）省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency_gate: Option<aether_control::ConcurrencyGateSnapshot>,
    /// ADR-039 D-039-5：启动检查状态（检查中不是降级；M10-10 异步化）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_check: Option<StorageCheckSnapshot>,
    /// ADR-039 D-039-5：备份/恢复维护态（除 `health`/`app_exit` 外命令拒绝）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<MaintenanceSnapshot>,
    pub ts: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degrade_trigger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_since_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 健康提供者：组合存储快照与 runtime 摘要（只读查询）。
pub struct HealthProvider {
    pipeline: Arc<dyn PipelineHealthSource>,
    runtimes: Arc<dyn RuntimeSummarySource>,
    /// 并发闸门快照单元（M7-02：由 `SessionManager` 单写；未接线 → 字段省略）。
    gate: Option<aether_control::ConcurrencyGateCell>,
    storage_check: Option<Arc<StorageCheckCell>>,
    maintenance: Option<MaintenanceSnapshot>,
    maintenance_cell: Option<Arc<MaintenanceCell>>,
}

impl HealthProvider {
    pub fn new(
        pipeline: Arc<dyn PipelineHealthSource>,
        runtimes: Arc<dyn RuntimeSummarySource>,
    ) -> Self {
        Self {
            pipeline,
            runtimes,
            gate: None,
            storage_check: None,
            maintenance: None,
            maintenance_cell: None,
        }
    }

    /// 接线并发闸门快照单元（M7-02/ADR-022：`health.concurrency_gate`）。
    #[must_use]
    pub fn with_concurrency_gate(mut self, cell: aether_control::ConcurrencyGateCell) -> Self {
        self.gate = Some(cell);
        self
    }

    /// 接线 `health.storage_check`（ADR-039 D-039-5；M10-09 同步 `ok`，M10-10 状态机）。
    #[must_use]
    pub fn with_storage_check(mut self, snapshot: StorageCheckSnapshot) -> Self {
        self.storage_check = Some(Arc::new(StorageCheckCell::new(snapshot)));
        self
    }

    /// 安装动态 `health.storage_check`（M10-10 后台检查）。
    #[must_use]
    pub fn with_storage_check_cell(mut self, cell: Arc<StorageCheckCell>) -> Self {
        self.storage_check = Some(cell);
        self
    }

    /// 接线 `health.maintenance`（ADR-039 D-039-5；备份/恢复维护态）。
    #[must_use]
    pub fn with_maintenance(mut self, snapshot: MaintenanceSnapshot) -> Self {
        self.maintenance = Some(snapshot);
        self
    }

    /// 安装动态 `health.maintenance`（与 IPC 维护态同一共享单元）。
    #[must_use]
    pub fn with_maintenance_cell(mut self, cell: Arc<MaintenanceCell>) -> Self {
        self.maintenance_cell = Some(cell);
        self
    }

    /// M10-10：首个 `health` 请求触发后台启动检查。
    pub fn request_storage_check(&self) {
        if let Some(cell) = self.storage_check.as_ref() {
            cell.request_start();
        }
    }

    /// 生成只读健康报告（不落库、不产生事件）。
    pub fn report(&self) -> HealthReport {
        let snapshot = self.pipeline.snapshot();
        HealthReport {
            storage_state: snapshot.storage_state,
            write_queue_depth: snapshot.write_queue_depth,
            runtimes: self.runtimes.summaries(),
            concurrency_gate: self.gate.as_ref().and_then(|cell| cell.snapshot()),
            storage_check: self.storage_check.as_ref().map(|cell| cell.snapshot()),
            maintenance: self
                .maintenance_cell
                .as_ref()
                .and_then(|cell| cell.get())
                .map(|state| MaintenanceSnapshot {
                    kind: state.kind,
                    started_at: state.started_at,
                    progress: state.progress,
                })
                .or_else(|| self.maintenance.clone()),
            ts: now_ms(),
            degrade_trigger: snapshot.degrade_trigger,
            degraded_since_ms: snapshot.degraded_since_ms,
            detail: snapshot.detail,
        }
    }
}

/// 核心健康后端：仅实现 `health`（其余命令沿用默认 `not_implemented`）。
///
/// 持有存储生命周期锚点（M2-08 起为 [`StorageSlot`]：写任务与读连接随应用生命周期
/// 存活，应用退出时经槽位取走所有权执行存储侧五步关闭）。
/// M2-07 起同时持有 `Arc<EventPipeline>`，供 RSS 巡检等运行期组件复用句柄。
pub struct CoreHealthBackend {
    provider: HealthProvider,
    storage: Option<Arc<StorageSlot>>,
    pipeline: Option<Arc<EventPipeline>>,
}

impl CoreHealthBackend {
    pub fn new(provider: HealthProvider, storage: Option<StoreRuntime>) -> Self {
        Self {
            provider,
            storage: storage.map(|runtime| Arc::new(StorageSlot::new(runtime))),
            pipeline: None,
        }
    }

    /// 由真实管线构造（`storage` 为存储运行时生命周期锚点）。
    pub fn from_pipeline(
        pipeline: EventPipeline,
        runtimes: Arc<dyn RuntimeSummarySource>,
        storage: StoreRuntime,
    ) -> Self {
        Self::from_pipeline_with_slot(
            pipeline,
            runtimes,
            Some(Arc::new(StorageSlot::new(storage))),
        )
    }

    /// 由真实管线构造（共享存储槽位版本；M2-08：退出编排与后端共享同一运行时）。
    pub fn from_pipeline_with_slot(
        pipeline: EventPipeline,
        runtimes: Arc<dyn RuntimeSummarySource>,
        storage: Option<Arc<StorageSlot>>,
    ) -> Self {
        Self::from_pipeline_with_slot_and_gate(pipeline, runtimes, storage, None)
    }

    /// 由真实管线构造 + 闸门快照单元（M7-02/ADR-022：`health.concurrency_gate` 数据源）。
    pub fn from_pipeline_with_slot_and_gate(
        pipeline: EventPipeline,
        runtimes: Arc<dyn RuntimeSummarySource>,
        storage: Option<Arc<StorageSlot>>,
        gate: Option<aether_control::ConcurrencyGateCell>,
    ) -> Self {
        let pipeline = Arc::new(pipeline);
        let source: Arc<dyn PipelineHealthSource> =
            Arc::clone(&pipeline) as Arc<dyn PipelineHealthSource>;
        let provider = HealthProvider::new(source, runtimes).with_storage_check(
            StorageCheckSnapshot::ok(aether_store::SHARD_COUNT, now_ms()),
        );
        let provider = match gate {
            Some(cell) => provider.with_concurrency_gate(cell),
            None => provider,
        };
        Self {
            provider,
            storage,
            pipeline: Some(pipeline),
        }
    }

    /// 只读报告（测试与诊断复用）。
    pub fn report(&self) -> HealthReport {
        self.provider.report()
    }

    /// M10-10：触发后台启动检查（`health` 首次调用）。
    pub fn request_storage_check(&self) {
        self.provider.request_storage_check();
    }

    /// 事件管线句柄（M2-07：RSS 巡检等运行期组件接线；降级后端无管线 → `None`）。
    pub fn pipeline(&self) -> Option<&Arc<EventPipeline>> {
        self.pipeline.as_ref()
    }

    /// 存储槽位（M2-08：退出编排取走所有权执行五步关闭；降级后端 → `None`）。
    pub fn storage_slot(&self) -> Option<Arc<StorageSlot>> {
        self.storage.clone()
    }

    /// M10-10：安装后台启动检查单元。
    #[must_use]
    pub fn with_storage_check_cell(mut self, cell: Arc<StorageCheckCell>) -> Self {
        self.provider = self.provider.with_storage_check_cell(cell);
        self
    }

    /// M10-10：安装动态维护态（备份/恢复 `health.maintenance`）。
    #[must_use]
    pub fn with_maintenance_cell(mut self, cell: Arc<MaintenanceCell>) -> Self {
        self.provider = self.provider.with_maintenance_cell(cell);
        self
    }
}

impl IpcBackend for CoreHealthBackend {
    fn health(&self) -> Result<Value, IpcError> {
        self.provider.request_storage_check();
        serde_json::to_value(self.provider.report())
            .map_err(|error| IpcError::internal(format!("健康报告序列化失败：{error}")))
    }
}

/// 启动失败降级后端（D3 安全模式 → `persist_degraded` 只读呈现）。
///
/// 触发源按 D3 口径记为 `integrity_failure`（库损坏/校验失败家族）；具体原因进 `detail`。
pub fn degraded_backend(reason: &str) -> CoreHealthBackend {
    CoreHealthBackend::new(degraded_provider(reason), None)
}

/// 启动失败降级快照的只读提供者（M3-05：诊断包降级态仍可导出健康段）。
pub fn degraded_provider(reason: &str) -> HealthProvider {
    let snapshot = StorageHealthSnapshot {
        storage_state: "persist_degraded".to_owned(),
        write_queue_depth: 0,
        degrade_trigger: Some("integrity_failure".to_owned()),
        degraded_since_ms: Some(now_ms()),
        detail: Some(reason.to_owned()),
    };
    HealthProvider::new(
        Arc::new(StaticHealthSource::new(snapshot)),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .with_storage_check(StorageCheckSnapshot::failed(
        aether_store::SHARD_COUNT,
        0,
        reason.to_owned(),
    ))
}

/// 核心启动错误（存储打开 / 管线启动）。
#[derive(Debug)]
pub enum CoreHealthBootError {
    /// 存储打开失败（安全模式/IO/SQLite）。
    Storage(StoreError),
    /// 事件管线启动失败（配置/运行时）。
    Pipeline(PipelineError),
}

impl std::fmt::Display for CoreHealthBootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "存储打开失败：{error}"),
            Self::Pipeline(error) => write!(f, "事件管线启动失败：{error}"),
        }
    }
}

impl std::error::Error for CoreHealthBootError {}

/// 生产启动：打开存储（`quick_check` + 迁移）→ 启动事件管线 → 构造健康后端。
///
/// 空间护栏原生探针在 M3-04/M3-05 接线（ADR-006 §5-2 口径：未知 → 不阻断），
/// 启动自检的 `free_bytes` 暂按空间护栏下限传入；完整性由 `StoreRuntime::open`
/// 的 `quick_check` 保证（失败即返回 [`CoreHealthBootError::Storage`]）。
///
/// `runtimes` 摘要源未接线（`health.runtimes = null`）；生产接线见
/// [`boot_core_health_with`]（M2-07 监督器摘要）。
pub fn boot_core_health(
    data_dir: &Path,
    handle: &tokio::runtime::Handle,
) -> Result<CoreHealthBackend, CoreHealthBootError> {
    boot_core_health_with(
        data_dir,
        handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
}

/// 生产启动（M2-07：监督器摘要接线版本）。
///
/// 与 [`boot_core_health`] 相同，仅额外接受 `runtimes` 摘要源：
/// 监督器已接线时传 [`SupervisorRuntimeSummaries`]（`[]` / 状态快照），
/// 未接线时传 [`StaticRuntimeSummaries::unwired`]（`null`）。
pub fn boot_core_health_with(
    data_dir: &Path,
    handle: &tokio::runtime::Handle,
    runtimes: Arc<dyn RuntimeSummarySource>,
) -> Result<CoreHealthBackend, CoreHealthBootError> {
    boot_core_health_with_slot(data_dir, handle, runtimes).map(|(backend, _slot)| backend)
}

/// 生产启动（M2-08：同时返回存储槽位，供应用退出编排执行五步关闭）。
///
/// 语义与 [`boot_core_health_with`] 完全一致；后端与退出编排共享同一
/// [`StorageSlot`]（后端持生命周期锚点，退出时由编排取走所有权）。
pub fn boot_core_health_with_slot(
    data_dir: &Path,
    handle: &tokio::runtime::Handle,
    runtimes: Arc<dyn RuntimeSummarySource>,
) -> Result<(CoreHealthBackend, Arc<StorageSlot>), CoreHealthBootError> {
    boot_core_full(data_dir, handle, runtimes).map(|boot| (boot.backend, boot.storage))
}

/// 核心启动结果（M3-02：额外暴露读连接池/写队列/管线句柄，供会话后端复用）。
pub struct CoreBoot {
    pub backend: CoreHealthBackend,
    pub storage: Arc<StorageSlot>,
    pub reads: aether_store::ReadPool,
    pub write: aether_store::WriteQueue,
    pub pipeline: EventPipeline,
    /// M10-10：可观测的启动检查状态（`health.storage_check` 同源）。
    pub storage_check: Arc<StorageCheckCell>,
    /// M10-10：后台检查任务（关闭/测试可等待）。
    pub storage_check_task: tokio::task::JoinHandle<()>,
    /// M10-14：运行期 7 天调度器（受监督；退出时 abort/cancel）。
    pub scheduler_task: tokio::task::JoinHandle<()>,
}

/// 核心启动选项（M10-14：调度器/测试注入；生产默认见 [`CoreBootOptions::default`]）。
#[derive(Default)]
pub struct CoreBootOptions {
    pub gate: Option<aether_control::ConcurrencyGateCell>,
    pub scheduler: SchedulerConfig,
    pub hooks: SchedulerHooks,
}

/// 生产启动（M3-02 会话后端接线版本）。
///
/// 与 [`boot_core_health_with_slot`] 同一启动序列；额外返回读/写/管线句柄
/// （均为克隆共享句柄，生命周期锚点仍在 [`StorageSlot`]）。
pub fn boot_core_full(
    data_dir: &Path,
    handle: &tokio::runtime::Handle,
    runtimes: Arc<dyn RuntimeSummarySource>,
) -> Result<CoreBoot, CoreHealthBootError> {
    boot_core_full_with_gate(data_dir, handle, runtimes, None)
}

/// 生产启动（M7-02 并发闸门接线版本）：
/// `gate` = 闸门快照单元（与 `SessionManager` 共享；`health.concurrency_gate` 数据源）。
pub fn boot_core_full_with_gate(
    data_dir: &Path,
    handle: &tokio::runtime::Handle,
    runtimes: Arc<dyn RuntimeSummarySource>,
    gate: Option<aether_control::ConcurrencyGateCell>,
) -> Result<CoreBoot, CoreHealthBootError> {
    boot_core_full_with_options(
        data_dir,
        handle,
        runtimes,
        CoreBootOptions {
            gate,
            ..CoreBootOptions::default()
        },
    )
}

/// 生产启动（M10-14：可注入调度器时钟/超时/故障钩子；测试/证据复用）。
pub fn boot_core_full_with_options(
    data_dir: &Path,
    handle: &tokio::runtime::Handle,
    runtimes: Arc<dyn RuntimeSummarySource>,
    options: CoreBootOptions,
) -> Result<CoreBoot, CoreHealthBootError> {
    let layout = aether_store::open_layout(data_dir, env!("CARGO_PKG_VERSION"), now_ms())
        .map_err(CoreHealthBootError::Storage)?;
    let storage =
        StoreRuntime::open_generation_deferred(layout.clone(), WriteQueueConfig::default(), handle)
            .map_err(CoreHealthBootError::Storage)?;
    let reads = storage.reads().clone();
    let write = storage.queue().clone();
    let background_flag = storage.background_check_flag();
    let full_check_due = storage.full_check_due((options.scheduler.now)());
    let (last_full, clean_count) = storage.full_check_state();
    let journal = Arc::new(StoreJournal::new(write.clone()));
    let source = Arc::new(StoreEventSource::new(reads.clone()));
    let startup = StartupSelfCheckReport::passing(SPACE_GUARD_MIN_FREE_BYTES);
    let pipeline =
        EventPipeline::start(PipelineConfig::default(), journal, source, &startup, handle)
            .map_err(CoreHealthBootError::Pipeline)?;
    let storage_check = Arc::new(StorageCheckCell::new(StorageCheckSnapshot::checking(
        layout.shard_paths().len(),
        now_ms(),
    )));
    let slot = Arc::new(StorageSlot::new(storage));
    let backend = CoreHealthBackend::from_pipeline_with_slot_and_gate(
        pipeline.clone(),
        runtimes,
        Some(Arc::clone(&slot)),
        options.gate,
    )
    .with_storage_check_cell(Arc::clone(&storage_check));
    let profile = startup_check_profile(layout.data_dir());
    let scheduler_pipeline = Arc::new(pipeline.clone());
    let write_for_boot = write.clone();
    let storage_check_task = start_storage_check(
        layout.clone(),
        Arc::clone(&storage_check),
        write.clone(),
        Arc::clone(&background_flag),
        Arc::clone(&scheduler_pipeline),
        handle,
        full_check_due,
        Arc::clone(&last_full),
        Arc::clone(&clean_count),
        options.scheduler.clone(),
        options.hooks.clone(),
    );
    let scheduler_task = spawn_runtime_scheduler(
        layout,
        Arc::clone(&storage_check),
        write,
        background_flag,
        scheduler_pipeline,
        handle,
        last_full,
        clean_count,
        profile,
        options.scheduler,
        options.hooks,
    );
    Ok(CoreBoot {
        backend,
        storage: slot,
        reads,
        write: write_for_boot,
        pipeline,
        storage_check,
        storage_check_task,
        scheduler_task,
    })
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_projects_snapshot_and_skips_absent_fields() {
        let source = Arc::new(StaticHealthSource::new(StorageHealthSnapshot {
            storage_state: "normal".to_owned(),
            write_queue_depth: 7,
            degrade_trigger: None,
            degraded_since_ms: None,
            detail: None,
        }));
        let provider = HealthProvider::new(source, Arc::new(StaticRuntimeSummaries::unwired()));
        let report = provider.report();
        assert_eq!(report.storage_state, "normal");
        assert_eq!(report.write_queue_depth, 7);
        assert!(report.runtimes.is_none(), "未接线 → null");
        assert!(report.ts > 0);
        let value = serde_json::to_value(&report).expect("序列化");
        assert!(
            value.get("runtimes").is_some(),
            "runtimes 键必须存在（值为 null）"
        );
        assert!(value["runtimes"].is_null(), "未接线 → null");
        assert!(
            value.get("degrade_trigger").is_none(),
            "可选字段缺省不序列化"
        );
        assert!(value.get("detail").is_none());
    }

    /// ADR-007 增量 2：`runtimes` 的 `null`（未接线）与 `[]`（已接线无 runtime）语义区分。
    #[test]
    fn runtime_summaries_distinguish_unwired_and_wired() {
        let source = Arc::new(StaticHealthSource::new(StorageHealthSnapshot {
            storage_state: "normal".to_owned(),
            write_queue_depth: 0,
            degrade_trigger: None,
            degraded_since_ms: None,
            detail: None,
        }));

        let unwired = HealthProvider::new(
            Arc::clone(&source) as Arc<dyn PipelineHealthSource>,
            Arc::new(StaticRuntimeSummaries::unwired()),
        )
        .report();
        assert!(unwired.runtimes.is_none());
        let value = serde_json::to_value(&unwired).expect("序列化");
        assert!(value["runtimes"].is_null(), "未接线 → null");

        let wired_empty = HealthProvider::new(
            Arc::clone(&source) as Arc<dyn PipelineHealthSource>,
            Arc::new(StaticRuntimeSummaries::wired(Vec::new())),
        )
        .report();
        assert_eq!(wired_empty.runtimes, Some(Vec::new()));
        let value = serde_json::to_value(&wired_empty).expect("序列化");
        assert_eq!(value["runtimes"], serde_json::json!([]), "已接线空 → []");

        let wired_one = HealthProvider::new(
            Arc::clone(&source) as Arc<dyn PipelineHealthSource>,
            Arc::new(StaticRuntimeSummaries::wired(vec![RuntimeSummary {
                id: "mock".to_owned(),
                status: "ready".to_owned(),
                status_reason: None,
            }])),
        )
        .report();
        let value = serde_json::to_value(&wired_one).expect("序列化");
        assert_eq!(value["runtimes"][0]["id"], "mock");
        assert_eq!(value["runtimes"][0]["status"], "ready");
    }

    #[test]
    fn device_classes_map_to_adr_039_concurrency() {
        assert_eq!(StorageDeviceClass::Nvme.startup_check_concurrency(), 8);
        assert_eq!(StorageDeviceClass::Ssd.startup_check_concurrency(), 4);
        assert_eq!(StorageDeviceClass::Hdd.startup_check_concurrency(), 2);
        assert_eq!(
            StorageDeviceClass::Unknown.startup_check_concurrency(),
            DEFAULT_STARTUP_CHECK_CONCURRENCY
        );
        assert_eq!(StorageDeviceClass::Hdd.code(), "hdd");
    }

    #[test]
    fn degraded_backend_serializes_read_only_state() {
        let backend = degraded_backend("quick_check 失败");
        let report = backend.report();
        assert_eq!(report.storage_state, "persist_degraded");
        assert_eq!(report.degrade_trigger.as_deref(), Some("integrity_failure"));
        assert_eq!(report.detail.as_deref(), Some("quick_check 失败"));
        assert!(report.degraded_since_ms.is_some());
    }
}
