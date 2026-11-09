//! M10-01/ADR-031：版本化工作流定义（DSL）校验。
//!
//! 白名单模式：固定 5 类节点、纯函数表达式、无任意脚本/shell/HTTP/eval。
//! 非法定义一次返回全部可发现错误（上限 50，超出 `truncated=true`），且不落库。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// 定义 schema 版本（固定 `1`）。
pub const SCHEMA_VERSION: i64 = 1;
/// 定义序列化后上限（256 KiB）。
pub const MAX_DEFINITION_BYTES: usize = 262_144;
/// 节点数上限。
pub const MAX_NODES: usize = 64;
/// DAG 深度上限。
pub const MAX_DEPTH: usize = 32;
/// 单节点 fan-out 上限。
pub const MAX_FANOUT: usize = 16;
/// 单次运行最大节点执行次数（含重试/展开）。
pub const MAX_NODE_EXECUTIONS: usize = 256;
/// `name` 上限（字符）。
pub const MAX_NAME_CHARS: usize = 128;
/// `description` 上限（字符）。
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// 节点 `prompt` 上限（字节）。
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// 节点输出上限（字节；超出截断并标记 `truncated=true`）。
pub const MAX_RESULT_BYTES: usize = 64 * 1024;
/// 节点超时下限（毫秒）。
pub const MIN_NODE_TIMEOUT_MS: i64 = 1_000;
/// 节点超时上限（毫秒，24h）。
pub const MAX_NODE_TIMEOUT_MS: i64 = 86_400_000;
/// 运行超时上限（毫秒，7 天）。
pub const MAX_RUN_TIMEOUT_MS: i64 = 604_800_000;
/// 缺省节点超时（30min）。
pub const DEFAULT_NODE_TIMEOUT_MS: i64 = 1_800_000;
/// 重试最大次数上限。
pub const MAX_ATTEMPTS: u32 = 5;
/// 一份定义最多返回的错误数。
pub const MAX_ISSUES: usize = 50;

/// 结构化定义错误项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionIssue {
    /// JSON Pointer（例如 `/nodes/0/id`）。
    pub path: String,
    /// 人类可读原因。
    pub reason: String,
    /// 稳定短码（`schema_version` / `duplicate_node_id` / ...）。
    pub code: String,
}

/// 结构化定义错误（一次返回全部可发现项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionError {
    pub issues: Vec<DefinitionIssue>,
    pub truncated: bool,
}

impl DefinitionError {
    fn from_issue(path: impl Into<String>, code: &str, reason: impl Into<String>) -> Self {
        Self {
            issues: vec![DefinitionIssue {
                path: path.into(),
                reason: reason.into(),
                code: code.to_owned(),
            }],
            truncated: false,
        }
    }

    pub fn message(&self) -> String {
        let first = self
            .issues
            .first()
            .map(|issue| format!("{}: {}", issue.path, issue.reason))
            .unwrap_or_else(|| "工作流定义非法".to_owned());
        if self.issues.len() > 1 {
            format!("工作流定义非法（{} 项）：{first}", self.issues.len())
        } else {
            format!("工作流定义非法：{first}")
        }
    }
}

/// 工作流定义（版本化 JSON；严格字段）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub schema_version: i64,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
    #[serde(default)]
    pub settings: WorkflowSettings,
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// 节点类型（固定 5 类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    Agent,
    Task,
    Condition,
    Parallel,
    Join,
}

impl NodeType {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "agent" => Some(Self::Agent),
            "task" => Some(Self::Task),
            "condition" => Some(Self::Condition),
            "parallel" => Some(Self::Parallel),
            "join" => Some(Self::Join),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Task => "task",
            Self::Condition => "condition",
            Self::Parallel => "parallel",
            Self::Join => "join",
        }
    }
}

/// 工作流节点（所有类型共用结构；按 `type` 做字段校验）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub context_refs: Option<Vec<ContextRef>>,
    #[serde(default)]
    pub runtime_id: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<i64>,
    #[serde(default)]
    pub retry: Option<RetryPolicy>,
    #[serde(default)]
    pub handler: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub expression: Option<Value>,
    #[serde(default)]
    pub fanout: Option<Value>,
    #[serde(default)]
    pub max_parallel: Option<u32>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub quorum: Option<u32>,
    #[serde(default)]
    pub on_failure: Option<String>,
}

