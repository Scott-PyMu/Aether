//! A4 实现级同步盘检测（M1-06；设计文档 A4「验证（实现级检测，评审修订 #2）」）。
//!
//! Windows 三类 + 粗筛（与 A4 一一对应）：
//! 1. `win.one_drive_env_prefix`：`OneDrive` / `OneDriveConsumer` / `OneDriveCommercial`
//!    环境变量的值是否为候选数据目录的祖先（canonicalize 后前缀比较）；
//! 2. `win.parent_reparse_point`：逐级父目录 `FILE_ATTRIBUTE_REPARSE_POINT`
//!    （云占位 / 分层文件 / Junction）——用 std 的安全 Windows 元数据接口读取，
//!    因工作区 `unsafe_code = "forbid"` 不引入裸 FFI；
//! 3. `win.reg_user_folder`：`HKCU\Software\Microsoft\OneDrive\Accounts\*` 的
//!    `UserFolder` 值比对（winreg 安全封装）；
//! 4. `win.network_drive_screen`：网络盘粗筛——UNC 形态 + `HKCU\Network\*` 映射盘
//!    （A4 的 `GetDriveType` 粗筛等价物；裸 FFI 被 unsafe 禁令阻断，见任务证据说明）。
//!
//! macOS 两类：
//! 1. `mac.icloud_ubiquitous`：iCloud Drive 容器（`~/Library/Mobile Documents`）——
//!    风险条款（实施计划 M1-06）允许的降级实现：**路径前缀匹配 + 用户确认**，
//!    未绑定 `NSURLIsUbiquitousItemKey`（CoreFoundation 绑定成本超预期），
//!    精度以 [`Precision::PathPrefix`] 标注并在 UI 明示；
//! 2. `mac.file_provider_path`：File Provider 挂载（`~/Library/CloudStorage/*`）。
//!
//! 判定为「默认拒绝」：任一检查命中 → [`Verdict::Reject`]（A4/评审 #9，无覆盖开关）。
//! 检测上下文可注入（环境变量、注册表、重解析点、网络盘清单），使 Win/mac 样本在
//! 任意宿主上可执行断言；生产路径使用 [`DetectionContext::native`]。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ipc::path::is_within;

/// Windows OneDrive 环境变量清单（A4 ①）。
pub const WINDOWS_ENV_VARIABLES: &[&str] = &["OneDrive", "OneDriveConsumer", "OneDriveCommercial"];

/// 注册表 OneDrive 账户根路径（A4 ③）。
#[cfg(windows)]
pub const REGISTRY_ACCOUNTS_PATH: &str = r"Software\Microsoft\OneDrive\Accounts";

/// 注册表网络映射盘根路径（网络盘粗筛，HKCU\Network\<盘符>）。
#[cfg(windows)]
pub const REGISTRY_NETWORK_PATH: &str = r"Network";

pub const CHECK_WIN_ENV_PREFIX: &str = "win.one_drive_env_prefix";
pub const CHECK_WIN_PARENT_REPARSE: &str = "win.parent_reparse_point";
pub const CHECK_WIN_REGISTRY: &str = "win.reg_user_folder";
pub const CHECK_WIN_NETWORK_DRIVE: &str = "win.network_drive_screen";
pub const CHECK_MAC_ICLOUD: &str = "mac.icloud_ubiquitous";
pub const CHECK_MAC_FILE_PROVIDER: &str = "mac.file_provider_path";

/// 检测平台（生产取宿主平台；样本测试可注入，使 macOS 判定在任意宿主可断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformKind {
    Windows,
    #[serde(rename = "macos")]
    MacOs,
    Other,
}

impl PlatformKind {
    pub fn native() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }
}

/// macOS 降级检测的精度限制说明（M1-06 风险条款：降级为路径前缀 + 用户手动确认，
/// 必须在 UI 与证据中明示；测试与验证脚本按本常量逐字断言）。
pub const MAC_PRECISION_NOTE: &str = "macOS 同步盘检测精度受限（iCloud 采用路径前缀近似，未绑定 NSURLIsUbiquitousItemKey）；请确认目录不在 iCloud/CloudStorage 下。";

