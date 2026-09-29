//! 磁盘空间探针（ADR-003 决策 19；M3-04 统一接线：备份/导出外部路径与迁移复用）。
//!
//! - [`ensure_writable`]：目录可写校验（创建并删除隐藏探针文件）；
//! - [`available_bytes`]：目标路径所在挂载点的可用字节数（`sysinfo`，跨平台安全 API；
//!   不引入 unsafe FFI，AGENTS §2.2 与工作区 `unsafe_code = forbid` 保持）。
//!
//! 空间护栏口径（ADR-003 决策 19 / D13）：可用空间 ≥ 当前 `db+wal` ×1.2；不足拒绝。
//! 未知挂载点（异常环境）返回错误，由调用方决定是否降级为「未知不阻断」
//! （迁移探针按 ADR-006 §5-2 口径处理）。

use std::path::Path;

use sysinfo::Disks;

/// 目录可写探针（创建并删除探针文件）；失败返回可展示原因。
pub fn ensure_writable(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!(".aether-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"probe")
        .map_err(|error| format!("目录不可写（{}）：{error}", dir.display()))?;
    std::fs::remove_file(&probe)
        .map_err(|error| format!("目录不可写（探针清理失败 {}）：{error}", dir.display()))?;
    Ok(())
}

/// 目标路径所在挂载点的可用字节数（`Err` = 无法确定，调用方决定阻断策略）。
pub fn available_bytes(path: &Path) -> Result<u64, String> {
    let normalized = normalize_for_match(path);
    let disks = Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64)> = None;
    for disk in disks.list() {
        let mount = normalize_for_match(disk.mount_point());
        if mount.is_empty() {
            continue;
        }
        if normalized == mount || normalized.starts_with(&mount) {
            let len = mount.len();
            let better = match best {
                None => true,
                Some((best_len, _)) => len > best_len,
            };
            if better {
                best = Some((len, disk.available_space()));
            }
        }
    }
    match best {
        Some((_, available)) => Ok(available),
        None => Err(format!("未找到目标路径所在挂载点（{}）", path.display())),
    }
}

/// 路径字符串归一化（挂载点前缀匹配）：小写 + `/`→`\` + 去 `\\?\` 扩展前缀 + 去尾部斜杠。
fn normalize_for_match(path: &Path) -> String {
    let mut text = path.to_string_lossy().to_lowercase().replace('/', "\\");
    if let Some(stripped) = text.strip_prefix("\\\\?\\") {
        text = stripped.to_owned();
    }
    while text.ends_with('\\') && text.len() > 1 {
        text.pop();
    }
    text
}
