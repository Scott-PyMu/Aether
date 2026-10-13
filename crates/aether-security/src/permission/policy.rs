//! 权限策略矩阵（M2-03 建立；M6-01 按 ADR-014 附录 B 扩展为会话级三档）。
//!
//! 三档与运行时**同名同序**（`read_only` / `workspace_write`（缺省）/ `danger_full_access`）：
//!
//! | 资源:动作 | `read_only` | `workspace_write`（缺省） | `danger_full_access` |
//! |---|---|---|---|
//! | `fs.read` 区内 / 区外 | allow / allow | allow / allow | allow / allow |
//! | `fs.write` 区内 | deny | allow | allow |
//! | `fs.write` 区外 | deny | ask（300s → deny + 审计） | allow |
//! | `exec` / `net`（上报型） | deny | deny | allow（**仅门层**；PF-05 维持 P3） |
//! | 记忆文件白名单 | deny | 被区内 `allow` 覆盖（留审计 + 1MB 上限） | 同左 |
//!
//! 本模块只产出**档位矩阵**结果（硬边界；ADR-019 决策顺序第 ① 步）：矩阵 `allow` 可被
//! 用户规则收紧为 deny、矩阵 `ask` 可被规则/授权放行；矩阵 `deny` 不可被规则放宽。
//! 规则层与授权匹配在 `aether-control` 权限服务（`permission_rules` / `always` 持久行）。
//!
//! 路径校验（ADR-014 不变量 3 / 问题 12）：区内目标沿用 P0 口径（工作区相对段组件
//! 形态检查）；**区外目标要求解析后不含软链接/Junction 重解析点**
//! （`ResolvedTarget.reparse_resolved=false`），否则一律 deny（fail-closed）。
//!
//! 边界（D9 评审修订 #1 / AGENTS §2.7）：本门仅约束适配器经线协议上报的工具调用；
//! 适配器进程内行为（含其自身执行的 shell 命令）不经此门，属信任级已知边界。

use std::path::{Path, PathBuf};

use aether_core::PermissionMode;

use super::path::{PathGuard, PathGuardError, PathViolation};

/// 记忆文件白名单（D9：对这 3 个文件名的写入 = allow；`read_only` 档恒 deny）。
pub const MEMORY_FILE_NAMES: [&str; 3] = ["AGENTS.md", "AETHER.md", "CLAUDE.md"];

/// 记忆文件写入上限（D9：1MB）。
pub const MEMORY_FILE_MAX_BYTES: u64 = 1_048_576;

/// 权限资源（D9 矩阵的资源维度；`resource` 字符串与线协议口径一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionResource {
    FsRead,
    FsWrite,
    Exec,
    Net,
}

impl PermissionResource {
    /// 线协议 `resource` 取值（D9：`fs.read` / `fs.write` / `exec` / `net`）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::Exec => "exec",
            Self::Net => "net",
        }
    }

    /// 解析线协议取值（未知资源 → `None`，由调用方按 deny 处理）。
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "fs.read" => Some(Self::FsRead),
            "fs.write" => Some(Self::FsWrite),
            "exec" => Some(Self::Exec),
            "net" => Some(Self::Net),
            _ => None,
        }
    }

    /// 是否文件系统类资源（需要路径校验）。
    pub const fn is_fs(self) -> bool {
        matches!(self, Self::FsRead | Self::FsWrite)
    }

    /// 规则层资源-动作兼容值（ADR-019 附录 A-1：`fs.read→read` 等）。
    pub const fn rule_action(self) -> &'static str {
        match self {
            Self::FsRead => "read",
            Self::FsWrite => "write",
            Self::Exec => "exec",
            Self::Net => "connect",
        }
    }
}

/// 策略判定（与 `permissions.decision` CHECK 枚举一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny,
    Ask,
}

