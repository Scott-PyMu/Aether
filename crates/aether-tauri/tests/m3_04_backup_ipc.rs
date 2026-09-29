//! M3-04 集成测试：备份/恢复命令面（设计 D13、ADR-004；实施计划 v1.18 §4）。
//!
//! 覆盖：
//! - DoD5：备份到外部路径（系统选择器 canonicalize 结果）→ 可写 + 空间护栏
//!   （可用空间 ≥ `db+wal` ×1.2 才写出；不足拒绝并提示）；产物可被恢复七步消费；
//! - DoD6：`backup_list` 内部清单 + 容量状态；`backup_restore` 参数/候选校验
//!   （内部 id 枚举 / 外部 canonicalize；高版本拒绝）→ 恢复请求登记（`restart_required`）
//!   → 启动序列执行七步 3–6（`boot_apply_pending_restore`，含核心重启语义）→ 审计；
//! - DoD1（T9 命令链）：备份 → 清库 → 恢复 → 一致（行数 + 哈希抽查）。
//!
//! 命令层（tauri mock invoke）覆盖严格解析与路径 canonicalize 拒绝；后端直连覆盖
//! 空间护栏与恢复链路（与 M3-03 的分层口径一致）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aether_store::backup::{program_schema_version, read_restore_journal, validate_candidate};
use aether_store::{Store, StoreRuntime, WriteQueueConfig};
use aether_tauri::backup_control::{
    boot_apply_pending_restore, write_boot_restore_audit, BackupControlBackend, SpaceProbe,
};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{BackupCreateRequest, BackupRestoreRequest, BackupSource};
use aether_tauri::ipc::{handler, IpcState};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};
use tempfile::TempDir;

#[cfg(windows)]
const INVOKE_URL: &str = "http://tauri.localhost";
#[cfg(not(windows))]
const INVOKE_URL: &str = "tauri://localhost";

// ===== 通用夹具 =====

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn seed_database(path: &Path, rows: i64) {
    let store = Store::open(path).expect("打开/迁移数据库");
    {
        let connection = store.connection();
        connection
            .execute_batch(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
            )
            .expect("插入运行时");
        for index in 0..rows {
            connection
                .execute_batch(&format!(
                    "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                     VALUES ('01J{index:023}', 'mock', '会话 {index}', 'idle', {created}, {created});",
                    index = index,
                    created = 1_700_000_000_000i64 + index
                ))
                .expect("插入会话");
        }
    }
}

fn fingerprint(path: &Path) -> String {
    let store = Store::open(path).expect("指纹连接");
    let connection = store.connection();
    connection
        .execute_batch("PRAGMA query_only = ON;")
        .expect("只读查询");
    let mut hasher = Sha256::new();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .expect("计数");
    hasher.update(format!("sessions:{count};").as_bytes());
    let mut statement = connection
        .prepare("SELECT id, title FROM sessions ORDER BY id")
        .expect("抽查语句");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("抽查");
    for row in rows {
        let (id, title) = row.expect("行");
        hasher.update(format!("{id}\u{1f}{title}\u{1e}").as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn remove_database(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if candidate.exists() {
            std::fs::remove_file(&candidate).expect("删除");
        }
    }
}

struct BackendFixture {
    temp: TempDir,
    runtime: tokio::runtime::Runtime,
    backend: Arc<BackupControlBackend>,
    storage: Option<StoreRuntime>,
}

impl BackendFixture {
    fn open() -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        seed_database(&temp.path().join("aether.db"), 12);
        let runtime = new_runtime();
        let handle = runtime.handle().clone();
        let storage = StoreRuntime::open(
            temp.path().join("aether.db"),
            WriteQueueConfig::default(),
            &handle,
        )
        .expect("打开存储运行时");
        let backend = Arc::new(BackupControlBackend::new(
            Arc::new(NotImplementedBackend),
            temp.path().to_path_buf(),
            Some(storage.reads().clone()),
            Some(storage.queue().clone()),
            handle,
        ));
        Self {
            temp,
            runtime,
            backend,
            storage: Some(storage),
        }
    }

    fn data_dir(&self) -> &Path {
        self.temp.path()
    }

    fn db_path(&self) -> PathBuf {
        self.temp.path().join("aether.db")
    }

    fn with_space_probe(mut self, probe: Arc<dyn SpaceProbe>) -> Self {
        let backend = BackupControlBackend::new(
            Arc::new(NotImplementedBackend),
            self.temp.path().to_path_buf(),
            self.storage.as_ref().map(|storage| storage.reads().clone()),
            self.storage.as_ref().map(|storage| storage.queue().clone()),
            self.runtime.handle().clone(),
        )
        .with_space_probe(probe);
        self.backend = Arc::new(backend);
        self
    }

    /// 模拟应用关闭（重启前的退出序列）。
    fn shutdown_storage(&mut self) {
        if let Some(storage) = self.storage.take() {
            self.runtime.block_on(storage.shutdown()).expect("存储关闭");
        }
    }

    fn create(&self, target: Option<&Path>) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        let request = BackupCreateRequest {
            label: Some("测试备份".to_owned()),
            target_dir: target.map(|path| path.to_string_lossy().to_string()),
        };
        let canonical = request.canonical_target_dir().expect("目标目录校验");
        self.backend.backup_create(&request, canonical.as_deref())
    }

    fn restore_internal(&self, id: &str) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        let request = BackupRestoreRequest {
            source: BackupSource::Internal { id: id.to_owned() },
        };
        self.backend.backup_restore(&request, None)
    }

    fn restore_external(&self, path: &Path) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        let request = BackupRestoreRequest {
            source: BackupSource::External {
                path: path.to_string_lossy().to_string(),
            },
        };
        let canonical = request.canonical_external_path().expect("外部候选校验");
        self.backend.backup_restore(&request, canonical.as_deref())
    }
}

