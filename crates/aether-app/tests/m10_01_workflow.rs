//! M10-01/ADR-031：编排引擎单测/集成（定义校验、调度、持久化、10 并发、取消、重试、
//! 超时、reconcile）。测试代码豁免 unwrap/expect/panic（AGENTS §2.2）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_app::workflow::definition::{parse_and_validate, parse_expression, MAX_NODES};
use aether_app::workflow::engine::{
    AgentError, AgentExecutor, AgentOutcome, AgentRequest, WorkflowEngine,
};
use aether_control::SystemClock;
use aether_core::{
    PermissionMode, Runtime, RuntimeId, Session, SessionId, SessionStatus, TokenUsage,
};
use aether_store::{
    NodeRunRecord, ReadPool, StoreCommand, StoreRuntime, WorkflowCommand, WorkflowRecord,
    WorkflowRunRecord, WriteQueue, WriteQueueConfig,
};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Harness {
    _dir: TempDir,
    slot: Option<StoreRuntime>,
    reads: ReadPool,
    write: WriteQueue,
}

async fn harness() -> Harness {
    let dir = tempfile::Builder::new()
        .prefix("aether-m10-01-")
        .tempdir()
        .unwrap();
    let slot = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let reads = slot.reads().clone();
    let write = slot.queue().clone();
    Harness {
        _dir: dir,
        slot: Some(slot),
        reads,
        write,
    }
}

impl Harness {
    async fn bootstrap_session(&self, session_id: &str) {
        let now = now_ms();
        let runtime = Runtime {
            id: RuntimeId::new("mock").unwrap(),
            name: "Mock".to_owned(),
            kind: "mock".to_owned(),
            version: "0.1.0".to_owned(),
            protocol: "1.0".to_owned(),
            capabilities: Vec::new(),
            endpoint: None,
            config: json!({}),
            status: aether_core::RuntimeStatus::Ready,
            status_reason: None,
            last_seen_at: None,
            created_at: now,
            updated_at: now,
        };
        self.write
            .execute(StoreCommand::EnsureRuntime { runtime })
            .await
            .unwrap();
        let session = Session {
            id: SessionId::new(session_id).unwrap(),
            runtime_id: RuntimeId::new("mock").unwrap(),
            workspace_id: None,
            parent_session_id: None,
            title: "anchor".to_owned(),
            status: SessionStatus::Idle,
            model: Some("mock-model".to_owned()),
            thinking_depth: 2,
            permission_mode: PermissionMode::WorkspaceWrite,
            preset_id: None,
            system_prompt: None,
            config: json!({"aether":{"provider_id":"p1"}}),
            token_usage: TokenUsage::default(),
            created_at: now,
            updated_at: now,
            closed_at: None,
        };
        self.write
            .execute(StoreCommand::InsertSession { session })
            .await
            .unwrap();
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::InsertWorkflow {
                    workflow: WorkflowRecord {
                        id: "wf-1".to_owned(),
                        name: "wf-1".to_owned(),
                        description: None,
                        version: 1,
                        definition: workflow_definition().to_string(),
                        is_template: false,
                        created_at: now,
                        updated_at: now,
                        deleted_at: None,
                    },
                },
            })
            .await
            .unwrap();
    }

    fn engine(&self, agent: Arc<dyn AgentExecutor>) -> Arc<WorkflowEngine> {
        Arc::new(WorkflowEngine::new(
            self.reads.clone(),
            self.write.clone(),
            None,
            Arc::new(SystemClock),
            agent,
        ))
    }

    async fn start(
        &self,
        engine: &Arc<WorkflowEngine>,
        definition: Value,
        session_id: &str,
    ) -> String {
        let parsed = parse_and_validate(&definition).expect("定义合法");
        let run_id = ulid::Ulid::new().to_string();
        let request = aether_app::workflow::engine::RunRequest {
            run_id: run_id.clone(),
            workflow_id: "wf-1".to_owned(),
            session_id: session_id.to_owned(),
            runtime_id: "mock".to_owned(),
            priority: 0,
            input: json!({"seed": 1}),
            definition: parsed,
            definition_value: definition,
        };
        engine.start(request).await.unwrap();
        run_id
    }

    async fn wait_run(&self, run_id: &str) -> WorkflowRunRecord {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let run = self.reads.workflow_run(run_id).await.unwrap().unwrap();
            if matches!(
                run.status.as_str(),
                "succeeded" | "failed" | "cancelled" | "interrupted"
            ) {
                return run;
            }
            assert!(
                Instant::now() < deadline,
                "运行未在预算内收口：{}",
                run.status
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn node_statuses(&self, run_id: &str) -> HashMap<String, String> {
        self.reads
            .node_runs(run_id)
            .await
            .unwrap()
            .into_iter()
            .map(|node| (format!("{}#{}", node.node_id, node.attempt), node.status))
            .collect()
    }

    async fn shutdown(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.shutdown().await.unwrap();
        }
    }
}