/// 显式上下文引用（agent 节点；只传引用，不继承父消息全文）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRef {
    pub kind: String,
    pub path: String,
}

/// 节点重试策略。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub multiplier: f64,
}

/// 边（`when` 为假时目标节点跳过）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEdge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub when: Option<Value>,
    #[serde(default)]
    pub label: Option<String>,
}

/// 运行设置。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSettings {
    #[serde(default)]
    pub fail_policy: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<i64>,
    #[serde(default)]
    pub max_parallel: Option<u32>,
    #[serde(default)]
    pub context_allowlist: Option<Vec<String>>,
}

impl WorkflowDefinition {
    /// 运行失败策略（缺省 `fail_fast`）。
    pub fn fail_fast(&self) -> bool {
        !matches!(self.settings.fail_policy.as_deref(), Some("continue"))
    }

    /// 运行超时（缺省无运行级超时，仅节点级）。
    pub fn run_timeout_ms(&self) -> Option<i64> {
        self.settings.timeout_ms
    }

    /// 每运行并行度（缺省 4；上限 16）。
    pub fn max_parallel(&self) -> usize {
        self.settings
            .max_parallel
            .map(|value| value.clamp(1, MAX_FANOUT as u32) as usize)
            .unwrap_or(4)
    }

    /// 上下文白名单（缺省空 = 不注入运行级上下文）。
    pub fn context_allowlist(&self) -> &[String] {
        self.settings.context_allowlist.as_deref().unwrap_or(&[])
    }

    pub fn node(&self, id: &str) -> Option<&WorkflowNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn node_type(&self, id: &str) -> Option<NodeType> {
        self.node(id)
            .and_then(|node| NodeType::parse(&node.node_type))
    }

    pub fn incoming(&self, id: &str) -> Vec<&WorkflowEdge> {
        self.edges.iter().filter(|edge| edge.to == id).collect()
    }

    pub fn outgoing(&self, id: &str) -> Vec<&WorkflowEdge> {
        self.edges.iter().filter(|edge| edge.from == id).collect()
    }

    /// 入口节点（无入边）。
    pub fn entry_nodes(&self) -> Vec<&WorkflowNode> {
        self.nodes
            .iter()
            .filter(|node| self.incoming(&node.id).is_empty())
            .collect()
    }
}

/// 校验并解析定义（大小/schema/结构/图/上限/表达式）。
pub fn parse_and_validate(value: &Value) -> Result<WorkflowDefinition, DefinitionError> {
    let serialized = serde_json::to_vec(value).map_err(|error| {
        DefinitionError::from_issue("/", "serialization", format!("定义无法序列化：{error}"))
    })?;
    if serialized.len() > MAX_DEFINITION_BYTES {
        return Err(DefinitionError::from_issue(
            "/",
            "limit_exceeded",
            format!(
                "定义大小 {} 字节超过上限 {MAX_DEFINITION_BYTES}",
                serialized.len()
            ),
        ));
    }
    let definition: WorkflowDefinition =
        serde_json::from_value(value.clone()).map_err(|error| {
            DefinitionError::from_issue("/", "invalid_shape", format!("定义结构非法：{error}"))
        })?;
    validate(&definition)?;
    Ok(definition)
}

