//! M7-09 集成：Agent 预设命令面（ADR-026 §2.2）与「预设 → 记忆」合成下发。
//!
//! 覆盖：
//! - DoD2：`presets_list` / `preset_add` / `preset_update` / `preset_delete` 全链路
//!   （内置不可删 `builtin_preset_undeletable`、查无 `preset_not_found`、审计动作、
//!   内置可编辑）；
//! - DoD3：`session_create` 合成口径（预设块置前、记忆块后置、禁止静默覆盖）+
//!   `SessionSummary.preset_id/preset_name` 生效来源回显；
//! - DoD4：Mock 适配器观测「合成提示词随 `session.create` 下发」（`AETHER_MOCK_ADAPTER`
//!   未设置时显式 SKIP，禁止静默跳过）。
//!
//! 运行：`AETHER_MOCK_ADAPTER=<Bun 编译产物> cargo test -p aether-tauri --test m7_09_presets`
//! （由 `scripts/test/m7-09/verify-m7-09.mjs` 构建并设置）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{
    EventPipeline, LifecycleConfig, PermissionConfig, PermissionService, SessionManager, SystemClock,
};
use aether_core::{PresetId, RuntimeId, Session, SessionId};
use aether_security::PolicyEngine;
use aether_store::ReadPool;
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    PresetAddRequest, PresetDeleteRequest, PresetUpdateRequest, PresetsListRequest,
    SessionCreateRequest, SessionListRequest, SessionSendRequest, WorkspaceSetRequest,
};
use aether_tauri::permission_loop::PermissionServiceGate;
use aether_tauri::preset_control::PresetControl;
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::Value;
use tempfile::TempDir;

const BUILTIN_DEFAULT: &str = "01J00000000000000000000P01";
const BUILTIN_CODE: &str = "01J00000000000000000000P02";
const BUILTIN_REVIEW: &str = "01J00000000000000000000P03";

/// Mock 适配器路径（opt-in；未设置时显式 SKIP 打印）。
fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!("SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m7-09 构建 Mock 后执行）");
            None
        }
    }
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

