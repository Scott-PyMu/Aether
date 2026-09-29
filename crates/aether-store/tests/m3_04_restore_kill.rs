//! M3-04 DoD4：恢复中断（kill -9 / TerminateProcess）→ 现场三件套完整可回退。
//!
//! 机制：父测试以 `current_exe()` 拉起「子测试」执行真实恢复（在「现场保护完成后」
//! 注入长暂停），轮询到 `.pre-restore-*` 三件套后强杀子进程（Windows
//! `TerminateProcess` / Unix `SIGKILL`，等价 kill -9），再以启动序列入口
//! [`recover_or_apply_pending_restore`] 回滚现场并断言旧库完整。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aether_store::backup::{
    execute_restore, program_schema_version, read_restore_journal,
    recover_or_apply_pending_restore, RestoreFault, RestoreRequest, RestoreStartupOutcome,
    RestoreStep, PRE_RESTORE_INFIX,
};
use aether_store::Store;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

const CHILD_ENV: &str = "AETHER_M3_04_RESTORE_CHILD";

/// 子进程入口（仅由父测试经 `--exact` 拉起；`PauseAfter(ProtectScene)` 模拟中断窗口）。
#[test]
fn m3_04_restore_child_process() {
    if std::env::var(CHILD_ENV).ok().as_deref() != Some("1") {
        return;
    }
    let db = PathBuf::from(std::env::var("AETHER_M3_04_DB").expect("子进程 db 路径"));
    let candidate = PathBuf::from(std::env::var("AETHER_M3_04_CANDIDATE").expect("子进程候选路径"));
    let result = execute_restore(
        &RestoreRequest {
            db_path: db,
            candidate_path: candidate,
            program_max_version: program_schema_version(),
            now_ms: 1_700_000_002_000,
            fault: RestoreFault::PauseAfter {
                step: RestoreStep::ProtectScene,
                pause: Duration::from_secs(120),
            },
        },
        || Ok(()),
    );
    // 未被父进程强杀时自然返回；无论结果如何直接退出（父测试负责断言）。
    let _ = result;
    std::process::exit(0);
}

#[test]
fn kill_during_restore_leaves_scene_recoverable() {
    if std::env::var(CHILD_ENV).ok().as_deref() == Some("1") {
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("aether.db");
    let before = seed_database(&db, 10);

    let candidate = dir.path().join("candidate.db");
    let store = Store::open(&db).expect("打开源库");
    store.backup_to(&candidate).expect("备份");
    drop(store);
    std::fs::write(sidecar(&db, "-wal"), b"wal-scene").expect("写 wal");
    std::fs::write(sidecar(&db, "-shm"), b"shm-scene").expect("写 shm");

    let exe = std::env::current_exe().expect("当前测试可执行文件");
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "m3_04_restore_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env("AETHER_M3_04_DB", &db)
        .env("AETHER_M3_04_CANDIDATE", &candidate)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起恢复子进程");

    // 等待「现场保护」完成（三件套改名出现）。
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut scene_files: Vec<PathBuf> = Vec::new();
    while Instant::now() < deadline {
        scene_files = pre_restore_files(&db);
        if scene_files.len() == 3 {
            break;
        }
        if let Some(status) = child.try_wait().expect("查询子进程") {
            panic!("恢复子进程提前退出（{status:?}），无法覆盖中断窗口");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        scene_files.len(),
        3,
        "现场保护应完成三件套改名（库/-wal/-shm）"
    );
    assert!(scene_files.iter().all(|path| path.exists()));

    // kill -9 等价强杀。
    child.kill().expect("强杀恢复子进程");
    let _ = child.wait();

    // 中断后现场：三件套完整保留（可回退），库路径缺失或为未完成产物。
    for path in &scene_files {
        assert!(path.exists(), "中断后现场文件必须完整：{}", path.display());
    }
    let journal = read_restore_journal(&db)
        .expect("读 journal")
        .expect("中断后 journal 必须保留（回滚依据）");
    assert_eq!(journal.status, "placing");
    assert_eq!(
        journal.scene.as_ref().map(|scene| scene.ts),
        Some(1_700_000_002_000)
    );

    // 模拟应用重启：启动序列入口回滚现场。
    let outcome = recover_or_apply_pending_restore(&db).expect("启动回滚");
    match outcome {
        RestoreStartupOutcome::InterruptedRolledBack { detail } => {
            assert!(detail.contains("已回滚现场三件套"), "detail={detail}");
        }
        other => panic!("必须为 InterruptedRolledBack：{other:?}"),
    }
    assert_eq!(&before, &fingerprint(&db), "回滚后旧库数据完整");
    assert!(
        pre_restore_files(&db).is_empty(),
        "回滚后 .pre-restore 文件已还原"
    );
    assert!(read_restore_journal(&db).unwrap().is_none());
}

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
    drop(store);
    fingerprint(path)
}

fn fingerprint(path: &Path) -> String {
    let connection = Connection::open(path).expect("指纹连接");
    connection
        .execute_batch("PRAGMA query_only = ON;")
        .expect("只读查询");
    let mut hasher = Sha256::new();
    let mut statement = connection
        .prepare("SELECT COUNT(*) FROM sessions")
        .expect("计数语句");
    let count: i64 = statement.query_row([], |row| row.get(0)).expect("计数");
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

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", db.display()))
}

/// 库目录内 `.pre-restore-*` 现场文件（按名称排序）。
fn pre_restore_files(db: &Path) -> Vec<PathBuf> {
    let Some(dir) = db.parent() else {
        return Vec::new();
    };
    let prefix = db
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut files = Vec::new();
    let Ok(listing) = std::fs::read_dir(dir) else {
        return files;
    };
    for item in listing.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && name.contains(PRE_RESTORE_INFIX) {
            files.push(item.path());
        }
    }
    files.sort();
    files
}
