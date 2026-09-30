//! 工作区记忆（M3-08；设计 D14「记忆：文件式记忆，无检索」）。
//!
//! 职责：
//! - **注入**：会话创建时按优先级 `AGENTS.md` > `AETHER.md` > `CLAUDE.md` 选择
//!   工作区内第一个可读记忆文件，注入上限 32KB（超出按 UTF-8 字符边界截断并附
//!   **显式标记**）；注入内容标记为「工作区约定」，优先级低于系统提示；
//! - **工具映射**：`memory.read` / `memory.append` / `memory.write` → D9 权限请求
//!   （`fs.read` / `fs.write`；记忆文件白名单 ≤1MB allow、工作区外 deny 由
//!   [`aether_security::PolicyEngine`] 判定，本模块不复制策略）。
//!
//! 分层（D14）：核心只负责注入组合与权限映射；实际文件读写由适配器侧工具执行
//! （经线协议上报 `tool.call_started/completed/failed`，权限经 `permission.request`
//! 回环，D9 边界不变）。
//!
//! 边界：本模块不做 I/O 缓存/文件监视（D14「文件变更由 `notify` 监视，仅对新会话
//! 生效」的自然满足方式 = 每次会话创建重新读取）；不写 `memories` 表（D14：无向量
//! 检索、无 `memories` 表使用）。

use std::path::{Path, PathBuf};

use aether_security::{
    PermissionResource, PolicyEngine, PolicyRequest, PolicyVerdict, MEMORY_FILE_MAX_BYTES,
    MEMORY_FILE_NAMES,
};

/// 记忆文件优先级（D14：`AGENTS.md` > `AETHER.md` > `CLAUDE.md`）。
pub const MEMORY_FILE_PRIORITY: [&str; 3] = MEMORY_FILE_NAMES;

/// 注入上限（D14：32KB；超出截断 + 显式标记）。
pub const MEMORY_INJECTION_MAX_BYTES: usize = 32 * 1024;

/// 注入内容的显式标记头（D14：「工作区约定」；优先级低于系统提示）。
pub const MEMORY_INJECTION_HEADER: &str = "# 工作区约定（记忆文件注入，优先级低于系统提示）";

/// 截断显式标记（D14：超出截断 + 显式标记）。
pub const MEMORY_TRUNCATION_MARKER: &str = "[aether] 记忆文件超出 32KB 注入上限，已截断";

/// 注入组合结果（会话创建时落 `sessions.system_prompt` 并经 `session.create` 下发）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryInjection {
    /// 命中的记忆文件名（未命中为 `None`）。
    pub file_name: Option<String>,
    /// 命中的记忆文件 canonical 路径（未命中为 `None`）。
    pub path: Option<PathBuf>,
    /// 注入正文（截断后；未命中为空串）。
    pub content: String,
    /// 是否发生截断。
    pub truncated: bool,
    /// 原始文件字节数（未命中为 0）。
    pub original_bytes: usize,
    /// 实际注入字节数（content 的字节数）。
    pub injected_bytes: usize,
    /// 读取失败诊断（候选文件存在但不可读时记录；不阻断会话创建）。
    pub read_error: Option<String>,
}

impl MemoryInjection {
    /// 是否为空注入（无记忆文件）。
    pub fn is_empty(&self) -> bool {
        self.file_name.is_none()
    }

    /// 渲染为注入文本（`None` = 空注入，不下发、不落 `system_prompt`）。
    ///
    /// 格式：显式标记头 + 文件名 + 正文；截断时正文尾部已含 [`MEMORY_TRUNCATION_MARKER`]。
    pub fn render(&self) -> Option<String> {
        let file_name = self.file_name.as_ref()?;
        Some(format!(
            "{MEMORY_INJECTION_HEADER}\n来源：{file_name}\n\n{}",
            self.content
        ))
    }
}

