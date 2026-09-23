//! M2-08 应用退出编排（D2 关闭序列 + D5 适配器终止段 + T11 退出预算）。
//!
//! 序列（与 D2 冻结顺序一致，M2-06 交付存储侧五步；本模块补齐应用级编排）：
//! 1. 广播 shutdown：停止事件生产（`EventPipeline::shutdown`，放弃未落盘 delta）；
//! 2. 适配器终止段：每个运行时执行 D5 逐步硬超时序列
//!    （`shutdown` RPC 5s → TERM/`taskkill /T` 5s → KILL/`TerminateJobObject` → 兜底
//!    `taskkill /T /F`；机制名进入退出证据）；
//! 3. 存储侧五步：drain → 关闭全部读连接 → `wal_checkpoint(TRUNCATE)` → 关闭写连接 → 退出
//!    （M2-06 `StoreRuntime::shutdown`）。
//!
//! T11（附录 D）：适配器无响应下应用退出 ≤10s。本模块以 [`APP_EXIT_BUDGET`] 对
//! 「管线停机 + 适配器终止 + 存储五步」整体加硬上限：超限即记录并放行退出
//! （D2「保证能退出优先」）；正常路径（deaf 适配器）终止段在 `shutdown` RPC 超时
//! 后由强制整树回收立即收口，整个序列 <10s。
//!
//! 接线：Tauri `RunEvent::ExitRequested` → [`on_exit_requested`]（`app_exit` 命令与
//! 窗口关闭同路径）；未接线（启动门阻断/测试骨架）时直接放行，不拦截退出。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_adapters::supervisor::Supervisor;
use aether_control::EventPipeline;
use aether_store::{ShutdownReport, StoreRuntime};
use serde::Serialize;
use tauri::Manager;
use tokio::task::{JoinHandle, JoinSet};

use crate::ipc::IpcState;

/// T11 上限：适配器无响应下退出应用 ≤10s（附录 D）。
pub const APP_EXIT_BUDGET: Duration = Duration::from_secs(10);
/// 退出证据行（stdout，一行一条；供 `scripts/test/m2-08/verify-m2-08.mjs` 解析）。
pub const EXIT_EVIDENCE_LINE: &str = "AETHER_M2_08_EXIT";
/// `run_blocking` 在预算之外额外等待的余量（仍超限则直接放行应用退出）。
pub const EXIT_BLOCKING_SLACK: Duration = Duration::from_secs(2);

/// 退出清理阶段（`ExitRequested` 防重入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitPhase {
    /// 尚未开始（未拦截过退出）。
    Idle,
    /// 清理执行中（重复的 ExitRequested 一律拦截）。
    Running,
    /// 清理完成（放行第二次退出请求，携带原始退出码）。
    Done,
}

impl ExitPhase {
    pub const fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Running => 1,
            Self::Done => 2,
        }
    }

    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Running,
            2 => Self::Done,
            _ => Self::Idle,
        }
    }
}

/// 存储运行时槽位：`CoreHealthBackend` 持有生命周期锚点，退出序列经槽位取走所有权
/// 执行五步关闭（只能取走一次，取走后仅剩余诊断信息）。
pub struct StorageSlot {
    cell: Mutex<Option<StoreRuntime>>,
    wal_path: PathBuf,
}

impl StorageSlot {
    pub fn new(runtime: StoreRuntime) -> Self {
        let wal_path = runtime.wal_path();
        Self {
            cell: Mutex::new(Some(runtime)),
            wal_path,
        }
    }

    /// 取走存储运行时（退出序列唯一消费者；重复调用返回 `None`）。
    pub fn take(&self) -> Option<StoreRuntime> {
        match self.cell.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    /// `-wal` 文件当前字节数（退出后应为 0；文件不存在 → 0）。
    pub fn wal_bytes(&self) -> u64 {
        std::fs::metadata(&self.wal_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }
}

/// 单个适配器的终止结果（平台机制序列作为断言/归档证据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdapterExitReport {
    pub runtime_id: String,
    /// 终止序列结束时整树是否已退出。
    pub exited: bool,
    pub total_ms: u64,
    /// 实际机制序列（如 `shutdown_rpc` / `taskkill_tree` / `terminate_job_object`）。
    pub mechanisms: Vec<String>,
    /// 实际步骤序列（`shutdown_rpc` / `graceful` / `force` / `fallback`）。
    pub steps: Vec<String>,
}

/// 存储侧关闭结果（五步顺序与 WAL 归零证据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageExitReport {
    /// 步骤顺序与 D2 完全一致（M2-06 shadow 日志判定）。
    pub d2_order: bool,
    pub drained: bool,
    pub duration_ms: u64,
    /// 关闭后 `-wal` 字节数（期望 0）。
    pub wal_bytes_after: u64,
    /// shadow 日志（逐步诊断）。
    pub shadow_log: String,
}