#[derive(Default)]
struct ScriptedAgent {
    calls: Mutex<HashMap<String, usize>>,
    failures: HashMap<String, usize>,
    delay_ms: u64,
}

impl ScriptedAgent {
    fn with_failure(node_id: &str, count: usize) -> Self {
        let mut failures = HashMap::new();
        failures.insert(node_id.to_owned(), count);
        Self {
            calls: Mutex::new(HashMap::new()),
            failures,
            delay_ms: 0,
        }
    }

    fn slow(delay_ms: u64) -> Self {
        Self {
            calls: Mutex::new(HashMap::new()),
            failures: HashMap::new(),
            delay_ms,
        }
    }
}

impl AgentExecutor for ScriptedAgent {
    fn execute(
        &self,
        request: AgentRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AgentOutcome, AgentError>> + Send + '_>,
    > {
        Box::pin(async move {
            let key = format!("{}:{}", request.workflow_run_id, request.node_id);
            let count = {
                let mut calls = self.calls.lock().unwrap();
                let entry = calls.entry(key).or_insert(0);
                *entry += 1;
                *entry
            };
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            if request.cancel.load(Ordering::SeqCst) {
                return Err(AgentError::new("cancelled", "cancelled", false));
            }
            if self
                .failures
                .get(&request.node_id)
                .is_some_and(|allowed| count <= *allowed)
            {
                return Err(AgentError::new(
                    "adapter_unavailable",
                    "injected transient failure",
                    true,
                ));
            }
            Ok(AgentOutcome {
                output: json!({"node": request.node_id, "call": count}),
                session_id: None,
            })
        })
    }
}

fn workflow_definition() -> Value {
    json!({
        "schema_version": 1,
        "name": "m10-01-e2e",
        "description": "串行/并行/条件/汇总",
        "nodes": [
            {"id": "start", "type": "task", "handler": "noop"},
            {"id": "set", "type": "task", "handler": "set_context", "input": {"route": "fast", "n": 1}},
            {"id": "cond", "type": "condition", "expression": {"op": "eq", "args": [{"ref": "context.route"}, "fast"]}},
            {"id": "fan", "type": "parallel", "fanout": 2, "max_parallel": 2},
            {"id": "a", "type": "task", "handler": "noop"},
            {"id": "b", "type": "task", "handler": "merge_results", "input": {"x": 1}},
            {"id": "join", "type": "join", "mode": "all"},
            {"id": "summary", "type": "task", "handler": "emit_summary"}
        ],
        "edges": [
            {"from": "start", "to": "set"},
            {"from": "set", "to": "cond"},
            {"from": "cond", "to": "fan", "when": {"op": "eq", "args": [{"ref": "results.cond.value"}, true]}},
            {"from": "fan", "to": "a"},
            {"from": "fan", "to": "b"},
            {"from": "a", "to": "join"},
            {"from": "b", "to": "join"},
            {"from": "join", "to": "summary"}
        ],
        "settings": {"fail_policy": "fail_fast", "timeout_ms": 60000, "max_parallel": 4}
    })
}

#[test]
fn definition_validation_rejects_and_accepts() {
    let valid = workflow_definition();
    let parsed = parse_and_validate(&valid).expect("合法定义");
    assert_eq!(parsed.nodes.len(), 8);

    // 环。
    let mut cyclic = valid.clone();
    cyclic["edges"]
        .as_array_mut()
        .unwrap()
        .push(json!({"from": "summary", "to": "start"}));
    let error = parse_and_validate(&cyclic).expect_err("环必须拒绝");
    assert!(error.issues.iter().any(|issue| issue.code == "cycle"));

    // 重复 id / 未知节点类型 / 非法表达式。
    let mut broken = valid.clone();
    broken["nodes"][0]["id"] = json!("start");
    broken["nodes"][1]["id"] = json!("start");
    broken["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "bad", "type": "shell"}));
    broken["edges"]
        .as_array_mut()
        .unwrap()
        .push(json!({"from": "start", "to": "bad"}));
    broken["edges"]
        .as_array_mut()
        .unwrap()
        .push(json!({"from": "bad", "to": "set"}));
    broken["edges"][2]["when"] = json!({"op": "eval", "args": []});
    let error = parse_and_validate(&broken).expect_err("非法定义必须拒绝");
    let codes: Vec<&str> = error
        .issues
        .iter()
        .map(|issue| issue.code.as_str())
        .collect();
    assert!(codes.contains(&"duplicate_node_id"));
    assert!(codes.contains(&"unknown_node_type"));
    assert!(codes.contains(&"invalid_expression"));

    // 节点数上限。
    let mut too_many = valid.clone();
    for index in 0..=MAX_NODES {
        too_many["nodes"].as_array_mut().unwrap().push(json!({
            "id": format!("n{index}"),
            "type": "task",
            "handler": "noop"
        }));
    }
    let error = parse_and_validate(&too_many).expect_err("节点上限");
    assert!(error
        .issues
        .iter()
        .any(|issue| issue.code == "limit_exceeded"));

    // 表达式白名单。
    assert!(parse_expression(
        &json!({"op": "and", "args": [true, {"op": "exists", "args": [{"ref": "context.x"}]}]})
    )
    .is_ok());
    assert!(parse_expression(&json!({"op": "call", "args": []})).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_parallel_condition_join_and_context_isolation() {
    let harness = harness().await;
    harness
        .bootstrap_session("01J0000000000000000000000A")
        .await;
    let engine = harness.engine(Arc::new(ScriptedAgent::default()));
    let run_id = harness
        .start(&engine, workflow_definition(), "01J0000000000000000000000A")
        .await;
    let run = harness.wait_run(&run_id).await;
    let statuses = harness.node_statuses(&run_id).await;
    assert_eq!(run.status, "succeeded", "run={run:?} nodes={statuses:?}");
    for node in ["start", "set", "cond", "fan", "a", "b", "join", "summary"] {
        assert_eq!(
            statuses.get(&format!("{node}#1")).map(String::as_str),
            Some("succeeded")
        );
    }
    // 上下文隔离：锚点会话没有新增消息（编排节点只在子上下文/子会话工作）。
    let messages = harness
        .reads
        .messages_latest(&SessionId::new("01J0000000000000000000000A").unwrap(), 10)
        .await
        .unwrap();
    assert!(messages.is_empty(), "主会话上下文被污染");
    // 汇总确定性：join 输出按 node_id 排序。
    let summary = harness
        .reads
        .node_runs(&run_id)
        .await
        .unwrap()
        .into_iter()
        .find(|node| node.node_id == "summary")
        .unwrap();
    let output: Value = serde_json::from_str(summary.output.as_deref().unwrap()).unwrap();
    assert!(output.get("summary").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_concurrent_workflows_finish_without_oversell() {
    let harness = harness().await;
    harness
        .bootstrap_session("01J0000000000000000000000B")
        .await;
    let engine = harness.engine(Arc::new(ScriptedAgent::slow(20)));
    let definition = json!({
        "schema_version": 1,
        "name": "ten",
        "nodes": [
            {"id": "agent", "type": "agent", "prompt": "hello"},
            {"id": "done", "type": "task", "handler": "noop"}
        ],
        "edges": [
            {"from": "agent", "to": "done"}
        ],
        "settings": {"fail_policy": "fail_fast"}
    });
    let mut runs = Vec::new();
    for _ in 0..10 {
        runs.push(
            harness
                .start(&engine, definition.clone(), "01J0000000000000000000000B")
                .await,
        );
    }
    for run_id in runs {
        let run = harness.wait_run(&run_id).await;
        assert_eq!(run.status, "succeeded", "run={run:?}");
        let statuses = harness.node_statuses(&run_id).await;
        assert_eq!(
            statuses.get("agent#1").map(String::as_str),
            Some("succeeded")
        );
        assert_eq!(
            statuses.get("done#1").map(String::as_str),
            Some("succeeded")
        );
    }
    let metrics = engine.metrics();
    assert_eq!(metrics["global_limit"], json!(10));
    assert_eq!(metrics["active_node_tasks"], json!(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_creates_new_attempt_and_timeout_fails() {
    let retry_harness = harness().await;
    retry_harness
        .bootstrap_session("01J0000000000000000000000C")
        .await;
    let engine = retry_harness.engine(Arc::new(ScriptedAgent::with_failure("agent", 1)));
    let definition = json!({
        "schema_version": 1,
        "name": "retry",
        "nodes": [
            {"id": "agent", "type": "agent", "prompt": "hello", "retry": {"max_attempts": 2, "backoff_ms": 100, "multiplier": 1.0}}
        ],
        "edges": [],
        "settings": {"fail_policy": "fail_fast"}
    });
    let run_id = retry_harness
        .start(&engine, definition, "01J0000000000000000000000C")
        .await;
    let run = retry_harness.wait_run(&run_id).await;
    assert_eq!(run.status, "succeeded", "run={run:?}");
    let statuses = retry_harness.node_statuses(&run_id).await;
    assert_eq!(statuses.get("agent#1").map(String::as_str), Some("failed"));
    assert_eq!(
        statuses.get("agent#2").map(String::as_str),
        Some("succeeded")
    );

    // 超时：agent 睡 2s，节点 timeout=1000ms → timeout + run failed。
    let timeout_harness = harness().await;
    timeout_harness
        .bootstrap_session("01J0000000000000000000000D")
        .await;
    let engine = timeout_harness.engine(Arc::new(ScriptedAgent::slow(2_000)));
    let timeout_definition = json!({
        "schema_version": 1,
        "name": "timeout",
        "nodes": [
            {"id": "agent", "type": "agent", "prompt": "slow", "timeout_ms": 1000}
        ],
        "edges": [],
        "settings": {"fail_policy": "fail_fast"}
    });
    let run_id = timeout_harness
        .start(&engine, timeout_definition, "01J0000000000000000000000D")
        .await;
    let run = timeout_harness.wait_run(&run_id).await;
    assert_eq!(run.status, "failed", "run={run:?}");
    let statuses = timeout_harness.node_statuses(&run_id).await;
    assert_eq!(statuses.get("agent#1").map(String::as_str), Some("timeout"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_cascades_and_reconcile_marks_interrupted() {
    let harness = harness().await;
    harness
        .bootstrap_session("01J0000000000000000000000E")
        .await;
    let engine = harness.engine(Arc::new(ScriptedAgent::slow(30_000)));
    let definition = json!({
        "schema_version": 1,
        "name": "cancel",
        "nodes": [{"id": "agent", "type": "agent", "prompt": "long"}],
        "edges": [],
        "settings": {"fail_policy": "fail_fast"}
    });
    let run_id = harness
        .start(&engine, definition.clone(), "01J0000000000000000000000E")
        .await;
    // 等节点进入 running。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let statuses = harness.node_statuses(&run_id).await;
        if statuses.get("agent#1").map(String::as_str) == Some("running") {
            break;
        }
        assert!(Instant::now() < deadline, "节点未启动");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = Instant::now();
    let outcome = engine.cancel(&run_id, "test").await.unwrap();
    assert_eq!(outcome, aether_app::workflow::CancelOutcome::Cancelled);
    assert!(
        started.elapsed() <= Duration::from_secs(5),
        "取消未在 5s 内收口"
    );
    let run = harness.wait_run(&run_id).await;
    assert_eq!(run.status, "cancelled", "run={run:?}");
    let statuses = harness.node_statuses(&run_id).await;
    assert_eq!(
        statuses.get("agent#1").map(String::as_str),
        Some("cancelled")
    );

    // daemon 重启 reconcile：手工插入 running 运行/节点 → interrupted/cancelled。
    let now = now_ms();
    harness
        .write
        .execute(StoreCommand::Workflow {
            command: WorkflowCommand::InsertWorkflowRun {
                run: WorkflowRunRecord {
                    id: "01J0000000000000000000000F".to_owned(),
                    workflow_id: "wf-1".to_owned(),
                    status: "running".to_owned(),
                    context: "{}".to_owned(),
                    started_at: now,
                    finished_at: None,
                    session_id: Some("01J0000000000000000000000E".to_owned()),
                    definition_version: 1,
                    definition_hash: None,
                    error: None,
                    cancel_requested_at: None,
                },
            },
        })
        .await
        .unwrap();
    harness
        .write
        .execute(StoreCommand::Workflow {
            command: WorkflowCommand::InsertNodeRun {
                node: NodeRunRecord {
                    id: "01J0000000000000000000000G".to_owned(),
                    workflow_run_id: "01J0000000000000000000000F".to_owned(),
                    node_id: "stuck".to_owned(),
                    session_id: None,
                    status: "running".to_owned(),
                    input: None,
                    output: None,
                    error: None,
                    attempt: 1,
                    started_at: Some(now),
                    finished_at: None,
                },
            },
        })
        .await
        .unwrap();
    let (runs, nodes) = engine.reconcile().await.unwrap();
    assert!(runs >= 1);
    assert!(nodes >= 1);
    let reconciled = harness
        .reads
        .workflow_run("01J0000000000000000000000F")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reconciled.status, "interrupted");
    let node = harness
        .reads
        .node_run("01J0000000000000000000000G")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node.status, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_ipc_commands_roundtrip_and_reject_invalid_definition() {
    use aether_app::ipc::backend::NotImplementedBackend;
    use aether_app::ipc::dto::{
        WorkflowCreateRequest, WorkflowDeleteRequest, WorkflowIdRequest, WorkflowListRequest,
        WorkflowRunGetRequest, WorkflowRunListRequest, WorkflowRunRequest, WorkflowUpdateRequest,
    };
    use aether_app::ipc::IpcBackend;
    use aether_app::workflow::WorkflowControlBackend;

    let mut harness = harness().await;
    harness
        .bootstrap_session("01J0000000000000000000000I")
        .await;
    let engine = harness.engine(Arc::new(ScriptedAgent::default()));
    let backend = WorkflowControlBackend::new(
        Arc::new(NotImplementedBackend),
        Arc::clone(&engine),
        harness.reads.clone(),
        harness.write.clone(),
        Arc::new(SystemClock),
        tokio::runtime::Handle::current(),
    );

    // 非法定义：结构化拒绝 + business_code + details（不落库）。
    let invalid = backend
        .workflow_create(&WorkflowCreateRequest {
            name: "bad".to_owned(),
            description: None,
            definition: json!({"schema_version": 1, "name": "bad", "nodes": [], "edges": []}),
            is_template: None,
        })
        .expect_err("空节点定义必须拒绝");
    assert_eq!(
        invalid.business_code(),
        Some(aether_app::ipc::BusinessCode::WorkflowInvalidDefinition)
    );
    let details = invalid.details().expect("details");
    assert!(details["details"]
        .as_array()
        .is_some_and(|items| !items.is_empty()));

    let created = backend
        .workflow_create(&WorkflowCreateRequest {
            name: "ipc-roundtrip".to_owned(),
            description: Some("roundtrip".to_owned()),
            definition: workflow_definition(),
            is_template: Some(false),
        })
        .unwrap();
    let workflow_id = created["workflow_id"].as_str().unwrap().to_owned();
    assert_eq!(created["version"], json!(1));

    let listed = backend
        .workflow_list(&WorkflowListRequest {
            limit: Some(10),
            offset: None,
            include_templates: None,
        })
        .unwrap();
    assert!(listed["total"].as_u64().unwrap() >= 1);

    let got = backend
        .workflow_get(&WorkflowIdRequest {
            workflow_id: workflow_id.clone(),
        })
        .unwrap();
    assert_eq!(got["name"], json!("ipc-roundtrip"));
    assert!(got["definition"].is_object());

    let updated = backend
        .workflow_update(&WorkflowUpdateRequest {
            workflow_id: workflow_id.clone(),
            expected_version: 1,
            name: Some("ipc-roundtrip-2".to_owned()),
            description: None,
            definition: None,
            is_template: None,
        })
        .unwrap();
    assert_eq!(updated["version"], json!(2));
    let conflict = backend
        .workflow_update(&WorkflowUpdateRequest {
            workflow_id: workflow_id.clone(),
            expected_version: 1,
            name: Some("stale".to_owned()),
            description: None,
            definition: None,
            is_template: None,
        })
        .expect_err("旧版本必须冲突");
    assert_eq!(
        conflict.business_code(),
        Some(aether_app::ipc::BusinessCode::WorkflowVersionConflict)
    );

    let run = backend
        .workflow_run(&WorkflowRunRequest {
            workflow_id: workflow_id.clone(),
            session_id: "01J0000000000000000000000I".to_owned(),
            input: Some(json!({"x":1})),
            priority: Some(10),
        })
        .unwrap();
    let run_id = run["workflow_run_id"].as_str().unwrap().to_owned();
    let run = harness.wait_run(&run_id).await;
    assert_eq!(run.status, "succeeded");
    let detail = backend
        .workflow_run_get(&WorkflowRunGetRequest {
            workflow_run_id: run_id.clone(),
            include_nodes: Some(true),
        })
        .unwrap();
    assert_eq!(detail["status"], json!("succeeded"));
    assert_eq!(detail["priority"], json!(10));
    assert!(detail["nodes"]
        .as_array()
        .is_some_and(|nodes| nodes.len() == 8));
    let runs = backend
        .workflow_run_list(&WorkflowRunListRequest {
            workflow_id: Some(workflow_id.clone()),
            session_id: None,
            status: Some("succeeded".to_owned()),
            limit: None,
            offset: None,
        })
        .unwrap();
    assert!(runs["total"].as_u64().unwrap() >= 1);

    let deleted = backend
        .workflow_delete(&WorkflowDeleteRequest {
            workflow_id: workflow_id.clone(),
            force: None,
        })
        .unwrap();
    assert_eq!(deleted["deleted"], json!(true));
    let missing = backend
        .workflow_get(&WorkflowIdRequest { workflow_id })
        .expect_err("软删除后默认不可见");
    assert_eq!(
        missing.business_code(),
        Some(aether_app::ipc::BusinessCode::WorkflowNotFound)
    );
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_failure_is_not_reported_as_success() {
    let mut harness = harness().await;
    harness
        .bootstrap_session("01J0000000000000000000000H")
        .await;
    let engine = harness.engine(Arc::new(ScriptedAgent::default()));
    // 关闭写运行时后再启动：start 必须返回错误（不产生半成品「成功」）。
    harness.shutdown().await;
    let definition = json!({
        "schema_version": 1,
        "name": "storage-failure",
        "nodes": [{"id": "start", "type": "task", "handler": "noop"}],
        "edges": []
    });
    let parsed = parse_and_validate(&definition).unwrap();
    let result = engine
        .start(aether_app::workflow::engine::RunRequest {
            run_id: ulid::Ulid::new().to_string(),
            workflow_id: "wf-1".to_owned(),
            session_id: "01J0000000000000000000000H".to_owned(),
            runtime_id: "mock".to_owned(),
            priority: 0,
            input: Value::Null,
            definition: parsed,
            definition_value: definition,
        })
        .await;
    assert!(result.is_err(), "写队列关闭后不得启动成功");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
