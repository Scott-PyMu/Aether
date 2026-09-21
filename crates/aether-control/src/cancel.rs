//! 取消树与任务看门狗（M2-05；设计 D8）。
//!
//! - **取消树**（D8「取消树：会话级 `CancellationToken`，interrupt/dispose/父会话取消
//!   级联；权限等待可取消」）：应用级根令牌 → 会话节点 → run 节点；父节点取消级联到
//!   全部后代（[`CancelTree::cancel_session`] / [`CancelTree::cancel_all`]）；
//! - **任务看门狗**（D8 失败表「取消风暴」「死锁/任务不退出」）：登记会话执行任务；
//!   任务被取消后 [`TASK_FORCE_CLEANUP_MS`]（10s）内未退出 → 记录 [`TaskDump`] 并
//!   强制清理（`JoinHandle::abort`，任务级，非进程 kill）；dump 进入诊断缓冲
//!   （[`TaskWatchdog::dumps`]，M3-05 诊断包消费）并以 `tracing::error!` 上报
//!   （M2-07 运行期日志汇聚端）。
//!
//! 线程模型：内部状态经 `Mutex` 保护，临界区不做 I/O；令牌取消为同步操作，
//! `cancelled()` 提供异步等待（权限等待/执行器响应级联取消）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use aether_core::{RunId, SessionId};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::ulid;

/// 取消后任务强制清理阈值（D8：10s 未退出的会话任务强制清理并记 dump）。
pub const TASK_FORCE_CLEANUP_MS: i64 = 10_000;
/// 任务 dump 环形缓冲容量（诊断包消费；超出丢弃最旧）。
pub const TASK_DUMP_CAPACITY: usize = 64;
/// 任务 dump 动作标记（强制清理）。
pub const TASK_DUMP_ACTION_FORCED_CLEANUP: &str = "forced_cleanup";

/// run/会话中断令牌（取消树节点；`cancel()` 级联到全部子节点）。
#[derive(Debug, Clone)]
pub struct RunCancelToken {
    token: Arc<CancellationToken>,
}

impl RunCancelToken {
    pub fn new() -> Self {
        Self {
            token: Arc::new(CancellationToken::new()),
        }
    }

    /// 以父令牌派生（父取消 → 本令牌级联取消；D8）。
    pub fn child_of(parent: &RunCancelToken) -> Self {
        Self {
            token: Arc::new(parent.token.child_token()),
        }
    }

    /// 由已存在的树节点构造（会话节点句柄；与树共享同一取消状态）。
    pub(crate) fn from_token(token: CancellationToken) -> Self {
        Self {
            token: Arc::new(token),
        }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// 异步等待取消（权限等待可取消：`interrupt`/`dispose` 级联时唤醒）。
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

impl Default for RunCancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for RunCancelToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.token, &other.token)
    }
}

impl Eq for RunCancelToken {}

#[derive(Debug)]
struct CancelTreeInner {
    root: CancellationToken,
    sessions: HashMap<SessionId, CancellationToken>,
}

/// 应用级取消树（D8）：根 → 会话 → run；父取消级联到全部后代。
#[derive(Clone)]
pub struct CancelTree {
    inner: Arc<Mutex<CancelTreeInner>>,
}

