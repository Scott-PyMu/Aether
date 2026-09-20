//! 权限策略矩阵（M2-03；设计 D9「策略矩阵（MVP 生效值）」）。
//!
//! | 资源:动作 | 工作区内 | 工作区外 |
//! |---|---|---|
//! | `fs.read` | allow | deny |
//! | `fs.write` | ask | deny（记忆文件白名单例外：allow，1MB 上限） |
//! | `exec` | deny | deny |
//! | `net`（工具发起） | deny | deny |
//!
//! 边界（D9 评审修订 #1 / AGENTS §2.7）：本门仅约束适配器经线协议上报的工具调用；
//! 适配器进程内行为（含其自身执行的 shell 命令）不经此门，属信任级已知边界。

use std::path::{Path, PathBuf};

use super::path::{PathGuard, PathGuardError, PathViolation};

/// 记忆文件白名单（D9：对这 3 个文件名的写入 = allow）。
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

/// 策略引擎：固定矩阵 + 工作区路径守卫（绑定工作区根）。
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

    /// 按 D9 矩阵评估一次工具调用请求。
    ///
    /// 非文件资源（`exec` / `net`）恒 deny（MVP 不提供执行/网络通道）；
    /// 未知资源由调用方在解析层拒绝（本方法不接收未知资源）。
    pub fn evaluate(&self, request: &PolicyRequest<'_>) -> PolicyVerdict {
        let mut verdict = match request.resource {
            PermissionResource::Exec => {
                PolicyVerdict::deny("exec：MVP 固定矩阵 deny（无执行通道）", None)
            }
            PermissionResource::Net => {
                PolicyVerdict::deny("net：MVP 固定矩阵 deny（适配器自身出站不受此门限制）", None)
            }
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
        match self.guard.resolve(target) {
            Ok(resolved) => PolicyVerdict::allow(
                "fs.read：工作区内 allow（canonicalize 后前缀校验通过）",
                Some(resolved),
            ),
            Err(violation) => PolicyVerdict::deny(
                format!("fs.read：工作区外/非法路径 deny（{}）", violation.code()),
                None,
            ),
        }
    }

    fn evaluate_fs_write(&self, request: &PolicyRequest<'_>) -> PolicyVerdict {
        let Some(target) = request.target else {
            return PolicyVerdict::deny("fs.write 缺少 target（矩阵按路径校验，缺路径拒绝）", None);
        };
        let resolved = match self.guard.resolve(target) {
            Ok(resolved) => resolved,
            Err(violation) => {
                return PolicyVerdict::deny(
                    format!("fs.write：工作区外/非法路径 deny（{}）", violation.code()),
                    None,
                )
            }
        };
        if let Some(memory_file) = self.memory_file_match(&resolved) {
            let size = request.content_bytes.unwrap_or(0);
            if size > MEMORY_FILE_MAX_BYTES {
                return PolicyVerdict::deny(
                    format!(
                        "fs.write：记忆文件 {memory_file} 超过 1MB 上限（{size} > {MEMORY_FILE_MAX_BYTES}）"
                    ),
                    Some(resolved),
                );
            }
            return PolicyVerdict::allow(
                format!("fs.write：记忆文件白名单 {memory_file}（≤1MB）allow"),
                Some(resolved),
            );
        }
        PolicyVerdict::ask("fs.write：工作区内 ask（D9 审批流）", Some(resolved))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn engine() -> (PolicyEngine, std::path::PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "aether-m2-03-policy-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("file.txt"), b"x").unwrap();
        let engine = PolicyEngine::new(&root).unwrap();
        (engine, root)
    }

    #[test]
    fn fs_read_workspace_allow_outside_deny() {
        let (engine, root) = engine();
        let inside = root
            .join("sub")
            .join("file.txt")
            .to_string_lossy()
            .to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsRead,
            action: "read",
            target: Some(&inside),
            content_bytes: None,
        });
        assert_eq!(verdict.decision, PolicyDecision::Allow);
        assert!(verdict.canonical_target.is_some());

        let outside = std::env::temp_dir()
            .join("aether-outside.txt")
            .to_string_lossy()
            .to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsRead,
            action: "read",
            target: Some(&outside),
            content_bytes: None,
        });
        assert_eq!(verdict.decision, PolicyDecision::Deny);

        // `..` 逃逸（不依赖目标是否存在）。
        let escape = root
            .join("..")
            .join("escaped.txt")
            .to_string_lossy()
            .to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsRead,
            action: "read",
            target: Some(&escape),
            content_bytes: None,
        });
        assert_eq!(verdict.decision, PolicyDecision::Deny);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn fs_write_inside_ask_outside_deny() {
        let (engine, root) = engine();
        let inside = root
            .join("sub")
            .join("new.txt")
            .to_string_lossy()
            .to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsWrite,
            action: "write",
            target: Some(&inside),
            content_bytes: Some(10),
        });
        assert_eq!(verdict.decision, PolicyDecision::Ask);

        let outside = std::env::temp_dir()
            .join("aether-outside-write.txt")
            .to_string_lossy()
            .to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsWrite,
            action: "write",
            target: Some(&outside),
            content_bytes: Some(10),
        });
        assert_eq!(verdict.decision, PolicyDecision::Deny);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn memory_file_whitelist_allow_with_1mb_limit() {
        let (engine, root) = engine();
        for name in MEMORY_FILE_NAMES {
            let target = root.join(name).to_string_lossy().to_string();
            let verdict = engine.evaluate(&PolicyRequest {
                resource: PermissionResource::FsWrite,
                action: "write",
                target: Some(&target),
                content_bytes: Some(MEMORY_FILE_MAX_BYTES),
            });
            assert_eq!(verdict.decision, PolicyDecision::Allow, "{name} 应 allow");

            let verdict = engine.evaluate(&PolicyRequest {
                resource: PermissionResource::FsWrite,
                action: "write",
                target: Some(&target),
                content_bytes: Some(MEMORY_FILE_MAX_BYTES + 1),
            });
            assert_eq!(verdict.decision, PolicyDecision::Deny, "{name} 超限应 deny");
        }
        // 非白名单的 .md 仍走 ask。
        let other = root.join("NOTES.md").to_string_lossy().to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsWrite,
            action: "write",
            target: Some(&other),
            content_bytes: Some(10),
        });
        assert_eq!(verdict.decision, PolicyDecision::Ask);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn exec_and_net_are_always_denied() {
        let (engine, root) = engine();
        for resource in [PermissionResource::Exec, PermissionResource::Net] {
            let verdict = engine.evaluate(&PolicyRequest {
                resource,
                action: "invoke",
                target: Some("anything"),
                content_bytes: None,
            });
            assert_eq!(verdict.decision, PolicyDecision::Deny, "{resource:?}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_target_is_denied_for_fs_resources() {
        let (engine, root) = engine();
        for resource in [PermissionResource::FsRead, PermissionResource::FsWrite] {
            let verdict = engine.evaluate(&PolicyRequest {
                resource,
                action: "read",
                target: None,
                content_bytes: None,
            });
            assert_eq!(verdict.decision, PolicyDecision::Deny, "{resource:?}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resource_codes_match_d9_contract() {
        for (resource, code) in [
            (PermissionResource::FsRead, "fs.read"),
            (PermissionResource::FsWrite, "fs.write"),
            (PermissionResource::Exec, "exec"),
            (PermissionResource::Net, "net"),
        ] {
            assert_eq!(resource.as_str(), code);
            assert_eq!(PermissionResource::from_code(code), Some(resource));
        }
        assert_eq!(PermissionResource::from_code("fs.unknown"), None);
        assert!(PermissionResource::FsRead.is_fs());
        assert!(!PermissionResource::Exec.is_fs());
    }

    #[test]
    fn denial_reason_carries_violation_code() {
        let (engine, root) = engine();
        let escape = root
            .join("..")
            .join("escaped.txt")
            .to_string_lossy()
            .to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsRead,
            action: "read",
            target: Some(&escape),
            content_bytes: None,
        });
        assert!(
            verdict.reason.contains("path_escape"),
            "拒绝原因需含稳定错误码: {}",
            verdict.reason
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn custom_memory_files_and_accessors() {
        let root = std::env::temp_dir().join(format!("aether-m2-03-custom-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let engine = PolicyEngine::with_memory_files(&root, vec!["NOTES.md".to_owned()]).unwrap();
        assert_eq!(
            engine.workspace_root(),
            std::fs::canonicalize(&root).unwrap()
        );
        let target = root.join("NOTES.md").to_string_lossy().to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsWrite,
            action: "write",
            target: Some(&target),
            content_bytes: Some(1),
        });
        assert_eq!(verdict.decision, PolicyDecision::Allow);
        assert_eq!(verdict.requested_target.as_deref(), Some(target.as_str()));
        assert!(verdict.canonical_target.is_some());
        // 自定义白名单外的记忆文件名 → ask。
        let other = root.join("AGENTS.md").to_string_lossy().to_string();
        let verdict = engine.evaluate(&PolicyRequest {
            resource: PermissionResource::FsWrite,
            action: "write",
            target: Some(&other),
            content_bytes: Some(1),
        });
        assert_eq!(verdict.decision, PolicyDecision::Ask);

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
