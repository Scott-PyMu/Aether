//! M7-02：能力路由（ADR-022；DoD1/2/5）。
//!
//! 覆盖：
//! - 路由矩阵（命令层）：固定优先级常量（claude-code=10 / codex=20 / mock=100）、
//!   `ready` 优先于 `cold`、`id` 字典序、`starting/degraded/disabled` 排除；
//! - 手动优先：显式 `runtime_id` 跳过打分（不写 `route.decided`）；
//! - 无候选：结构化错误 `no_candidate_runtime`（全部禁用 / 无匹配模型）；
//! - 模型硬条件：存在启用 provider 提供该模型（`provider_models` 启用行）；
//! - 热插拔联动：禁用即排除、启用即重新纳入（无粘性缓存）；
//! - 审计与事件：`route.decided`（actor=system）+ `session.created.routed=true`；
//! - 可用性校验：未注册 → `invalid_enum`；`enabled=0` → `invalid_value`。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{
    ExecutorFuture, ExecutorOutcome, LifecycleConfig, RunExecutor, RunRequest, SessionManager,
    SystemClock,
};
use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
use aether_store::{AuditQuery, ReadPool, StoreCommand, WriteQueue};
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::credential_broker::BrokerInfo;
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{SessionCreateRequest, SessionSendRequest};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::provider_env::ProviderEnvHook;
use aether_tauri::runtime_control::boot_supervisor;
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

/// 测试执行器：记录调用并在取消时返回（路由测试不执行任务，创建即断言）。
#[derive(Default)]
struct StubExecutor {
    calls: Mutex<Vec<RunRequest>>,
}

impl RunExecutor for StubExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async move {
            match self.calls.lock() {
                Ok(mut guard) => guard.push(request.clone()),
                Err(poisoned) => poisoned.into_inner().push(request.clone()),
            }
            request.cancel.cancelled().await;
            ExecutorOutcome::Cancelled {
                reason: Some("stub".to_owned()),
            }
        })
    }
}

#[derive(Default)]
struct MemorySecrets {
    map: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl SecretStore for MemorySecrets {
    fn level(&self) -> SecurityLevel {
        SecurityLevel::OsKeychain
    }
    fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
        self.map
            .lock()
            .expect("?")
            .get(&reference.to_uri())
            .map(|value| SecretValue::new(value.clone()))
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }
    fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
        self.map
            .lock()
            .expect("?")
            .insert(reference.to_uri(), value.expose().to_owned());
        Ok(())
    }
    fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
        self.map
            .lock()
            .expect("?")
            .remove(&reference.to_uri())
            .map(|_| ())
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }
}

const DEFAULT_PROVIDER: &str = "01J0000000000000000000R01";

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: aether_control::EventPipeline,
    reads: ReadPool,
    write: WriteQueue,
    #[allow(dead_code)]
    manager: SessionManager,
    backend: Arc<dyn IpcBackend>,
    supervisor: Arc<Supervisor>,
}

