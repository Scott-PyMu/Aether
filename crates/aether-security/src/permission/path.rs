//! 工作区路径校验（M2-03；设计 D9「路径校验细则」，评审修订 #10 / T7）。
//!
//! 流程：文本层 Windows 特殊路径拒绝 → 词法规范化（`..` 收敛）→ canonicalize
//! （解析软链接 / Junction / 重解析点）→ 前缀比较（Windows/macOS 大小写不敏感，
//! Linux 敏感）。
//!
//! 已知限制（D9 显式声明）：canonicalize 与文件操作之间存在 TOCTOU 竞态窗口；
//! MVP 信任级接受该限制（防误操作，不防蓄意攻击），P3 沙箱以句柄级校验消除。

use std::path::{Component, Path, PathBuf};

/// 路径拒绝类别（稳定错误码，供审计与诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathViolation {
    /// 空路径 / NUL / 控制字符。
    Malformed { reason: String },
    /// Windows 特殊路径（UNC、扩展前缀、8.3 短名、ADS、尾随点/空格、保留设备名）。
    WindowsSpecialPath {
        /// 命中的规则名（稳定标识）。
        pattern: &'static str,
        detail: String,
    },
    /// 规范化后落在工作区根之外（含 `..` 逃逸与软链接/Junction 逃逸）。
    Escape { resolved: PathBuf, root: PathBuf },
    /// 无法规范化（根不存在、IO 错误等）。
    CanonicalizeFailed { path: PathBuf, reason: String },
}

impl PathViolation {
    /// 稳定错误码。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Malformed { .. } => "malformed_path",
            Self::WindowsSpecialPath { .. } => "windows_special_path",
            Self::Escape { .. } => "path_escape",
            Self::CanonicalizeFailed { .. } => "path_canonicalize_failed",
        }
    }

    /// 拒绝原因（审计/UI 展示；不含规范化结果时的可读描述）。
    pub fn reason(&self) -> String {
        match self {
            Self::Malformed { reason } => format!("路径形态非法：{reason}"),
            Self::WindowsSpecialPath { pattern, detail } => {
                format!("Windows 特殊路径拒绝（{pattern}）：{detail}")
            }
            Self::Escape { resolved, root } => format!(
                "路径逃逸工作区：resolved={} root={}",
                resolved.display(),
                root.display()
            ),
            Self::CanonicalizeFailed { path, reason } => {
                format!("路径规范化失败（{}）：{reason}", path.display())
            }
        }
    }
}

impl std::fmt::Display for PathViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason())
    }
}

impl std::error::Error for PathViolation {}

/// 工作区根不可用（不存在/非目录/无法 canonicalize）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathGuardError {
    RootUnavailable { root: PathBuf, reason: String },
}

impl std::fmt::Display for PathGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootUnavailable { root, reason } => {
                write!(f, "工作区根不可用（{}）：{reason}", root.display())
            }
        }
    }
}

impl std::error::Error for PathGuardError {}

/// 当前平台是否按大小写不敏感做前缀比较（D9：Windows 大小写不敏感；macOS 常见
/// 文件系统同样不敏感；Linux 敏感）。
pub const fn case_insensitive_platform() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}

/// 工作区路径守卫（构造时锁定 canonical 根）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathGuard {
    root: PathBuf,
    case_insensitive: bool,
}

impl PathGuard {
    /// 绑定工作区根（必须存在且为目录）。
    pub fn new(root: impl AsRef<Path>) -> Result<Self, PathGuardError> {
        let raw = root.as_ref();
        let canonical =
            std::fs::canonicalize(raw).map_err(|error| PathGuardError::RootUnavailable {
                root: raw.to_path_buf(),
                reason: error.to_string(),
            })?;
        if !canonical.is_dir() {
            return Err(PathGuardError::RootUnavailable {
                root: raw.to_path_buf(),
                reason: "不是目录".to_owned(),
            });
        }
        Ok(Self {
            root: canonical,
            case_insensitive: case_insensitive_platform(),
        })
    }