/// 证据落盘（`AETHER_M7_09_EVIDENCE_DIR` 设置时写出逐用例 JSON；M3-08 同口径）。
fn evidence(name: &str, value: &Value) {
    println!("[m7-09] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M7_09_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let _ = std::fs::write(dir.join(format!("{name}.json")), text);
}

/// canonical 路径 → 目标字符串形态（剥离 `\\?\` 扩展前缀）。
fn plain(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    workspace: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: EventPipeline,
    reads: ReadPool,
    supervisor: Arc<Supervisor>,
    backend: Arc<SessionBackend>,
    /// Mock 会话调用记录（JSON Lines；仅提供 adapter 时产生）。
    session_log: Option<PathBuf>,
    /// 真实运行时线上 tap 记录（JSON Lines；仅 opt-in 真实适配器用例产生）。
    tap_log: Option<PathBuf>,
    /// 真实适配器下调用的 CLI 参数记录（fake-claude 观测；仅真实适配器用例产生）。
    cli_invocations: Option<PathBuf>,
}

impl Harness {
    /// `adapter = None`：注册一个不预热的运行时（cold；无需真实适配器进程）。
    fn open(adapter: Option<&Path>) -> Self {
        let dir = TempDir::new().expect("临时数据目录");
        let workspace = TempDir::new().expect("临时工作区");
        let session_log = adapter.map(|_| dir.path().join("mock-session-log.jsonl"));
        let program = adapter
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.path().join("missing-adapter"));
        let mut manifest = RuntimeManifest::new("mock", "Mock", program).official(true);
        if let Some(log) = &session_log {
            manifest = manifest.with_args(vec![
                "--stream-deltas".to_owned(),
                "4".to_owned(),
                "--stream-interval-ms".to_owned(),
                "1".to_owned(),
                "--session-log".to_owned(),
                log.to_string_lossy().to_string(),
            ]);
        }
        let spec = RuntimeSpec::with_fresh_token(manifest);
        Self::assemble(dir, workspace, session_log, None, None, spec, adapter.is_some())
    }

    /// 真实运行时（Claude Code 适配器 bundle）经 `rpc-tap.mjs` 透明代理注册：
    /// tap 记录核心 → 适配器的 `session.create` 请求帧，适配器再以 **fake-claude**
    /// （`--claude-bin node --claude-arg <脚本>`）为 CLI 替身，记录适配器 → CLI 的
    /// 实际参数（断言 `--append-system-prompt-file` 与文件内容；DoD4 端到端）。
    fn open_real_adapter(bundle: &Path) -> Self {
        let dir = TempDir::new().expect("临时数据目录");
        let workspace = TempDir::new().expect("临时工作区");
        let tap_log = dir.path().join("rpc-tap.jsonl");
        let cli_invocations = dir.path().join("cli-invocations.jsonl");
        let tap = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("test")
            .join("m7-09")
            .join("rpc-tap.mjs");
        let fake_cli = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("test")
            .join("m2-02")
            .join("fake-claude")
            .join("cli.mjs");
        let manifest = RuntimeManifest::new("claude-code", "Claude Code", "node")
            .official(true)
            .with_args(vec![
                tap.to_string_lossy().to_string(),
                "--log".to_owned(),
                tap_log.to_string_lossy().to_string(),
                "--".to_owned(),
                bundle.to_string_lossy().to_string(),
                "--workspace".to_owned(),
                workspace.path().to_string_lossy().to_string(),
                // CLI 替身（确定性 stream-json；无网络/无凭证），记录实际 argv。
                "--claude-bin".to_owned(),
                "node".to_owned(),
                "--claude-arg".to_owned(),
                fake_cli.to_string_lossy().to_string(),
            ])
            .with_env(vec![
                (
                    "FAKE_CLAUDE_INVOCATION_FILE".to_owned(),
                    cli_invocations.to_string_lossy().to_string(),
                ),
                (
                    "FAKE_CLAUDE_HOME".to_owned(),
                    dir.path().join("fake-claude-home").to_string_lossy().to_string(),
                ),
            ]);
        let spec = RuntimeSpec::with_fresh_token(manifest);
        Self::assemble(
            dir,
            workspace,
            None,
            Some(tap_log),
            Some(cli_invocations),
            spec,
            true,
        )
    }

    /// 真实运行时（Codex 适配器 bundle）经 `rpc-tap.mjs` 透明代理注册；适配器以
    /// **fake-codex** 为 CLI 替身（`--codex-bin node --codex-arg <脚本>`），记录
    /// 适配器 → CLI 的实际参数（断言 `-c developer_instructions=…`；DoD4 端到端）。
    fn open_real_codex_adapter(bundle: &Path) -> Self {
        let dir = TempDir::new().expect("临时数据目录");
        let workspace = TempDir::new().expect("临时工作区");
        let tap_log = dir.path().join("rpc-tap.jsonl");
        let cli_invocations = dir.path().join("cli-invocations.jsonl");
        let tap = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("test")
            .join("m7-09")
            .join("rpc-tap.mjs");
        let fake_cli = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("test")
            .join("m2-11")
            .join("fake-codex")
            .join("cli.mjs");
        let manifest = RuntimeManifest::new("codex", "Codex", "node")
            .official(true)
            .with_args(vec![
                tap.to_string_lossy().to_string(),
                "--log".to_owned(),
                tap_log.to_string_lossy().to_string(),
                "--".to_owned(),
                bundle.to_string_lossy().to_string(),
                "--workspace".to_owned(),
                workspace.path().to_string_lossy().to_string(),
                "--sandbox".to_owned(),
                "workspace-write".to_owned(),
                "--codex-home".to_owned(),
                dir.path().join("codex-home").to_string_lossy().to_string(),
                "--codex-bin".to_owned(),
                "node".to_owned(),
                "--codex-arg".to_owned(),
                fake_cli.to_string_lossy().to_string(),
            ])
            .with_env(vec![(
                "FAKE_CODEX_INVOCATION_FILE".to_owned(),
                cli_invocations.to_string_lossy().to_string(),
            )]);
        let spec = RuntimeSpec::with_fresh_token(manifest);
        Self::assemble(
            dir,
            workspace,
            None,
            Some(tap_log),
            Some(cli_invocations),
            spec,
            true,
        )
    }

    fn assemble(
        dir: TempDir,
        workspace: TempDir,
        session_log: Option<PathBuf>,
        tap_log: Option<PathBuf>,
        cli_invocations: Option<PathBuf>,        spec: RuntimeSpec,
        warmup: bool,
    ) -> Self {
        let runtime = new_runtime();
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
        let service = PermissionService::new(
            PermissionConfig {
                wait_timeout: None,
                ..PermissionConfig::default()
            },
            Arc::new(SystemClock),
            PolicyEngine::new(dir.path()).expect("策略引擎（数据目录基准）"),
            write.clone(),
            reads.clone(),
            pipeline.clone(),
        );
        let supervisor = Arc::new(
            boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json")))
                .expect("构造监督器"),
        );
        if warmup {
            let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
            for (runtime_id, outcome) in &startup.warmups {
                assert!(
                    outcome.is_ready(),
                    "{runtime_id} 预热必须 Ready：{outcome:?}"
                );
            }
        }
        let gate = PermissionServiceGate::new(service.clone());
        let executor = Arc::new(AdapterRunExecutor::new(
            Arc::clone(&supervisor),
            pipeline.clone(),
            reads.clone(),
            write.clone(),
            handle.clone(),
            Some(gate),
        ));
        let manager = SessionManager::new(
            LifecycleConfig::default(),
            Arc::new(SystemClock),
            write.clone(),
            reads.clone(),
            pipeline.clone(),
            executor.clone(),
        );
        let backend = Arc::new(
            SessionBackend::new(
                Arc::new(NotImplementedBackend),
                Some(manager),
                Some(executor),
                Some(reads.clone()),
                Some(Arc::clone(&supervisor)),
                handle.clone(),
            )
            .with_permissions(service)
            .with_workspace_store(write.clone())
            .with_presets(PresetControl::new(reads.clone(), write.clone(), handle)),
        );
        Self {
            dir,
            workspace,
            runtime,
            slot,
            pipeline,
            reads,
            supervisor,
            backend,
            session_log,
            tap_log,
            cli_invocations,
        }
    }

    fn list_presets(&self) -> Vec<Value> {
        self.backend
            .presets_list(&PresetsListRequest {})
            .expect("presets_list")["presets"]
            .as_array()
            .expect("presets 数组")
            .clone()
    }

    /// 绑定工作区（记忆注入源；M3-08 `workspace_set`）。
    fn bind_workspace(&self, root: &Path) -> String {
        let root_text = plain(&std::fs::canonicalize(root).expect("canonicalize 工作区根"));
        let canonical = aether_tauri::ipc::path::validate_workspace_root(root_text.as_str())
            .expect("工作区根校验");
        let value = self
            .backend
            .workspace_set(
                &WorkspaceSetRequest {
                    workspace_id: None,
                    root_path: Some(root_text),
                },
                Some(&canonical),
            )
            .expect("workspace_set");
        value["workspace_id"].as_str().expect("workspace_id").to_owned()
    }

    fn create_session(&self, preset_id: Option<&str>) -> Value {
        self.create_session_for("mock", preset_id)
    }

    fn create_session_for(&self, runtime_id: &str, preset_id: Option<&str>) -> Value {
        self.backend
            .session_create(&SessionCreateRequest {
                runtime_id: Some(runtime_id.to_owned()),
                title: "M7-09".to_owned(),
                workspace_id: None,
                model: None,
                thinking_depth: None,
                permission_mode: None,
                preset_id: preset_id.map(str::to_owned),
            })
            .expect("session_create")
    }

    fn audit_actions(&self) -> Vec<String> {
        self.runtime
            .block_on(self.reads.audit_log(200))
            .unwrap()
            .into_iter()
            .map(|record| record.action)
            .collect()
    }

    fn session_log_records(&self) -> Vec<Value> {
        let Some(path) = &self.session_log else {
            return Vec::new();
        };
        match std::fs::read_to_string(path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn shutdown(self) {
        self.runtime.block_on(self.supervisor.shutdown_all());
        self.runtime.block_on(self.pipeline.shutdown()).ok();
        if let Some(storage) = self.slot.take() {
            self.runtime.block_on(storage.shutdown()).ok();
        }
        let _ = &self.dir;
    }
}

/// DoD2：预设四命令全链路（清单顺序 / 内置可编辑不可删 / 自定义增改删 / 审计 / 稳定码）。
#[test]
fn preset_commands_full_cycle() {
    let harness = Harness::open(None);

    // 播种 3 条内置（迁移 0007），内置在前、创建时间升序。
    let listed = harness.list_presets();
    assert_eq!(listed.len(), 3, "内置播种 3 条");
    assert_eq!(
        listed.iter().map(|item| item["id"].as_str().unwrap()).collect::<Vec<_>>(),
        vec![BUILTIN_DEFAULT, BUILTIN_CODE, BUILTIN_REVIEW]
    );
    assert_eq!(listed[0]["name"], "默认助手");
    assert_eq!(listed[1]["name"], "代码专家");
    assert_eq!(listed[2]["name"], "代码审查");
    assert!(listed.iter().all(|item| item["is_builtin"] == true));
    assert!(listed[0]["system_prompt"]
        .as_str()
        .unwrap()
        .contains("Aether 工作台中的通用 AI 助手"));

    // 新增自定义预设。
    let created = harness
        .backend
        .preset_add(&PresetAddRequest {
            name: "审查助手".to_owned(),
            system_prompt: "你是审查助手".to_owned(),
        })
        .expect("preset_add");
    assert_eq!(created["is_builtin"], false);
    let custom_id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(harness.list_presets().len(), 4);

    // 更新（自定义）：名称 + 提示词。
    let updated = harness
        .backend
        .preset_update(&PresetUpdateRequest {
            preset_id: custom_id.clone(),
            name: Some("审查助手 v2".to_owned()),
            system_prompt: Some("你是审查助手 v2".to_owned()),
        })
        .expect("preset_update");
    assert_eq!(updated["name"], "审查助手 v2");
    assert_eq!(updated["system_prompt"], "你是审查助手 v2");
    assert_eq!(updated["is_builtin"], false);

    // 内置可编辑（仅删除受保护；ADR-026 §2.2-2）。
    let builtin_updated = harness
        .backend
        .preset_update(&PresetUpdateRequest {
            preset_id: BUILTIN_DEFAULT.to_owned(),
            name: Some("默认助手（改）".to_owned()),
            system_prompt: None,
        })
        .expect("内置可编辑");
    assert_eq!(builtin_updated["name"], "默认助手（改）");
    assert_eq!(builtin_updated["is_builtin"], true, "is_builtin 保持");

    // 内置不可删（硬约束；不落库、不消失）。
    let error = harness
        .backend
        .preset_delete(&PresetDeleteRequest {
            preset_id: BUILTIN_CODE.to_owned(),
        })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "builtin_preset_undeletable");
    assert_eq!(error.field.as_deref(), Some("preset_id"));
    assert!(harness
        .list_presets()
        .iter()
        .any(|item| item["id"] == BUILTIN_CODE));

    // 查无 → preset_not_found（更新/删除同码）。
    for error in [
        harness
            .backend
            .preset_update(&PresetUpdateRequest {
                preset_id: "01J00000000000000000000ZZZZ".to_owned(),
                name: Some("x".to_owned()),
                system_prompt: None,
            })
            .unwrap_err(),
        harness
            .backend
            .preset_delete(&PresetDeleteRequest {
                preset_id: "01J00000000000000000000ZZZZ".to_owned(),
            })
            .unwrap_err(),
    ] {
        assert_eq!(error.code.as_str(), "preset_not_found");
    }

    // 删除自定义 → 再次删除 → preset_not_found。
    let deleted = harness
        .backend
        .preset_delete(&PresetDeleteRequest {
            preset_id: custom_id.clone(),
        })
        .expect("preset_delete");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(harness.list_presets().len(), 3);
    let error = harness
        .backend
        .preset_delete(&PresetDeleteRequest { preset_id: custom_id })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "preset_not_found");

    // 审计动作齐备（ADR-026 附录 A-3）。
    let actions = harness.audit_actions();
    for expected in ["preset.created", "preset.updated", "preset.deleted"] {
        assert!(actions.contains(&expected.to_owned()), "缺少审计动作 {expected}: {actions:?}");
    }

    harness.shutdown();
}