/// 注册 claude-code / codex / mock 三运行时（不启动；状态 `cold`）。
fn harness() -> Harness {
    let dir = TempDir::new().expect("临时目录");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时");
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage: slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心（存储 + 管线）");
    let spec = |id: &str| {
        RuntimeSpec::with_fresh_token(
            RuntimeManifest::new(id, id, "definitely-missing-adapter-binary").official(true),
        )
    };
    let supervisor = Arc::new(
        boot_supervisor(
            vec![spec("claude-code"), spec("codex"), spec("mock")],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("构造监督器"),
    );
    // M7-10????????????? provider+model???????????????
    let secrets = Arc::new(MemorySecrets::default());
    let reference = KeychainRef::new("provider", DEFAULT_PROVIDER).expect("??");
    secrets
        .set(&reference, &SecretValue::new("test-key".to_owned()))
        .expect("????");
    runtime
        .block_on(write.execute(StoreCommand::InsertProvider {
            provider: aether_store::ProviderRecord {
                id: DEFAULT_PROVIDER.to_owned(),
                name: "???????".to_owned(),
                provider_type: "deepseek".to_owned(),
                base_url: Some("https://api.deepseek.com".to_owned()),
                endpoints: "{}".to_owned(),
                api_key_ref: Some(reference.to_uri()),
                enabled: true,
                is_builtin: false,
                created_at: 1,
                updated_at: 1,
            },
        }))
        .expect("?????");
    for (index, model) in ["model-a", "deepseek-chat"].iter().enumerate() {
        runtime
            .block_on(write.execute(StoreCommand::InsertProviderModel {
                model: aether_store::ProviderModelRecord {
                    id: format!("route-model-{index}"),
                    provider_id: DEFAULT_PROVIDER.to_owned(),
                    model_id: (*model).to_owned(),
                    display_name: (*model).to_owned(),
                    enabled: true,
                    created_at: 1,
                },
            }))
            .expect("????");
    }
    let hook = ProviderEnvHook::new(dir.path().to_path_buf());
    hook.wire(
        reads.clone(),
        Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
    );
    hook.set_broker_info(BrokerInfo {
        endpoint: "test-endpoint".to_owned(),
        token: "test-token".to_owned(),
    });
    let executor = Arc::new(StubExecutor::default());
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor,
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager.clone()),
            None,
            Some(reads.clone()),
            Some(Arc::clone(&supervisor)),
            handle,
        )
        .with_provider_env(hook),
    );
    Harness {
        dir,
        runtime,
        slot,
        pipeline,
        reads,
        write,
        manager,
        backend,
        supervisor,
    }
}

fn create_request_legacy(runtime_id: Option<&str>, model: Option<&str>) -> SessionCreateRequest {
    SessionCreateRequest {
        runtime_id: runtime_id.map(str::to_owned),
        title: "M7-02".to_owned(),
        workspace_id: None,
        model: model.map(str::to_owned),
        provider_id: None,
        thinking_depth: None,
        permission_mode: None,
        preset_id: None,
    }
}

fn create_request(runtime_id: Option<&str>, model: Option<&str>) -> SessionCreateRequest {
    // ????????????provider+model ???mock/???????????
    let official = runtime_id.map_or(true, |id| {
        matches!(id, "claude-code" | "codex" | "deepseek-harness")
    });
    SessionCreateRequest {
        runtime_id: runtime_id.map(str::to_owned),
        title: "M7-02".to_owned(),
        workspace_id: None,
        model: if official {
            Some(model.unwrap_or("model-a").to_owned())
        } else {
            model.map(str::to_owned)
        },
        thinking_depth: None,
        permission_mode: None,
        preset_id: None,
        provider_id: if official {
            Some(DEFAULT_PROVIDER.to_owned())
        } else {
            None
        },
    }
}

fn create_auto(h: &Harness, title: &str, model: Option<&str>) -> Result<Value, String> {
    h.backend
        .session_create(&SessionCreateRequest {
            title: title.to_owned(),
            ..create_request(None, model)
        })
        .map_err(|error| format!("{}：{}", error.code.as_str(), error.message))
}

fn set_runtime_enabled(h: &Harness, runtime_id: &str, enabled: bool) {
    let manifest = h
        .supervisor
        .get(runtime_id)
        .expect("运行时")
        .manifest()
        .clone();
    let now = 1_700_000_000_000i64;
    let row = aether_core::Runtime {
        id: aether_core::RuntimeId::new(manifest.id.clone()).expect("id"),
        name: manifest.name.clone(),
        kind: manifest.kind.clone(),
        version: manifest.version.clone(),
        protocol: manifest.protocol.clone(),
        capabilities: Vec::new(),
        endpoint: Some(manifest.program.display().to_string()),
        config: json!({}),
        status: h
            .supervisor
            .get(runtime_id)
            .expect("运行时")
            .summary_snapshot()
            .0,
        status_reason: None,
        last_seen_at: None,
        created_at: now,
        updated_at: now,
    };
    h.runtime
        .block_on(h.write.execute(StoreCommand::SetRuntimeEnabled {
            runtime: row,
            enabled,
        }))
        .expect("偏好写入");
}

