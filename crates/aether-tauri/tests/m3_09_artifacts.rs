//! M3-09 集成测试：文件引用面板后端（ADR-010 决策 1；只读引用，不列目录/不预览）。
//!
//! 覆盖实施计划 M3-09 DoD1–3/DoD6 的 IPC 后端面：
//! - DoD1：迁移 0003 已应用（`schema_version = 3`；表/约束行为见
//!   `aether-store/tests/m3_09_artifacts.rs`）；引用写路径经单写队列（D3）；
//! - DoD2：`artifact_add` canonicalize + 可访问性检查（失败 `artifact_path_rejected`；
//!   合法路径不误拒）/ 文件与目录 `kind` 探测 / 重复添加幂等（返回既有引用）；
//! - DoD3：`artifact_remove` 幂等（不存在 `removed=false`）；`artifacts_list`
//!   排序（`created_at` 升序）与形状断言；跨重启保留（关闭后重开列表一致）；
//! - DoD6：引用操作不产生事件（`events` 行数不变）、不写 `workspaces`
//!   （不触发 `workspace_set` 语义）；`artifacts_list` 为只读。
//!
//! `ref_pick` 命令层（MockRuntime 注入替身）见 `m3_09_ref_pick.rs`。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;

use aether_core::{
    Runtime, RuntimeId, RuntimeStatus, Session, SessionId, SessionStatus, TokenUsage,
};
use aether_store::WriteQueueConfig;
use aether_store::{ReadPool, StoreCommand, StoreOutcome, StoreRuntime, WriteQueue};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{ArtifactAddRequest, ArtifactRemoveRequest, ArtifactsListRequest};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

const SESSION: &str = "01J0000000000000000000000S";

/// 证据归档（`AETHER_M3_09_EVIDENCE_DIR`；供 Gate 3 逐条出示）。
fn evidence(name: &str, value: &Value) {
    println!("[m3-09] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_09_EVIDENCE_DIR") else {
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

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn runtime_record() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: vec![],
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn session_record() -> Session {
    Session {
        id: SessionId::new(SESSION).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: "M3-09 引用会话".to_owned(),
        status: SessionStatus::Idle,
        model: None,
        thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
    }
}

struct Harness {
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    reads: ReadPool,
    write: WriteQueue,
    backend: Arc<SessionBackend>,
    storage: Option<StoreRuntime>,
}

fn open_backend(
    dir: &TempDir,
    runtime: &tokio::runtime::Runtime,
) -> (ReadPool, WriteQueue, Arc<SessionBackend>, StoreRuntime) {
    let handle = runtime.handle().clone();
    let storage = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &handle,
    )
    .expect("打开存储运行时");
    let reads = storage.reads().clone();
    let write = storage.queue().clone();
    let backend = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            None,
            None,
            Some(reads.clone()),
            None,
            handle,
        )
        .with_workspace_store(write.clone()),
    );
    (reads, write, backend, storage)
}

fn harness() -> Harness {
    let dir = TempDir::new().expect("临时数据目录");
    let runtime = new_runtime();
    let (reads, write, backend, storage) = open_backend(&dir, &runtime);
    runtime
        .block_on(write.execute(StoreCommand::EnsureRuntime {
            runtime: runtime_record(),
        }))
        .expect("运行时登记");
    runtime
        .block_on(write.execute(StoreCommand::InsertSession {
            session: session_record(),
        }))
        .expect("会话登记");
    Harness {
        dir,
        runtime,
        reads,
        write,
        backend,
        storage: Some(storage),
    }
}

impl Harness {
    /// 命令层同口径：canonicalize + 可访问性检查后调用后端。
    fn add(&self, path: &std::path::Path) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        let request = ArtifactAddRequest {
            session_id: SESSION.to_owned(),
            path: path.to_string_lossy().to_string(),
        };
        let resolved = request.resolve_path()?;
        self.backend.artifact_add(&request, &resolved)
    }

    fn list(&self) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.artifacts_list(&ArtifactsListRequest {
            session_id: SESSION.to_owned(),
        })
    }

    fn remove(&self, artifact_id: &str) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.artifact_remove(&ArtifactRemoveRequest {
            session_id: SESSION.to_owned(),
            artifact_id: artifact_id.to_owned(),
        })
    }

    fn shutdown_storage(&mut self) {
        let storage = self.storage.take().expect("存储运行时句柄");
        self.runtime
            .block_on(storage.shutdown())
            .expect("存储关闭序列");
    }

    fn reopen(&mut self) {
        let (reads, write, backend, storage) = open_backend(&self.dir, &self.runtime);
        self.reads = reads;
        self.write = write;
        self.backend = backend;
        self.storage = Some(storage);
    }
}

fn write_file(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("写测试文件");
    path
}

/// DoD1：迁移 0003 已应用（引用写路径经单写队列在 store 测试以计数断言锁定）。
#[test]
fn dod1_migration_0003_is_applied() {
    let h = harness();
    let summary = h.runtime.block_on(h.reads.store_summary()).expect("库摘要");
    assert_eq!(
        summary.schema_version,
        Some(3),
        "迁移必须到 0003（ADR-010）"
    );
    evidence(
        "dod1_migration_0003",
        &json!({ "schema_version": summary.schema_version }),
    );
}