impl StorageExitReport {
    fn from_report(report: &ShutdownReport, wal_bytes_after: u64) -> Self {
        Self {
            d2_order: report.matches_d2_order(),
            drained: report.drained,
            duration_ms: report.duration_ms,
            wal_bytes_after,
            shadow_log: report.shadow_log(),
        }
    }
}

/// 应用退出报告（T11 证据 + 归档）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppExitReport {
    pub total_ms: u64,
    pub budget_ms: u64,
    /// 是否在 T11 预算内完成全部退出步骤。
    pub within_budget: bool,
    /// 管线停机（广播 shutdown）是否成功。
    pub pipeline_shutdown_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_shutdown_error: Option<String>,
    pub adapters: Vec<AdapterExitReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageExitReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_error: Option<String>,
    /// 是否触发退出总预算（超限：记录并放行退出，D2「保证能退出优先」）。
    pub deadline_expired: bool,
}

impl AppExitReport {
    /// 证据行（stdout，JSON；脚本解析后归档）。
    pub fn evidence_line(&self) -> String {
        match serde_json::to_string(self) {
            Ok(json) => format!("{EXIT_EVIDENCE_LINE} {json}"),
            Err(error) => format!("{EXIT_EVIDENCE_LINE} {{\"serialize_error\":\"{error}\"}}"),
        }
    }

    /// 无退出对象（启动门阻断/空后端）时合成。
    pub fn empty(budget: Duration) -> Self {
        Self {
            total_ms: 0,
            budget_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
            within_budget: true,
            pipeline_shutdown_ok: true,
            pipeline_shutdown_error: None,
            adapters: Vec::new(),
            storage: None,
            storage_error: None,
            deadline_expired: false,
        }
    }
}

/// 应用退出编排器（构造于核心启动完成、注入 `IpcState`）。
pub struct AppShutdown {
    pipeline: Option<EventPipeline>,
    supervisor: Option<Arc<Supervisor>>,
    storage: Option<Arc<StorageSlot>>,
    monitors: Mutex<Vec<(String, JoinHandle<()>)>>,
    handle: tokio::runtime::Handle,
    budget: Duration,
}

impl AppShutdown {
    pub fn new(
        pipeline: Option<EventPipeline>,
        supervisor: Option<Arc<Supervisor>>,
        storage: Option<Arc<StorageSlot>>,
        monitors: Vec<(String, JoinHandle<()>)>,
        handle: tokio::runtime::Handle,
    ) -> Arc<Self> {
        Arc::new(Self {
            pipeline,
            supervisor,
            storage,
            monitors: Mutex::new(monitors),
            handle,
            budget: APP_EXIT_BUDGET,
        })
    }

    /// 退出预算（测试可观测）。
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// 执行退出序列（D2：广播 shutdown → 适配器终止段 → 存储五步）。
    pub async fn run(&self) -> AppExitReport {
        let started = Instant::now();
        let deadline = started + self.budget;

        // 0. 停止监控任务：退出期间不再触发心跳/资源采样与自动重启。
        {
            let mut monitors = match self.monitors.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            for (_, handle) in monitors.drain(..) {
                handle.abort();
            }
        }

        // 1. 广播 shutdown：停止接受新事件并放弃未落盘 delta（D2 第一步）。
        let (pipeline_shutdown_ok, pipeline_shutdown_error) = match &self.pipeline {
            Some(pipeline) => match pipeline.shutdown().await {
                Ok(()) => (true, None),
                Err(error) => (false, Some(error.to_string())),
            },
            None => (true, None),
        };

        // 2. 适配器终止段（D5 序列，跨运行时并发；整体受剩余预算硬约束）。
        let mut adapter_reports = Vec::new();
        let mut deadline_expired = false;
        if let Some(supervisor) = &self.supervisor {
            let mut joinset: JoinSet<(String, aether_adapters::supervisor::TerminationReport)> =
                JoinSet::new();
            for runtime_id in supervisor.runtime_ids() {
                if let Some(runtime) = supervisor.get(&runtime_id) {
                    joinset.spawn(async move {
                        let report = runtime.shutdown().await;
                        (runtime_id, report)
                    });
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, async {
                let mut entries = Vec::new();
                while let Some(joined) = joinset.join_next().await {
                    if let Ok(entry) = joined {
                        entries.push(entry);
                    }
                }
                entries
            })
            .await
            {
                Ok(entries) => {
                    for (runtime_id, report) in entries {
                        adapter_reports.push(AdapterExitReport {
                            runtime_id,
                            exited: report.exited,
                            total_ms: report.total_ms,
                            mechanisms: report
                                .mechanisms()
                                .into_iter()
                                .map(str::to_owned)
                                .collect(),
                            steps: report
                                .executed_steps()
                                .into_iter()
                                .map(|step| step.as_str().to_owned())
                                .collect(),
                        });
                    }
                }
                Err(_) => {
                    deadline_expired = true;
                    joinset.abort_all();
                    tracing::warn!("适配器终止段超出退出预算（已中止，交由强制回收/进程退出兜底）");
                }
            }
        }

        // 3. 存储侧五步（剩余预算内；超限记录后放行退出）。
        let (storage_report, storage_error) = match &self.storage {
            Some(slot) => match slot.take() {
                Some(runtime) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        deadline_expired = true;
                        drop(runtime);
                        (
                            None,
                            Some("存储关闭未在退出预算内开始（已放弃，退出优先）".to_owned()),
                        )
                    } else {
                        match tokio::time::timeout(remaining, runtime.shutdown()).await {
                            Ok(Ok(report)) => (
                                Some(StorageExitReport::from_report(&report, slot.wal_bytes())),
                                None,
                            ),
                            Ok(Err(error)) => (None, Some(error.to_string())),
                            Err(_) => {
                                deadline_expired = true;
                                (None, Some(format!("存储关闭超出退出预算（{remaining:?}）")))
                            }
                        }
                    }
                }
                None => (None, None),
            },
            None => (None, None),
        };

