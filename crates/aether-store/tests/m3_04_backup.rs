//! M3-04 DoD1–3：备份/恢复七步、外部候选、现场回滚（D13）。
//!
//! 分层：本文件验证 `aether-store` 恢复引擎（单进程集成）；进程 kill 中断路径见
//! `m3_04_restore_kill.rs`；IPC 命令面见 `aether-tauri/tests/m3_04_backup_ipc.rs`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use aether_store::backup::{
    execute_restore, finalize_restore, program_schema_version, read_restore_journal,
    recover_or_apply_pending_restore, request_restore, validate_candidate, BackupRecord,
    RestoreFault, RestoreRequest, RestoreStartupOutcome, RestoreStep, PRE_RESTORE_INFIX,
};
use aether_store::{Store, StoreCommand, StoreError};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

fn seed_database(path: &Path, rows: i64) -> String {
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
                .execute(
                    "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                     VALUES (?1, 'mock', ?2, 'idle', ?3, ?3)",
                    rusqlite::params![
                        format!("01J00000000000000000000{index:04}"),
                        format!("会话 {index}"),
                        1_700_000_000_000i64 + index
                    ],
                )
                .expect("插入会话");
        }
    }
    fingerprint(path)
}

/// 行数 + 内容哈希抽查（T9 一致性口径）。
fn fingerprint(path: &Path) -> String {
    let connection = Connection::open(path).expect("指纹连接");
    connection
        .execute_batch("PRAGMA query_only = ON;")
        .expect("只读查询");
    let mut hasher = Sha256::new();
    for table in [
        "sessions",
        "runtimes",
        "messages",
        "events",
        "audit_log",
        "backups",
    ] {
        let mut statement = connection
            .prepare(&format!("SELECT COUNT(*) FROM {table}"))
            .expect("计数语句");
        let count: i64 = statement.query_row([], |row| row.get(0)).expect("计数");
        hasher.update(format!("{table}:{count};").as_bytes());
    }
    let mut statement = connection
        .prepare("SELECT id, title, status FROM sessions ORDER BY id")
        .expect("会话抽查语句");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("会话抽查");
    for row in rows {
        let (id, title, status) = row.expect("行");
        hasher.update(format!("{id}\u{1f}{title}\u{1f}{status}\u{1e}").as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn make_backup(source_db: &Path, dest: &Path) {
    let store = Store::open(source_db).expect("打开源库");
    store.backup_to(dest).expect("VACUUM INTO 备份");
}

fn remove_database(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        if candidate.exists() {
            std::fs::remove_file(&candidate).expect("删除");
        }
    }
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", db.display()))
}

fn write_dummy_sidecars(db: &Path) {
    std::fs::write(sidecar(db, "-wal"), b"wal-scene").expect("写 wal");
    std::fs::write(sidecar(db, "-shm"), b"shm-scene").expect("写 shm");
}

fn pre_restore_files(db: &Path) -> Vec<PathBuf> {
    let dir = db.parent().expect("父目录");
    let prefix = format!("{}", db.file_name().expect("文件名").to_string_lossy());
    let mut files = Vec::new();
    for item in std::fs::read_dir(dir).expect("列目录") {
        let item = item.expect("条目");
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && name.contains(PRE_RESTORE_INFIX) {
            files.push(item.path());
        }
    }
    files.sort();
    files
}

/// DoD1：T9 备份 → 清库 → 恢复 → 一致（行数 + 哈希抽查）。
#[test]
fn t9_backup_restore_roundtrip_is_consistent() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("aether.db");
    let before = seed_database(&db, 24);

    let backup = dir.path().join("aether-backup.db");
    make_backup(&db, &backup);

    // 清库：删除库与侧车（模拟数据目录丢失）
    remove_database(&db);
    assert!(!db.exists());

    let report = execute_restore(
        &RestoreRequest {
            db_path: db.clone(),
            candidate_path: backup.clone(),
            program_max_version: program_schema_version(),
            now_ms: 1_700_000_000_500,
            fault: RestoreFault::None,
        },
        || Ok(()),
    )
    .expect("恢复成功");
    finalize_restore(&db).expect("清理 journal");

    assert!(report.applied);
    assert!(!report.rolled_back);
    assert_eq!(report.verified_schema_version, program_schema_version());
    let step_codes: Vec<&str> = report.steps.iter().map(|step| step.step.as_str()).collect();
    assert_eq!(
        step_codes,
        vec![
            "validate_candidate",
            "stop_writes",
            "protect_scene",
            "place_candidate",
            "verify"
        ]
    );
    assert!(report.steps.iter().all(|step| step.ok));

    let after = fingerprint(&db);
    assert_eq!(before, after, "恢复后行数与内容哈希必须一致");
    assert!(read_restore_journal(&db).unwrap().is_none());
}

/// DoD2：七步逐项断言（`-wal`/`-shm` 改名、`.pre-restore` 保留、停写回调）。
#[test]
fn seven_steps_rename_scene_and_retain_pre_restore_files() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("aether.db");
    let before = seed_database(&db, 6);

    let backup = dir.path().join("candidate.db");
    make_backup(&db, &backup);
    write_dummy_sidecars(&db);

    let mut stopped = false;
    let report = execute_restore(
        &RestoreRequest {
            db_path: db.clone(),
            candidate_path: backup,
            program_max_version: program_schema_version(),
            now_ms: 1_700_000_000_700,
            fault: RestoreFault::None,
        },
        || {
            stopped = true;
            Ok(())
        },
    )
    .expect("恢复成功");
    assert!(stopped, "第 3 步停写回调必须被调用");

    let scene = report.scene.clone().expect("现场保护记录");
    assert_eq!(scene.ts, 1_700_000_000_700);
    let db_scene = scene.db.expect("库改名");
    let wal_scene = scene.wal.expect("-wal 改名");
    let shm_scene = scene.shm.expect("-shm 改名");
    assert!(db_scene.exists(), ".pre-restore 库文件保留");
    assert!(wal_scene.exists(), ".pre-restore -wal 保留");
    assert!(shm_scene.exists(), ".pre-restore -shm 保留");
    assert!(db_scene.to_string_lossy().contains(PRE_RESTORE_INFIX));
    assert_eq!(std::fs::read(&wal_scene).unwrap(), b"wal-scene");
    assert_eq!(
        pre_restore_files(&db).len(),
        3,
        "现场三件套完整保留（DoD2）"
    );
    assert_eq!(&before, &fingerprint(&db), "恢复后数据一致");

    // 验收通过后 journal 为 done（由调用方 finalize 清理）。
    let journal = read_restore_journal(&db).unwrap().expect("journal 存在");
    assert_eq!(journal.status, "done");
    finalize_restore(&db).unwrap();
    assert!(read_restore_journal(&db).unwrap().is_none());
}