impl PolicyDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// 策略评估请求（原始 target 与规范化结果同时保留，D9：UI 对照展示防视觉欺骗）。
#[derive(Debug, Clone)]
pub struct PolicyRequest<'a> {
    pub resource: PermissionResource,
    /// 资源动作（`read` / `write`；D9 矩阵按资源:动作生效）。
    pub action: &'a str,
    /// 原始 target（未规范化的用户可见原文）。
    pub target: Option<&'a str>,
    /// 写入内容大小（`fs.write` 记忆白名单 1MB 上限判定；未知为 `None`）。
    pub content_bytes: Option<u64>,
    /// 会话级权限模式（ADR-014 决策 3；按会话解析，缺省 [`PermissionMode::WorkspaceWrite`]）。
    pub permission_mode: PermissionMode,
}

impl<'a> PolicyRequest<'a> {
    /// 以缺省档（`workspace_write`）构造（既有调用点/测试的兼容入口）。
    pub fn new(resource: PermissionResource, action: &'a str, target: Option<&'a str>) -> Self {
        Self {
            resource,
            action,
            target,
            content_bytes: None,
            permission_mode: PermissionMode::default(),
        }
    }
}

/// 策略评估结果（审计/审批展示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyVerdict {
    pub decision: PolicyDecision,
    /// 原始 target（未规范化）。
    pub requested_target: Option<String>,
    /// canonical 结果（路径类资源且校验通过时为 `Some`）。
    pub canonical_target: Option<PathBuf>,
    /// 判定原因（稳定短语 + 细节）。
    pub reason: String,
}

impl PolicyVerdict {
    fn allow(reason: impl Into<String>, canonical_target: Option<PathBuf>) -> Self {
        Self {
            decision: PolicyDecision::Allow,
            requested_target: None,
            canonical_target,
            reason: reason.into(),
        }
    }

    fn deny(reason: impl Into<String>, canonical_target: Option<PathBuf>) -> Self {
        Self {
            decision: PolicyDecision::Deny,
            requested_target: None,
            canonical_target,
            reason: reason.into(),
        }
    }

    fn ask(reason: impl Into<String>, canonical_target: Option<PathBuf>) -> Self {
        Self {
            decision: PolicyDecision::Ask,
            requested_target: None,
            canonical_target,
            reason: reason.into(),
        }
    }
}

/// 策略引擎：档位三档矩阵 + 工作区路径守卫（绑定工作区根）。
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    guard: PathGuard,
    memory_files: Vec<String>,
}

impl PolicyEngine {
    /// 绑定工作区根（canonicalize 必须成功）。
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, PathGuardError> {
        Ok(Self {
            guard: PathGuard::new(workspace_root)?,
            memory_files: MEMORY_FILE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        })
    }