impl CancelTree {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CancelTreeInner {
                root: CancellationToken::new(),
                sessions: HashMap::new(),
            })),
        }
    }

    /// 取/建会话节点；`parent` 命中已登记节点时作为其子节点（父取消级联）。
    ///
    /// 父节点尚未登记时先建父节点（根子节点）再派生（P0 子会话仅一层）。
    pub fn session_token(
        &self,
        session_id: &SessionId,
        parent: Option<&SessionId>,
    ) -> RunCancelToken {
        let mut inner = lock(&self.inner);
        if let Some(token) = inner.sessions.get(session_id) {
            return RunCancelToken::from_token(token.clone());
        }
        let registered_parent = parent.and_then(|parent| inner.sessions.get(parent).cloned());
        let token = match registered_parent {
            Some(parent_token) => parent_token.child_token(),
            None => {
                if let Some(parent_id) = parent {
                    let parent_token = inner.root.child_token();
                    inner
                        .sessions
                        .insert(parent_id.clone(), parent_token.clone());
                    parent_token.child_token()
                } else {
                    inner.root.child_token()
                }
            }
        };
        inner.sessions.insert(session_id.clone(), token.clone());
        RunCancelToken::from_token(token)
    }

    /// 取消会话节点（级联全部 run 子节点）；返回是否命中已登记节点。
    pub fn cancel_session(&self, session_id: &SessionId) -> bool {
        let inner = lock(&self.inner);
        match inner.sessions.get(session_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// 会话节点是否已取消（未登记 = `false`）。
    pub fn is_cancelled(&self, session_id: &SessionId) -> bool {
        lock(&self.inner)
            .sessions
            .get(session_id)
            .is_some_and(CancellationToken::is_cancelled)
    }

    /// 根取消：全部会话与后代级联取消（应用关闭/核心停机路径）。
    pub fn cancel_all(&self) {
        lock(&self.inner).root.cancel();
    }

    /// 已登记会话节点数（诊断）。
    pub fn session_count(&self) -> usize {
        lock(&self.inner).sessions.len()
    }
}

impl Default for CancelTree {
    fn default() -> Self {
        Self::new()
    }
}

/// 任务 dump（诊断包消费；D8「看门狗记录任务 dump 并按 bug 上报」）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskDump {
    /// 任务名（`session:<id> run:<id>`）。
    pub task: String,
    pub session_id: String,
    pub run_id: String,
    pub started_at_ms: i64,
    /// 任务被取消（在途 run 被摘除）的时刻。
    pub orphaned_at_ms: i64,
    /// 记录 dump 并强制清理的时刻。
    pub dumped_at_ms: i64,
    /// 取消 → 强制清理的时长（≥ 阈值）。
    pub elapsed_ms: i64,
    /// 处置动作（[`TASK_DUMP_ACTION_FORCED_CLEANUP`]）。
    pub action: String,
}

#[derive(Debug)]
struct TaskRecord {
    name: String,
    session_id: SessionId,
    run_id: RunId,
    started_at_ms: i64,
    orphaned_at_ms: Option<i64>,
    forced_cleanup: bool,
    /// 任务中止句柄（M2-07：会话任务经 `JoinSet` 管理，登记 `AbortHandle`）。
    handle: AbortHandle,
}

#[derive(Debug, Default)]
struct WatchdogInner {
    tasks: HashMap<String, TaskRecord>,
    dumps: VecDeque<TaskDump>,
}

/// 会话任务看门狗（D8）：登记执行任务 → 取消后超阈值未退出 → dump + 强制清理。
#[derive(Clone)]
pub struct TaskWatchdog {
    inner: Arc<Mutex<WatchdogInner>>,
    force_cleanup_ms: i64,
    dump_capacity: usize,
}