/// 检测精度（风险条款：macOS iCloud 标记降级为路径前缀时必须在 UI 与证据中明示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    /// 按设计指定机制精确判定。
    Exact,
    /// 路径前缀近似（降级实现，存在精度限制）。
    PathPrefix,
}

/// 单条检查的结果。
#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    pub id: &'static str,
    pub label: &'static str,
    pub hit: bool,
    pub detail: String,
    pub precision: Precision,
}

/// 总体判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allow,
    Reject,
}

/// 检测报告（UI 展示 + 证据归档）。
#[derive(Debug, Clone, Serialize)]
pub struct DetectionReport {
    pub platform: PlatformKind,
    pub candidate: String,
    pub resolved: String,
    pub verdict: Verdict,
    pub checks: Vec<CheckOutcome>,
    pub reasons: Vec<String>,
    /// 精度限制说明（降级实现时非空，UI 明示）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl DetectionReport {
    pub fn is_reject(&self) -> bool {
        self.verdict == Verdict::Reject
    }
}

/// 注册表 OneDrive `UserFolder` 来源。
#[derive(Debug, Clone)]
pub enum RegistrySource {
    /// 读取真实 `HKCU\Software\Microsoft\OneDrive\Accounts\*`。
    Native,
    /// 读取 `HKCU\<路径>` 下的账户子键（测试用沙箱键，避免触碰真实 OneDrive 配置）。
    NativeAt(String),
    /// 注入样本：`(账户名, UserFolder)`。
    Values(Vec<(String, PathBuf)>),
    Unavailable,
}

/// 逐级父目录重解析点来源。
#[derive(Debug, Clone)]
pub enum ReparseSource {
    /// 使用 std 安全元数据接口读取真实 `FILE_ATTRIBUTE_REPARSE_POINT`。
    Native,
    /// 注入样本：视为重解析点的路径清单。
    Paths(Vec<PathBuf>),
    Unavailable,
}

/// 网络盘粗筛来源（A4 ④ GetDriveType 的等价实现）。
#[derive(Debug, Clone)]
pub enum NetworkDriveSource {
    /// 读取 `HKCU\Network\*` 映射盘符。
    Native,
    /// 注入样本：视为网络映射盘的盘符（如 `"Z"`）。
    Letters(Vec<String>),
    Unavailable,
}

/// 检测上下文（生产 [`DetectionContext::native`]；样本测试注入）。
#[derive(Debug, Clone)]
pub struct DetectionContext {
    pub platform: PlatformKind,
    pub home: Option<PathBuf>,
    pub env: BTreeMap<String, PathBuf>,
    pub registry: RegistrySource,
    pub reparse: ReparseSource,
    pub network_drives: NetworkDriveSource,
}

impl DetectionContext {
    pub fn native() -> Self {
        let mut env = BTreeMap::new();
        for variable in WINDOWS_ENV_VARIABLES {
            if let Some(value) = std::env::var_os(variable) {
                if !value.is_empty() {
                    env.insert((*variable).to_string(), PathBuf::from(value));
                }
            }
        }
        Self {
            platform: PlatformKind::native(),
            home: dirs::home_dir(),
            env,
            registry: if cfg!(windows) {
                RegistrySource::Native
            } else {
                RegistrySource::Unavailable
            },
            reparse: if cfg!(windows) {
                ReparseSource::Native
            } else {
                ReparseSource::Unavailable
            },
            network_drives: if cfg!(windows) {
                NetworkDriveSource::Native
            } else {
                NetworkDriveSource::Unavailable
            },
        }
    }
}