/// DoD2：canonicalize + 可访问性检查 / kind 探测 / 幂等。
#[test]
fn dod2_artifact_add_canonicalizes_probes_kind_and_is_idempotent() {
    let h = harness();
    let file = write_file(h.dir.path(), "ref.txt", "hello");
    let folder = h.dir.path().join("refs");
    std::fs::create_dir_all(&folder).expect("建目录");

    let added = h.add(&file).expect("合法文件必须被接受（不误拒）");
    let keys: Vec<&str> = added
        .as_object()
        .expect("对象形状")
        .keys()
        .map(String::as_str)
        .collect();
    let mut sorted_keys = keys.clone();
    sorted_keys.sort_unstable();
    assert_eq!(
        sorted_keys,
        vec!["created_at", "id", "kind", "path", "size_bytes"],
        "响应形状 = ADR-010 附录 B.1 列表元素（不含 session_id）"
    );
    assert_eq!(added["kind"], "file");
    assert_eq!(added["size_bytes"], 5, "文件字节数探测");
    let canonical = added["path"].as_str().expect("path");
    assert!(
        canonical.ends_with("ref.txt"),
        "canonicalize 后路径：{canonical}"
    );
    let artifact_id = added["id"].as_str().expect("id").to_owned();

    let directory = h.add(&folder).expect("合法目录必须被接受（附加文件夹）");
    assert_eq!(directory["kind"], "directory");
    assert_eq!(
        directory["size_bytes"],
        Value::Null,
        "目录 size_bytes 为 null"
    );

    // 重复添加幂等：返回既有引用（同 id / created_at），列表不新增行。
    let again = h.add(&file).expect("重复添加必须幂等");
    assert_eq!(again["id"], artifact_id, "重复添加返回既有引用 id");
    assert_eq!(again["created_at"], added["created_at"]);
    let list = h.list().expect("列表");
    let items = list["artifacts"].as_array().expect("artifacts 数组");
    assert_eq!(items.len(), 2, "重复添加不得新增行");

    // 合法但形态特殊的名字（含 ~1）不得被静默误拒（Windows 短名形态在真实文件上合法）。
    let tilde = write_file(h.dir.path(), "archive~1.txt", "x");
    h.add(&tilde).expect("合法路径不误拒（含 ~1 的文件名）");

    evidence(
        "dod2_add_idempotent",
        &json!({
            "added_file": added,
            "added_directory": directory,
            "duplicate_id_equal": again["id"] == added["id"],
            "list_len_after_duplicate": items.len(),
        }),
    );
}

/// DoD2：路径拒绝矩阵（canonicalize 失败 / 相对路径 / 不存在 / 非法 session）。
#[test]
fn dod2_artifact_add_rejects_unusable_paths_with_dedicated_code() {
    let h = harness();
    let missing = h.dir.path().join("missing.txt");
    let missing_error = h.add(&missing).expect_err("不存在的路径必须拒绝");
    assert_eq!(
        missing_error.code,
        IpcErrorCode::ArtifactPathRejected,
        "{missing_error}"
    );
    assert!(
        missing_error.message.contains("canonicalize"),
        "拒绝原因应可读：{missing_error}"
    );

    let relative = ArtifactAddRequest {
        session_id: SESSION.to_owned(),
        path: "relative.txt".to_owned(),
    };
    let relative_error = relative.resolve_path().expect_err("相对路径必须拒绝");
    assert_eq!(
        relative_error.code,
        IpcErrorCode::ArtifactPathRejected,
        "{relative_error}"
    );

    let unknown_session = ArtifactAddRequest {
        session_id: "01J0000000000000000000000X".to_owned(),
        path: h.dir.path().to_string_lossy().to_string(),
    };
    let resolved = unknown_session.resolve_path().expect("路径本身合法");
    let unknown_error = h
        .backend
        .artifact_add(&unknown_session, &resolved)
        .expect_err("不存在会话必须拒绝");
    assert_eq!(
        unknown_error.code,
        IpcErrorCode::InvalidValue,
        "{unknown_error}"
    );
    assert!(unknown_error.message.contains("不存在"), "{unknown_error}");

    // artifacts_list：不存在会话同样 invalid_value（ADR-010 附录 B.1）。
    let list_error = h
        .backend
        .artifacts_list(&ArtifactsListRequest {
            session_id: "01J0000000000000000000000X".to_owned(),
        })
        .expect_err("不存在会话的列表必须拒绝");
    assert_eq!(list_error.code, IpcErrorCode::InvalidValue, "{list_error}");

    evidence(
        "dod2_path_rejected",
        &json!({
            "missing_path_code": missing_error.code.as_str(),
            "relative_code": relative_error.code.as_str(),
            "unknown_session_code": unknown_error.code.as_str(),
            "unknown_session_list_code": list_error.code.as_str(),
        }),
    );
}