/// DoD3：`session_create` 合成口径（预设块 → 记忆块）+ 生效来源回显 + 未选预设回退 P0 行为。
#[test]
fn session_create_composes_preset_before_memory_and_echoes_source() {
    let harness = Harness::open(None);
    std::fs::write(
        harness.workspace.path().join("AGENTS.md"),
        "工作区约定内容",
    )
    .expect("写入记忆文件");
    harness.bind_workspace(harness.workspace.path());

    // 选预设：合成结果 = 预设块 + 记忆块（预设置前，两块完整保留）。
    let summary = harness.create_session(Some(BUILTIN_CODE));
    assert_eq!(summary["preset_id"], BUILTIN_CODE);
    assert_eq!(summary["preset_name"], "代码专家");
    let system_prompt = summary["system_prompt"].as_str().expect("system_prompt");
    let preset_marker = format!("<!-- aether:preset:{BUILTIN_CODE} -->");
    assert!(system_prompt.starts_with(&preset_marker), "预设块置前：{system_prompt}");
    assert!(
        system_prompt.contains("你是资深软件工程师"),
        "预设提示词完整保留"
    );
    let preset_at = system_prompt.find(&preset_marker).unwrap();
    let memory_at = system_prompt.find("# 工作区约定").unwrap();
    assert!(preset_at < memory_at, "记忆块后置，不得覆盖预设");
    assert!(system_prompt.contains("工作区约定内容"), "记忆内容完整保留");
    assert!(system_prompt.contains("来源：AGENTS.md"));

    // 落库口径：`sessions.system_prompt` 即合成结果，`sessions.preset_id` 为生效来源。
    let session_id = SessionId::new(summary["id"].as_str().unwrap()).unwrap();
    let stored: Session = harness
        .runtime
        .block_on(harness.reads.session(&session_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.preset_id.as_ref().map(PresetId::as_str),
        Some(BUILTIN_CODE)
    );
    assert_eq!(stored.system_prompt.as_deref(), Some(system_prompt));

    // `session_list` 回显 preset_name（生效来源可查）。
    let listed = harness
        .backend
        .session_list(&SessionListRequest {
            runtime_id: None,
            status: None,
            limit: None,
        })
        .expect("session_list");
    let item = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == summary["id"])
        .expect("列表含新会话");
    assert_eq!(item["preset_name"], "代码专家");

    // 未选预设：仅记忆注入（与 P0 行为一致）。
    let plain = harness.create_session(None);
    assert!(plain["preset_id"].is_null(), "未选预设不回显 preset_id");
    assert!(plain["preset_name"].is_null());
    let plain_prompt = plain["system_prompt"].as_str().unwrap();
    assert!(plain_prompt.starts_with("# 工作区约定"));
    assert!(!plain_prompt.contains("<!-- aether:preset:"));

    evidence(
        "dod3_composition_and_source_echo",
        &serde_json::json!({
            "preset_id": BUILTIN_CODE,
            "preset_name": "代码专家",
            "order_preset_before_memory": true,
            "preset_block_present": system_prompt.contains(&preset_marker),
            "memory_block_present": system_prompt.contains("# 工作区约定"),
            "memory_content_present": system_prompt.contains("工作区约定内容"),
            "unselected_preset_falls_back_to_memory_only": !plain_prompt
                .contains("<!-- aether:preset:"),
            "system_prompt": system_prompt,
        }),
    );

    harness.shutdown();
}

