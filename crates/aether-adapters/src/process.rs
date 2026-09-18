//! 适配器进程宿主（M1-09 基线 + M1-10 监督语义）。
//!
//! **边界 B2（三处留痕，见 `docs/M1-09-证据.md` B2/B5）**：M1-09 提交（`962de37`）
//! 的 `process.rs` 仅为最小宿主（spawn + stdio 连接 + stderr 尾 50 行），可执行代码
//! 不含进程组、Job Object、PID 台账、退避/熔断与终止序列；上述生命周期管理自 M1-10
//! 起在本文件与 `supervisor/` 承接（`docs/M1-10-证据.md`）。边界 B4 静态检查固化该分工。
//!
//! - spawn：`tokio::process::Command` 经 `process-wrap` 包装，stdin/stdout/stderr 管道，
//!   `kill_on_drop(false)`（手动管理生命周期，D5）；
//! - 进程组（评审修订 #4）：Unix `setsid`（`process-wrap::ProcessSession`）、
//!   Windows `CREATE_NEW_PROCESS_GROUP` + **Job Object**（`process-wrap::JobObject`）；
//! - 终止（ADR-004）：经 [`ProcessTerminationTarget`] 接入 D5 逐步硬超时序列——
//!   Windows 强杀优先 `TerminateJobObject`（`JobObjectChild::start_kill`），
//!   兜底 `taskkill /PID x /T /F`；Job Object 仅用于进程树回收与资源管理，**非沙箱**；
//! - 禁止裸 kill 单个 PID（AGENTS §6）：Unix 一律按进程组发信号。

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use process_wrap::tokio::{TokioChildWrapper, TokioCommandWrap};
use serde_json::json;
use thiserror::Error;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinHandle;

#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};

use crate::connection::AdapterConnection;
use crate::protocol::Method;
use crate::supervisor::termination::{ActionFuture, TerminationStep, TerminationTarget};

/// D5：启动即崩时抓取 stderr 尾 50 行（`start_failed` 原因展示）。
pub const STDERR_TAIL_LINES: usize = 50;

/// Unix `SIGTERM`（D5 终止序列步骤 2）。
#[cfg(unix)]
pub const SIGTERM: i32 = 15;
/// Unix `SIGKILL`（D5 终止序列步骤 3/4）。
#[cfg(unix)]
pub const SIGKILL: i32 = 9;

/// Windows `CREATE_NEW_PROCESS_GROUP`（D5 进程组；Win32 常量 0x00000200）。
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// 进程宿主错误。
#[derive(Debug, Error)]
pub enum ProcessError {
    /// 无法启动进程。
    #[error("无法启动适配器进程: {0}")]
    Spawn(String),
    /// stdio 管道缺失（重复 connect 或进程已退出）。
    #[error("进程 stdio 管道不可用: {0}")]
    MissingPipes(String),
    /// 等待退出超时。
    #[error("等待进程退出超时（{0:?}）")]
    WaitTimeout(Duration),
    /// 其它 IO 错误。
    #[error("进程 IO 错误: {0}")]
    Io(String),
}

/// 适配器进程（stdout/stdin 交给 [`AdapterConnection`]）。
pub struct AdapterProcess {
    child: Box<dyn TokioChildWrapper>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_task: JoinHandle<()>,
    connected: bool,
    pid: Option<u32>,
}