/// DoD3：删除幂等 / 列表排序 / 跨重启保留。
#[test]
fn dod3_remove_is_idempotent_list_sorted_and_survives_restart() {
    let mut h = harness();
    let first = write_file(h.dir.path(), "a.txt", "a");
    let second = write_file(h.dir.path(), "b.txt", "b");
    let folder = h.dir.path().join("c-dir");
    std::fs::create_dir_all(&folder).expect("建目录");
    let first_id = h.add(&first).unwrap()["id"].as_str().unwrap().to_owned();
    let second_id = h.add(&second).unwrap()["id"].as_str().unwrap().to_owned();
    let third_id = h.add(&folder).unwrap()["id"].as_str().unwrap().to_owned();

    let list = h.list().expect("列表");
    let items = list["artifacts"].as_array().expect("artifacts 数组");
    assert_eq!(items.len(), 3);
    let created: Vec<i64> = items
        .iter()
        .map(|item| item["created_at"].as_i64().expect("created_at"))
        .collect();
    let mut sorted = created.clone();
    sorted.sort_unstable();
    assert_eq!(created, sorted, "列表按 created_at 升序");
    assert!(
        items[0]["kind"].as_str().is_some() && items[2]["size_bytes"].is_null(),
        "形状：directory 项 size_bytes 为 null"
    );

    // 删除幂等：命中 → true；再次 → false。
    assert_eq!(h.remove(&second_id).unwrap()["removed"], true);
    assert_eq!(h.remove(&second_id).unwrap()["removed"], false);
    assert_eq!(
        h.remove("01J000000000000000000000ZZ").unwrap()["removed"],
        false
    );

    // 跨重启保留：关闭存储运行时 → 重开 → 列表一致（v/顺序/形状）。
    let before = h.list().expect("重启前列表");
    h.shutdown_storage();
    h.reopen();
    let after = h.list().expect("重启后列表");
    assert_eq!(after, before, "重启后引用列表必须一致");
    let remaining: Vec<&str> = after["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(remaining, vec![first_id.as_str(), third_id.as_str()]);

    evidence(
        "dod3_list_restart",
        &json!({
            "sorted_created_at": created,
            "remove_idempotent": [true, false, false],
            "restart_list_equal": after == before,
            "remaining_ids": remaining,
        }),
    );
}

/// DoD1：引用写路径经单写队列（提交计数 +1/+1）。
#[test]
fn dod1_artifact_writes_go_through_single_write_queue() {
    let h = harness();
    let file = write_file(h.dir.path(), "queue.txt", "q");
    let committed_before = h.write.metrics().committed_entries;
    let added = h.add(&file).expect("添加");
    let removed = h.remove(added["id"].as_str().unwrap()).expect("删除");
    assert_eq!(removed["removed"], true);
    assert_eq!(
        h.write.metrics().committed_entries,
        committed_before + 2,
        "artifact_add/artifact_remove 必须经单写队列（D3）"
    );
    let committed_delta = h.write.metrics().committed_entries - committed_before;
    let idempotent = match h
        .runtime
        .block_on(h.write.execute(StoreCommand::RemoveArtifact {
            session_id: SessionId::new(SESSION).unwrap(),
            artifact_id: "01J000000000000000000000ZZ".to_owned(),
        }))
        .expect("幂等删除")
    {
        StoreOutcome::Applied { affected } => affected,
        other => panic!("期望 Applied，实际 {other:?}"),
    };
    assert_eq!(idempotent, 0);
    evidence(
        "dod1_write_queue",
        &json!({
            "committed_delta": committed_delta,
            "idempotent_remove_affected": idempotent,
        }),
    );
}

/// DoD6：引用操作不产生事件、不写 `workspaces`（不触发 `workspace_set` 语义）。
#[test]
fn dod6_artifacts_emit_no_events_and_do_not_bind_workspace() {
    let h = harness();
    let session_id = SessionId::new(SESSION).unwrap();
    let events_before = h
        .runtime
        .block_on(h.reads.event_count(&session_id))
        .expect("事件计数");
    let file = write_file(h.dir.path(), "silent.txt", "s");
    let added = h.add(&file).expect("添加");
    let _ = h.list().expect("列表");
    h.remove(added["id"].as_str().unwrap()).expect("删除");
    let events_after = h
        .runtime
        .block_on(h.reads.event_count(&session_id))
        .expect("事件计数");
    assert_eq!(
        events_after, events_before,
        "引用操作不得产生事件（事件表行数不变）"
    );

    assert_eq!(
        h.runtime
            .block_on(h.reads.workspaces_latest())
            .expect("工作区读取"),
        None,
        "引用操作不得写 workspaces（不触发 workspace_set 语义）"
    );
    let session = h
        .runtime
        .block_on(h.reads.session(&session_id))
        .expect("会话读取")
        .expect("会话存在");
    assert_eq!(
        session.workspace_id, None,
        "会话工作区绑定不得被引用操作改写"
    );

    evidence(
        "dod6_no_side_effects",
        &json!({
            "events_before": events_before,
            "events_after": events_after,
            "workspace_bound": false,
            "session_workspace_id": session.workspace_id,
        }),
    );
}