/// DoD2：失败回滚（现场三件套完整，旧库继续可用）。
#[test]
fn failure_after_placement_rolls_back_scene() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("aether.db");
    let before = seed_database(&db, 8);
    let backup = dir.path().join("candidate.db");
    make_backup(&db, &backup);
    write_dummy_sidecars(&db);

    let error = execute_restore(
        &RestoreRequest {
            db_path: db.clone(),
            candidate_path: backup,
            program_max_version: program_schema_version(),
            now_ms: 1_700_000_000_800,
            fault: RestoreFault::FailAfter {
                step: RestoreStep::PlaceCandidate,
                message: "注入：就位后失败".to_owned(),
            },
        },
        || Ok(()),
    )
    .expect_err("必须失败");
    assert!(
        error.to_string().contains("已回滚现场"),
        "错误必须声明回滚结果：{error}"
    );

    assert!(db.exists(), "旧库已还原");
    assert_eq!(
        std::fs::read(sidecar(&db, "-wal")).unwrap(),
        b"wal-scene",
        "-wal 现场还原"
    );
    assert_eq!(&before, &fingerprint(&db), "回滚后数据与恢复前一致");
    assert!(
        pre_restore_files(&db).is_empty(),
        "回滚后 .pre-restore 文件应被还原（不残留）"
    );
    assert!(
        read_restore_journal(&db).unwrap().is_none(),
        "失败后 journal 清理"
    );
}

/// DoD3：外部候选——高版本拒绝、损坏候选拒绝、正常候选成功。
#[test]
fn external_candidate_validation_matrix() {
    let dir = tempfile::TempDir::new().unwrap();

    // 正常候选：版本 ≤ 程序版本 → 通过。
    let good_db = dir.path().join("good.db");
    seed_database(&good_db, 2);
    let good = validate_candidate(&good_db, program_schema_version()).expect("正常候选通过");
    assert_eq!(good.schema_version, program_schema_version());

    // 高版本拒绝（提示升级程序）。
    let newer = dir.path().join("newer.db");
    seed_database(&newer, 1);
    {
        let connection = Connection::open(&newer).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, 'x', 1)",
                [(program_schema_version() + 1)],
            )
            .unwrap();
    }
    let error = validate_candidate(&newer, program_schema_version()).expect_err("高版本拒绝");
    match error {
        StoreError::SchemaNewerThanProgram { database, program } => {
            assert_eq!(database, program_schema_version() + 1);
            assert_eq!(program, program_schema_version());
        }
        other => panic!("必须为 SchemaNewerThanProgram：{other:?}"),
    }

    // 损坏候选拒绝（非数据库文件）。
    let corrupt = dir.path().join("corrupt.db");
    std::fs::write(&corrupt, b"this is not a sqlite database").unwrap();
    let error = validate_candidate(&corrupt, program_schema_version()).expect_err("损坏拒绝");
    assert_eq!(error.code(), "backup_candidate_invalid");
}