impl TaskWatchdog {
    pub fn new(force_cleanup_ms: i64, dump_capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(WatchdogInner::default())),
            force_cleanup_ms,
            dump_capacity,
        }
    }

    /// 登记会话执行任务；返回任务 id（诊断/断言用）。
    ///
    /// `handle` 来自 `JoinSet::spawn` 的 [`AbortHandle`]（M2-07：任务统一经
    /// `JoinSet` 管理，panic 由 `JoinError` 捕获；本看门狗仅负责取消后的兜底清理）。
    pub fn register(
        &self,
        name: String,
        session_id: SessionId,
        run_id: RunId,
        started_at_ms: i64,
        handle: AbortHandle,
    ) -> String {
        let task_id = ulid::generate();
        let mut inner = lock(&self.inner);
        inner.tasks.insert(
            task_id.clone(),
            TaskRecord {
                name,
                session_id,
                run_id,
                started_at_ms,
                orphaned_at_ms: None,
                forced_cleanup: false,
                handle,
            },
        );
        task_id
    }

    /// 标记 run 对应的任务已被取消（超时/中断/降级/关闭摘除在途 run）。
    /// 返回是否命中在册任务。
    pub fn mark_orphaned_by_run(&self, run_id: &RunId, now_ms: i64) -> bool {
        let mut inner = lock(&self.inner);
        for record in inner.tasks.values_mut() {
            if &record.run_id == run_id {
                if record.orphaned_at_ms.is_none() {
                    record.orphaned_at_ms = Some(now_ms);
                }
                return true;
            }
        }
        false
    }

    /// 巡检：清理已退出任务；对「取消后超过阈值仍未退出」的任务记 dump + 强制清理。
    pub fn sweep(&self, now_ms: i64) -> Vec<TaskDump> {
        let mut inner = lock(&self.inner);
        let mut finished: Vec<String> = Vec::new();
        let mut dumps: Vec<TaskDump> = Vec::new();
        for (task_id, record) in inner.tasks.iter_mut() {
            if record.handle.is_finished() {
                finished.push(task_id.clone());
                continue;
            }
            if record.forced_cleanup {
                continue;
            }
            let Some(orphaned_at) = record.orphaned_at_ms else {
                continue;
            };
            let elapsed = now_ms.saturating_sub(orphaned_at);
            if elapsed < self.force_cleanup_ms {
                continue;
            }
            dumps.push(TaskDump {
                task: record.name.clone(),
                session_id: record.session_id.as_str().to_owned(),
                run_id: record.run_id.as_str().to_owned(),
                started_at_ms: record.started_at_ms,
                orphaned_at_ms: orphaned_at,
                dumped_at_ms: now_ms,
                elapsed_ms: elapsed,
                action: TASK_DUMP_ACTION_FORCED_CLEANUP.to_owned(),
            });
            record.forced_cleanup = true;
            record.handle.abort();
        }
        for task_id in finished {
            inner.tasks.remove(&task_id);
        }
        for dump in &dumps {
            if inner.dumps.len() >= self.dump_capacity {
                inner.dumps.pop_front();
            }
            inner.dumps.push_back(dump.clone());
            tracing::error!(
                task = %dump.task,
                session_id = %dump.session_id,
                run_id = %dump.run_id,
                elapsed_ms = dump.elapsed_ms,
                action = %dump.action,
                "会话任务 dump：取消后 {elapsed}ms 未退出（阈值 {threshold}ms），已强制清理（D8 看门狗）",
                elapsed = dump.elapsed_ms,
                threshold = self.force_cleanup_ms,
            );
        }
        dumps
    }

    /// 在册任务数（含强制清理后尚未确认退出的任务；`sweep` 后为真实运行数）。
    pub fn active_count(&self) -> usize {
        lock(&self.inner).tasks.len()
    }

    /// 任务 dump 快照（诊断包消费；环形缓冲，最旧在前）。
    pub fn dumps(&self) -> Vec<TaskDump> {
        lock(&self.inner).dumps.iter().cloned().collect()
    }
}

impl Default for TaskWatchdog {
    fn default() -> Self {
        Self::new(TASK_FORCE_CLEANUP_MS, TASK_DUMP_CAPACITY)
    }
}

