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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_adapters::supervisor::Supervisor;
use aether_control::{
    DegradeTrigger, EventPipeline, PipelineConfig, PipelineError, StartupSelfCheckReport,
    StoreEventSource, StoreJournal, SPACE_GUARD_MIN_FREE_BYTES,
};
use aether_store::{
    quick_check_path, StorageLayout, StoreError, StoreRuntime, WriteQueue, WriteQueueConfig,
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

/// `health.storage_check`（ADR-039 D-039-5 §1；M10-09 启动检查同步完成后为 `ok`，
/// M10-10 改为并行异步状态机）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageCheckSnapshot {
    /// `checking` / `ok` / `failed`。
    pub state: String,
    /// `startup` / `background`。
    pub mode: String,
    /// `checking` / `open`（启动准入；不是降级状态）。
    pub write_gate: String,
    /// 主库 `quick_check` 是否完成。
    pub main_checked: bool,
    /// 已完成分片数（x/8）。
    pub checked_shards: usize,
    pub total_shards: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StorageCheckSnapshot {
    /// 检查进行中（M10-10 启动；写门关闭）。
    pub fn checking(total_shards: usize, started_at: i64) -> Self {
        Self {
            state: "checking".to_owned(),
            mode: "startup".to_owned(),
            write_gate: "checking".to_owned(),
            main_checked: false,
            checked_shards: 0,
            total_shards,
            started_at: Some(started_at),
            completed_at: None,
            error: None,
        }
    }

    /// 启动同步检查完成（M10-09 口径；主库 + 8 分片）。
    pub fn ok(total_shards: usize, completed_at: i64) -> Self {
        Self {
            state: "ok".to_owned(),
            mode: "startup".to_owned(),
            write_gate: "open".to_owned(),
            main_checked: true,
            checked_shards: total_shards,
            total_shards,
            started_at: Some(completed_at),
            completed_at: Some(completed_at),
            error: None,
        }
    }

    /// 检查失败（主库/分片）。
    pub fn failed(total_shards: usize, checked_shards: usize, error: String) -> Self {
        Self {
            state: "failed".to_owned(),
            mode: "startup".to_owned(),
            write_gate: "open".to_owned(),
            main_checked: false,
            checked_shards,
            total_shards,
            started_at: None,
            completed_at: None,
            error: Some(error),
        }
    }
}

/// 动态 `health.storage_check` 单元（M10-10：后台并行 `quick_check` 更新）。
#[derive(Debug)]
pub struct StorageCheckCell {
    snapshot: Mutex<StorageCheckSnapshot>,
    /// 首个 `health` 请求触发后台检查（避免 5GB quick_check 抢占首屏 I/O）；
    /// 10s 兜底用于无健康轮的测试/无 UI 场景。
    start: tokio::sync::Notify,
}

impl StorageCheckCell {
    pub fn new(snapshot: StorageCheckSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
            start: tokio::sync::Notify::new(),
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
                    .min(guard.total_shards);
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.checked_shards = guard
                    .checked_shards
                    .saturating_add(1)
                    .min(guard.total_shards);
            }
        }
    }

    pub fn complete_ok(&self) {
        match self.snapshot.lock() {
            Ok(mut guard) => {
                guard.state = "ok".to_owned();
                guard.write_gate = "open".to_owned();
                guard.main_checked = true;
                guard.checked_shards = guard.total_shards;
                guard.completed_at = Some(now_ms());
                guard.error = None;
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.state = "ok".to_owned();
                guard.write_gate = "open".to_owned();
                guard.main_checked = true;
                guard.checked_shards = guard.total_shards;
                guard.completed_at = Some(now_ms());
                guard.error = None;
            }
        }
    }

    pub fn fail(&self, error: String) {
        match self.snapshot.lock() {
            Ok(mut guard) => {
                guard.state = "failed".to_owned();
                guard.write_gate = "checking".to_owned();
                guard.completed_at = Some(now_ms());
                guard.error = Some(error);
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.state = "failed".to_owned();
                guard.write_gate = "checking".to_owned();
                guard.completed_at = Some(now_ms());
                guard.error = Some(error);
            }
        }
    }
}

