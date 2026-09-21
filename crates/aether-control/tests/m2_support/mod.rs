//! M2 集成测试公共夹具（M2-01 生命周期 / M2-03 权限）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_control::{
    Clock, EventPipeline, ExecutorFuture, ExecutorOutcome, LifecycleConfig, ManualClock,
    PermissionConfig, PermissionService, PipelineConfig, RunExecutor, RunRequest, SessionManager,
    SharedClock, StartupSelfCheckReport, StoreEventSource, StoreJournal, SystemClock,
};
use aether_core::{Runtime, RuntimeId, RuntimeStatus, Session, SessionId, TokenUsage, WorkspaceId};
use aether_store::{ReadPool, StoreRuntime, WriteQueue, WriteQueueConfig};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

/// 测试 run 执行器：记录调用、按信号量阻塞、按脚本返回终态。
pub struct ScriptedExecutor {
    pub calls: Mutex<Vec<RunRequest>>,
    /// 每次执行消耗 1 个许可；测试 `add_permits(1)` 放行一次。
    pub permits: Arc<Semaphore>,
    pub outcome: Mutex<ExecutorOutcome>,
}

impl ScriptedExecutor {
    pub fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(permits)),
            outcome: Mutex::new(ExecutorOutcome::Completed {
                assistant_text: None,
                usage: None,
            }),
        })
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    pub fn run_ids(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.run_id.to_string())
            .collect()
    }

    pub fn set_outcome(&self, outcome: ExecutorOutcome) {
        *self.outcome.lock().unwrap() = outcome;
    }

    /// 放行 `count` 次执行。
    pub fn release(&self, count: usize) {
        self.permits.add_permits(count);
    }
}

impl RunExecutor for ScriptedExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        self.calls.lock().unwrap().push(request);
        let permits = Arc::clone(&self.permits);
        let outcome = Arc::new(self.outcome.lock().unwrap().clone());
        Box::pin(async move {
            let _permit = permits.acquire().await;
            match &*outcome {
                ExecutorOutcome::Completed {
                    assistant_text,
                    usage,
                } => ExecutorOutcome::Completed {
                    assistant_text: assistant_text.clone(),
                    usage: *usage,
                },
                ExecutorOutcome::Failed { error } => ExecutorOutcome::Failed {
                    error: error.clone(),
                },
                ExecutorOutcome::Cancelled { reason } => ExecutorOutcome::Cancelled {
                    reason: reason.clone(),
                },
            }
        })
    }
}

/// 测试核心：同一数据目录上的存储 + 管线（可关闭后重开模拟核心重启）。
pub struct TestCore {
    pub temp: tempfile::TempDir,
    pub db_path: PathBuf,
    pub storage: StoreRuntime,
    pub pipeline: EventPipeline,
}

impl TestCore {
    pub async fn open() -> Self {
        Self::open_with(WriteQueueConfig::default(), PipelineConfig::default()).await
    }

    /// 以自定义写队列/管线配置打开（M2-04 故障注入：提交延迟、广播容量）。
    pub async fn open_with(
        write_config: WriteQueueConfig,
        pipeline_config: PipelineConfig,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("aether.db");
        let storage = StoreRuntime::open(&db_path, write_config, &Handle::current()).unwrap();
        let pipeline = start_pipeline_with(&storage, pipeline_config);
        Self {
            temp,
            db_path,
            storage,
            pipeline,
        }
    }

    pub fn write(&self) -> WriteQueue {
        self.storage.queue().clone()
    }

    pub fn reads(&self) -> ReadPool {
        self.storage.reads().clone()
    }

    /// 关闭管线与存储（模拟核心退出，保留数据目录）。
    pub async fn shutdown(self) -> (tempfile::TempDir, PathBuf) {
        self.pipeline.shutdown().await.unwrap();
        self.storage.shutdown().await.unwrap();
        (self.temp, self.db_path)
    }
}

/// 在既有数据目录上重开核心（重启重放断言）。
pub async fn reopen_core(temp: tempfile::TempDir, db_path: PathBuf) -> TestCore {
    let storage =
        StoreRuntime::open(&db_path, WriteQueueConfig::default(), &Handle::current()).unwrap();
    let pipeline = start_pipeline(&storage);
    TestCore {
        temp,
        db_path,
        storage,
        pipeline,
    }
}

pub fn start_pipeline(storage: &StoreRuntime) -> EventPipeline {
    start_pipeline_with(storage, PipelineConfig::default())
}

