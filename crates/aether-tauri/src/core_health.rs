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
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_adapters::supervisor::Supervisor;
use aether_control::{
    EventPipeline, PipelineConfig, PipelineError, StartupSelfCheckReport, StoreEventSource,
    StoreJournal, SPACE_GUARD_MIN_FREE_BYTES,
};
use aether_store::{StoreError, StoreRuntime, WriteQueueConfig};
use serde::Serialize;
use serde_json::Value;

use crate::ipc::backend::IpcBackend;
use crate::ipc::error::IpcError;
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

/// `health` 命令返回（ADR-007 附录 A；字段语义见该附录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthReport {
    pub storage_state: String,
    pub write_queue_depth: usize,
    /// `array | null`：`null` = 监督器未接线；`[]` = 已接线且无 runtime；`[...]` = 快照。
    pub runtimes: Option<Vec<RuntimeSummary>>,
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
}

impl HealthProvider {
    pub fn new(
        pipeline: Arc<dyn PipelineHealthSource>,
        runtimes: Arc<dyn RuntimeSummarySource>,
    ) -> Self {
        Self { pipeline, runtimes }
    }

    /// 生成只读健康报告（不落库、不产生事件）。
    pub fn report(&self) -> HealthReport {
        let snapshot = self.pipeline.snapshot();
        HealthReport {
            storage_state: snapshot.storage_state,
            write_queue_depth: snapshot.write_queue_depth,
            runtimes: self.runtimes.summaries(),
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
        let pipeline = Arc::new(pipeline);
        let source: Arc<dyn PipelineHealthSource> =
            Arc::clone(&pipeline) as Arc<dyn PipelineHealthSource>;
        Self {
            provider: HealthProvider::new(source, runtimes),
            storage,
            pipeline: Some(pipeline),
        }
    }

    /// 只读报告（测试与诊断复用）。
    pub fn report(&self) -> HealthReport {
        self.provider.report()
    }

    /// 事件管线句柄（M2-07：RSS 巡检等运行期组件接线；降级后端无管线 → `None`）。
    pub fn pipeline(&self) -> Option<&Arc<EventPipeline>> {
        self.pipeline.as_ref()
    }

    /// 存储槽位（M2-08：退出编排取走所有权执行五步关闭；降级后端 → `None`）。
    pub fn storage_slot(&self) -> Option<Arc<StorageSlot>> {
        self.storage.clone()
    }
}

impl IpcBackend for CoreHealthBackend {
    fn health(&self) -> Result<Value, IpcError> {
        serde_json::to_value(self.provider.report())
            .map_err(|error| IpcError::internal(format!("健康报告序列化失败：{error}")))
    }
}

/// 启动失败降级后端（D3 安全模式 → `persist_degraded` 只读呈现）。
///
/// 触发源按 D3 口径记为 `integrity_failure`（库损坏/校验失败家族）；具体原因进 `detail`。
pub fn degraded_backend(reason: &str) -> CoreHealthBackend {
    let snapshot = StorageHealthSnapshot {
        storage_state: "persist_degraded".to_owned(),
        write_queue_depth: 0,
        degrade_trigger: Some("integrity_failure".to_owned()),
        degraded_since_ms: Some(now_ms()),
        detail: Some(reason.to_owned()),
    };
    CoreHealthBackend::new(
        HealthProvider::new(
            Arc::new(StaticHealthSource::new(snapshot)),
            Arc::new(StaticRuntimeSummaries::unwired()),
        ),
        None,
    )
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
    let db_path = data_dir.join("aether.db");
    let storage = StoreRuntime::open(&db_path, WriteQueueConfig::default(), handle)
        .map_err(CoreHealthBootError::Storage)?;
    let journal = Arc::new(StoreJournal::new(storage.queue().clone()));
    let source = Arc::new(StoreEventSource::new(storage.reads().clone()));
    let startup = StartupSelfCheckReport::passing(SPACE_GUARD_MIN_FREE_BYTES);
    let pipeline =
        EventPipeline::start(PipelineConfig::default(), journal, source, &startup, handle)
            .map_err(CoreHealthBootError::Pipeline)?;
    let slot = Arc::new(StorageSlot::new(storage));
    let backend =
        CoreHealthBackend::from_pipeline_with_slot(pipeline, runtimes, Some(Arc::clone(&slot)));
    Ok((backend, slot))
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