    /// 覆盖记忆文件白名单（测试/未来设置项；默认 D9 三文件）。
    pub fn with_memory_files(
        workspace_root: impl AsRef<Path>,
        memory_files: Vec<String>,
    ) -> Result<Self, PathGuardError> {
        Ok(Self {
            guard: PathGuard::new(workspace_root)?,
            memory_files,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        self.guard.root()
    }

    /// 工作区路径守卫（规则 target 的 canonicalize 与授权重建复用）。
    pub fn guard(&self) -> &PathGuard {
        &self.guard
    }

    /// 按 D9 三档矩阵评估一次工具调用请求。
    ///
    /// 未知资源由调用方在解析层拒绝（本方法不接收未知资源）。
    pub fn evaluate(&self, request: &PolicyRequest<'_>) -> PolicyVerdict {
        let mut verdict = match request.resource {
            PermissionResource::Exec => evaluate_shell(request, "exec"),
            PermissionResource::Net => evaluate_shell(request, "net"),
            PermissionResource::FsRead => self.evaluate_fs_read(request),
            PermissionResource::FsWrite => self.evaluate_fs_write(request),
        };
        verdict.requested_target = request.target.map(str::to_owned);
        verdict
    }

    fn evaluate_fs_read(&self, request: &PolicyRequest<'_>) -> PolicyVerdict {
        let Some(target) = request.target else {
            return PolicyVerdict::deny("fs.read 缺少 target（矩阵按路径校验，缺路径拒绝）", None);
        };
        match self.guard.resolve_any(target) {
            Ok(resolved) => {
                if resolved.inside {
                    PolicyVerdict::allow(
                        "fs.read：工作区内 allow（canonicalize 后前缀校验通过；三档一致）",
                        Some(resolved.canonical),
                    )
                } else if resolved.reparse_resolved {
                    PolicyVerdict::deny(
                        "fs.read：区外目标解析含软链接/Junction 重解析点 deny（ADR-014 不变量 3）",
                        Some(resolved.canonical),
                    )
                } else {
                    PolicyVerdict::allow(
                        "fs.read：工作区外 allow（三档一致；无重解析点）",
                        Some(resolved.canonical),
                    )
                }
            }
            Err(violation) => PolicyVerdict::deny(
                format!("fs.read：路径拒绝 deny（{}）", violation.code()),
                None,
            ),
        }
    }

    fn evaluate_fs_write(&self, request: &PolicyRequest<'_>) -> PolicyVerdict {
        // ① 硬边界：`read_only` 档写请求直接 deny（规则不可放宽、无升权路径）。
        if request.permission_mode == PermissionMode::ReadOnly {
            return PolicyVerdict::deny(
                "fs.write：read_only 档写请求 deny（档位硬边界，无升权路径）",
                None,
            );
        }
        let Some(target) = request.target else {
            return PolicyVerdict::deny("fs.write 缺少 target（矩阵按路径校验，缺路径拒绝）", None);
        };
        let resolved = match self.guard.resolve_any(target) {
            Ok(resolved) => resolved,
            Err(violation) => {
                return PolicyVerdict::deny(
                    format!("fs.write：路径拒绝 deny（{}）", violation.code()),
                    None,
                )
            }
        };
        if resolved.inside {
            if let Some(memory_file) = self.memory_file_match(&resolved.canonical) {
                let size = request.content_bytes.unwrap_or(0);
                if size > MEMORY_FILE_MAX_BYTES {
                    return PolicyVerdict::deny(
                        format!(
                            "fs.write：记忆文件 {memory_file} 超过 1MB 上限（{size} > {MEMORY_FILE_MAX_BYTES}）"
                        ),
                        Some(resolved.canonical),
                    );
                }
                return PolicyVerdict::allow(
                    format!("fs.write：记忆文件白名单 {memory_file}（≤1MB）allow"),
                    Some(resolved.canonical),
                );
            }
            return PolicyVerdict::allow(
                match request.permission_mode {
                    PermissionMode::WorkspaceWrite => {
                        "fs.write：工作区内 allow（缺省档免逐次审批；ADR-014 矩阵）"
                    }
                    _ => "fs.write：工作区内 allow（顶档；仍经网关 + 审计）",
                },
                Some(resolved.canonical),
            );
        }
        // 区外：先要求无重解析点（不变量 3 / 问题 12），再按档位分流。
        if resolved.reparse_resolved {
            return PolicyVerdict::deny(
                "fs.write：区外目标解析含软链接/Junction 重解析点 deny（ADR-014 不变量 3）",
                Some(resolved.canonical),
            );
        }
        match request.permission_mode {
            PermissionMode::DangerFullAccess => PolicyVerdict::allow(
                "fs.write：顶档区外写 allow（仍经网关 + 审计）",
                Some(resolved.canonical),
            ),
            _ => PolicyVerdict::ask(
                "fs.write：工作区外 ask（缺省档完整审批流 + 300s → deny + 审计）",
                Some(resolved.canonical),
            ),
        }
    }

    fn memory_file_match(&self, resolved: &Path) -> Option<String> {
        self.memory_files
            .iter()
            .find(|name| self.guard.file_name_eq(resolved, name))
            .cloned()
    }

    /// 路径违规 → 稳定错误码（审计字段；M2-10 回环复用）。
    pub const fn violation_code(violation: &PathViolation) -> &'static str {
        violation.code()
    }
}

fn evaluate_shell(request: &PolicyRequest<'_>, code: &str) -> PolicyVerdict {
    match request.permission_mode {
        PermissionMode::DangerFullAccess => PolicyVerdict::allow(
            format!("{code}：顶档 allow（仅门层判定；仍经网关 + 审计；PF-05 维持 P3）"),
            None,
        ),
        _ => PolicyVerdict::deny(
            format!("{code}：低档 deny（danger_full_access 档方放行；仅门层）"),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn engine() -> (PolicyEngine, std::path::PathBuf, std::path::PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "aether-m6-01-policy-{}-{unique}",
            std::process::id()
        ));
        let root = base.join("ws");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("file.txt"), b"x").unwrap();
        std::fs::write(base.join("outside.txt"), b"o").unwrap();
        // canonical 基准（规避 CI 临时目录的 8.3 短名别名；区外样本从 canonical 基准派生）。
        let base = std::fs::canonicalize(&base).unwrap();
        let root = base.join("ws");
        let engine = PolicyEngine::new(&root).unwrap();
        (engine, root, base.join("outside.txt"))
    }