/// 语义校验（假定结构已反序列化成功）。
pub fn validate(definition: &WorkflowDefinition) -> Result<(), DefinitionError> {
    let mut issues = Vec::new();
    let mut push = |path: String, code: &str, reason: String| {
        if issues.len() < MAX_ISSUES {
            issues.push(DefinitionIssue {
                path,
                reason,
                code: code.to_owned(),
            });
        }
    };

    if definition.schema_version != SCHEMA_VERSION {
        push(
            "/schema_version".to_owned(),
            "schema_version",
            format!("schema_version 必须为 {SCHEMA_VERSION}"),
        );
    }
    if definition.name.is_empty() || definition.name.chars().count() > MAX_NAME_CHARS {
        push(
            "/name".to_owned(),
            "limit_exceeded",
            format!("name 必须为 1..={MAX_NAME_CHARS} 字符"),
        );
    }
    if let Some(description) = &definition.description {
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            push(
                "/description".to_owned(),
                "limit_exceeded",
                format!("description 不得超过 {MAX_DESCRIPTION_CHARS} 字符"),
            );
        }
    }
    if definition.nodes.is_empty() {
        push(
            "/nodes".to_owned(),
            "invalid_value",
            "至少需要一个节点".to_owned(),
        );
    }
    if definition.nodes.len() > MAX_NODES {
        push(
            "/nodes".to_owned(),
            "limit_exceeded",
            format!("节点数 {} 超过上限 {MAX_NODES}", definition.nodes.len()),
        );
    }

    let mut ids = HashSet::new();
    for (index, node) in definition.nodes.iter().enumerate() {
        let path = format!("/nodes/{index}");
        if !valid_node_id(&node.id) {
            push(
                format!("{path}/id"),
                "invalid_node_id",
                "节点 id 必须匹配 ^[a-zA-Z][a-zA-Z0-9_-]{0,63}$".to_owned(),
            );
        }
        if !ids.insert(node.id.clone()) {
            push(
                format!("{path}/id"),
                "duplicate_node_id",
                format!("节点 id 重复：{}", node.id),
            );
        }
        let Some(node_type) = NodeType::parse(&node.node_type) else {
            push(
                format!("{path}/type"),
                "unknown_node_type",
                format!("未知节点类型：{}", node.node_type),
            );
            continue;
        };
        validate_node_fields(node, node_type, path, &mut push);
    }

    let mut edge_keys = BTreeSet::new();
    let mut outgoing: HashMap<&str, usize> = HashMap::new();
    let mut incoming: HashMap<&str, usize> = HashMap::new();
    for (index, edge) in definition.edges.iter().enumerate() {
        let path = format!("/edges/{index}");
        if !ids.contains(&edge.from) {
            push(
                format!("{path}/from"),
                "dangling_edge",
                format!("边起点不存在：{}", edge.from),
            );
        }
        if !ids.contains(&edge.to) {
            push(
                format!("{path}/to"),
                "dangling_edge",
                format!("边终点不存在：{}", edge.to),
            );
        }
        if edge.from == edge.to {
            push(path.clone(), "cycle", "不允许自环".to_owned());
        }
        let key = (edge.from.clone(), edge.to.clone());
        if !edge_keys.insert(key) {
            push(
                path.clone(),
                "duplicate_edge",
                format!("重复边：{} → {}", edge.from, edge.to),
            );
        }
        *outgoing.entry(edge.from.as_str()).or_default() += 1;
        *incoming.entry(edge.to.as_str()).or_default() += 1;
        if let Some(when) = &edge.when {
            validate_expression(when, &format!("{path}/when"), &mut push);
        }
    }
    for node in &definition.nodes {
        let out = outgoing.get(node.id.as_str()).copied().unwrap_or(0);
        if out > MAX_FANOUT {
            push(
                format!("/nodes/{}", node.id),
                "limit_exceeded",
                format!("单节点 fan-out {out} 超过上限 {MAX_FANOUT}"),
            );
        }
        if node.node_type == "parallel" {
            if out == 0 {
                push(
                    format!("/nodes/{}", node.id),
                    "invalid_value",
                    "parallel 节点至少需要一条出边".to_owned(),
                );
            }
            match &node.fanout {
                Some(Value::Number(number)) => {
                    let value = number.as_u64().unwrap_or(0);
                    if value == 0 || value > MAX_FANOUT as u64 {
                        push(
                            format!("/nodes/{}/fanout", node.id),
                            "limit_exceeded",
                            format!("fanout 必须为 1..={MAX_FANOUT}"),
                        );
                    } else if value as usize != out {
                        push(
                            format!("/nodes/{}/fanout", node.id),
                            "invalid_value",
                            format!("fanout={value} 必须等于出边数 {out}"),
                        );
                    }
                }
                Some(Value::Array(items)) => {
                    if items.is_empty() || items.len() > MAX_FANOUT {
                        push(
                            format!("/nodes/{}/fanout", node.id),
                            "limit_exceeded",
                            format!("fanout 引用数组必须为 1..={MAX_FANOUT} 项"),
                        );
                    } else if items.len() != out {
                        push(
                            format!("/nodes/{}/fanout", node.id),
                            "invalid_value",
                            format!("fanout 数组长度 {} 必须等于出边数 {out}", items.len()),
                        );
                    }
                }
                Some(_) => push(
                    format!("/nodes/{}/fanout", node.id),
                    "invalid_type",
                    "fanout 必须是整数或引用数组".to_owned(),
                ),
                None => push(
                    format!("/nodes/{}/fanout", node.id),
                    "missing_field",
                    "parallel 节点缺少 fanout".to_owned(),
                ),
            }
        }
        if node.node_type == "join" {
            let incoming_count = incoming.get(node.id.as_str()).copied().unwrap_or(0);
            if incoming_count == 0 {
                push(
                    format!("/nodes/{}", node.id),
                    "invalid_value",
                    "join 节点至少需要一条入边".to_owned(),
                );
            }
            match node.mode.as_deref() {
                Some("quorum") => match node.quorum {
                    Some(quorum) if quorum >= 1 && quorum as usize <= incoming_count => {}
                    _ => push(
                        format!("/nodes/{}/quorum", node.id),
                        "invalid_value",
                        format!("quorum 必须为 1..={incoming_count}"),
                    ),
                },
                Some("all") | Some("any") => {
                    if node.quorum.is_some() {
                        push(
                            format!("/nodes/{}/quorum", node.id),
                            "invalid_value",
                            "quorum 仅允许 mode=quorum".to_owned(),
                        );
                    }
                }
                Some(other) => push(
                    format!("/nodes/{}/mode", node.id),
                    "invalid_enum",
                    format!("join mode 非法：{other}"),
                ),
                None => push(
                    format!("/nodes/{}/mode", node.id),
                    "missing_field",
                    "join 节点缺少 mode".to_owned(),
                ),
            }
            if let Some(on_failure) = node.on_failure.as_deref() {
                if !matches!(on_failure, "fail" | "continue" | "skip") {
                    push(
                        format!("/nodes/{}/on_failure", node.id),
                        "invalid_enum",
                        format!("on_failure 非法：{on_failure}"),
                    );
                }
            }
        }
    }

    // DAG：Kahn 拓扑 + 深度。
    let mut indegree: BTreeMap<&str, usize> = definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), 0usize))
        .collect();
    for edge in &definition.edges {
        if let Some(value) = indegree.get_mut(edge.to.as_str()) {
            *value += 1;
        }
    }
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0usize;
    let mut depth: HashMap<&str, usize> = definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), 0usize))
        .collect();
    while let Some(id) = queue.pop() {
        visited += 1;
        let current_depth = depth.get(id).copied().unwrap_or(0);
        if current_depth > MAX_DEPTH {
            push(
                format!("/nodes/{id}"),
                "limit_exceeded",
                format!("DAG 深度 {} 超过上限 {MAX_DEPTH}", current_depth),
            );
        }
        for edge in definition.edges.iter().filter(|edge| edge.from == id) {
            let target_depth = current_depth.saturating_add(1);
            let entry = depth.entry(edge.to.as_str()).or_insert(0);
            if target_depth > *entry {
                *entry = target_depth;
            }
            if let Some(value) = indegree.get_mut(edge.to.as_str()) {
                *value = value.saturating_sub(1);
                if *value == 0 {
                    queue.push(edge.to.as_str());
                }
            }
        }
    }
    if visited != definition.nodes.len() {
        push(
            "/edges".to_owned(),
            "cycle",
            "定义包含环（DAG 必须无环）".to_owned(),
        );
    }

    for (index, node) in definition.nodes.iter().enumerate() {
        if !reachable(&definition.edges, &node.id) {
            push(
                format!("/nodes/{index}"),
                "unreachable",
                format!("节点不可从入口到达：{}", node.id),
            );
        }
    }

    if let Some(timeout_ms) = definition.settings.timeout_ms {
        if !(MIN_NODE_TIMEOUT_MS..=MAX_RUN_TIMEOUT_MS).contains(&timeout_ms) {
            push(
                "/settings/timeout_ms".to_owned(),
                "out_of_range",
                format!("运行超时必须在 {MIN_NODE_TIMEOUT_MS}..={MAX_RUN_TIMEOUT_MS} ms"),
            );
        }
    }
    if let Some(max_parallel) = definition.settings.max_parallel {
        if !(1..=MAX_FANOUT as u32).contains(&max_parallel) {
            push(
                "/settings/max_parallel".to_owned(),
                "out_of_range",
                format!("max_parallel 必须为 1..={MAX_FANOUT}"),
            );
        }
    }
    if let Some(policy) = definition.settings.fail_policy.as_deref() {
        if !matches!(policy, "fail_fast" | "continue") {
            push(
                "/settings/fail_policy".to_owned(),
                "invalid_enum",
                format!("fail_policy 非法：{policy}"),
            );
        }
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(DefinitionError {
            truncated: issues.len() >= MAX_ISSUES,
            issues,
        })
    }
}

