//! 适配器进程最小宿主（M1-09）：spawn + stdio 连接 + stderr 尾部。
//!
//! 进程组、PID 台账、退避/熔断、终止序列等监督语义属 M1-10；本模块不实现。

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::connection::AdapterConnection;

/// D5：启动即崩时抓取 stderr 尾 50 行（M1-10 展示 `start_failed` 原因）。
pub const STDERR_TAIL_LINES: usize = 50;

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
    child: Child,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_task: JoinHandle<()>,
    connected: bool,
}

impl AdapterProcess {
    /// 启动适配器进程（stdin/stdout/stderr 均为管道）。
    pub async fn spawn<S, I, A>(program: S, args: I) -> Result<Self, ProcessError>
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        let mut child = command
            .spawn()
            .map_err(|error| ProcessError::Spawn(error.to_string()))?;

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let stderr_task = match child.stderr.take() {
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
        })
    }

    /// 取走 stdio 并建立线协议连接（仅允许一次）。
    pub fn connect(&mut self) -> Result<AdapterConnection, ProcessError> {
        if self.connected {
            return Err(ProcessError::MissingPipes("已建立过连接".to_owned()));
        }
        let stdin = self
            .child
            .stdin
            .take()
            .ok_or_else(|| ProcessError::MissingPipes("stdin 不可用".to_owned()))?;
        let stdout = self
            .child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::MissingPipes("stdout 不可用".to_owned()))?;
        self.connected = true;
        Ok(AdapterConnection::spawn(stdout, stdin))
    }

    /// 进程 ID（诊断/台账用）。
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// stderr 尾部（最多 [`STDERR_TAIL_LINES`] 行）。
    pub fn stderr_tail(&self) -> Vec<String> {
        match self.stderr_tail.lock() {
            Ok(tail) => tail.iter().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 在超时内等待退出；超时返回错误（调用方决定强杀）。
    pub async fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<std::process::ExitStatus, ProcessError> {
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => Err(ProcessError::Io(error.to_string())),
            Err(_) => Err(ProcessError::WaitTimeout(timeout)),
        }
    }

    /// 非阻塞查询退出状态（尚未退出返回 `None`）。
    pub fn try_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 强杀（M1-10 负责按进程组整树回收；此处仅单进程兜底）。
    pub async fn kill(&mut self) -> Result<(), ProcessError> {
        self.child
            .kill()
            .await
            .map_err(|error| ProcessError::Io(error.to_string()))
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

async fn drain_stderr<R>(stderr: R, tail: Arc<Mutex<VecDeque<String>>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(mut tail) = tail.lock() {
            if tail.len() >= STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
}