        let total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let budget_ms = u64::try_from(self.budget.as_millis()).unwrap_or(u64::MAX);
        AppExitReport {
            total_ms,
            budget_ms,
            within_budget: !deadline_expired && total_ms <= budget_ms,
            pipeline_shutdown_ok,
            pipeline_shutdown_error,
            adapters: adapter_reports,
            storage: storage_report,
            storage_error,
            deadline_expired,
        }
    }

    /// 在核心 tokio 运行时上执行 [`AppShutdown::run`] 并同步等待（Tauri 退出回调线程用）。
    ///
    /// 等待上限 = 预算 + [`EXIT_BLOCKING_SLACK`]；超限返回 `deadline_expired` 报告，
    /// 调用方仍应立即放行应用退出（D2「保证能退出优先」）。
    pub fn run_blocking(self: &Arc<Self>) -> AppExitReport {
        let this = Arc::clone(self);
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(this.run().await);
        });
        match receiver.recv_timeout(self.budget + EXIT_BLOCKING_SLACK) {
            Ok(report) => report,
            Err(_) => {
                let mut report = AppExitReport::empty(self.budget);
                report.within_budget = false;
                report.deadline_expired = true;
                report.storage_error = Some(format!(
                    "退出序列在 {:?} 内未返回（放行进程退出）",
                    self.budget + EXIT_BLOCKING_SLACK
                ));
                report
            }
        }
    }
}

/// Tauri `RunEvent::ExitRequested` 处理（`app_exit` 命令、窗口关闭、探针 `app.exit` 同路径）。
///
/// - 未接线退出编排（启动门阻断/骨架测试）→ 不拦截，直接放行；
/// - 首次请求 → `prevent_exit` + 后台线程执行 [`AppShutdown::run_blocking`] → 完成后再
///   `exit(原始退出码)`（第二次请求经 [`ExitPhase::Done`] 放行）；
/// - 清理执行中的重复请求一律拦截（避免并发退出序列）。
pub fn on_exit_requested<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    code: Option<i32>,
    api: &tauri::ExitRequestApi,
) {
    // 未接线退出编排（启动门阻断/骨架测试）→ 不拦截，直接放行。
    let Some(orchestrator) = app.state::<IpcState>().shutdown_orchestrator() else {
        return;
    };

    match app.state::<IpcState>().exit_phase() {
        ExitPhase::Done => return,
        ExitPhase::Running => {
            api.prevent_exit();
            return;
        }
        ExitPhase::Idle => {}
    }
    api.prevent_exit();
    if !app.state::<IpcState>().begin_exit_cleanup() {
        return;
    }

    let app = app.clone();
    std::thread::spawn(move || {
        let report = orchestrator.run_blocking();
        println!("{}", report.evidence_line());
        app.state::<IpcState>().finish_exit_cleanup();
        app.exit(code.unwrap_or(0));
    });
}

/// `IpcState` 侧退出阶段读写（供 [`on_exit_requested`] 与测试复用）。
pub(crate) struct ExitFlag(AtomicU8);

impl ExitFlag {
    pub(crate) const fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    pub(crate) fn phase(&self) -> ExitPhase {
        ExitPhase::from_code(self.0.load(Ordering::SeqCst))
    }

    /// `Idle → Running` 原子抢占（返回 false 表示已有清理在执行/已完成）。
    pub(crate) fn begin(&self) -> bool {
        self.0
            .compare_exchange(
                ExitPhase::Idle.code(),
                ExitPhase::Running.code(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    pub(crate) fn finish(&self) {
        self.0.store(ExitPhase::Done.code(), Ordering::SeqCst);
    }
}