/// M10-10 启动检查并发上限（ADR-039 D-039-3：默认 4；SSD/HHD 可后续按设备调整）。
pub const STARTUP_CHECK_CONCURRENCY: usize = 8;
/// 小型 generation（测试/新库）在启动路径内直接完成检查，避免测试与首启竞态；
/// 大型 generation 仍走后台并行检查。
pub const STARTUP_INLINE_CHECK_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// M10-10：启动后台并行 `quick_check`（主库 + 8 分片）。
///
/// - 严格 sidecar 全匹配：直接 `ok` 并恢复写门（不跑 quick_check）；
/// - 否则以 4 并发检查各库，逐项更新 `health.storage_check`；
/// - 全部通过：`write_gate=open`，写队列恢复，sidecar 完成标志置位；
/// - 任一失败：`state=failed`、写门保持关闭，并 `signal_degraded(integrity_failure)`。
pub fn start_storage_check(
    layout: StorageLayout,
    cell: Arc<StorageCheckCell>,
    write: WriteQueue,
    background_flag: Arc<AtomicBool>,
    pipeline: Arc<EventPipeline>,
    handle: &tokio::runtime::Handle,
) -> tokio::task::JoinHandle<()> {
    let total_shards = layout.shard_paths().len();
    cell.set(StorageCheckSnapshot::checking(total_shards, now_ms()));
    if aether_store::generation_live_bytes(&layout) <= STARTUP_INLINE_CHECK_MAX_BYTES {
        let mut failure: Option<String> = None;
        if !aether_store::clean_shutdown_is_strict(&layout, env!("CARGO_PKG_VERSION")) {
            for path in layout.all_db_paths() {
                match quick_check_path(&path) {
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
        }
        if let Some(error) = failure {
            cell.fail(error.clone());
            let pipeline = Arc::clone(&pipeline);
            handle.spawn(async move {
                let _ = pipeline
                    .signal_degraded(DegradeTrigger::IntegrityFailure { detail: error })
                    .await;
            });
        } else {
            cell.complete_ok();
            background_flag.store(true, Ordering::Release);
            write.resume_writes();
        }
        return handle.spawn(async {});
    }
    handle.spawn(async move {
        if aether_store::clean_shutdown_is_strict(&layout, env!("CARGO_PKG_VERSION")) {
            cell.complete_ok();
            background_flag.store(true, Ordering::Release);
            write.resume_writes();
            return;
        }
        cell.wait_for_start().await;
        // 让首个 health 响应/壳层 ready 先完成，避免启动 I/O 与首屏争抢。
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        let main_path = layout.main_db().to_path_buf();
        let shard_paths = layout.shard_paths().to_vec();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(STARTUP_CHECK_CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();

        // 主库检查很小，不占 8 个分片 worker 名额（主库仍计入 main_checked）。
        let main_cell = Arc::clone(&cell);
        tasks.spawn_blocking(move || {
            let result = quick_check_path(&main_path);
            (true, result, main_cell)
        });
        for path in shard_paths {
            let permit = match Arc::clone(&semaphore).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    cell.fail("启动检查并发闸门已关闭".to_owned());
                    return;
                }
            };
            let shard_cell = Arc::clone(&cell);
            tasks.spawn_blocking(move || {
                let result = quick_check_path(&path);
                drop(permit);
                (false, result, shard_cell)
            });
        }

        let mut failure: Option<String> = None;
        while let Some(joined) = tasks.join_next().await {
            let Ok((is_main, result, task_cell)) = joined else {
                failure.get_or_insert_with(|| "启动检查任务异常退出".to_owned());
                continue;
            };
            if is_main {
                task_cell.mark_main_checked();
            } else {
                task_cell.mark_shard_checked();
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

        if let Some(error) = failure {
            cell.fail(error.clone());
            let _ = pipeline
                .signal_degraded(DegradeTrigger::IntegrityFailure { detail: error })
                .await;
        } else {
            cell.complete_ok();
            background_flag.store(true, Ordering::Release);
            write.resume_writes();
        }
    })
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
            storage_check: self
                .storage_check
                .as_ref()
                .map(|cell| cell.snapshot()),
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
    let layout = aether_store::open_layout(data_dir, env!("CARGO_PKG_VERSION"), now_ms())
        .map_err(CoreHealthBootError::Storage)?;
    let storage = StoreRuntime::open_generation_deferred(
        layout.clone(),
        WriteQueueConfig::default(),
        handle,
    )
    .map_err(CoreHealthBootError::Storage)?;
    let reads = storage.reads().clone();
    let write = storage.queue().clone();
    let background_flag = storage.background_check_flag();
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
        gate,
    )
    .with_storage_check_cell(Arc::clone(&storage_check));
    let storage_check_task = start_storage_check(
        layout,
        Arc::clone(&storage_check),
        write.clone(),
        background_flag,
        Arc::new(pipeline.clone()),
        handle,
    );
    Ok(CoreBoot {
        backend,
        storage: slot,
        reads,
        write,
        pipeline,
        storage_check,
        storage_check_task,
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
    fn degraded_backend_serializes_read_only_state() {
        let backend = degraded_backend("quick_check 失败");
        let report = backend.report();
        assert_eq!(report.storage_state, "persist_degraded");
        assert_eq!(report.degrade_trigger.as_deref(), Some("integrity_failure"));
        assert_eq!(report.detail.as_deref(), Some("quick_check 失败"));
        assert!(report.degraded_since_ms.is_some());
    }
}