fn disable_runtime(h: &Harness, runtime_id: &str) {
    h.runtime
        .block_on(h.supervisor.runtime_disable(runtime_id))
        .expect("监督器禁用");
    set_runtime_enabled(h, runtime_id, false);
}

fn enable_runtime(h: &Harness, runtime_id: &str) {
    // 启用走冷启动路径（`disabled → cold`，不实际 spawn 适配器进程——路由候选
    // 只看状态与偏好；真实启动/预热链路由 M7-01 覆盖）。
    h.runtime
        .block_on(h.supervisor.get(runtime_id).expect("运行时").begin_enable())
        .expect("监督器启用（→ cold）");
    set_runtime_enabled(h, runtime_id, true);
}

fn audit_actions(h: &Harness, session_id: &str) -> Vec<String> {
    let result = h
        .runtime
        .block_on(h.reads.audit_query(AuditQuery {
            session_id: Some(session_id.to_owned()),
            actor: None,
            action: None,
            resource: None,
            from_ts: None,
            to_ts: None,
            limit: Some(100),
            offset: 0,
        }))
        .expect("审计查询");
    result.rows.iter().map(|row| row.action.clone()).collect()
}

fn session_created_routed(h: &Harness, session_id: &str) -> Option<bool> {
    let frame = h
        .runtime
        .block_on(
            h.pipeline
                .readback(&aether_core::SessionId::new(session_id).expect("id"), 0),
        )
        .expect("readback");
    frame
        .events
        .iter()
        .find(|event| event.event_type().as_str() == "session.created")
        .and_then(|event| event.payload.to_value().ok())
        .and_then(|payload| payload.get("routed").and_then(Value::as_bool))
}

/// DoD1/5：缺省自动路由按固定优先级选 `claude-code`（cold 可参与），
/// 审计 `route.decided` + `session.created.routed=true`；手动指定跳过打分。
#[test]
fn auto_route_uses_fixed_priority_and_audits_decision() {
    let h = harness();
    let created = create_auto(&h, "auto", None).expect("自动路由应成功");
    let session_id = created["id"].as_str().expect("会话 id").to_owned();
    assert_eq!(created["runtime_id"], "claude-code", "固定优先级 10 最小");
    assert_eq!(
        session_created_routed(&h, &session_id),
        Some(true),
        "session.created 回显 routed=true"
    );
    let actions = audit_actions(&h, &session_id);
    assert!(
        actions.iter().any(|action| action == "route.decided"),
        "自动路由必须写 route.decided 审计：{actions:?}"
    );

    // 手动指定：跳过打分（选 mock），不写路由决策审计。
    let manual = h
        .backend
        .session_create(&create_request(Some("mock"), None))
        .expect("手动创建");
    let manual_id = manual["id"].as_str().expect("会话 id").to_owned();
    assert_eq!(manual["runtime_id"], "mock");
    assert_eq!(
        session_created_routed(&h, &manual_id),
        None,
        "手动无 routed 字段"
    );
    let manual_actions = audit_actions(&h, &manual_id);
    assert!(
        !manual_actions
            .iter()
            .any(|action| action == "route.decided"),
        "手动指定不写路由决策：{manual_actions:?}"
    );

    // 手动可用性校验：未注册 → invalid_enum；已注册但 enabled=0 → invalid_value。
    let unknown = h
        .backend
        .session_create(&create_request(Some("unknown-runtime"), None))
        .expect_err("未注册");
    assert_eq!(unknown.code, IpcErrorCode::InvalidEnum);
    set_runtime_enabled(&h, "mock", false);
    let disabled_pref = h
        .backend
        .session_create(&create_request(Some("mock"), None))
        .expect_err("enabled=0");
    assert_eq!(disabled_pref.code, IpcErrorCode::InvalidValue);
    assert!(disabled_pref.message.contains("enabled=0"));
}

