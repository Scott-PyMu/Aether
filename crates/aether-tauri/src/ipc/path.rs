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
    let canonical = canonicalize_checked(raw)?;

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

/// 校验外部候选文件（D13 `backup_restore` 的外部 `.db`）：绝对路径、存在、必须为文件、
/// 后缀匹配（大小写不敏感）。外部选择器路径不受允许根目录限制（D13：不做默认目录信任）。
pub fn validate_external_file(raw: &str, extension: &str) -> Result<PathBuf, IpcError> {
    let canonical = canonicalize_checked(raw)?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| IpcError::path_rejected(format!("路径不可读：{error}")))?;
    if !metadata.is_file() {
        return Err(IpcError::path_rejected("候选必须是文件（不能是目录）"));
    }
    let suffix_ok = canonical
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension));
    if !suffix_ok {
        return Err(IpcError::path_rejected(format!(
            "候选文件必须以 .{extension} 结尾"
        )));
    }
    Ok(canonical)
}

/// 校验工作区根目录（D7 `workspace_set`）：绝对路径、存在且为目录、同步盘预检拒绝。
///
/// P0 仅对新会话生效（D14）；已有会话不迁移。
pub fn validate_workspace_root(raw: &str) -> Result<PathBuf, IpcError> {
    let canonical = canonicalize_checked(raw)?;
    if !canonical.is_dir() {
        return Err(IpcError::path_rejected("工作区根路径必须是已存在的目录"));
    }
    reject_cloud_sync_path(&canonical)?;
    Ok(canonical)
}

/// 校验迁移目标目录（M1-06 `startup_migrate`）：绝对路径、存在且为目录。
///
/// 同步盘 / 源目录关系校验在 [`crate::startup::StartupGate::migrate`] 内用同一
/// A4 检测上下文复核（目标自身也不得位于同步盘）。
pub fn validate_migration_target(raw: &str) -> Result<PathBuf, IpcError> {
    let canonical = canonicalize_checked(raw)?;
    if !canonical.is_dir() {
        return Err(IpcError::path_rejected("迁移目标必须是已存在的目录"));
    }
    Ok(canonical)
}

/// A4 同步盘预检：M1-08 路径段最小集 + M1-06 实现级检测（环境变量前缀、父目录
/// 重解析点、注册表 `UserFolder`、macOS File Provider / iCloud、网络盘粗筛），
/// 供 `workspace_set` 等路径入口复用。
///
/// 命中即拒绝（A4：默认拒绝，不提供覆盖开关）。
pub fn reject_cloud_sync_path(canonical: &Path) -> Result<(), IpcError> {
    if let Some(marker) = cloud_sync_marker(canonical) {
        return Err(IpcError::path_rejected(format!(
            "命中同步盘/云目录拒绝清单（{marker}）；数据目录必须位于本地磁盘（A4）"
        )));
    }
    let report = crate::startup::detect::detect_data_dir(
        canonical,
        &crate::startup::detect::DetectionContext::native(),
    );
    if report.is_reject() {
        return Err(IpcError::path_rejected(format!(
            "命中 A4 实现级同步盘检测（M1-06）：{}",
            report.reasons.join("；")
        )));
    }
    Ok(())
}

/// 同步盘标记识别（Windows 环境变量前缀 + 路径段；macOS File Provider/iCloud）。
fn cloud_sync_marker(canonical: &Path) -> Option<&'static str> {
    for variable in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        let Some(value) = std::env::var_os(variable) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let Ok(root) = std::fs::canonicalize(PathBuf::from(value)) else {
            continue;
        };
        if is_within(canonical, &root) {
            return Some("OneDrive 环境变量前缀祖先");
        }
    }

    let parts: Vec<String> = canonical
        .components()
        .filter_map(|component| match component {
            Component::Prefix(prefix) => {
                Some(prefix.as_os_str().to_string_lossy().to_ascii_lowercase())
            }
            Component::Normal(part) => Some(part.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    for (index, part) in parts.iter().enumerate() {
        if part == "onedrive" {
            return Some("路径段 OneDrive");
        }
        if part == "cloudstorage"
            && index > 0
            && parts.get(index - 1).is_some_and(|value| value == "library")
        {
            return Some("~/Library/CloudStorage（File Provider）");
        }
        if part == "mobile documents" {
            return Some("iCloud Documents");
        }
    }
    None
}

/// 公共路径检查：空/长度/NUL → 绝对路径 → Windows 特殊形态 → canonicalize（解析软链接/Junction）。
fn canonicalize_checked(raw: &str) -> Result<PathBuf, IpcError> {
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

    std::fs::canonicalize(&candidate).map_err(|error| {
        IpcError::path_rejected(format!("路径不可解析（canonicalize 失败）：{error}"))
    })
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