/// 固定空间探针（覆盖空间护栏分支）。
struct FixedSpaceProbe {
    writable: bool,
    available: u64,
}

impl SpaceProbe for FixedSpaceProbe {
    fn ensure_writable(&self, _dir: &Path) -> Result<(), String> {
        if self.writable {
            Ok(())
        } else {
            Err("注入：目录不可写".to_owned())
        }
    }

    fn available_bytes(&self, _dir: &Path) -> Result<u64, String> {
        Ok(self.available)
    }
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m3-04] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_04_EVIDENCE_DIR") else {
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

// ===== DoD5/DoD6：创建（内部/外部）与空间护栏 =====

#[test]
fn create_internal_and_external_respects_space_guard() {
    let fixture = BackendFixture::open();

    // 内部（默认 backups 目录）。
    let internal = fixture.create(None).expect("内部备份成功");
    let internal_path = PathBuf::from(internal["backup"]["path"].as_str().expect("路径"));
    assert_eq!(internal["backup"]["kind"], "internal");
    assert!(internal_path.starts_with(fixture.data_dir().join("backups")));
    assert!(internal_path.exists());
    assert!(internal["backup"]["size_bytes"].as_u64().unwrap_or(0) > 0);

    // 外部（系统选择器返回原始路径；命令层 canonicalize）。
    let external_dir = fixture.data_dir().join("external-target");
    std::fs::create_dir_all(&external_dir).expect("外部目录");
    let external = fixture.create(Some(&external_dir)).expect("外部备份成功");
    assert_eq!(external["backup"]["kind"], "external");
    let external_path = PathBuf::from(external["backup"]["path"].as_str().expect("路径"));
    assert!(
        external_path.starts_with(&external_dir),
        "产物必须落在选择的外部目录：{}",
        external_path.display()
    );
    assert!(external_path.exists());

    // `backup_list`：清单 + 容量状态（D13 2GB/5GB 阈值口径）。
    let list = fixture.backend.backup_list().expect("备份清单");
    let backups = list["backups"].as_array().expect("数组");
    assert_eq!(backups.len(), 2);
    assert_eq!(list["capacity"]["level"], "ok");
    assert!(
        list["capacity"]["total_bytes"].as_u64().unwrap_or(0) > 0,
        "容量必须统计 db+wal"
    );
    write_evidence(
        "dod5_create_list",
        &json!({
            "internal": internal,
            "external": external,
            "list": list,
        }),
    );

    // 空间不足：注入可用空间 < 需求 → 拒绝且不写出（ADR-003 决策 19）。
    let fresh = BackendFixture::open().with_space_probe(Arc::new(FixedSpaceProbe {
        writable: true,
        available: 0,
    }));
    let before_files = std::fs::read_dir(fresh.data_dir().join("backups"))
        .map(|listing| listing.count())
        .unwrap_or(0);
    let error = fresh.create(None).expect_err("空间不足必须拒绝");
    assert_eq!(error.code.as_str(), "invalid_value");
    assert!(
        error.message.contains("backup_space_insufficient"),
        "必须携带稳定业务码：{error}"
    );
    let after_files = std::fs::read_dir(fresh.data_dir().join("backups"))
        .map(|listing| listing.count())
        .unwrap_or(0);
    assert_eq!(before_files, after_files, "拒绝后不得写出备份文件");
    write_evidence(
        "dod5_space_insufficient",
        &json!({ "code": error.code.as_str(), "message": error.message }),
    );

    // 不可写：注入可写校验失败 → 拒绝。
    let unwritable = BackendFixture::open().with_space_probe(Arc::new(FixedSpaceProbe {
        writable: false,
        available: u64::MAX,
    }));
    let error = unwritable.create(None).expect_err("不可写必须拒绝");
    assert!(
        error.message.contains("backup_target_not_writable"),
        "必须携带稳定业务码：{error}"
    );
}