/// DoD3：`session_create` 的 `preset_id` 查无 → `preset_not_found`（不创建会话）。
#[test]
fn session_create_rejects_unknown_preset_without_creating_session() {
    let harness = Harness::open(None);
    harness.bind_workspace(harness.workspace.path());

    let error = harness
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: Some("mock".to_owned()),
            title: "M7-09-unknown".to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth: None,
            permission_mode: None,
            preset_id: Some("01J00000000000000000000ZZZZ".to_owned()),
        })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "preset_not_found");

    let listed = harness
        .backend
        .session_list(&SessionListRequest {
            runtime_id: None,
            status: None,
            limit: None,
        })
        .expect("session_list");
    assert!(
        listed.as_array().unwrap().is_empty(),
        "查无预设不得落会话：{listed}"
    );

    harness.shutdown();
}

/// DoD4：合成提示词随 `session.create` 下发适配器（Mock 观测；opt-in，缺适配器显式 SKIP）。
#[test]
fn mock_adapter_observes_composed_prompt_delivery() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let harness = Harness::open(Some(&binary));
    std::fs::write(
        harness.workspace.path().join("AGENTS.md"),
        "工作区约定内容",
    )
    .expect("写入记忆文件");
    harness.bind_workspace(harness.workspace.path());

    let summary = harness.create_session(Some(BUILTIN_REVIEW));
    let session_id = summary["id"].as_str().unwrap().to_owned();
    // 真实下发在首个 run（`session.create` 由执行器发出）。
    let ack = harness
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.clone(),
            text: "hi".to_owned(),
            client_msg_id: ulid::Ulid::new().to_string(),
            thinking_depth: None,
            references: Vec::new(),
        })
        .expect("session_send");
    let run_id = ack["run_id"].as_str().unwrap().to_owned();
    let run_id = aether_core::RunId::new(run_id).unwrap();
    assert!(
        wait_until(Duration::from_secs(30), || {
            matches!(
                harness.runtime.block_on(harness.reads.run(&run_id)).ok().flatten().map(|run| run.status),
                Some(aether_core::RunStatus::Succeeded)
                    | Some(aether_core::RunStatus::Failed)
                    | Some(aether_core::RunStatus::Cancelled)
                    | Some(aether_core::RunStatus::Timeout)
            )
        }),
        "run 必须到达终态"
    );

    assert!(
        wait_until(Duration::from_secs(10), || !harness.session_log_records().is_empty()),
        "Mock 必须记录 session.create"
    );
    let record = harness
        .session_log_records()
        .into_iter()
        .find(|record| record["method"] == "session.create")
        .expect("session.create 记录");
    assert_eq!(record["has_system_prompt"], true);
    let prompt = record["system_prompt"].as_str().unwrap();
    let preset_marker = format!("<!-- aether:preset:{BUILTIN_REVIEW} -->");
    assert!(prompt.contains(&preset_marker), "适配器收到预设块：{prompt}");
    assert!(prompt.contains("严格的代码审查者"));
    assert!(prompt.contains("# 工作区约定"), "适配器收到记忆块：{prompt}");
    assert!(
        prompt.find(&preset_marker).unwrap() < prompt.find("# 工作区约定").unwrap(),
        "下发顺序：预设块 → 记忆块"
    );
    evidence(
        "dod4_mock_delivery",
        &serde_json::json!({
            "method": "session.create",
            "has_system_prompt": true,
            "preset_marker": preset_marker,
            "preset_prompt_present": prompt.contains("严格的代码审查者"),
            "memory_block_present": prompt.contains("# 工作区约定"),
            "order_preset_before_memory": prompt.find(&preset_marker).unwrap()
                < prompt.find("# 工作区约定").unwrap(),
            "system_prompt": prompt,
        }),
    );

    harness.shutdown();
}