    /// canonical 工作区根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 解析并校验目标路径：
    /// - 相对路径按工作区根拼接（工具上报相对路径的容错口径）；
    /// - 返回 canonical 目标（软链接/Junction 已解析）；
    /// - 任一逃逸/特殊路径 → [`PathViolation`]。
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, PathViolation> {
        check_textual(raw)?;
        let candidate = self.absolute_candidate(raw);
        let resolved = canonicalize_with_nonexistent_tail(&candidate).map_err(|error| {
            PathViolation::CanonicalizeFailed {
                path: candidate.clone(),
                reason: error,
            }
        })?;
        if !self.contains(&resolved) {
            return Err(PathViolation::Escape {
                resolved,
                root: self.root.clone(),
            });
        }
        Ok(resolved)
    }

    /// 是否位于工作区内（含根自身）。
    pub fn contains(&self, path: &Path) -> bool {
        if path == self.root {
            return true;
        }
        let path_components: Vec<Component<'_>> = path.components().collect();
        let root_components: Vec<Component<'_>> = self.root.components().collect();
        if path_components.len() < root_components.len() {
            return false;
        }
        if path_components.len() == root_components.len() {
            return self.case_insensitive && eq_ignore_case(path, &self.root);
        }
        for (expected, actual) in root_components.iter().zip(path_components.iter()) {
            if !component_eq(expected, actual, self.case_insensitive) {
                return false;
            }
        }
        true
    }

    /// 原始 target 是否为记忆文件白名单命中（仅文件名 | 大小写不敏感）。
    pub fn file_name_eq(&self, path: &Path, name: &str) -> bool {
        match path.file_name().and_then(|value| value.to_str()) {
            Some(file_name) => {
                if self.case_insensitive {
                    file_name.eq_ignore_ascii_case(name)
                } else {
                    file_name == name
                }
            }
            None => false,
        }
    }

    fn absolute_candidate(&self, raw: &str) -> PathBuf {
        let path = Path::new(raw);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        lexical_normalize(&joined)
    }
}

fn component_eq(expected: &Component<'_>, actual: &Component<'_>, case_insensitive: bool) -> bool {
    match (expected, actual) {
        (Component::Normal(left), Component::Normal(right)) => {
            let left = left.to_string_lossy();
            let right = right.to_string_lossy();
            if case_insensitive {
                left.eq_ignore_ascii_case(&right)
            } else {
                left == right
            }
        }
        (Component::Prefix(left), Component::Prefix(right)) => left == right,
        (Component::RootDir, Component::RootDir) => true,
        (Component::CurDir, Component::CurDir) => true,
        (Component::ParentDir, Component::ParentDir) => true,
        _ => false,
    }
}