fn lock<T>(mutex: &Arc<Mutex<T>>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn session_id(label: &str) -> SessionId {
        SessionId::new(format!("01J{label:0>23}")).expect("合法 ULID")
    }

    fn run_id(label: &str) -> RunId {
        RunId::new(format!("01K{label:0>23}")).expect("合法 ULID")
    }

    #[test]
    fn cancel_token_flips_once_and_stays() {
        let token = RunCancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        // 等价性（Arc::ptr_eq）与默认构造。
        assert_eq!(token, token.clone());
        assert_ne!(token, RunCancelToken::new());
        assert!(!RunCancelToken::default().is_cancelled());
    }

    #[tokio::test]
    async fn parent_cancel_cascades_to_child_but_not_reverse() {
        let parent = RunCancelToken::new();
        let child = RunCancelToken::child_of(&parent);
        let grandchild = RunCancelToken::child_of(&child);

        // 子取消不反向影响父。
        child.cancel();
        assert!(child.is_cancelled());
        assert!(grandchild.is_cancelled(), "父取消级联到孙节点");
        assert!(!parent.is_cancelled(), "子取消不得反向影响父");

        // 父取消级联到全部后代（异步等待路径同口径）。
        let parent2 = RunCancelToken::new();
        let child2 = RunCancelToken::child_of(&parent2);
        parent2.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), child2.cancelled())
                .await
                .is_ok(),
            "cancelled() 必须被父级联唤醒"
        );
        assert!(child2.is_cancelled());
    }

    #[test]
    fn cancel_tree_session_nodes_are_stable_and_cascade() {
        let tree = CancelTree::new();
        let parent = session_id("P");
        let child = session_id("C");
        let other = session_id("O");

        let parent_token = tree.session_token(&parent, None);
        let child_token = tree.session_token(&child, Some(&parent));
        let other_token = tree.session_token(&other, None);
        // 重复取用返回同一状态（克隆句柄）。
        assert!(!tree.session_token(&parent, None).is_cancelled());
        assert_eq!(tree.session_count(), 3);

        tree.cancel_session(&parent);
        assert!(tree.is_cancelled(&parent));
        assert!(parent_token.is_cancelled());
        assert!(child_token.is_cancelled(), "父会话取消级联到子会话");
        assert!(!other_token.is_cancelled(), "兄弟会话不受影响");
        assert!(!tree.is_cancelled(&session_id("N")), "未登记节点 = 未取消");

        // 根取消：全部会话级联。
        tree.cancel_all();
        assert!(other_token.is_cancelled());
        assert!(tree.session_token(&session_id("N"), None).is_cancelled());
    }

    #[tokio::test]
    async fn watchdog_force_cleans_orphaned_task_and_records_dump() {
        let watchdog = TaskWatchdog::new(10_000, 8);
        let handle = tokio::spawn(std::future::pending::<()>());
        let session = session_id("W");
        let run = run_id("W");
        let task_id = watchdog.register(
            "session:W run:W".to_owned(),
            session,
            run.clone(),
            1_000,
            handle.abort_handle(),
        );

        assert_eq!(watchdog.active_count(), 1);
        assert!(
            !watchdog.mark_orphaned_by_run(&run_id("X"), 2_000),
            "未命中 run"
        );
        assert!(watchdog.mark_orphaned_by_run(&run, 2_000));
        // 取消后 10s 未退出：9_999ms 不处置，10_000ms 记 dump + 强制清理。
        assert!(watchdog.sweep(11_999).is_empty());
        let dumps = watchdog.sweep(12_000);
        assert_eq!(dumps.len(), 1);
        let dump = &dumps[0];
        assert_eq!(dump.task, "session:W run:W");
        assert_eq!(dump.elapsed_ms, 10_000);
        assert_eq!(dump.action, TASK_DUMP_ACTION_FORCED_CLEANUP);
        assert_eq!(watchdog.dumps(), dumps, "dump 进入诊断缓冲");
        // 重复 sweep 不重复记 dump；abort 生效后任务出册。
        assert!(watchdog.sweep(13_000).is_empty());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while watchdog.active_count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
            watchdog.sweep(14_000);
        }
        assert_eq!(
            watchdog.active_count(),
            0,
            "强制清理后任务必须出册（{task_id}）"
        );
        assert_eq!(watchdog.dumps().len(), 1, "dump 不因出册丢失");
    }

    #[tokio::test]
    async fn watchdog_prunes_finished_tasks_without_dump() {
        let watchdog = TaskWatchdog::new(10_000, 8);
        let handle = tokio::spawn(async {});
        let session = session_id("F");
        let run = run_id("F");
        watchdog.register(
            "session:F run:F".to_owned(),
            session,
            run.clone(),
            0,
            handle.abort_handle(),
        );
        assert!(watchdog.mark_orphaned_by_run(&run, 1));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while watchdog.active_count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(watchdog.sweep(20_000).is_empty());
        }
        assert_eq!(watchdog.active_count(), 0, "已完成任务出册");
        assert!(watchdog.dumps().is_empty(), "正常退出不产生 dump");
    }

    #[tokio::test]
    async fn watchdog_dump_buffer_is_bounded() {
        let watchdog = TaskWatchdog::new(1, 2);
        for index in 0..3 {
            let handle = tokio::spawn(std::future::pending::<()>());
            let session = session_id(&format!("{index}"));
            let run = run_id(&format!("{index}"));
            watchdog.register(
                format!("task-{index}"),
                session,
                run.clone(),
                0,
                handle.abort_handle(),
            );
            assert!(watchdog.mark_orphaned_by_run(&run, 0));
            let dumps = watchdog.sweep(1);
            assert_eq!(dumps.len(), 1);
        }
        let dumps = watchdog.dumps();
        assert_eq!(dumps.len(), 2, "环形缓冲上限 2");
        assert_eq!(dumps[0].task, "task-1", "最旧被丢弃");
        assert_eq!(dumps[1].task, "task-2");
    }
}