fn validate_node_fields(
    node: &WorkflowNode,
    node_type: NodeType,
    path: String,
    push: &mut impl FnMut(String, &str, String),
) {
    if let Some(timeout_ms) = node.timeout_ms {
        if !(MIN_NODE_TIMEOUT_MS..=MAX_NODE_TIMEOUT_MS).contains(&timeout_ms) {
            push(
                format!("{path}/timeout_ms"),
                "out_of_range",
                format!("节点超时必须在 {MIN_NODE_TIMEOUT_MS}..={MAX_NODE_TIMEOUT_MS} ms"),
            );
        }
    }
    if let Some(retry) = &node.retry {
        if !(1..=MAX_ATTEMPTS).contains(&retry.max_attempts) {
            push(
                format!("{path}/retry/max_attempts"),
                "out_of_range",
                format!("max_attempts 必须为 1..={MAX_ATTEMPTS}"),
            );
        }
        if !(100..=60_000).contains(&retry.backoff_ms) {
            push(
                format!("{path}/retry/backoff_ms"),
                "out_of_range",
                "backoff_ms 必须为 100..=60000".to_owned(),
            );
        }
        if !(1.0..=4.0).contains(&retry.multiplier) {
            push(
                format!("{path}/retry/multiplier"),
                "out_of_range",
                "multiplier 必须为 1..=4".to_owned(),
            );
        }
    }
    match node_type {
        NodeType::Agent => {
            match node.prompt.as_deref() {
                Some(prompt) if !prompt.is_empty() => {
                    if prompt.len() > MAX_PROMPT_BYTES {
                        push(
                            format!("{path}/prompt"),
                            "limit_exceeded",
                            format!("prompt 不得超过 {MAX_PROMPT_BYTES} 字节"),
                        );
                    }
                }
                _ => push(
                    format!("{path}/prompt"),
                    "missing_field",
                    "agent 节点缺少 prompt".to_owned(),
                ),
            }
            if let Some(refs) = &node.context_refs {
                if refs.len() > 32 {
                    push(
                        format!("{path}/context_refs"),
                        "limit_exceeded",
                        "context_refs 不得超过 32 条".to_owned(),
                    );
                }
                for (index, reference) in refs.iter().enumerate() {
                    if !matches!(reference.kind.as_str(), "file" | "directory" | "session") {
                        push(
                            format!("{path}/context_refs/{index}/kind"),
                            "invalid_enum",
                            format!("context_ref kind 非法：{}", reference.kind),
                        );
                    }
                    if reference.path.is_empty() {
                        push(
                            format!("{path}/context_refs/{index}/path"),
                            "invalid_value",
                            "context_ref path 不能为空".to_owned(),
                        );
                    }
                }
            }
            if let Some(mode) = node.permission_mode.as_deref() {
                if aether_core::PermissionMode::from_code(mode).is_none() {
                    push(
                        format!("{path}/permission_mode"),
                        "invalid_enum",
                        format!("permission_mode 非法：{mode}"),
                    );
                }
            }
        }
        NodeType::Task => {
            let handler = node.handler.as_deref();
            if !matches!(
                handler,
                Some("noop" | "set_context" | "merge_results" | "emit_summary")
            ) {
                push(
                    format!("{path}/handler"),
                    "invalid_enum",
                    format!(
                        "task handler 非法：{}；允许 noop/set_context/merge_results/emit_summary",
                        handler.unwrap_or("<missing>")
                    ),
                );
            }
            if let Some(input) = &node.input {
                if serde_json::to_vec(input)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0)
                    > MAX_RESULT_BYTES
                {
                    push(
                        format!("{path}/input"),
                        "limit_exceeded",
                        format!("task input 不得超过 {MAX_RESULT_BYTES} 字节"),
                    );
                }
            }
        }
        NodeType::Condition => match &node.expression {
            Some(expression) => {
                validate_expression(expression, &format!("{path}/expression"), push)
            }
            None => push(
                format!("{path}/expression"),
                "missing_field",
                "condition 节点缺少 expression".to_owned(),
            ),
        },
        NodeType::Parallel => {
            if let Some(max_parallel) = node.max_parallel {
                if !(1..=MAX_FANOUT as u32).contains(&max_parallel) {
                    push(
                        format!("{path}/max_parallel"),
                        "out_of_range",
                        format!("max_parallel 必须为 1..={MAX_FANOUT}"),
                    );
                }
            }
        }
        NodeType::Join => {}
    }
}