/// 运行 A4 检测。`candidate` 为待检查的数据目录（可不存在；不存在时解析到最深的
/// 已存在祖先再拼接剩余组件，保证 Junction / iCloud 符号链接可被解析命中）。
pub fn detect_data_dir(candidate: &Path, ctx: &DetectionContext) -> DetectionReport {
    let resolved = resolve_existing_prefix(candidate);
    let mut checks = Vec::new();
    match ctx.platform {
        PlatformKind::Windows => {
            checks.push(check_windows_env_prefix(&resolved, candidate, ctx));
            checks.push(check_windows_reparse(candidate, ctx));
            checks.push(check_windows_registry(&resolved, candidate, ctx));
            checks.push(check_windows_network_drive(&resolved, candidate, ctx));
        }
        PlatformKind::MacOs => {
            checks.push(check_macos_icloud(&resolved, candidate, ctx));
            checks.push(check_macos_file_provider(&resolved, candidate, ctx));
        }
        PlatformKind::Other => {}
    }

    let reasons: Vec<String> = checks
        .iter()
        .filter(|check| check.hit)
        .map(|check| format!("{}：{}", check.label, check.detail))
        .collect();
    let verdict = if reasons.is_empty() {
        Verdict::Allow
    } else {
        Verdict::Reject
    };
    // macOS 全量检测均含路径前缀近似成分（iCloud 容器不绑定 NSURLIsUbiquitousItemKey，
    // File Provider 为路径判定），因此 macOS 上下文一律附带精度限制说明（UI 明示）。
    let note = if ctx.platform == PlatformKind::MacOs {
        Some(MAC_PRECISION_NOTE.to_string())
    } else {
        None
    };

    DetectionReport {
        platform: ctx.platform,
        candidate: candidate.to_string_lossy().to_string(),
        resolved: resolved.to_string_lossy().to_string(),
        verdict,
        checks,
        reasons,
        note,
    }
}

fn check_windows_env_prefix(resolved: &Path, raw: &Path, ctx: &DetectionContext) -> CheckOutcome {
    let mut hits = Vec::new();
    for variable in WINDOWS_ENV_VARIABLES {
        let Some(value) = ctx.env.get(*variable) else {
            continue;
        };
        if value.as_os_str().is_empty() {
            continue;
        }
        let root = resolve_existing_prefix(value);
        if within_candidate(resolved, raw, &root) {
            hits.push(format!("{variable}={}", root.display()));
        }
    }
    if hits.is_empty() {
        return allow(
            CHECK_WIN_ENV_PREFIX,
            "Windows：OneDrive 环境变量前缀祖先",
            "未命中：候选目录不在 OneDrive/OneDriveConsumer/OneDriveCommercial 前缀内",
        );
    }
    reject(
        CHECK_WIN_ENV_PREFIX,
        "Windows：OneDrive 环境变量前缀祖先",
        format!("命中：{}", hits.join("；")),
    )
}

fn check_windows_reparse(raw: &Path, ctx: &DetectionContext) -> CheckOutcome {
    let label = "Windows：父目录重解析点（云占位 / Junction）";
    let hit = match &ctx.reparse {
        ReparseSource::Unavailable => None,
        ReparseSource::Paths(paths) => raw.ancestors().find_map(|ancestor| {
            paths
                .iter()
                .find(|path| same_path(path, ancestor))
                .map(|path| path.to_path_buf())
        }),
        ReparseSource::Native => native_reparse_ancestor(raw),
    };
    match (hit, &ctx.reparse) {
        (Some(path), _) => reject(
            CHECK_WIN_PARENT_REPARSE,
            label,
            format!("命中：{} 带 FILE_ATTRIBUTE_REPARSE_POINT", path.display()),
        ),
        (None, ReparseSource::Unavailable) => allow(
            CHECK_WIN_PARENT_REPARSE,
            label,
            "未执行：当前宿主无 Windows 重解析点接口",
        ),
        (None, _) => allow(
            CHECK_WIN_PARENT_REPARSE,
            label,
            "未命中：逐级父目录均无重解析点标记",
        ),
    }
}