// ===== DoD1/DoD6：恢复请求 → 启动序列（含核心重启语义）→ 一致 + 审计 =====

#[test]
fn restore_via_pending_restart_replaces_database_and_audits() {
    let mut fixture = BackendFixture::open();
    let db = fixture.db_path();
    let before = fingerprint(&db);

    let created = fixture.create(None).expect("创建备份");
    let backup_id = created["backup"]["id"].as_str().expect("id").to_owned();

    // 命令面：恢复请求（内部 id）→ 重启前只登记现场日志。
    let response = fixture.restore_internal(&backup_id).expect("恢复请求登记");
    assert_eq!(response["restoring"], true);
    assert_eq!(response["restart_required"], true);
    assert_eq!(response["source"], "internal");
    assert_eq!(
        response["candidate"]["schema_version"].as_i64(),
        Some(program_schema_version())
    );
    let journal = read_restore_journal(&db)
        .expect("读日志")
        .expect("日志存在");
    assert_eq!(journal.status, "requested");

    // 「核心重启」：退出序列关闭存储 → 清库（模拟数据丢失）→ 启动序列执行七步 3–6。
    fixture.shutdown_storage();
    remove_database(&db);
    assert!(!db.exists());

    let outcome = boot_apply_pending_restore(fixture.data_dir()).expect("启动处理");
    match &outcome {
        aether_store::backup::RestoreStartupOutcome::Applied(report) => {
            assert!(report.applied);
            assert_eq!(report.verified_schema_version, program_schema_version());
        }
        other => panic!("必须为 Applied：{other:?}"),
    }
    assert_eq!(fingerprint(&db), before, "恢复后与备份时一致（行数+哈希）");
    assert!(read_restore_journal(&db).unwrap().is_none(), "日志已清理");

    // 重启后的核心：写恢复结果审计（D13 第 7 步）。
    let runtime = new_runtime();
    let storage = StoreRuntime::open(&db, WriteQueueConfig::default(), runtime.handle())
        .expect("重启后打开存储");
    write_boot_restore_audit(storage.queue(), runtime.handle(), &outcome);
    let audit = runtime
        .block_on(storage.reads().audit_log(10))
        .expect("审计查询");
    let restore_audit = audit
        .iter()
        .find(|record| record.action == "backup.restore")
        .expect("恢复审计必须落库");
    assert_eq!(restore_audit.result.as_deref(), Some("completed"));
    write_evidence(
        "dod1_t9_restore_chain",
        &json!({
            "before_fingerprint": before,
            "after_fingerprint": fingerprint(&db),
            "restore_response": response,
            "audit": { "action": restore_audit.action, "result": restore_audit.result, "ts": restore_audit.ts },
        }),
    );
    runtime.block_on(storage.shutdown()).expect("关闭");
}

/// 产物可被恢复七步消费：外部备份（经 `backup_create.target_dir`）作为恢复候选。
#[test]
fn external_backup_product_is_consumable_by_restore() {
    let mut fixture = BackendFixture::open();
    let db = fixture.db_path();
    let before = fingerprint(&db);

    let external_dir = fixture.data_dir().join("ext");
    std::fs::create_dir_all(&external_dir).expect("外部目录");
    let created = fixture.create(Some(&external_dir)).expect("外部备份");
    let file = PathBuf::from(created["backup"]["path"].as_str().expect("路径"));

    // 候选校验（第 2 步）可直接消费外部产物。
    let candidate = validate_candidate(&file, program_schema_version()).expect("候选可消费");
    assert_eq!(candidate.schema_version, program_schema_version());

    let response = fixture.restore_external(&file).expect("外部恢复请求");
    assert_eq!(response["source"], "external");
    assert_eq!(response["restart_required"], true);

    fixture.shutdown_storage();
    remove_database(&db);
    let outcome = boot_apply_pending_restore(fixture.data_dir()).expect("启动处理");
    assert!(matches!(
        outcome,
        aether_store::backup::RestoreStartupOutcome::Applied(_)
    ));
    assert_eq!(fingerprint(&db), before);
}

