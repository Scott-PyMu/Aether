//! 数据目录 OS 独占锁（ADR-029 P2-8；M9-03 DoD-5）。
//!
//! 口径：
//! - 锁文件 `<data_dir>/.aetherd.lock`，**handle-based**（不是存在性锁）；
//! - 崩溃/被杀后由 OS 自动释放（无陈旧锁问题；不参与数据目录复制）；
//! - 第二个 daemon 取锁失败 → 业务码 `data_dir_locked` + 退出码 20（ADR-029 §4.3）；
//! - 锁在进程生命周期内持有（[`std::fs::File`] 存活）；Drop 释放。
//!
//! 实现：`fs4` 的跨平台建议锁（Windows `LockFileEx` / Unix `flock`），
//! 纯 Rust（`rustix`），无 `unsafe`（工作区 `unsafe_code = "forbid"`）。
//! 显式以 `fs4::FileExt::try_lock` 调用，避免与 std 1.89 的固有方法 `File::try_lock`
//! 名称冲突（保持 workspace `rust-version = 1.80`）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 锁文件名（ADR-029 §3.5）。
pub const LOCK_FILE_NAME: &str = ".aetherd.lock";

use fs4::FileExt;

/// 数据目录独占锁（持有期间进程拥有该数据目录）。
#[derive(Debug)]
pub struct DataDirLock {
    file: Option<File>,
    path: PathBuf,
}

impl DataDirLock {
    /// 获取独占锁；已被其他进程持有 → `Err`（消息含锁路径，供 S-12 展示）。
    pub fn acquire(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|error| format!("数据目录不可创建（{}）：{error}", data_dir.display()))?;
        let path = data_dir.join(LOCK_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("锁文件不可打开（{}）：{error}", path.display()))?;
        match FileExt::try_lock(&file) {
            Ok(()) => {}
            Err(error) => {
                return Err(format!(
                    "数据目录已被另一个 Aether 核心占用（{}）：{error}；请关闭其他实例后重试",
                    path.display()
                ))
            }
        }
        // 诊断信息（PID/时间）；写失败不影响锁语义（锁已持有）。
        let pid = std::process::id();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        let _ = file.set_len(0);
        let _ = write!(file, "pid={pid}\nsince_ms={now_ms}\n");
        let _ = file.flush();
        Ok(Self {
            file: Some(file),
            path,
        })
    }

    /// 锁文件路径（诊断/证据）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DataDirLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_within_process_and_released_on_drop() {
        let dir = tempfile::tempdir().expect("临时目录");
        let first = DataDirLock::acquire(dir.path()).expect("首次取锁");
        assert!(first.path().ends_with(LOCK_FILE_NAME));
        let second = DataDirLock::acquire(dir.path());
        assert!(second.is_err(), "同进程第二次取锁必须失败");
        drop(first);
        let third = DataDirLock::acquire(dir.path()).expect("释放后可再次取锁");
        drop(third);
    }
}