impl AdapterProcess {
    /// 启动适配器进程（stdin/stdout/stderr 均为管道；自动纳入进程组/Job Object）。
    pub async fn spawn<S, I, A>(program: S, args: I) -> Result<Self, ProcessError>
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        let mut wrap = TokioCommandWrap::with_new(program, |command| {
            command
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(false);
        });
        // Unix：setsid 新建会话/进程组（整树回收的前提，D5/评审 #4）。
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessSession);
        // Windows：独立进程组 + 纳入 Job Object（终止优先 TerminateJobObject，ADR-004；
        // Job Object 仅进程树回收与资源管理，非文件/网络沙箱）。
        #[cfg(windows)]
        {
            wrap.wrap(CreationFlags(
                windows::Win32::System::Threading::PROCESS_CREATION_FLAGS(CREATE_NEW_PROCESS_GROUP),
            ));
            wrap.wrap(JobObject);
        }
        let mut child = wrap
            .spawn()
            .map_err(|error| ProcessError::Spawn(error.to_string()))?;
        let pid = child.id();

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let stderr_task = match child.stderr().take() {
            Some(stderr) => {
                let tail = Arc::clone(&stderr_tail);
                tokio::spawn(drain_stderr(stderr, tail))
            }
            None => tokio::spawn(async {}),
        };

        Ok(Self {
            child,
            stderr_tail,
            stderr_task,
            connected: false,
            pid,
        })
    }

    /// 取走 stdio 并建立线协议连接（仅允许一次）。
    pub fn connect(&mut self) -> Result<AdapterConnection, ProcessError> {
        if self.connected {
            return Err(ProcessError::MissingPipes("已建立过连接".to_owned()));
        }
        let stdin = self
            .child
            .stdin()
            .take()
            .ok_or_else(|| ProcessError::MissingPipes("stdin 不可用".to_owned()))?;
        let stdout = self
            .child
            .stdout()
            .take()
            .ok_or_else(|| ProcessError::MissingPipes("stdout 不可用".to_owned()))?;
        self.connected = true;
        Ok(AdapterConnection::spawn(stdout, stdin))
    }

    /// 进程 ID（诊断/台账用；spawn 时缓存，退出后仍可查询）。
    pub fn id(&self) -> Option<u32> {
        self.pid
    }

    /// Unix：会话/进程组 id（`setsid` 保证等于子进程 pid，平台断言用）。
    #[cfg(unix)]
    pub fn process_group_id(&self) -> Option<u32> {
        self.pid
    }

    /// Windows：进程组由 `CREATE_NEW_PROCESS_GROUP` + Job Object 承担（断言见终止序列）。
    #[cfg(windows)]
    pub fn process_group_id(&self) -> Option<u32> {
        None
    }

    /// stderr 尾部（最多 [`STDERR_TAIL_LINES`] 行）。
    pub fn stderr_tail(&self) -> Vec<String> {
        match self.stderr_tail.lock() {
            Ok(tail) => tail.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        }
    }

    /// 在超时内等待退出；超时返回错误（调用方决定强杀）。
    pub async fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<std::process::ExitStatus, ProcessError> {
        match tokio::time::timeout(timeout, Box::into_pin(self.child.wait())).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => Err(ProcessError::Io(error.to_string())),
            Err(_) => Err(ProcessError::WaitTimeout(timeout)),
        }
    }

    /// 非阻塞查询退出状态（尚未退出返回 `None`）。
    pub fn try_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 强制整树回收：Unix SIGKILL 进程组 / Windows `TerminateJobObject`（D5 步骤 3）。
    pub fn force_kill(&mut self) -> Result<(), ProcessError> {
        self.child
            .start_kill()
            .map_err(|error| ProcessError::Io(error.to_string()))
    }

    /// 优雅终止：Unix `kill -TERM -<pgid>` / Windows `taskkill /PID x /T`（D5 步骤 2）。
    ///
    /// Windows `taskkill` 必须以硬超时约束（本例程内中止），避免吞掉终止序列的后续步骤。
    pub async fn terminate_gracefully(&mut self, timeout: Duration) -> Result<(), ProcessError> {
        #[cfg(unix)]
        {
            let _ = timeout;
            self.child
                .signal(SIGTERM)
                .map_err(|error| ProcessError::Io(error.to_string()))
        }
        #[cfg(windows)]
        {
            let pid = self
                .pid
                .ok_or_else(|| ProcessError::Io("进程 pid 不可用".to_owned()))?;
            taskkill_bounded(pid, false, taskkill_bound(timeout))
                .await
                .map_err(ProcessError::Io)
        }
    }

    /// 兜底终止：Unix SIGKILL 进程组重试 / Windows `taskkill /PID x /T /F`（D5 步骤 4）。
    pub async fn terminate_fallback(&mut self, timeout: Duration) -> Result<(), ProcessError> {
        #[cfg(unix)]
        {
            let _ = timeout;
            self.force_kill()
        }
        #[cfg(windows)]
        {
            let pid = self
                .pid
                .ok_or_else(|| ProcessError::Io("进程 pid 不可用".to_owned()))?;
            taskkill_bounded(pid, true, taskkill_bound(timeout))
                .await
                .map_err(ProcessError::Io)
        }
    }

    /// 强杀（树回收优先；与 D5 终止序列口径一致，避免裸 kill 单 PID）。
    pub async fn kill(&mut self) -> Result<(), ProcessError> {
        self.force_kill()
    }

    /// 等待 stderr 采集任务结束（进程退出后调用）。
    pub async fn join_stderr(&mut self) {
        if !self.stderr_task.is_finished() {
            let handle = &mut self.stderr_task;
            let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
        }
    }
}

impl Drop for AdapterProcess {
    fn drop(&mut self) {
        self.stderr_task.abort();
    }
}