fn valid_node_id(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    if value.len() > 64 {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// 入口可达性：从所有无入边节点出发。
fn reachable(edges: &[WorkflowEdge], target: &str) -> bool {
    let mut has_incoming: HashSet<&str> = HashSet::new();
    for edge in edges {
        has_incoming.insert(edge.to.as_str());
    }
    let entries: Vec<&str> = edges
        .iter()
        .map(|edge| edge.from.as_str())
        .filter(|id| !has_incoming.contains(id))
        .collect();
    if entries.is_empty() {
        // 单节点定义（无任何边）：入口即自身。
        return !has_incoming.contains(target);
    }
    let mut visited: HashSet<&str> = entries.into_iter().collect();
    let mut stack: Vec<&str> = visited.iter().copied().collect();
    while let Some(id) = stack.pop() {
        for edge in edges.iter().filter(|edge| edge.from == id) {
            if visited.insert(edge.to.as_str()) {
                stack.push(edge.to.as_str());
            }
        }
    }
    visited.contains(target)
}

/// 表达式 AST（白名单纯函数；深度 ≤16、节点数 ≤256）。
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Literal(Value),
    Ref(String),
    Op { op: String, args: Vec<Expression> },
}

/// 解析表达式（结构错误返回稳定短码）。
pub fn parse_expression(value: &Value) -> Result<Expression, String> {
    let mut count = 0usize;
    parse_expression_inner(value, 0, &mut count)
}

fn parse_expression_inner(
    value: &Value,
    depth: usize,
    count: &mut usize,
) -> Result<Expression, String> {
    if depth > 16 {
        return Err("表达式深度超过 16".to_owned());
    }
    *count += 1;
    if *count > 256 {
        return Err("表达式节点数超过 256".to_owned());
    }
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("ref") {
                if object.len() != 1 {
                    return Err("ref 叶子不得包含其他字段".to_owned());
                }
                let Some(path) = reference.as_str() else {
                    return Err("ref 必须是字符串".to_owned());
                };
                if !valid_ref(path) {
                    return Err(format!("引用路径非法：{path}"));
                }
                return Ok(Expression::Ref(path.to_owned()));
            }
            let Some(op) = object.get("op").and_then(Value::as_str) else {
                return Err("表达式缺少 op".to_owned());
            };
            if object.len() != 2 {
                return Err("表达式只允许 op/args 两个字段".to_owned());
            }
            if !matches!(
                op,
                "eq" | "ne"
                    | "gt"
                    | "gte"
                    | "lt"
                    | "lte"
                    | "contains"
                    | "exists"
                    | "and"
                    | "or"
                    | "not"
            ) {
                return Err(format!("表达式 op 非法：{op}"));
            }
            let Some(args) = object.get("args").and_then(Value::as_array) else {
                return Err("表达式缺少 args 数组".to_owned());
            };
            let expected = match op {
                "not" => (1, 1),
                "and" | "or" => (1, 64),
                "exists" => (1, 1),
                _ => (2, 2),
            };
            if args.len() < expected.0 || args.len() > expected.1 {
                return Err(format!("op={op} 的 args 数量非法（{}）", args.len()));
            }
            let mut parsed = Vec::with_capacity(args.len());
            for arg in args {
                parsed.push(parse_expression_inner(arg, depth + 1, count)?);
            }
            Ok(Expression::Op {
                op: op.to_owned(),
                args: parsed,
            })
        }
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null | Value::Array(_) => {
            Ok(Expression::Literal(value.clone()))
        }
    }
}