/// DoD4：合成提示词经 `session.create` 下发**真实运行时**（Claude Code 适配器 bundle；
/// 经 `scripts/test/m7-09/rpc-tap.mjs` 透明代理观测线上 JSON-RPC 帧）。
///
/// opt-in（`AETHER_M7_09_REAL=1` + `AETHER_CLAUDE_ADAPTER=<bundle>`）；缺失时
/// **显式 SKIP**（禁止静默跳过）。`session.create` 本身不启动 CLI，故无需真实凭据；
/// 随后的一次 send 仅用于触发执行器的适配器会话创建（run 终态不作断言）。
#[test]
fn real_claude_adapter_receives_composed_prompt_over_wire() {
    if std::env::var("AETHER_M7_09_REAL").as_deref() != Ok("1") {
        eprintln!(
            "SKIP：AETHER_M7_09_REAL 未设置（opt-in 真实运行时断言；DoD4「≥1 真实运行时」）"
        );
        return;
    }
    let Some(bundle) = std::env::var_os("AETHER_CLAUDE_ADAPTER").map(PathBuf::from) else {
        eprintln!("SKIP：AETHER_CLAUDE_ADAPTER 未设置（真实运行时断言）");
        return;
    };
    let harness = Harness::open_real_adapter(&bundle);
    std::fs::write(
        harness.workspace.path().join("AGENTS.md"),
        "工作区约定内容",
    )
    .expect("写入记忆文件");
    harness.bind_workspace(harness.workspace.path());

    let summary = harness.create_session_for("claude-code", Some(BUILTIN_CODE));
    let session_id = summary["id"].as_str().unwrap().to_owned();
    // 触发执行器的适配器会话创建（真实适配器进程；run 终态不作断言）。
    let _ = harness.backend.session_send(&SessionSendRequest {
        session_id,
        text: "hi".to_owned(),
        client_msg_id: ulid::Ulid::new().to_string(),
        thinking_depth: None,
        references: Vec::new(),
    });
    let tap_log = harness.tap_log.clone().expect("tap log 路径");
    let contains_create = || {
        std::fs::read_to_string(&tap_log)
            .map(|text| text.contains("\"method\":\"session.create\""))
            .unwrap_or(false)
    };
    assert!(
        wait_until(Duration::from_secs(30), contains_create),
        "真实适配器必须收到 session.create（tap 日志：{}）",
        tap_log.display()
    );

    let text = std::fs::read_to_string(&tap_log).expect("读取 tap 日志");
    let frame = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| {
            value["direction"] == "request" && value["method"] == "session.create"
        })
        .expect("session.create 请求帧");
    let prompt = frame["params"]["system_prompt"]
        .as_str()
        .expect("system_prompt 随 session.create 下发");
    let preset_marker = format!("<!-- aether:preset:{BUILTIN_CODE} -->");
    assert!(prompt.contains(&preset_marker), "真实运行时收到预设块：{prompt}");
    assert!(prompt.contains("你是资深软件工程师"));
    assert!(prompt.contains("# 工作区约定"), "真实运行时收到记忆块：{prompt}");
    assert!(
        prompt.find(&preset_marker).unwrap() < prompt.find("# 工作区约定").unwrap(),
        "下发顺序：预设块 → 记忆块"
    );

    // DoD4 端到端（适配器消费）：真实 Claude Code 适配器把 system_prompt 映射为
    // CLI 原生 `--append-system-prompt-file`，且文件内容与线协议载荷一致。
    let cli_invocations = harness
        .cli_invocations
        .clone()
        .expect("CLI 调用记录路径（fake-claude 观测）");
    let invocations_recorded = || {
        std::fs::read_to_string(&cli_invocations)
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
    };
    assert!(
        wait_until(Duration::from_secs(30), invocations_recorded),
        "fake-claude 必须被真实适配器调用（记录：{}）",
        cli_invocations.display()
    );
    let invocation_text = std::fs::read_to_string(&cli_invocations).expect("读取 CLI 调用记录");
    let invocation: Value = serde_json::from_str(
        invocation_text
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("至少一条 CLI 调用记录"),
    )
    .expect("CLI 调用记录为 JSON");
    let argv: Vec<&str> = invocation["argv"]
        .as_array()
        .expect("argv 数组")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let flag_index = argv
        .iter()
        .position(|arg| *arg == "--append-system-prompt-file")
        .expect("适配器必须传 --append-system-prompt-file");
    let prompt_file = argv
        .get(flag_index + 1)
        .expect("--append-system-prompt-file 缺少文件参数");
    let cli_prompt = std::fs::read_to_string(prompt_file).expect("读取物化的系统提示词文件");
    assert_eq!(cli_prompt, prompt, "CLI 系统提示词文件必须与线协议载荷一致");
    assert!(
        !argv.contains(&"--append-system-prompt"),
        "不得同时使用内联变体（避免重复注入）"
    );
    evidence(
        "dod4_real_runtime_wire",
        &serde_json::json!({
            "runtime": "claude-code",
            "adapter": bundle.to_string_lossy(),
            "transport": "JSON-RPC over JSON-Lines（rpc-tap 透明代理观测）",
            "method": "session.create",
            "preset_marker": preset_marker,
            "memory_block_present": prompt.contains("# 工作区约定"),
            "order_preset_before_memory": true,
            "system_prompt": prompt,
            "cli_consumer": {
                "flag": "--append-system-prompt-file",
                "prompt_file_matches_wire": cli_prompt == prompt,
                "inline_variant_absent": true,
            },
        }),
    );

    harness.shutdown();
}