/// 按 D14 优先级组合工作区记忆注入。
///
/// - 依优先级取第一个**存在且可读**的候选文件；读取失败时记录诊断并继续下一候选；
/// - 读取上限 [`MEMORY_INJECTION_MAX_BYTES`] + 1 字节用于判定截断；
/// - 截断按 UTF-8 字符边界回退（不产生半个字符），并在尾部追加显式标记；
/// - 无候选文件 → [`MemoryInjection::is_empty`] 为 `true`。
pub fn compose_memory_injection(workspace_root: &Path) -> MemoryInjection {
    let mut last_error: Option<String> = None;
    for name in MEMORY_FILE_PRIORITY {
        let candidate = workspace_root.join(name);
        let metadata = match std::fs::metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(_) => continue, // 不存在（常见路径）→ 下一候选。
        };
        if !metadata.is_file() {
            last_error = Some(format!("{} 不是普通文件（跳过）", candidate.display()));
            continue;
        }
        let original_bytes = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let raw = match std::fs::read(&candidate) {
            Ok(raw) => raw,
            Err(error) => {
                last_error = Some(format!("读取 {} 失败：{error}", candidate.display()));
                continue;
            }
        };
        let truncated = raw.len() > MEMORY_INJECTION_MAX_BYTES;
        let content = if truncated {
            let boundary = utf8_boundary(&raw, MEMORY_INJECTION_MAX_BYTES);
            let head = String::from_utf8_lossy(&raw[..boundary]).into_owned();
            format!("{head}\n\n{MEMORY_TRUNCATION_MARKER}（原始 {original_bytes} 字节）")
        } else {
            String::from_utf8_lossy(&raw).into_owned()
        };
        let injected_bytes = content.len();
        return MemoryInjection {
            file_name: Some(name.to_owned()),
            path: Some(candidate),
            content,
            truncated,
            original_bytes,
            injected_bytes,
            read_error: last_error,
        };
    }
    MemoryInjection {
        read_error: last_error,
        ..MemoryInjection::default()
    }
}

/// 记忆工具（D14：`memory.read` / `memory.append` / `memory.write`）。
///
/// 映射契约（与适配器侧实现一致；本模块为权威定义）：
/// - `memory.read` → `fs.read`（工作区内 allow / 外 deny）；
/// - `memory.append` / `memory.write` → `fs.write`（记忆文件白名单 allow ≤1MB；
///   工作区内非白名单文件 ask；工作区外 deny）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTool {
    Read,
    Append,
    Write,
}

impl MemoryTool {
    /// 三个工具全集（DoD2 断言用）。
    pub const ALL: [Self; 3] = [Self::Read, Self::Append, Self::Write];

    /// 线协议工具名。
    pub const fn tool_name(self) -> &'static str {
        match self {
            Self::Read => "memory.read",
            Self::Append => "memory.append",
            Self::Write => "memory.write",
        }
    }

    /// 解析工具名（未知 → `None`）。
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.tool_name() == name)
    }

    /// D9 权限资源映射。
    pub const fn resource(self) -> PermissionResource {
        match self {
            Self::Read => PermissionResource::FsRead,
            Self::Append | Self::Write => PermissionResource::FsWrite,
        }
    }

    /// D9 权限动作映射（矩阵动作集 `read` / `write`；append 属写语义）。
    pub const fn action(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Append | Self::Write => "write",
        }
    }

    /// 是否追加语义（`memory.append`）。
    pub const fn is_append(self) -> bool {
        matches!(self, Self::Append)
    }
}

/// 以记忆工具语义评估一次权限请求（`fs.read` / `fs.write` → 策略矩阵）。
///
/// `content_bytes`：写入内容字节数（`memory.read` 传 `None`）；记忆白名单 1MB 上限
/// 由策略引擎判定（[`MEMORY_FILE_MAX_BYTES`]）。
pub fn evaluate_memory_tool(
    policy: &PolicyEngine,
    tool: MemoryTool,
    target: &str,
    content_bytes: Option<u64>,
) -> PolicyVerdict {
    policy.evaluate(&PolicyRequest {
        resource: tool.resource(),
        action: tool.action(),
        target: Some(target),
        content_bytes,
    })
}

/// 读取上限判定：写入内容是否超过 D9 记忆白名单 1MB（供适配器侧构造
/// `content_bytes` 诊断；策略判定仍由 [`evaluate_memory_tool`] 收口）。
pub fn memory_write_within_limit(content_bytes: u64) -> bool {
    content_bytes <= MEMORY_FILE_MAX_BYTES
}