// ===== DoD3/DoD6：候选拒绝（高版本 / 损坏 / 不存在 id） =====

#[test]
fn restore_rejects_newer_corrupt_and_unknown_candidates() {
    let fixture = BackendFixture::open();
    let db = fixture.db_path();

    // 高版本候选：schema 版本高于当前程序 → 提示升级程序。
    let newer = fixture.data_dir().join("newer.db");
    seed_database(&newer, 1);
    {
        let store = Store::open(&newer).expect("打开候选");
        store
            .connection()
            .execute(
                "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, 'x', 1)",
                [program_schema_version() + 1],
            )
            .expect("注入高版本");
    }
    let error = fixture.restore_external(&newer).expect_err("高版本拒绝");
    assert!(
        error.message.contains("backup_candidate_newer"),
        "必须提示升级程序：{error}"
    );
    assert!(
        read_restore_journal(&db).unwrap().is_none(),
        "拒绝后不得登记恢复日志"
    );

    // 损坏候选。
    let corrupt = fixture.data_dir().join("corrupt.db");
    std::fs::write(&corrupt, b"not a database").expect("写损坏文件");
    let error = fixture.restore_external(&corrupt).expect_err("损坏拒绝");
    assert!(
        error.message.contains("backup_candidate_invalid"),
        "损坏候选业务码：{error}"
    );

    // 不存在的内部 id。
    let error = fixture
        .restore_internal("01J000000000000000000000Z9")
        .expect_err("未知 id 拒绝");
    assert!(
        error.message.contains("backup_not_found"),
        "未知 id 业务码：{error}"
    );

    write_evidence(
        "dod3_candidate_rejections",
        &json!({
            "newer": "backup_candidate_newer",
            "corrupt": "backup_candidate_invalid",
            "unknown_id": "backup_not_found",
        }),
    );
}

// ===== 保留策略：超过 10 份删除最旧（D13） =====

#[test]
fn retention_keeps_latest_ten_backups() {
    let fixture = BackendFixture::open();
    for index in 0..11 {
        fixture.create(None).expect("备份成功");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _ = index;
    }
    let list = fixture.backend.backup_list().expect("清单");
    let backups = list["backups"].as_array().expect("数组");
    assert_eq!(backups.len(), 10, "保留最近 10 份（D13）");
    assert!(
        list["backups"][0]["created_at"].as_i64().unwrap_or(0)
            >= list["backups"][9]["created_at"].as_i64().unwrap_or(0),
        "清单按最新在前排序"
    );
    write_evidence("retention", &json!({ "kept": backups.len() }));
}

// ===== 命令层（tauri mock invoke）：严格解析 + canonicalize 拒绝 + 重启旗标 =====

/// 记录调用次数的后端（命令层校验失败时下游必须为 0 次）。
struct CountingBackend {
    calls: AtomicUsize,
}

impl IpcBackend for CountingBackend {
    fn backup_create(
        &self,
        _request: &BackupCreateRequest,
        _canonical_target_dir: Option<&Path>,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "backup": { "id": "01J000000000000000000000B1" } }))
    }

    fn backup_list(&self) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "backups": [], "capacity": { "level": "ok" } }))
    }

    fn backup_restore(
        &self,
        _request: &BackupRestoreRequest,
        _canonical_external_path: Option<&Path>,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "restoring": true, "restart_required": true }))
    }
}

struct MockFixture {
    /// 保持 mock 应用存活（不直接读取；webview/state 经其派生）。
    #[allow(dead_code)]
    app: App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    #[allow(dead_code)]
    dir: TempDir,
}

fn mock_fixture(state: IpcState, dir: TempDir) -> MockFixture {
    let app = mock_builder()
        .invoke_handler(handler())
        .manage(state)
        .build(mock_context(noop_assets()))
        .expect("构建 mock 应用");
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("创建 mock webview");
    MockFixture { app, webview, dir }
}