/// DoD4：合成提示词经 `session.create` 下发**真实 Codex 适配器**，并由适配器映射为
/// CLI `-c developer_instructions=…`（fake-codex 观测 argv；§4.3）。
///
/// opt-in（`AETHER_M7_09_REAL=1` + `AETHER_CODEX_ADAPTER=<bundle>`）；缺失时显式 SKIP。
#[test]
fn real_codex_adapter_passes_composed_prompt_to_cli() {
    if std::env::var("AETHER_M7_09_REAL").as_deref() != Ok("1") {
        eprintln!("SKIP：AETHER_M7_09_REAL 未设置（opt-in 真实运行时断言；Codex 消费链路）");
        return;
    }
    let Some(bundle) = std::env::var_os("AETHER_CODEX_ADAPTER").map(PathBuf::from) else {
        eprintln!("SKIP：AETHER_CODEX_ADAPTER 未设置（Codex 消费链路断言）");
        return;
    };
    let harness = Harness::open_real_codex_adapter(&bundle);
    std::fs::write(
        harness.workspace.path().join("AGENTS.md"),
        "工作区约定内容",
    )
    .expect("写入记忆文件");
    harness.bind_workspace(harness.workspace.path());

    let summary = harness.create_session_for("codex", Some(BUILTIN_REVIEW));
    assert_eq!(summary["preset_id"], BUILTIN_REVIEW);
    let session_id = summary["id"].as_str().unwrap().to_owned();
    let _ = harness.backend.session_send(&SessionSendRequest {
        session_id,
        text: "hi".to_owned(),
        client_msg_id: ulid::Ulid::new().to_string(),
        thinking_depth: None,
        references: Vec::new(),
    });

    // 线协议侧：适配器收到合成提示词（预设块 → 记忆块）。
    let tap_log = harness.tap_log.clone().expect("tap log 路径");
    assert!(
        wait_until(Duration::from_secs(30), || {
            std::fs::read_to_string(&tap_log)
                .map(|text| text.contains("\"method\":\"session.create\""))
                .unwrap_or(false)
        }),
        "真实 Codex 适配器必须收到 session.create（tap：{}）",
        tap_log.display()
    );
    let text = std::fs::read_to_string(&tap_log).expect("读取 tap 日志");
    let frame = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["direction"] == "request" && value["method"] == "session.create")
        .expect("session.create 请求帧");
    let prompt = frame["params"]["system_prompt"]
        .as_str()
        .expect("system_prompt 随 session.create 下发");
    let preset_marker = format!("<!-- aether:preset:{BUILTIN_REVIEW} -->");
    assert!(prompt.contains(&preset_marker), "Codex 收到预设块：{prompt}");
    assert!(prompt.contains("# 工作区约定"), "Codex 收到记忆块：{prompt}");

    // CLI 侧：适配器把提示词映射为 `-c developer_instructions=<TOML 字符串>`。
    let cli_invocations = harness
        .cli_invocations
        .clone()
        .expect("CLI 调用记录路径（fake-codex 观测）");
    assert!(
        wait_until(Duration::from_secs(30), || {
            std::fs::read_to_string(&cli_invocations)
                .map(|text| !text.trim().is_empty())
                .unwrap_or(false)
        }),
        "fake-codex 必须被真实适配器调用（记录：{}）",
        cli_invocations.display()
    );
    let invocation_text = std::fs::read_to_string(&cli_invocations).expect("读取 CLI 调用记录");
    let invocation: Value = serde_json::from_str(
        invocation_text
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("至少一条 CLI 调用记录"),
    )
    .expect("CLI 调用记录为 JSON");
    let argv: Vec<&str> = invocation["argv"]
        .as_array()
        .expect("argv 数组")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let encoded = argv
        .iter()
        .find_map(|arg| arg.strip_prefix("developer_instructions="))
        .expect("Codex CLI 必须收到 -c developer_instructions=…");
    // 值为 JSON 基本字符串（与 TOML 基本字符串等价）：直接按 JSON 字符串解码。
    let decoded: String =
        serde_json::from_str(encoded).expect("developer_instructions 为 TOML/JSON 基本字符串编码");
    assert_eq!(decoded, prompt, "CLI developer_instructions 必须与线协议载荷一致");
    assert!(
        !invocation["prompt"]
            .as_str()
            .unwrap_or_default()
            .contains("aether-system-prompt"),
        "未超长时不得走前缀块回退"
    );
    evidence(
        "dod4_real_runtime_codex",
        &serde_json::json!({
            "runtime": "codex",
            "adapter": bundle.to_string_lossy(),
            "transport": "JSON-RPC over JSON-Lines（rpc-tap 透明代理观测）",
            "method": "session.create",
            "preset_marker": preset_marker,
            "memory_block_present": prompt.contains("# 工作区约定"),
            "system_prompt": prompt,
            "cli_consumer": {
                "config_override": "developer_instructions",
                "decoded_matches_wire": decoded == prompt,
                "fallback_prefix_block_absent": true,
            },
        }),
    );

    harness.shutdown();
}

/// DoD3：`PresetId` 往返解析（命令层 ULID 校验的前置不变量）。
#[test]
fn preset_id_round_trip() {
    let id = PresetId::new(BUILTIN_DEFAULT).unwrap();
    assert_eq!(id.as_str(), BUILTIN_DEFAULT);
    assert!(PresetId::new("").is_err());
    assert_eq!(RuntimeId::new("mock").unwrap().as_str(), "mock");
}
