//! P0 运行期日志汇聚端（M2-07 DoD6；ADR-007 §5-1：`tracing` 输出端接线）。
//!
//! 形态：**环形缓冲 + 数据目录文件**（`<data_dir>/logs/aether.log`）：
//! - 环形缓冲（[`LogSink::snapshot`] / [`LogSink::export_text`]）供 M3-05 诊断包导出；
//! - 文件为追加写（进程运行期可外部查看）；文件不可写时退化为纯环形缓冲，不阻断启动；
//! - 经 [`tracing_subscriber`] 全局订阅器接线（唯一输出端），捕获全部 `tracing` 事件，
//!   包括 ADR-007 `attempt=n/3` 写失败重试诊断与 `persist_degraded` 诊断。
//!
//! 约束：日志汇聚端**不**参与业务语义（`health` 不承载日志内容；先日志后广播不变）；
//! 写入失败不得 panic、不得递归产生日志。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// 日志子目录（数据目录内）。
pub const LOG_DIR_NAME: &str = "logs";
/// 运行期日志文件名。
pub const LOG_FILE_NAME: &str = "aether.log";
/// 环形缓冲容量（行；超出丢弃最旧）。
pub const DEFAULT_RING_CAPACITY: usize = 2_000;

static GLOBAL_SINK: OnceLock<Arc<LogSink>> = OnceLock::new();

/// 运行期日志汇聚端（环形缓冲 + 可选文件）。
pub struct LogSink {
    capacity: usize,
    ring: Mutex<VecDeque<String>>,
    file: Mutex<Option<File>>,
    path: Option<PathBuf>,
}

impl LogSink {
    /// 纯环形缓冲（文件不可用/单测）。
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.max(1),
            ring: Mutex::new(VecDeque::new()),
            file: Mutex::new(None),
            path: None,
        })
    }

    /// 环形缓冲 + 追加写文件（目录自动创建）。
    pub fn with_file(capacity: usize, path: &Path) -> io::Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Arc::new(Self {
            capacity: capacity.max(1),
            ring: Mutex::new(VecDeque::new()),
            file: Mutex::new(Some(file)),
            path: Some(path.to_path_buf()),
        }))
    }

    /// 追加一行（自动换行；写入失败静默丢弃，不 panic、不递归）。
    pub fn push_line(&self, line: &str) {
        {
            let mut ring = match self.ring.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if ring.len() >= self.capacity {
                ring.pop_front();
            }
            ring.push_back(line.to_owned());
        }
        let mut file = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(file) = file.as_mut() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.write_all(b"\n");
        }
    }

    /// 环形缓冲快照（最旧在前）。
    pub fn snapshot(&self) -> Vec<String> {
        match self.ring.lock() {
            Ok(guard) => guard.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        }
    }

    /// 诊断包导出文本（M3-05 消费；环形缓冲快照以换行连接）。
    pub fn export_text(&self) -> String {
        self.snapshot().join("\n")
    }

    /// 日志文件路径（纯环形缓冲时为 `None`）。
    pub fn file_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 当前缓冲行数。
    pub fn line_count(&self) -> usize {
        match self.ring.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

/// 全局汇聚端句柄（`run()` 接线后可用；M3-05 诊断包/测试消费）。
pub fn global_sink() -> Option<Arc<LogSink>> {
    GLOBAL_SINK.get().cloned()
}

/// 全局接线错误（订阅器已安装/写入端不可用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogInitError {
    pub reason: String,
}

impl std::fmt::Display for LogInitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "日志汇聚端接线失败：{}", self.reason)
    }
}

impl std::error::Error for LogInitError {}

/// 数据目录接线（生产入口）：`<data_dir>/logs/aether.log`；文件失败 → 纯环形缓冲。
pub fn init_for_data_dir(data_dir: &Path) -> Result<Arc<LogSink>, LogInitError> {
    let path = data_dir.join(LOG_DIR_NAME).join(LOG_FILE_NAME);
    let sink = match LogSink::with_file(DEFAULT_RING_CAPACITY, &path) {
        Ok(sink) => sink,
        Err(error) => {
            eprintln!("[aether] 日志文件不可写（{error}）：退化为纯环形缓冲（{path:?}）");
            LogSink::new(DEFAULT_RING_CAPACITY)
        }
    };
    init(Arc::clone(&sink))?;
    Ok(sink)
}

/// 安装全局订阅器（`tracing_subscriber` → [`LogSink`]）。
///
/// 重复安装返回错误（只允许一个输出端；测试可多次调用 [`LogSink::push_line`] 直接驱动）。
pub fn init(sink: Arc<LogSink>) -> Result<(), LogInitError> {
    let writer = LogWriter {
        sink: Arc::clone(&sink),
    };
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .try_init()
        .map_err(|error| LogInitError {
            reason: error.to_string(),
        })?;
    let _ = GLOBAL_SINK.set(sink);
    Ok(())
}

/// `tracing` → [`LogSink`] 写入适配（按行切分；`fmt` 层可能分多次写同一行）。
#[derive(Clone)]
struct LogWriter {
    sink: Arc<LogSink>,
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogWriter {
    type Writer = LogWriterGuard;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriterGuard {
            sink: Arc::clone(&self.sink),
            buffer: Vec::new(),
        }
    }
}

struct LogWriterGuard {
    sink: Arc<LogSink>,
    buffer: Vec<u8>,
}

impl Write for LogWriterGuard {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            let text = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).into_owned();
            self.sink.push_line(&text);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriterGuard {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let text = String::from_utf8_lossy(&self.buffer).into_owned();
            self.buffer.clear();
            self.sink.push_line(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_is_bounded_and_exportable() {
        let sink = LogSink::new(3);
        for index in 0..5 {
            sink.push_line(&format!("line-{index}"));
        }
        assert_eq!(sink.line_count(), 3, "环形缓冲上限 3");
        assert_eq!(sink.snapshot(), vec!["line-2", "line-3", "line-4"]);
        assert_eq!(sink.export_text(), "line-2\nline-3\nline-4");
        assert!(sink.file_path().is_none());
    }

    #[test]
    fn file_sink_appends_lines() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("logs").join("aether.log");
        let sink = LogSink::with_file(8, &path).expect("文件日志");
        sink.push_line("first");
        sink.push_line("second");
        let content = std::fs::read_to_string(&path).expect("读取日志");
        assert_eq!(content, "first\nsecond\n");
        assert_eq!(sink.file_path(), Some(path.as_path()));
    }

    #[test]
    fn writer_adapter_splits_lines() {
        let sink = LogSink::new(4);
        let writer = LogWriter {
            sink: Arc::clone(&sink),
        };
        {
            use tracing_subscriber::fmt::MakeWriter;
            let mut guard = writer.make_writer();
            // 同一行分两次写；再写一整行带换行。
            guard.write_all(b"part-").expect("写");
            guard.write_all(b"one\nfull-line\n").expect("写");
        }
        assert_eq!(sink.snapshot(), vec!["part-one", "full-line"]);
    }

    #[test]
    fn global_sink_starts_unset_in_unit_test_process() {
        // 单测进程不安装全局订阅器（安装仅由 run()/集成测试显式驱动）。
        assert!(global_sink().is_none());
    }
}