fn check_windows_registry(resolved: &Path, raw: &Path, ctx: &DetectionContext) -> CheckOutcome {
    let label = "Windows：注册表 UserFolder 比对";
    let values = match &ctx.registry {
        RegistrySource::Unavailable => {
            return allow(CHECK_WIN_REGISTRY, label, "未执行：当前宿主无注册表接口");
        }
        RegistrySource::Values(values) => values.clone(),
        RegistrySource::Native => native_user_folders(REGISTRY_ACCOUNTS_PATH),
        RegistrySource::NativeAt(path) => native_user_folders(path),
    };
    let mut hits = Vec::new();
    for (account, user_folder) in &values {
        let root = resolve_existing_prefix(user_folder);
        if within_candidate(resolved, raw, &root) {
            hits.push(format!("{account}: {}", root.display()));
        }
    }
    if hits.is_empty() {
        return allow(
            CHECK_WIN_REGISTRY,
            label,
            format!("未命中：已比对 {} 个账户 UserFolder", values.len()),
        );
    }
    reject(
        CHECK_WIN_REGISTRY,
        label,
        format!("命中：{}", hits.join("；")),
    )
}

fn check_windows_network_drive(
    resolved: &Path,
    raw: &Path,
    ctx: &DetectionContext,
) -> CheckOutcome {
    let label = "Windows：网络盘粗筛（UNC / 映射盘）";
    if is_unc(raw) || is_unc(resolved) {
        return reject(
            CHECK_WIN_NETWORK_DRIVE,
            label,
            format!("命中：UNC 网络路径 {}", raw.display()),
        );
    }
    let letters = match &ctx.network_drives {
        NetworkDriveSource::Unavailable => {
            return allow(
                CHECK_WIN_NETWORK_DRIVE,
                label,
                "未执行：当前宿主无网络盘清单接口",
            );
        }
        NetworkDriveSource::Letters(letters) => letters.clone(),
        NetworkDriveSource::Native => native_network_drive_letters(),
    };
    let drive = drive_letter(raw).or_else(|| drive_letter(resolved));
    if let Some(drive) = &drive {
        let mapped = letters.iter().any(|letter| {
            drive_letter_of(letter).is_some_and(|candidate| candidate.eq_ignore_ascii_case(drive))
        });
        if mapped {
            return reject(
                CHECK_WIN_NETWORK_DRIVE,
                label,
                format!("命中：{drive}: 为 HKCU\\Network 映射的网络盘"),
            );
        }
    }
    allow(
        CHECK_WIN_NETWORK_DRIVE,
        label,
        format!("未命中：已比对 {} 个映射盘符", letters.len()),
    )
}

fn check_macos_icloud(resolved: &Path, raw: &Path, ctx: &DetectionContext) -> CheckOutcome {
    let label = "macOS：iCloud Drive 容器（Mobile Documents）";
    let Some(home) = &ctx.home else {
        return allow(CHECK_MAC_ICLOUD, label, "未执行：无法确定用户主目录");
    };
    let root = home.join("Library").join("Mobile Documents");
    if within_candidate(resolved, raw, &root) {
        return CheckOutcome {
            id: CHECK_MAC_ICLOUD,
            label,
            hit: true,
            detail: format!(
                "命中：候选目录位于 {}（iCloud Drive 容器；路径前缀近似）",
                root.display()
            ),
            precision: Precision::PathPrefix,
        };
    }
    CheckOutcome {
        id: CHECK_MAC_ICLOUD,
        label,
        hit: false,
        detail: "未命中：候选目录不在 ~/Library/Mobile Documents 内".to_string(),
        precision: Precision::PathPrefix,
    }
}