/// DoD6 前置：`request_restore` 落日志 → 启动序列执行恢复（含重启路径语义）。
#[test]
fn pending_restore_is_applied_on_next_start() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("aether.db");
    seed_database(&db, 4);
    let backup = dir.path().join("candidate.db");
    make_backup(&db, &backup);
    let backup_fingerprint = fingerprint(&backup);

    // 改变现场（清库），模拟「恢复请求 → 重启」。
    remove_database(&db);

    let journal = request_restore(&db, &backup, 1_700_000_001_000).expect("登记恢复请求");
    assert_eq!(journal.status, "requested");
    assert!(read_restore_journal(&db).unwrap().is_some());

    let outcome = recover_or_apply_pending_restore(&db).expect("启动处理");
    match outcome {
        RestoreStartupOutcome::Applied(report) => {
            assert!(report.applied);
            assert_eq!(
                report.steps.first().map(|step| step.step.as_str()),
                Some("validate_candidate")
            );
        }
        other => panic!("必须为 Applied：{other:?}"),
    }
    assert_eq!(fingerprint(&db), backup_fingerprint, "启动恢复后数据一致");
    assert!(read_restore_journal(&db).unwrap().is_none(), "日志已清理");
}

/// DoD4（确定性部分）：`placing` 状态的中断日志 → 启动回滚现场三件套。
#[test]
fn interrupted_placing_journal_rolls_back_on_start() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("aether.db");
    let before = seed_database(&db, 3);

    // 构造 kill -9 窗口：库已改名、候选未就位（journal=placing）。
    let ts = 1_700_000_001_500i64;
    let renamed = dir.path().join(format!("aether.db{PRE_RESTORE_INFIX}{ts}"));
    std::fs::rename(&db, &renamed).unwrap();
    let journal = aether_store::backup::RestoreJournal {
        v: 1,
        status: "placing".to_owned(),
        db_path: db.to_string_lossy().to_string(),
        candidate_path: dir.path().join("nope.db").to_string_lossy().to_string(),
        requested_at: ts,
        scene: Some(aether_store::backup::SceneProtection {
            db: Some(renamed.clone()),
            wal: None,
            shm: None,
            ts,
        }),
        error: None,
    };
    aether_store::backup::write_restore_journal(&db, &journal).expect("写 placing 日志");

    let outcome = recover_or_apply_pending_restore(&db).expect("启动回滚");
    match outcome {
        RestoreStartupOutcome::InterruptedRolledBack { detail } => {
            assert!(detail.contains("已回滚现场三件套"));
        }
        other => panic!("必须为 InterruptedRolledBack：{other:?}"),
    }
    assert!(db.exists(), "旧库已还原");
    assert_eq!(&before, &fingerprint(&db), "回滚后数据一致");
    assert!(!renamed.exists(), "改名文件已还原");
    assert!(read_restore_journal(&db).unwrap().is_none());
}

/// 台账读写（`backups` 表；M3-04 保留策略支撑）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backups_table_roundtrip() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("aether.db");
    drop(Store::open(&db).unwrap());
    let runtime = aether_store::StoreRuntime::open(
        &db,
        aether_store::WriteQueueConfig::default(),
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let reads = runtime.reads().clone();
    let queue = runtime.queue().clone();

    let record = BackupRecord {
        id: "01J000000000000000000000B1".to_owned(),
        path: dir.path().join("aether-1.db"),
        size_bytes: 42,
        encrypted: false,
        kind: "internal".to_owned(),
        created_at: 11,
    };
    queue
        .execute(StoreCommand::InsertBackup {
            record: record.clone(),
        })
        .await
        .unwrap();
    let listed = reads.backups().await.unwrap();
    assert_eq!(listed, vec![record.clone()]);
    assert_eq!(
        reads.backup(&record.id).await.unwrap(),
        Some(record.clone())
    );
    assert!(reads.backup("missing").await.unwrap().is_none());

    queue
        .execute(StoreCommand::DeleteBackup {
            id: record.id.clone(),
        })
        .await
        .unwrap();
    assert!(reads.backups().await.unwrap().is_empty());
    runtime.shutdown().await.unwrap();
}
