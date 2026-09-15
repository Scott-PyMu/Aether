//! 路径类 IPC 参数校验（设计 D9 评审 #10 + D7 安全基线）。
//!
//! 固定流程：**先拒绝 Windows 特殊路径形态 → canonicalize（解析软链接 / Junction /
//! 重解析点）→ 与允许根目录做前缀比较（Windows 大小写不敏感）**。
//!
//! TOCTOU 限制声明（设计 D9）：canonicalize 与后续文件操作之间存在检查-使用窗口，
//! MVP 信任级模型下接受该限制（防误操作，不防蓄意攻击）；P3 沙箱以句柄级校验消除。

use std::path::{Component, Path, PathBuf};

use super::error::IpcError;
use super::validate::MAX_PATH_CHARS;

/// 校验用户提供的路径：必须为绝对路径、存在、且位于允许根目录内。
///
/// `allowed_roots` 为空表示「尚未配置允许目录」——一律拒绝（默认拒绝原则）。
pub fn validate_user_path(raw: &str, allowed_roots: &[PathBuf]) -> Result<PathBuf, IpcError> {
    if raw.is_empty() {
        return Err(IpcError::path_rejected("路径为空"));
    }
    if raw.chars().count() > MAX_PATH_CHARS {
        return Err(IpcError::path_rejected(format!(
            "路径字符数超过上限 {MAX_PATH_CHARS}"
        )));
    }
    if raw.contains('\0') {
        return Err(IpcError::path_rejected("路径包含 NUL 字符"));
    }

    let candidate = PathBuf::from(raw);
    if !candidate.is_absolute() {
        return Err(IpcError::path_rejected("必须是绝对路径"));
    }

    reject_windows_special_forms(raw)?;

    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        IpcError::path_rejected(format!("路径不可解析（canonicalize 失败）：{error}"))
    })?;

    if allowed_roots.is_empty() {
        return Err(IpcError::path_rejected(
            "未配置允许根目录；按默认拒绝策略拒绝该路径",
        ));
    }

    let within = allowed_roots.iter().any(|root| {
        let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        is_within(&canonical, &canonical_root)
    });
    if !within {
        return Err(IpcError::path_rejected(
            "路径不在允许根目录内（canonicalize 后前缀校验失败）",
        ));
    }

    Ok(canonical)
}

/// `path` 是否位于 `root` 之下或等于 `root`（按组件比较；Windows 大小写不敏感）。
pub fn is_within(path: &Path, root: &Path) -> bool {
    let path_components = components(path);
    let root_components = components(root);
    if path_components.len() < root_components.len() {
        return false;
    }
    path_components
        .iter()
        .zip(root_components.iter())
        .all(|(left, right)| left == right)
}

fn components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().to_string()),
            Component::RootDir | Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
        })
        .map(|part| normalize(&part))
        .collect()
}

#[cfg(windows)]
fn normalize(part: &str) -> String {
    part.to_lowercase()
}

#[cfg(not(windows))]
fn normalize(part: &str) -> String {
    part.to_string()
}

/// Windows 特殊路径拒绝清单（设计 D9 评审 #10）：
/// UNC 与扩展前缀（`\\server\share`、`\\?\`、`\\.\`）、8.3 短名（`PROGRA~1`）、
/// ADS（`file.txt:stream`）、尾随点/空格（`foo.`、`foo `）、保留设备名
/// （`CON/NUL/AUX/PRN/COM1-9/LPT1-9`，含带扩展名变体）。
#[cfg(windows)]
pub fn reject_windows_special_forms(raw: &str) -> Result<(), IpcError> {
    if raw.starts_with(r"\\") || raw.starts_with("//") {
        return Err(IpcError::path_rejected(
            "拒绝 UNC / 扩展长度前缀路径（\\\\server\\share、\\\\?\\、\\\\.\\）",
        ));
    }

    let bytes = raw.as_bytes();
    let rest = if bytes.len() >= 2 && bytes[1] == b':' {
        &raw[2..]
    } else {
        raw
    };

    if rest.contains(':') {
        return Err(IpcError::path_rejected("拒绝包含备用数据流（ADS）的路径"));
    }

    for segment in rest.split(['\\', '/']) {
        if segment.is_empty() {
            continue;
        }
        if looks_like_short_name(segment) {
            return Err(IpcError::path_rejected("拒绝 8.3 短名路径（如 PROGRA~1）"));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(IpcError::path_rejected(
                "拒绝以点或空格结尾的路径段（Windows 会截断）",
            ));
        }
        if is_reserved_device_name(segment) {
            return Err(IpcError::path_rejected(
                "拒绝 Windows 保留设备名（CON/NUL/AUX/PRN/COM1-9/LPT1-9）",
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn reject_windows_special_forms(_raw: &str) -> Result<(), IpcError> {
    Ok(())
}

#[cfg(windows)]
fn looks_like_short_name(segment: &str) -> bool {
    match segment.find('~') {
        Some(index) => {
            let tail = &segment[index + 1..];
            let digits = tail.split('.').next().unwrap_or("");
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

#[cfg(windows)]
fn is_reserved_device_name(segment: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let base = segment.split('.').next().unwrap_or(segment).trim();
    RESERVED
        .iter()
        .any(|reserved| base.eq_ignore_ascii_case(reserved))
}