fn check_macos_file_provider(resolved: &Path, raw: &Path, ctx: &DetectionContext) -> CheckOutcome {
    let label = "macOS：File Provider 挂载（~/Library/CloudStorage）";
    let Some(home) = &ctx.home else {
        return allow(CHECK_MAC_FILE_PROVIDER, label, "未执行：无法确定用户主目录");
    };
    let root = home.join("Library").join("CloudStorage");
    if within_candidate(resolved, raw, &root) {
        return reject(
            CHECK_MAC_FILE_PROVIDER,
            label,
            format!(
                "命中：候选目录位于 File Provider 挂载点 {}（Dropbox/Google Drive/OneDrive 等）",
                root.display()
            ),
        );
    }
    allow(
        CHECK_MAC_FILE_PROVIDER,
        label,
        "未命中：候选目录不在 ~/Library/CloudStorage 内",
    )
}

fn allow(id: &'static str, label: &'static str, detail: impl Into<String>) -> CheckOutcome {
    CheckOutcome {
        id,
        label,
        hit: false,
        detail: detail.into(),
        precision: Precision::Exact,
    }
}

fn reject(id: &'static str, label: &'static str, detail: impl Into<String>) -> CheckOutcome {
    CheckOutcome {
        id,
        label,
        hit: true,
        detail: detail.into(),
        precision: Precision::Exact,
    }
}

fn within_candidate(resolved: &Path, raw: &Path, root: &Path) -> bool {
    is_within(resolved, root) || is_within(raw, root)
}

fn is_unc(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        // `\\?\C:\...` 为 verbatim 本地路径；`\\?\UNC\server\share` 才是 UNC。
        return rest.starts_with(r"UNC\");
    }
    text.starts_with(r"\\") || text.starts_with("//")
}

fn drive_letter(path: &Path) -> Option<String> {
    use std::path::Component;
    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            let text = prefix.as_os_str().to_string_lossy().to_string();
            drive_letter_of(&text)
        }
        _ => None,
    }
}

fn drive_letter_of(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some((bytes[0] as char).to_ascii_uppercase().to_string());
    }
    None
}

#[cfg(windows)]
fn native_reparse_ancestor(candidate: &Path) -> Option<PathBuf> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    candidate
        .ancestors()
        .find(|ancestor| {
            std::fs::symlink_metadata(ancestor)
                .map(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
                .unwrap_or(false)
        })
        .map(Path::to_path_buf)
}

#[cfg(not(windows))]
fn native_reparse_ancestor(_candidate: &Path) -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn native_user_folders(root: &str) -> Vec<(String, PathBuf)> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let mut values = Vec::new();
    let Ok(accounts) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(root) else {
        return values;
    };
    for account in accounts.enum_keys().flatten() {
        let Ok(key) = accounts.open_subkey(&account) else {
            continue;
        };
        if let Ok(user_folder) = key.get_value::<String, _>("UserFolder") {
            if !user_folder.trim().is_empty() {
                values.push((account, PathBuf::from(user_folder)));
            }
        }
    }
    values
}

#[cfg(not(windows))]
fn native_user_folders(_root: &str) -> Vec<(String, PathBuf)> {
    Vec::new()
}

#[cfg(windows)]
fn native_network_drive_letters() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let mut letters = Vec::new();
    let Ok(network) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(REGISTRY_NETWORK_PATH) else {
        return letters;
    };
    for drive in network.enum_keys().flatten() {
        if let Some(letter) = drive_letter_of(&drive) {
            letters.push(letter);
        }
    }
    letters
}

#[cfg(not(windows))]
fn native_network_drive_letters() -> Vec<String> {
    Vec::new()
}

/// 解析路径到「最深可 canonicalize 祖先 + 剩余组件」：既解析 Junction / 符号链接，
/// 又允许目标尚不存在（检测发生在目录创建之前）。
pub fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    let mut suffix: Vec<OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&candidate) {
            let mut result = strip_verbatim(canonical);
            for part in suffix.iter().rev() {
                result.push(part);
            }
            return result;
        }
        let Some(name) = candidate.file_name().map(|name| name.to_os_string()) else {
            return path.to_path_buf();
        };
        suffix.push(name);
        if !candidate.pop() {
            return path.to_path_buf();
        }
    }
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix(r"UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    path
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// 路径比较（Windows 大小写不敏感，其余平台按组件直接比较）。
pub fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}