/// 在 `limit` 处向前回退到 UTF-8 字符边界（`limit` 超长时返回全长）。
fn utf8_boundary(bytes: &[u8], limit: usize) -> usize {
    if bytes.len() <= limit {
        return bytes.len();
    }
    let mut boundary = limit;
    while boundary > 0 && !is_char_boundary(bytes, boundary) {
        boundary -= 1;
    }
    boundary
}

/// 字节串在 `index` 处是否为字符边界（与 `str::is_char_boundary` 同口径）。
fn is_char_boundary(bytes: &[u8], index: usize) -> bool {
    match bytes.get(index) {
        None => index == bytes.len(),
        Some(byte) => (*byte as i8) >= -0x40,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use aether_security::{expand_t7_sample, PolicyDecision, T7_TEXTUAL_SAMPLES};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aether-m3-08-memory-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn priority_prefers_agents_then_aether_then_claude() {
        let dir = scratch_dir();
        // 仅 CLAUDE.md。
        std::fs::write(dir.join("CLAUDE.md"), b"claude").unwrap();
        let injection = compose_memory_injection(&dir);
        assert_eq!(injection.file_name.as_deref(), Some("CLAUDE.md"));
        assert_eq!(injection.content, "claude");

        // 加 AETHER.md：优先级高于 CLAUDE.md。
        std::fs::write(dir.join("AETHER.md"), b"aether").unwrap();
        let injection = compose_memory_injection(&dir);
        assert_eq!(injection.file_name.as_deref(), Some("AETHER.md"));
        assert_eq!(injection.content, "aether");

        // 加 AGENTS.md：最高优先级。
        std::fs::write(dir.join("AGENTS.md"), b"agents").unwrap();
        let injection = compose_memory_injection(&dir);
        assert_eq!(injection.file_name.as_deref(), Some("AGENTS.md"));
        assert_eq!(injection.content, "agents");
        assert!(!injection.truncated);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_memory_file_is_empty_injection() {
        let dir = scratch_dir();
        let injection = compose_memory_injection(&dir);
        assert!(injection.is_empty());
        assert!(injection.render().is_none());
        assert_eq!(injection.injected_bytes, 0);
        assert_eq!(injection.original_bytes, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversize_is_truncated_on_utf8_boundary_with_explicit_marker() {
        let dir = scratch_dir();
        // 每个「啊」3 字节；写 20k 个 = 60000 字节 > 32KB；
        // 32257 位置切在字符中间 → 必须回退到 32256 的字符边界。
        let text = "啊".repeat(20_000);
        std::fs::write(dir.join("AGENTS.md"), text.as_bytes()).unwrap();
        let injection = compose_memory_injection(&dir);
        assert!(injection.truncated, "超出 32KB 必须显式截断");
        assert_eq!(injection.original_bytes, 60_000);
        assert!(
            injection.content.contains(MEMORY_TRUNCATION_MARKER),
            "截断必须带显式标记"
        );
        let head = injection
            .content
            .split(MEMORY_TRUNCATION_MARKER)
            .next()
            .unwrap()
            .trim_end();
        assert!(head.is_char_boundary(head.len()));
        assert_eq!(
            head.len(),
            32_766,
            "字节上限内最大 UTF-8 字符边界（10922×3）"
        );
        assert!(injection.injected_bytes <= MEMORY_INJECTION_MAX_BYTES + 200);

        // 恰好 32KB 不截断；32KB+1 截断。
        std::fs::write(
            dir.join("AGENTS.md"),
            vec![b'x'; MEMORY_INJECTION_MAX_BYTES],
        )
        .unwrap();
        let injection = compose_memory_injection(&dir);
        assert!(!injection.truncated);
        assert!(!injection.content.contains(MEMORY_TRUNCATION_MARKER));
        std::fs::write(
            dir.join("AGENTS.md"),
            vec![b'x'; MEMORY_INJECTION_MAX_BYTES + 1],
        )
        .unwrap();
        let injection = compose_memory_injection(&dir);
        assert!(injection.truncated);
        assert!(injection.content.contains(MEMORY_TRUNCATION_MARKER));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_candidate_is_skipped_with_diagnostic() {
        let dir = scratch_dir();
        std::fs::create_dir_all(dir.join("AGENTS.md")).unwrap();
        std::fs::write(dir.join("AETHER.md"), b"fallback").unwrap();
        let injection = compose_memory_injection(&dir);
        assert_eq!(injection.file_name.as_deref(), Some("AETHER.md"));
        assert!(injection.read_error.is_some(), "不可读候选需留诊断");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_marks_workspace_convention_and_source() {
        let dir = scratch_dir();
        std::fs::write(dir.join("AGENTS.md"), "约定内容".as_bytes()).unwrap();
        let rendered = compose_memory_injection(&dir).render().unwrap();
        assert!(rendered.contains(MEMORY_INJECTION_HEADER));
        assert!(rendered.contains("来源：AGENTS.md"));
        assert!(rendered.contains("约定内容"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_tools_map_to_d9_resources() {
        assert_eq!(MemoryTool::ALL.len(), 3);
        for (tool, name, resource, action) in [
            (
                MemoryTool::Read,
                "memory.read",
                PermissionResource::FsRead,
                "read",
            ),
            (
                MemoryTool::Append,
                "memory.append",
                PermissionResource::FsWrite,
                "write",
            ),
            (
                MemoryTool::Write,
                "memory.write",
                PermissionResource::FsWrite,
                "write",
            ),
        ] {
            assert_eq!(tool.tool_name(), name);
            assert_eq!(MemoryTool::parse(name), Some(tool));
            assert_eq!(tool.resource(), resource);
            assert_eq!(tool.action(), action);
        }
        assert_eq!(MemoryTool::parse("memory.delete"), None);
        assert!(MemoryTool::Append.is_append());
        assert!(!MemoryTool::Write.is_append());
    }

    #[test]
    fn memory_tool_policy_in_workspace_allow_outside_deny_and_1mb_limit() {
        let dir = scratch_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let policy = PolicyEngine::new(&dir).unwrap();
        let agents = dir.join("AGENTS.md").to_string_lossy().to_string();

        // 注入/读取：工作区内 allow。
        let verdict = evaluate_memory_tool(&policy, MemoryTool::Read, &agents, None);
        assert_eq!(verdict.decision, PolicyDecision::Allow, "memory.read allow");
        // 写/追加：记忆白名单 allow（≤1MB）。
        for tool in [MemoryTool::Append, MemoryTool::Write] {
            assert_eq!(
                evaluate_memory_tool(&policy, tool, &agents, Some(MEMORY_FILE_MAX_BYTES)).decision,
                PolicyDecision::Allow,
                "{} ≤1MB allow",
                tool.tool_name()
            );
            assert_eq!(
                evaluate_memory_tool(&policy, tool, &agents, Some(MEMORY_FILE_MAX_BYTES + 1))
                    .decision,
                PolicyDecision::Deny,
                "{} >1MB deny",
                tool.tool_name()
            );
        }

        // 工作区内非白名单文件：写走 ask（D9 矩阵）。
        let notes = dir.join("NOTES.md").to_string_lossy().to_string();
        assert_eq!(
            evaluate_memory_tool(&policy, MemoryTool::Write, &notes, Some(10)).decision,
            PolicyDecision::Ask
        );

        // 工作区外 deny（读/写一致）。
        let outside = std::env::temp_dir()
            .join("aether-m3-08-outside.md")
            .to_string_lossy()
            .to_string();
        for tool in MemoryTool::ALL {
            assert_eq!(
                evaluate_memory_tool(&policy, tool, &outside, Some(1)).decision,
                PolicyDecision::Deny,
                "{} 工作区外 deny",
                tool.tool_name()
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_tool_targets_reuse_t7_path_samples() {
        let dir = scratch_dir();
        let policy = PolicyEngine::new(&dir).unwrap();
        let root = dir.to_string_lossy().to_string();
        for (label, template) in T7_TEXTUAL_SAMPLES {
            let target = expand_t7_sample(template, &root);
            for tool in MemoryTool::ALL {
                let verdict = evaluate_memory_tool(&policy, tool, &target, Some(1));
                assert_eq!(
                    verdict.decision,
                    PolicyDecision::Deny,
                    "T7 样本 {label} 对 {} 必须 deny（target={target}）",
                    tool.tool_name()
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_write_limit_helper_matches_d9_constant() {
        assert!(memory_write_within_limit(MEMORY_FILE_MAX_BYTES));
        assert!(!memory_write_within_limit(MEMORY_FILE_MAX_BYTES + 1));
    }
}