    /// canonical 路径 → 目标字符串形态（剥离 `\\?\` 扩展前缀；该前缀属输入检查拒绝形态，
    /// 测试夹具不得以其构造 target）。
    fn plain(path: &std::path::Path) -> String {
        let text = path.to_string_lossy().to_string();
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    }

    fn request<'a>(
        resource: PermissionResource,
        action: &'a str,
        target: Option<&'a str>,
        mode: PermissionMode,
    ) -> PolicyRequest<'a> {
        PolicyRequest {
            resource,
            action,
            target,
            content_bytes: Some(10),
            permission_mode: mode,
        }
    }

    #[test]
    fn fs_read_inside_allow_all_modes() {
        let (engine, root, _) = engine();
        let inside = plain(&root.join("sub").join("file.txt"));
        for mode in PermissionMode::ALL {
            let verdict = engine.evaluate(&request(
                PermissionResource::FsRead,
                "read",
                Some(&inside),
                mode,
            ));
            assert_eq!(verdict.decision, PolicyDecision::Allow, "{mode:?}");
            assert!(verdict.canonical_target.is_some());
        }
        // 区内 `..` 收敛仍在根内 → allow。
        let converged = plain(&root.join("sub").join("..").join("sub").join("file.txt"));
        let verdict = engine.evaluate(&request(
            PermissionResource::FsRead,
            "read",
            Some(&converged),
            PermissionMode::WorkspaceWrite,
        ));
        assert_eq!(verdict.decision, PolicyDecision::Allow);
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn fs_read_outside_allow_all_modes_without_reparse() {
        let (engine, root, outside) = engine();
        let outside = plain(&outside);
        for mode in PermissionMode::ALL {
            let verdict = engine.evaluate(&request(
                PermissionResource::FsRead,
                "read",
                Some(&outside),
                mode,
            ));
            assert_eq!(verdict.decision, PolicyDecision::Allow, "{mode:?}");
            assert!(verdict.canonical_target.is_some());
        }
        // 尚不存在的区外写目标（词法=canonical，现有祖先可规范化）→ allow（读）。
        let ghost = plain(&root.parent().unwrap().join("ghost-dir").join("new.txt"));
        let verdict = engine.evaluate(&request(
            PermissionResource::FsRead,
            "read",
            Some(&ghost),
            PermissionMode::ReadOnly,
        ));
        assert_eq!(verdict.decision, PolicyDecision::Allow);
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn fs_read_outside_reparse_denied() {
        let (engine, root, _) = engine();
        let base = root.parent().unwrap();
        let link = base.join("link-outside");
        let created = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(base.join("outside.txt"), &link).is_ok()
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_file(base.join("outside.txt"), &link).is_ok()
            }
            #[cfg(not(any(unix, windows)))]
            {
                false
            }
        };
        if !created {
            // 无权限创建符号链接的环境（如未开启开发者模式的 Windows）：跳过动态样本，
            // 由集成测试（m2-03 动态夹具）覆盖。
            std::fs::remove_dir_all(base).ok();
            return;
        }
        let raw = plain(&link);
        let verdict = engine.evaluate(&request(
            PermissionResource::FsRead,
            "read",
            Some(&raw),
            PermissionMode::WorkspaceWrite,
        ));
        assert_eq!(
            verdict.decision,
            PolicyDecision::Deny,
            "区外重解析点必须 deny"
        );
        assert!(verdict.reason.contains("重解析点"));
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn fs_write_read_only_denies_all_targets() {
        let (engine, root, outside) = engine();
        let inside = plain(&root.join("new.txt"));
        let memory = plain(&root.join("AGENTS.md"));
        let outside = plain(&outside);
        for target in [inside.as_str(), memory.as_str(), outside.as_str()] {
            let verdict = engine.evaluate(&request(
                PermissionResource::FsWrite,
                "write",
                Some(target),
                PermissionMode::ReadOnly,
            ));
            assert_eq!(verdict.decision, PolicyDecision::Deny, "{target}");
            assert!(verdict.reason.contains("read_only"));
        }
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn fs_write_workspace_write_inside_allow_outside_ask() {
        let (engine, root, outside) = engine();
        let inside = plain(&root.join("sub").join("new.txt"));
        let verdict = engine.evaluate(&request(
            PermissionResource::FsWrite,
            "write",
            Some(&inside),
            PermissionMode::WorkspaceWrite,
        ));
        assert_eq!(
            verdict.decision,
            PolicyDecision::Allow,
            "缺省档区内写免审批"
        );

        let outside = plain(&outside);
        let verdict = engine.evaluate(&request(
            PermissionResource::FsWrite,
            "write",
            Some(&outside),
            PermissionMode::WorkspaceWrite,
        ));
        assert_eq!(verdict.decision, PolicyDecision::Ask, "缺省档区外写 ask");
        assert!(verdict.canonical_target.is_some());
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn fs_write_danger_allows_outside() {
        let (engine, root, outside) = engine();
        let outside = plain(&outside);
        let verdict = engine.evaluate(&request(
            PermissionResource::FsWrite,
            "write",
            Some(&outside),
            PermissionMode::DangerFullAccess,
        ));
        assert_eq!(verdict.decision, PolicyDecision::Allow);
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn memory_file_whitelist_limited_to_1mb_in_write_modes() {
        let (engine, root, _) = engine();
        for name in MEMORY_FILE_NAMES {
            let target = plain(&root.join(name));
            for mode in [
                PermissionMode::WorkspaceWrite,
                PermissionMode::DangerFullAccess,
            ] {
                let mut req = request(PermissionResource::FsWrite, "write", Some(&target), mode);
                req.content_bytes = Some(MEMORY_FILE_MAX_BYTES);
                assert_eq!(engine.evaluate(&req).decision, PolicyDecision::Allow);

                req.content_bytes = Some(MEMORY_FILE_MAX_BYTES + 1);
                assert_eq!(
                    engine.evaluate(&req).decision,
                    PolicyDecision::Deny,
                    "{name}"
                );
            }
        }
        // 非白名单 .md 在缺省档区内同样 allow（矩阵覆盖，不再是 ask）。
        let other = plain(&root.join("NOTES.md"));
        let verdict = engine.evaluate(&request(
            PermissionResource::FsWrite,
            "write",
            Some(&other),
            PermissionMode::WorkspaceWrite,
        ));
        assert_eq!(verdict.decision, PolicyDecision::Allow);
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn exec_and_net_follow_mode_boundary() {
        let (engine, root, _) = engine();
        for resource in [PermissionResource::Exec, PermissionResource::Net] {
            for mode in [PermissionMode::ReadOnly, PermissionMode::WorkspaceWrite] {
                let verdict = engine.evaluate(&request(resource, "invoke", Some("x"), mode));
                assert_eq!(
                    verdict.decision,
                    PolicyDecision::Deny,
                    "{resource:?} {mode:?}"
                );
            }
            let verdict = engine.evaluate(&request(
                resource,
                "invoke",
                Some("x"),
                PermissionMode::DangerFullAccess,
            ));
            assert_eq!(verdict.decision, PolicyDecision::Allow, "{resource:?} 顶档");
        }
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn missing_target_is_denied_for_fs_resources() {
        let (engine, root, _) = engine();
        for resource in [PermissionResource::FsRead, PermissionResource::FsWrite] {
            for mode in PermissionMode::ALL {
                let verdict = engine.evaluate(&request(resource, "read", None, mode));
                assert_eq!(
                    verdict.decision,
                    PolicyDecision::Deny,
                    "{resource:?} {mode:?}"
                );
            }
        }
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn resource_codes_match_d9_contract() {
        for (resource, code, action) in [
            (PermissionResource::FsRead, "fs.read", "read"),
            (PermissionResource::FsWrite, "fs.write", "write"),
            (PermissionResource::Exec, "exec", "exec"),
            (PermissionResource::Net, "net", "connect"),
        ] {
            assert_eq!(resource.as_str(), code);
            assert_eq!(resource.rule_action(), action);
            assert_eq!(PermissionResource::from_code(code), Some(resource));
        }
        assert_eq!(PermissionResource::from_code("fs.unknown"), None);
        assert!(PermissionResource::FsRead.is_fs());
        assert!(!PermissionResource::Exec.is_fs());
    }

    #[test]
    fn path_violation_samples_are_denied_for_reads() {
        // T7 文本样本（形态违规类）在 fs.read 下仍全部 deny（路径拒绝清单不变）；
        // 纯逃逸类（`..` 穿越）按 ADR-014 矩阵区外读 allow——单独断言。
        use super::super::path::{expand_t7_sample, T7_TEXTUAL_SAMPLES};
        let (engine, root, _) = engine();
        let root_text = plain(&root);
        let escape_only = [
            "parent_traversal",
            "parent_traversal_forward",
            "relative_traversal",
        ];
        for (label, template) in T7_TEXTUAL_SAMPLES {
            let raw = expand_t7_sample(template, &root_text);
            let verdict = engine.evaluate(&request(
                PermissionResource::FsRead,
                "read",
                Some(&raw),
                PermissionMode::WorkspaceWrite,
            ));
            if escape_only.contains(label) {
                assert_eq!(
                    verdict.decision,
                    PolicyDecision::Allow,
                    "样本 {label}（区外读 allow）"
                );
            } else {
                assert_eq!(
                    verdict.decision,
                    PolicyDecision::Deny,
                    "样本 {label} 必须 deny"
                );
            }
        }
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn denial_reason_carries_violation_code() {
        let (engine, root, _) = engine();
        let reserved = plain(&root.join("CON"));
        let verdict = engine.evaluate(&request(
            PermissionResource::FsRead,
            "read",
            Some(&reserved),
            PermissionMode::WorkspaceWrite,
        ));
        assert_eq!(verdict.decision, PolicyDecision::Deny);
        assert!(
            verdict.reason.contains("windows_special_path"),
            "拒绝原因需含稳定错误码: {}",
            verdict.reason
        );
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn custom_memory_files_and_accessors() {
        let root = std::env::temp_dir().join(format!("aether-m6-01-custom-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let engine = PolicyEngine::with_memory_files(&root, vec!["NOTES.md".to_owned()]).unwrap();
        assert_eq!(
            engine.workspace_root(),
            std::fs::canonicalize(&root).unwrap()
        );
        let target = root.join("NOTES.md").to_string_lossy().to_string();
        let verdict = engine.evaluate(&request(
            PermissionResource::FsWrite,
            "write",
            Some(&target),
            PermissionMode::WorkspaceWrite,
        ));
        assert_eq!(verdict.decision, PolicyDecision::Allow);
        assert_eq!(verdict.requested_target.as_deref(), Some(target.as_str()));
        assert!(verdict.canonical_target.is_some());

        // 违规码稳定映射。
        let violation = PathViolation::WindowsSpecialPath {
            pattern: "ads",
            detail: "x".to_owned(),
        };
        assert_eq!(
            PolicyEngine::violation_code(&violation),
            "windows_special_path"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