/// DoD2：热插拔联动——禁用即排除（无粘性缓存）、启用即重新纳入。
#[test]
fn hotplug_disable_and_enable_toggles_routing_candidates() {
    let h = harness();
    // 禁用 claude-code（监督器状态 + 偏好）→ 自动路由落到 codex。
    disable_runtime(&h, "claude-code");
    let created = create_auto(&h, "hotplug-1", None).expect("应路由到 codex");
    assert_eq!(created["runtime_id"], "codex");

    // 禁用 codex → 自动路由落到 mock（100）。
    disable_runtime(&h, "codex");
    let created = h
        .backend
        .session_create(&create_request_legacy(None, None))
        .expect("route to mock");
    assert_eq!(created["runtime_id"], "mock");

    // 全部禁用 → no_candidate_runtime（结构化错误；message 区分原因）。
    disable_runtime(&h, "mock");
    let error = create_auto(&h, "hotplug-3", None).expect_err("全部禁用");
    assert!(error.contains("no_candidate_runtime"), "{error}");
    assert!(error.contains("全部已禁用"), "原因文本：{error}");
    assert!(
        error.contains("claude-code"),
        "候选清单应含 claude-code：{error}"
    );

    // 重新启用 claude-code → 立即重新纳入（无粘性缓存）。
    enable_runtime(&h, "claude-code");
    let created = create_auto(&h, "hotplug-4", None).expect("启用后应重新纳入");
    assert_eq!(created["runtime_id"], "claude-code");
}

/// DoD1：模型硬条件——无启用 provider 提供该模型 → `no_candidate_runtime`；
/// 添加启用 provider + 启用模型行后 → 路由成功。
#[test]
fn model_condition_requires_enabled_provider() {
    let h = harness();
    let error = create_auto(&h, "model-miss", Some("no-such-model")).expect_err("no match");
    assert!(error.contains("no_candidate_runtime"), "{error}");
    assert!(error.contains("model"), "{error}");

    let created = create_auto(&h, "model-hit", Some("model-a")).expect("model routable");
    assert_eq!(created["runtime_id"], "claude-code");
    assert_eq!(created["provider_id"], DEFAULT_PROVIDER);
    assert_eq!(created["model_state"], "active");
}

/// DoD2：已禁用运行时不参与自动路由（监督器状态 `disabled` 排除，即使偏好被改写）。
#[test]
fn supervisor_disabled_status_excluded_from_auto_route() {
    let h = harness();
    // 仅监督器状态禁用（偏好仍 enabled）——路由以状态硬条件为准。
    h.runtime
        .block_on(h.supervisor.runtime_disable("claude-code"))
        .expect("监督器禁用");
    let created = create_auto(&h, "status-disabled", None).expect("应路由到 codex");
    assert_eq!(created["runtime_id"], "codex");

    // 手动指定 disabled 运行时 → invalid_value（D5 既有校验回归）。
    let error = h
        .backend
        .session_create(&create_request(Some("claude-code"), None))
        .expect_err("disabled 手动");
    assert_eq!(error.code, IpcErrorCode::InvalidValue);
}

/// DoD4（T3 口径冒烟）：10 并发会话 × 每会话 1 run 在缺省闸门（10）下全部派发，
/// 不进入排队——自动路由亦可承载（本用例用手动 mock 聚焦闸门与路由解耦）。
#[test]
fn ten_concurrent_sessions_do_not_queue_at_default_limit() {
    let h = harness();
    for index in 0..10 {
        let created = h
            .backend
            .session_create(&create_request(Some("mock"), None))
            .expect("创建");
        let session_id = created["id"].as_str().expect("id").to_owned();
        let ack = h
            .backend
            .session_send(&SessionSendRequest {
                session_id,
                text: "hi".to_owned(),
                client_msg_id: format!("01J0000000000000000000{:04}", index),
                thinking_depth: None,
                references: Vec::new(),
                provider_id: None,
                model: None,
            })
            .expect("发送");
        assert!(
            ack.get("queue").is_none(),
            "缺省闸门（10）下第 {index} 个 run 不应排队：{ack}"
        );
    }
}