fn eq_ignore_case(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

/// 词法规范化：折叠 `.`、收敛 `..`（不越过根）；保留 `Prefix`/`RootDir`。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 仅弹出普通组件；根/Prefix 不可弹（保持根语义，后续前缀比较判逃逸）。
                let can_pop = normalized
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if can_pop {
                    normalized.pop();
                } else {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// canonicalize；目标不存在时向上找到最近存在祖先规范化后拼接剩余组件
/// （写路径的常规形态：文件尚未创建）。
fn canonicalize_with_nonexistent_tail(path: &Path) -> Result<PathBuf, String> {
    match std::fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            let mut cursor = path;
            loop {
                match cursor.parent() {
                    Some(parent) if parent != cursor => {
                        let Some(name) = cursor.file_name() else {
                            return Err("路径缺少文件名".to_owned());
                        };
                        tail.push(name.to_os_string());
                        match std::fs::canonicalize(parent) {
                            Ok(mut resolved) => {
                                tail.reverse();
                                for component in tail {
                                    resolved.push(component);
                                }
                                return Ok(resolved);
                            }
                            Err(parent_error)
                                if parent_error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                cursor = parent;
                            }
                            Err(parent_error) => return Err(parent_error.to_string()),
                        }
                    }
                    _ => return Err(error.to_string()),
                }
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

/// 文本层 Windows 特殊路径拒绝（跨平台统一执行，防 Windows 特殊路径在任一侧误用）。
fn check_textual(raw: &str) -> Result<(), PathViolation> {
    if raw.is_empty() {
        return Err(PathViolation::Malformed {
            reason: "空路径".to_owned(),
        });
    }
    if raw.contains('\0') || raw.chars().any(|ch| ch.is_control()) {
        return Err(PathViolation::Malformed {
            reason: "含 NUL/控制字符".to_owned(),
        });
    }
    // UNC 与扩展前缀：`\\server\share`、`\\?\`、`\\.\`（以及正斜杠变体）。
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return Err(special("unc_or_extended_prefix", raw.to_owned()));
    }
    // 盘符形式：ADS（`file.txt:stream`、`::$DATA`）与 drive-relative（`C:foo`）。
    if let Some(rest) = strip_drive_prefix(raw) {
        if rest.contains(':') {
            return Err(special("ads", raw.to_owned()));
        }
        if !rest.is_empty() && !starts_with_separator(rest) {
            return Err(special("drive_relative", raw.to_owned()));
        }
    } else if raw.contains(':') {
        // 非盘符却含冒号：POSIX 下冒号合法，但作为工具 target 一律按 ADS 形态拒绝。
        return Err(special("ads", raw.to_owned()));
    }

    for component in split_components(raw) {
        if component.is_empty() {
            continue;
        }
        let trimmed_dots = component.trim_end_matches(['.', ' ']);
        if trimmed_dots.len() != component.len() && component != "." && component != ".." {
            return Err(special("trailing_dot_or_space", component.to_owned()));
        }
        let stem = component.split('.').next().unwrap_or(component);
        if is_reserved_device(stem) {
            return Err(special("reserved_device_name", component.to_owned()));
        }
        if is_8_3_short_name(component) {
            return Err(special("short_name_8_3", component.to_owned()));
        }
    }
    Ok(())
}

fn special(pattern: &'static str, detail: String) -> PathViolation {
    PathViolation::WindowsSpecialPath { pattern, detail }
}

fn strip_drive_prefix(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !(raw.starts_with("/") || raw.starts_with("\\"))
    {
        Some(&raw[2..])
    } else {
        None
    }
}

fn starts_with_separator(value: &str) -> bool {
    value.starts_with('/') || value.starts_with('\\')
}

fn split_components(raw: &str) -> Vec<&str> {
    raw.split(['/', '\\']).collect()
}

fn is_reserved_device(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    const FIXED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    if FIXED.contains(&upper.as_str()) {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(digit) = upper.strip_prefix(prefix) {
            if digit.len() == 1 && matches!(digit.as_bytes()[0], b'1'..=b'9') {
                return true;
            }
        }
    }
    false
}

/// 8.3 短名形态：组件内 `~` 后紧跟 1 位以上数字（如 `PROGRA~1`、`DOCUME~1.TXT`）。
fn is_8_3_short_name(component: &str) -> bool {
    let Some(tilde) = component.find('~') else {
        return false;
    };
    let after = &component[tilde + 1..];
    let digits: String = after.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    // `~` 后数字段之后只允许 `.` 开始的扩展名或结束（`foo~1bar` 不形态化拒绝）。
    let remainder = &after[digits.len()..];
    remainder.is_empty() || remainder.starts_with('.')
}

/// T7 路径逃逸样本模板（`{root}` 替换为真实工作区根的字符串形态）。
///
/// 分类覆盖（设计 D9 / 附录 D T7）：`..` 穿越、UNC 与扩展前缀、8.3 短名、
/// ADS、尾随点/空格、保留设备名、drive-relative；软链接/Junction 为文件系统样本，
/// 在集成测试内动态创建（见 `m2_03_path_escape`）。
pub const T7_TEXTUAL_SAMPLES: &[(&str, &str)] = &[
    (
        "parent_traversal",
        "{root}{sep}..{sep}..{sep}Windows{sep}System32{sep}config{sep}SAM",
    ),
    ("parent_traversal_forward", "{root}/../../etc/passwd"),
    ("relative_traversal", "..{sep}outside.txt"),
    ("unc_server_share", "\\\\server\\share\\secret.txt"),
    ("unc_forward_slash", "//server/share/secret.txt"),
    ("extended_prefix", "\\\\?\\C:\\Windows\\System32"),
    ("device_prefix", "\\\\.\\PhysicalDrive0"),
    ("short_name_8_3", "C:\\Windows\\PROGRA~1\\secret.txt"),
    ("short_name_8_3_lower", "{root}{sep}DOCUME~1{sep}secret.txt"),
    ("ads_stream", "C:\\workspace\\report.txt:secret"),
    ("ads_data_stream", "C:\\workspace\\file.txt::$DATA"),
    ("trailing_dot", "C:\\workspace\\note."),
    ("trailing_space", "C:\\workspace\\note "),
    ("reserved_con", "C:\\workspace\\CON"),
    ("reserved_con_ext", "C:\\workspace\\con.txt"),
    ("reserved_com1", "C:\\workspace\\COM1.log"),
    ("reserved_lpt9", "C:\\workspace\\sub\\LPT9"),
    ("reserved_nul_in_dir", "{root}{sep}NUL.txt"),
    ("drive_relative", "C:secret.txt"),
    ("nul_byte", "{root}{sep}bad\u{0}name.txt"),
];

/// 展开 T7 样本模板（`{root}` → 工作区根；`{sep}` → 平台分隔符），保持样本集跨平台语义一致。
pub fn expand_t7_sample(template: &str, root: &str) -> String {
    template
        .replace("{root}", root)
        .replace("{sep}", std::path::MAIN_SEPARATOR_STR)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn textual_samples_are_all_rejected() {
        let temp = std::env::temp_dir().join(format!("aether-m2-03-guard-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let guard = PathGuard::new(&temp).unwrap();
        let root = temp.to_string_lossy().to_string();
        let mut denied = 0usize;
        for (label, template) in T7_TEXTUAL_SAMPLES {
            let raw = expand_t7_sample(template, &root);
            let result = guard.resolve(&raw);
            assert!(result.is_err(), "样本 {label} 必须被拒绝，实际：{result:?}");
            denied += 1;
        }
        assert_eq!(denied, T7_TEXTUAL_SAMPLES.len());
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn inside_workspace_paths_are_allowed_after_canonicalize() {
        let temp = std::env::temp_dir().join(format!("aether-m2-03-allow-{}", std::process::id()));
        std::fs::create_dir_all(temp.join("sub")).unwrap();
        std::fs::write(temp.join("sub").join("a.txt"), b"x").unwrap();
        let guard = PathGuard::new(&temp).unwrap();

        let file = guard
            .resolve(&temp.join("sub").join("a.txt").to_string_lossy())
            .unwrap();
        assert!(guard.contains(&file));

        // 尚不存在的写目标（workspace 内）允许且路径规范化。
        let new_file = guard
            .resolve(&temp.join("sub").join("new.txt").to_string_lossy())
            .unwrap();
        assert!(guard.contains(&new_file));

        // 相对路径按工作区根解析（平台分隔符）。
        let relative = guard
            .resolve(&format!("sub{}a.txt", std::path::MAIN_SEPARATOR))
            .unwrap();
        assert_eq!(relative, file);

        // 工作区内 `..` 收敛后仍在根内 → 允许（canonicalize 语义）。
        let converged = guard
            .resolve(
                &temp
                    .join("sub")
                    .join("..")
                    .join("sub")
                    .join("a.txt")
                    .to_string_lossy(),
            )
            .unwrap();
        assert_eq!(converged, file);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn escape_by_parent_traversal_is_denied_even_for_nonexistent_target() {
        let temp = std::env::temp_dir().join(format!("aether-m2-03-escape-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let guard = PathGuard::new(&temp).unwrap();
        let raw = temp
            .join("..")
            .join("definitely-outside.txt")
            .to_string_lossy()
            .to_string();
        let error = guard.resolve(&raw).unwrap_err();
        assert_eq!(error.code(), "path_escape");
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn violation_codes_are_stable() {
        assert_eq!(
            PathViolation::WindowsSpecialPath {
                pattern: "ads",
                detail: "x".to_owned()
            }
            .code(),
            "windows_special_path"
        );
        assert_eq!(
            PathViolation::Escape {
                resolved: PathBuf::from("C:/x"),
                root: PathBuf::from("C:/ws")
            }
            .code(),
            "path_escape"
        );
        assert_eq!(
            PathViolation::Malformed {
                reason: "空".to_owned()
            }
            .code(),
            "malformed_path"
        );
        assert_eq!(
            PathViolation::CanonicalizeFailed {
                path: PathBuf::from("x"),
                reason: "io".to_owned()
            }
            .code(),
            "path_canonicalize_failed"
        );
    }

    #[test]
    fn reserved_device_variants_are_detected() {
        for name in [
            "CON", "con", "Con.txt", "NUL", "nul.log", "AUX", "PRN", "COM1", "com9.txt", "LPT1",
            "lpt9.dat",
        ] {
            assert!(
                is_reserved_device(name.split('.').next().unwrap()),
                "{name}"
            );
        }
        for name in ["CONSOLE", "COM10", "LPT0", "NULLABLE", "MYCON"] {
            assert!(!is_reserved_device(name), "{name}");
        }
    }

    #[test]
    fn short_name_detection_is_precise() {
        assert!(is_8_3_short_name("PROGRA~1"));
        assert!(is_8_3_short_name("DOCUME~1.TXT"));
        assert!(is_8_3_short_name("A~12"));
        assert!(!is_8_3_short_name("normal.txt"));
        assert!(!is_8_3_short_name("tilde~name"));
        assert!(!is_8_3_short_name("no-digits~"));
    }

    #[test]
    fn guard_requires_existing_directory_root() {
        let missing = std::env::temp_dir().join("aether-m2-03-missing-root-xyz");
        let error = PathGuard::new(&missing).unwrap_err();
        assert!(error.to_string().contains("工作区根不可用"));
        assert!(matches!(error, PathGuardError::RootUnavailable { .. }));
    }

    #[test]
    fn malformed_paths_and_violation_display() {
        let temp = std::env::temp_dir().join(format!("aether-m2-03-mal-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let guard = PathGuard::new(&temp).unwrap();
        let error = guard.resolve("").unwrap_err();
        assert_eq!(error.code(), "malformed_path");
        assert!(error.to_string().contains("路径形态非法"));
        let error = guard.resolve("bad\u{0}path").unwrap_err();
        assert_eq!(error.code(), "malformed_path");
        let error = guard.resolve("\\\\?\\C:\\x").unwrap_err();
        assert!(error.to_string().contains("Windows 特殊路径拒绝"));
        let error = PathViolation::CanonicalizeFailed {
            path: PathBuf::from("x"),
            reason: "io".to_owned(),
        };
        assert!(error.to_string().contains("规范化失败"));
        let error = PathViolation::Escape {
            resolved: PathBuf::from("C:/other"),
            root: PathBuf::from("C:/ws"),
        };
        assert!(error.to_string().contains("逃逸"));
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn contains_and_file_name_helpers() {
        let temp =
            std::env::temp_dir().join(format!("aether-m2-03-contains-{}", std::process::id()));
        std::fs::create_dir_all(temp.join("sub")).unwrap();
        let guard = PathGuard::new(&temp).unwrap();
        assert!(guard.contains(guard.root()));
        assert!(guard.contains(&guard.root().join("sub")));
        let sibling = std::env::temp_dir().join("aether-m2-03-sibling");
        assert!(!guard.contains(&sibling));
        assert!(guard.file_name_eq(Path::new("C:/ws/AGENTS.md"), "AGENTS.md"));
        assert!(!guard.file_name_eq(Path::new("C:/ws/AGENTS.md"), "CLAUDE.md"));
        assert!(!guard.file_name_eq(Path::new("C:/ws"), "other"));
        // 平台大小写规则：Windows/macOS 不敏感，Linux 敏感。
        assert_eq!(
            case_insensitive_platform(),
            cfg!(any(target_os = "windows", target_os = "macos"))
        );
        std::fs::remove_dir_all(&temp).ok();
    }
}