fn invoke(
    webview: &WebviewWindow<MockRuntime>,
    command: &str,
    payload: Value,
) -> Result<Value, Value> {
    let body = if payload.is_null() {
        tauri::ipc::InvokeBody::default()
    } else {
        tauri::ipc::InvokeBody::Json(json!({ "payload": payload }))
    };
    let response = tauri::test::get_ipc_response(
        webview,
        InvokeRequest {
            cmd: command.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: INVOKE_URL.parse().expect("URL"),
            body,
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );
    match response {
        Ok(body) => Ok(body.deserialize::<Value>().unwrap_or(Value::Null)),
        Err(value) => Err(value),
    }
}

#[test]
fn command_layer_rejects_invalid_targets_without_downstream_call() {
    let dir = TempDir::new().expect("临时目录");
    let backend = Arc::new(CountingBackend {
        calls: AtomicUsize::new(0),
    });
    let state = IpcState::new(backend.clone(), Vec::new());
    let fixture = mock_fixture(state, dir);

    // 未知成员（严格解析）。
    let error = invoke(
        &fixture.webview,
        "backup_create",
        json!({ "label": "x", "unknown": 1 }),
    )
    .expect_err("未知成员必须拒绝");
    assert_eq!(error["code"], "unknown_field");

    // 相对路径（canonicalize 拒绝）。
    let error = invoke(
        &fixture.webview,
        "backup_create",
        json!({ "target_dir": "relative\\dir" }),
    )
    .expect_err("相对路径必须拒绝");
    assert_eq!(error["code"], "path_rejected");

    // 不存在的目录。
    let error = invoke(
        &fixture.webview,
        "backup_create",
        json!({ "target_dir": std::env::temp_dir().join("aether-m3-04-missing").to_string_lossy() }),
    )
    .expect_err("不存在目录必须拒绝");
    assert_eq!(error["code"], "path_rejected");

    // 非法内部 id（格式层 ULID）。
    let error = invoke(
        &fixture.webview,
        "backup_restore",
        json!({ "source": { "internal": { "id": "not-a-ulid" } } }),
    )
    .expect_err("非法 id 必须拒绝");
    assert_eq!(error["code"], "invalid_format");

    // 外部候选后缀必须为 .db（canonicalize + 后缀）。
    let dir_path = TempDir::new().expect("候选目录");
    let not_db = dir_path.path().join("candidate.txt");
    std::fs::write(&not_db, b"x").expect("写文件");
    let error = invoke(
        &fixture.webview,
        "backup_restore",
        json!({ "source": { "external": { "path": not_db.to_string_lossy() } } }),
    )
    .expect_err("非 .db 必须拒绝");
    assert_eq!(error["code"], "path_rejected");

    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        0,
        "校验失败不得调用下游（不落库、不透传）"
    );
}

#[test]
fn command_layer_accepts_valid_requests_and_passes_restart_flag() {
    let mut fixture = BackendFixture::open();
    let dir = TempDir::new().expect("临时目录");
    let state = IpcState::new(fixture.backend.clone(), Vec::new());
    let mock = mock_fixture(state, dir);

    // 内部创建 → 下游真实执行（文件出现）。
    let created = invoke(&mock.webview, "backup_create", json!({ "label": "命令层" }))
        .expect("命令层创建成功");
    let path = PathBuf::from(created["backup"]["path"].as_str().expect("路径"));
    assert!(path.exists());

    // 清单。
    let list = invoke(&mock.webview, "backup_list", Value::Null).expect("清单成功");
    assert_eq!(list["backups"].as_array().map(Vec::len), Some(1));

    // 恢复请求 → `restart_required:true`（mock 无应用句柄，不实际重启；日志已登记）。
    let backup_id = created["backup"]["id"].as_str().expect("id").to_owned();
    let response = invoke(
        &mock.webview,
        "backup_restore",
        json!({ "source": { "internal": { "id": backup_id } } }),
    )
    .expect("恢复请求成功");
    assert_eq!(response["restart_required"], true);
    let journal = read_restore_journal(&fixture.db_path())
        .expect("读日志")
        .expect("日志存在");
    assert_eq!(journal.status, "requested");

    // 收尾：关闭存储（避免临时目录句柄残留）。
    fixture.shutdown_storage();
}

/// 生产装饰器链路：`SessionBackend(BackupControlBackend(…))` 必须把备份命令委派到内层
/// （M3-04 修正：外层未覆写的方法不得命中 trait 默认 `not_implemented`）。
#[test]
fn production_chain_delegates_backup_commands() {
    use aether_tauri::session_backend::SessionBackend;

    let fixture = BackendFixture::open();
    let chain: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
        fixture.backend.clone(),
        None,
        None,
        Some(fixture.storage.as_ref().expect("存储").reads().clone()),
        None,
        fixture.runtime.handle().clone(),
    ));

    let created = chain
        .backup_create(
            &BackupCreateRequest {
                label: None,
                target_dir: None,
            },
            None,
        )
        .expect("链路创建备份可达");
    assert!(created["backup"]["path"].as_str().is_some());
    let list = chain.backup_list().expect("链路清单可达");
    assert_eq!(list["backups"].as_array().map(Vec::len), Some(1));
}