/// 以自定义管线配置启动（M2-04：广播容量注入）。
pub fn start_pipeline_with(storage: &StoreRuntime, config: PipelineConfig) -> EventPipeline {
    EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..config
        },
        Arc::new(StoreJournal::new(storage.queue().clone())),
        Arc::new(StoreEventSource::new(storage.reads().clone())),
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &Handle::current(),
    )
    .unwrap()
}

/// Mock 运行时（runtimes 行 + 白名单 id）。
pub fn mock_runtime() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: Vec::new(),
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

/// 组装生命周期管理器（时钟与执行器可注入）。
pub fn build_manager(
    core: &TestCore,
    clock: SharedClock,
    executor: Arc<dyn RunExecutor>,
    config: LifecycleConfig,
) -> SessionManager {
    SessionManager::new(
        config,
        clock,
        core.write(),
        core.reads(),
        core.pipeline.clone(),
        executor,
    )
}

/// 组装权限服务（策略引擎绑定 workspace 根；时钟注入）。
pub fn build_permission_service(
    core: &TestCore,
    workspace_root: &std::path::Path,
    clock: SharedClock,
    config: PermissionConfig,
) -> PermissionService {
    let policy = aether_security::PolicyEngine::new(workspace_root).unwrap();
    PermissionService::new(
        config,
        clock,
        policy,
        core.write(),
        core.reads(),
        core.pipeline.clone(),
    )
}

/// 组装 M2 权限请求（session/runtime 可选）。
pub fn permission_request(
    request_id: &str,
    session_id: Option<&SessionId>,
    resource: &str,
    action: &str,
    target: Option<&str>,
    content_bytes: Option<u64>,
) -> aether_control::PermissionRequest {
    aether_control::PermissionRequest {
        request_id: request_id.to_owned(),
        session_id: session_id.cloned(),
        runtime_id: Some(RuntimeId::new("mock").unwrap()),
        resource: resource.to_owned(),
        action: action.to_owned(),
        target: target.map(str::to_owned),
        content_bytes,
    }
}

/// 创建测试会话并返回会话。
pub async fn create_session(manager: &SessionManager, title: &str) -> Session {
    manager
        .create_session(mock_runtime(), title, None, None)
        .await
        .unwrap()
}

/// 系统时钟句柄（真实时间场景）。
pub fn system_clock() -> SharedClock {
    Arc::new(SystemClock)
}

/// 手动时钟句柄（超时注入）。
pub fn manual_clock(start_ms: i64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(start_ms))
}

/// 轮询等待条件（异步断言；避免固定 sleep）。
pub async fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    condition()
}

/// 轮询等待 DB pending 行数达到期望（权限请求的持久化晚于内存入队）。
pub async fn wait_permissions_pending(reads: &ReadPool, expected: usize) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let count = reads
            .permissions_pending(None)
            .await
            .map(|rows| rows.len())
            .unwrap_or(0);
        if count == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

/// 轮询等待 run 达到指定状态（读库，避免测试内 `block_on`）。
pub async fn wait_run_status(
    manager: &SessionManager,
    run_id: &aether_core::RunId,
    expected: aether_core::RunStatus,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let status = manager
            .run(run_id)
            .await
            .ok()
            .flatten()
            .map(|run| run.status);
        if status == Some(expected) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    manager
        .run(run_id)
        .await
        .ok()
        .flatten()
        .map(|run| run.status)
        == Some(expected)
}

/// 轮询等待会话达到指定状态（读库）。
pub async fn wait_session_status(
    manager: &SessionManager,
    session_id: &SessionId,
    expected: aether_core::SessionStatus,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if manager.session_status(session_id).await.ok() == Some(expected) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    manager.session_status(session_id).await.ok() == Some(expected)
}

/// 唯一会话标题（避免测试间歧义）。
pub fn unique_label(prefix: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("{prefix}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 会话内消息数（读连接池）。
pub async fn message_count(reads: &ReadPool, session_id: &SessionId) -> usize {
    reads
        .messages_page(session_id, None, 500)
        .await
        .unwrap()
        .len()
}

/// 会话内 run 行数（按输入消息关联；简化为读 events/runs 的辅助断言由具体测试完成）。
pub async fn runtime_id() -> RuntimeId {
    RuntimeId::new("mock").unwrap()
}

/// `Clock` 引用辅助（避免未使用导入告警的占位）。
pub fn clock_now(clock: &dyn Clock) -> i64 {
    clock.now_ms()
}

/// 空会话占位（workspace 绑定测试用）。
pub fn workspace_id() -> WorkspaceId {
    WorkspaceId::new("ws").unwrap()
}

/// TokenUsage 占位。
pub fn token_usage() -> TokenUsage {
    TokenUsage::default()
}