/// Windows `taskkill` 兜底（D5：`/T` 优雅，`/F` 强杀；严禁裸 kill 单 PID）。
///
/// 以硬超时约束子进程；超时则中止本次 `taskkill` 并交由序列下一步处理。
#[cfg(windows)]
async fn taskkill_bounded(pid: u32, force: bool, timeout: Duration) -> Result<(), String> {
    let mut command = tokio::process::Command::new("taskkill");
    command
        .args(["/PID", &pid.to_string(), "/T"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if force {
        command.arg("/F");
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("taskkill 不可用：{error}"))?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!(
            "taskkill /PID {pid} /T{} 退出码 {status}",
            if force { " /F" } else { "" }
        )),
        Ok(Err(error)) => Err(format!("等待 taskkill：{error}")),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(format!("taskkill 超出硬超时（{timeout:?}）已中止"))
        }
    }
}

/// `taskkill` 内部超时：为终止序列的逐步硬超时预留余量。
#[cfg(windows)]
fn taskkill_bound(step_timeout: Duration) -> Duration {
    const MARGIN: Duration = Duration::from_millis(250);
    step_timeout
        .saturating_sub(MARGIN)
        .max(Duration::from_millis(250))
}

/// D5 终止序列的目标适配器（真实进程树 + 线协议连接）。
pub struct ProcessTerminationTarget<'a> {
    process: &'a mut AdapterProcess,
    connection: Option<&'a AdapterConnection>,
}

impl<'a> ProcessTerminationTarget<'a> {
    pub fn new(process: &'a mut AdapterProcess, connection: Option<&'a AdapterConnection>) -> Self {
        Self {
            process,
            connection,
        }
    }
}

impl TerminationTarget for ProcessTerminationTarget<'_> {
    fn pid(&self) -> u32 {
        self.process.id().unwrap_or_default()
    }

    fn is_alive(&mut self) -> bool {
        self.process.try_status().is_none()
    }

    fn shutdown_rpc(&self) -> ActionFuture<'_> {
        Box::pin(async move {
            match self.connection {
                Some(connection) => match connection.request(Method::Shutdown, json!({})).await {
                    Ok(_) => Ok(()),
                    Err(error) => Err(error.to_string()),
                },
                None => Err("无连接（进程尚未建立线协议）".to_owned()),
            }
        })
    }

    fn graceful(&mut self, timeout: Duration) -> ActionFuture<'_> {
        Box::pin(async move {
            self.process
                .terminate_gracefully(timeout)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn force(&mut self, timeout: Duration) -> ActionFuture<'_> {
        // 强杀为即时动作（TerminateJobObject / SIGKILL 进程组）；同步下发后立即返回。
        let _ = timeout;
        let result = self.process.force_kill().map_err(|error| error.to_string());
        Box::pin(async move { result })
    }

    fn fallback(&mut self, timeout: Duration) -> ActionFuture<'_> {
        Box::pin(async move {
            self.process
                .terminate_fallback(timeout)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn mechanism(&self, step: TerminationStep) -> &'static str {
        #[cfg(unix)]
        {
            match step {
                TerminationStep::ShutdownRpc => "shutdown_rpc",
                TerminationStep::Graceful => "sigterm_pgid",
                TerminationStep::Force => "sigkill_pgid",
                TerminationStep::Fallback => "sigkill_pgid_retry",
            }
        }
        #[cfg(windows)]
        {
            match step {
                TerminationStep::ShutdownRpc => "shutdown_rpc",
                TerminationStep::Graceful => "taskkill_tree",
                TerminationStep::Force => "terminate_job_object",
                TerminationStep::Fallback => "taskkill_tree_force",
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = step;
            "unsupported"
        }
    }
}

async fn drain_stderr<R>(stderr: R, tail: Arc<Mutex<VecDeque<String>>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut guard = match tail.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.len() >= STDERR_TAIL_LINES {
            guard.pop_front();
        }
        guard.push_back(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stderr_tail_keeps_last_50_lines() {
        // 用当前测试进程之外的真实子进程不可行于单测；此处直接验证环形缓冲语义。
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        for index in 0..60 {
            let mut guard = tail.lock().unwrap();
            if guard.len() >= STDERR_TAIL_LINES {
                guard.pop_front();
            }
            guard.push_back(format!("line-{index}"));
        }
        let guard = tail.lock().unwrap();
        assert_eq!(guard.len(), STDERR_TAIL_LINES);
        assert_eq!(guard.front().map(String::as_str), Some("line-10"));
        assert_eq!(guard.back().map(String::as_str), Some("line-59"));
    }

    #[test]
    fn stderr_tail_lines_constant_is_50() {
        assert_eq!(STDERR_TAIL_LINES, 50);
    }

    #[cfg(unix)]
    #[test]
    fn unix_signal_constants_match_posix() {
        assert_eq!(SIGTERM, 15);
        assert_eq!(SIGKILL, 9);
    }

    #[cfg(windows)]
    #[test]
    fn windows_creation_flag_is_new_process_group() {
        assert_eq!(CREATE_NEW_PROCESS_GROUP, 0x0000_0200);
    }
}