fn valid_ref(value: &str) -> bool {
    let Some((root, rest)) = value.split_once('.') else {
        return false;
    };
    if !matches!(root, "context" | "input" | "results") || rest.is_empty() {
        return false;
    }
    rest.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    })
}

fn validate_expression(value: &Value, path: &str, push: &mut impl FnMut(String, &str, String)) {
    if let Err(reason) = parse_expression(value) {
        push(path.to_owned(), "invalid_expression", reason);
    }
}

/// 表达式求值环境。
pub struct EvalContext<'a> {
    pub input: &'a Value,
    pub context: &'a Value,
    pub results: &'a BTreeMap<String, Value>,
}

/// 纯函数求值（返回 JSON 标量/数组；`and/or/not` 返回布尔）。
pub fn eval_expression(expression: &Expression, env: &EvalContext<'_>) -> Result<Value, String> {
    match expression {
        Expression::Literal(value) => Ok(value.clone()),
        Expression::Ref(path) => {
            let mut segments = path.split('.');
            let root = segments.next().unwrap_or_default();
            let mut current = match root {
                "input" => env.input,
                "context" => env.context,
                "results" => {
                    let Some(node_id) = segments.next() else {
                        return Err(format!("引用路径非法：{path}"));
                    };
                    match env.results.get(node_id) {
                        Some(value) => value,
                        None => return Ok(Value::Null),
                    }
                }
                other => return Err(format!("引用根非法：{other}")),
            };
            for segment in segments {
                let Some(next) = current.get(segment) else {
                    return Ok(Value::Null);
                };
                current = next;
            }
            Ok(current.clone())
        }
        Expression::Op { op, args } => match op.as_str() {
            "and" => {
                for arg in args {
                    if !truthy(&eval_expression(arg, env)?) {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }
            "or" => {
                for arg in args {
                    if truthy(&eval_expression(arg, env)?) {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            "not" => Ok(Value::Bool(!truthy(&eval_expression(&args[0], env)?))),
            "exists" => Ok(Value::Bool(!eval_expression(&args[0], env)?.is_null())),
            "contains" => {
                let left = eval_expression(&args[0], env)?;
                let right = eval_expression(&args[1], env)?;
                match (left, right) {
                    (Value::String(text), Value::String(needle)) => {
                        Ok(Value::Bool(text.contains(&needle)))
                    }
                    (Value::Array(items), value) => Ok(Value::Bool(items.contains(&value))),
                    (Value::Object(object), Value::String(key)) => {
                        Ok(Value::Bool(object.contains_key(&key)))
                    }
                    _ => Ok(Value::Bool(false)),
                }
            }
            _ => {
                let left = eval_expression(&args[0], env)?;
                let right = eval_expression(&args[1], env)?;
                compare(op, &left, &right)
            }
        },
    }
}

fn compare(op: &str, left: &Value, right: &Value) -> Result<Value, String> {
    let ordering = match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .ok_or_else(|| "数值比较失败".to_owned())?,
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        _ => {
            let equal = left == right;
            return Ok(Value::Bool(match op {
                "eq" => equal,
                "ne" => !equal,
                _ => false,
            }));
        }
    };
    let result = match op {
        "eq" => ordering.is_eq(),
        "ne" => !ordering.is_eq(),
        "gt" => ordering.is_gt(),
        "gte" => !ordering.is_lt(),
        "lt" => ordering.is_lt(),
        "lte" => !ordering.is_gt(),
        _ => false,
    };
    Ok(Value::Bool(result))
}

/// 布尔转换（仅 `true` 为真；非空字符串/非零数值不属于布尔值）。
pub fn truthy(value: &Value) -> bool {
    matches!(value, Value::Bool(true))
}

/// 规范 JSON（递归按 key 排序；sha256 输入）。
pub fn canonical_json(value: &Value) -> String {
    let mut output = String::new();
    write_canonical(value, &mut output);
    output
}

fn write_canonical(value: &Value, output: &mut String) {
    match value {
        Value::Object(object) => {
            output.push('{');
            let sorted: BTreeMap<&String, &Value> = object.iter().collect();
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).unwrap_or_default());
                output.push(':');
                write_canonical(value, output);
            }
            output.push('}');
        }
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical(item, output);
            }
            output.push(']');
        }
        other => {
            output.push_str(&serde_json::to_string(other).unwrap_or_default());
        }
    }
}

/// 定义快照哈希（sha256 canonical JSON，小写十六进制）。
pub fn definition_hash(value: &Value) -> String {
    hex::encode(Sha256::digest(canonical_json(value).as_bytes()))
}

/// 输出截断（>64KiB）：保留前 64KiB 并附加 `truncated=true` 标记。
pub fn clamp_result(value: Value) -> (Value, bool) {
    let Ok(serialized) = serde_json::to_vec(&value) else {
        return (Value::Null, true);
    };
    if serialized.len() <= MAX_RESULT_BYTES {
        return (value, false);
    }
    let text = String::from_utf8_lossy(&serialized[..MAX_RESULT_BYTES]).into_owned();
    (
        serde_json::json!({
            "truncated": true,
            "preview": text,
        }),
        true,
    )
}

/// 稳定键序对象（测试/汇总确定性）。
pub fn object(entries: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Value::Object(map)
}
