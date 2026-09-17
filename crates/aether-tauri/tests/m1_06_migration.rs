//! M1-06 DoD3/DoD5 支撑：迁移执行（复制 → 校验 → 原子替换 → 指针锁定）与启动门状态机。
//!
//! 迁移 E2E（真实 WebView 的拒绝启动流）在 `scripts/test/m1-06/e2e-startup-guard.mjs`；
//! 本文件覆盖可注入上下文的确定性断言：sha256 校验、失败路径、指针原子替换、
//! 门状态机（阻塞 → 迁移 → Ready → 锁定新目录）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use aether_tauri::startup::detect::{
    DetectionContext, NetworkDriveSource, PlatformKind, RegistrySource, ReparseSource,
};
use aether_tauri::startup::migrate::{migrate_data_dir, sha256_file, MigrationErrorKind};
use aether_tauri::startup::{pointer, DataDirSource, StartupGate, StartupPhase};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "aether-m1-06-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

/// IPC 迁移目标的长路径形式（无 8.3 短名）。
///
/// CI 的 `%TEMP%` 形如 `C:\Users\RUNNER~1\...`；短名是 D9/T7 的**拒绝样本**，
/// 而 `startup_migrate` 走 `validate_migration_target`（含 Windows 特殊形态拒绝）。
/// 测试需以 canonicalize 后的长路径调用迁移（生产由目录选择器/用户输入保证）。
fn long_path(path: &Path) -> String {
    // 目标可能不存在（负样本）——canonicalize 失败时回退原始路径。
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy().to_string();
    #[cfg(windows)]
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    text
}

/// 允许上下文（无任何同步盘命中）。
fn allow_context() -> DetectionContext {
    DetectionContext {
        platform: PlatformKind::Windows,
        home: None,
        env: std::collections::BTreeMap::new(),
        registry: RegistrySource::Unavailable,
        reparse: ReparseSource::Unavailable,
        network_drives: NetworkDriveSource::Unavailable,
    }
}

/// 命中上下文：`OneDrive` 指向 `sync_root`。
fn sync_context(sync_root: &Path) -> DetectionContext {
    let mut ctx = allow_context();
    ctx.env
        .insert("OneDrive".to_string(), sync_root.to_path_buf());
    ctx
}

fn make_source(root: &Path) -> PathBuf {
    let source = root.join("sync-root").join("Aether");
    std::fs::create_dir_all(source.join("backups")).expect("创建源目录结构");
    std::fs::write(source.join("aether.db"), b"aether-db-bytes").expect("写入主库");
    std::fs::write(source.join("aether.db-wal"), b"wal-bytes").expect("写入 WAL");
    std::fs::write(source.join("backups").join("b1.db"), b"backup-bytes").expect("写入备份");
    source
}

#[test]
fn migration_copies_verifies_and_preserves_source() {
    let root = temp_dir("migrate-ok");
    let source = make_source(&root);
    let target = root.join("local-target");
    std::fs::create_dir_all(&target).expect("创建目标目录");

    let outcome = migrate_data_dir(&source, &target, &allow_context()).expect("迁移成功");

    assert_eq!(outcome.entries.len(), 3, "应复制 3 个文件：{outcome:?}");
    assert!(outcome.total_bytes > 0);
    for entry in &outcome.entries {
        let source_file = source.join(&entry.relative);
        let target_file = target.join(&entry.relative);
        assert!(target_file.is_file(), "目标文件缺失：{}", entry.relative);
        assert_eq!(
            sha256_file(&source_file).expect("源摘要"),
            entry.sha256,
            "源文件摘要应等于记录值：{}",
            entry.relative
        );
        assert_eq!(
            sha256_file(&target_file).expect("目标摘要"),
            entry.sha256,
            "目标文件摘要应与源一致（校验口径）：{}",
            entry.relative
        );
        assert_eq!(
            std::fs::metadata(&target_file).expect("目标元数据").len(),
            entry.bytes
        );
    }
    assert!(
        source.join("aether.db").is_file(),
        "迁移后源目录必须原样保留（回滚副本）"
    );
    let leftovers: Vec<String> = std::fs::read_dir(&target)
        .expect("读取目标目录")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".aether-migration-"))
        .collect();
    assert!(leftovers.is_empty(), "暂存目录必须清理：{leftovers:?}");
    println!(
        "[m1-06] migration entries={} bytes={}",
        outcome.entries.len(),
        outcome.total_bytes
    );
}

#[test]
fn migration_rejects_invalid_and_sync_targets() {
    let root = temp_dir("migrate-bad");
    let source = make_source(&root);
    let ctx = allow_context();

    let missing_source = root.join("missing-source");
    let target = root.join("local-target");
    std::fs::create_dir_all(&target).expect("创建目标目录");
    assert_eq!(
        migrate_data_dir(&missing_source, &target, &ctx)
            .expect_err("源缺失必须失败")
            .kind,
        MigrationErrorKind::SourceInvalid
    );
    assert_eq!(
        migrate_data_dir(&source, &source, &ctx)
            .expect_err("目标=源必须失败")
            .kind,
        MigrationErrorKind::TargetInvalid
    );

    let nested = source.join("backups");
    assert_eq!(
        migrate_data_dir(&source, &nested, &ctx)
            .expect_err("目标位于源内部必须失败")
            .kind,
        MigrationErrorKind::TargetInsideSource
    );

    let non_empty = root.join("non-empty");
    std::fs::create_dir_all(&non_empty).expect("创建非空目标");
    std::fs::write(non_empty.join("occupied.txt"), b"x").expect("写入占用文件");
    assert_eq!(
        migrate_data_dir(&source, &non_empty, &ctx)
            .expect_err("非空目标必须失败")
            .kind,
        MigrationErrorKind::TargetNotEmpty
    );

    let sync_target = root.join("sync-root").join("same-disk-target");
    std::fs::create_dir_all(&sync_target).expect("创建同步盘内目标");
    assert_eq!(
        migrate_data_dir(
            &source,
            &sync_target,
            &sync_context(&root.join("sync-root"))
        )
        .expect_err("同步盘目标必须失败")
        .kind,
        MigrationErrorKind::SyncTarget
    );
}

#[test]
fn pointer_roundtrip_and_corruption_are_handled() {
    let root = temp_dir("pointer");
    let pointer_file = root.join("config").join("data-location.json");

    assert_eq!(
        pointer::read_pointer(&pointer_file).expect("缺失指针应为 Ok(None)"),
        None
    );

    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).expect("创建数据目录");
    pointer::write_pointer(&pointer_file, &data_dir).expect("写入指针");
    assert_eq!(
        pointer::read_pointer(&pointer_file).expect("读取指针"),
        Some(data_dir.clone())
    );

    std::fs::write(&pointer_file, b"{not-json").expect("写入损坏指针");
    assert!(
        pointer::read_pointer(&pointer_file).is_err(),
        "损坏指针必须报错"
    );
    std::fs::write(&pointer_file, br#"{ "v": 99, "data_dir": "C:\\x" }"#)
        .expect("写入未知版本指针");
    assert!(
        pointer::read_pointer(&pointer_file).is_err(),
        "未知版本指针必须报错"
    );
}

#[test]
fn gate_blocks_then_migrates_and_locks_new_dir() {
    let root = temp_dir("gate");
    let source = make_source(&root);
    let sync_root = root.join("sync-root");
    let pointer_file = root.join("config").join("data-location.json");

    let gate = StartupGate::bootstrap_at(
        source.clone(),
        DataDirSource::Default,
        sync_context(&sync_root),
        Some(pointer_file.clone()),
    );
    let snapshot = gate.snapshot();
    assert_eq!(snapshot.phase, StartupPhase::BlockedSyncDir);
    let blocked = gate.ensure_ready().expect_err("阻塞态必须阻断业务命令");
    assert_eq!(blocked.code.as_str(), "startup_blocked");
    println!(
        "[m1-06] gate blocked: {:?}",
        snapshot.detection.as_ref().map(|d| d.reasons.clone())
    );

    let target = root.join("local-target");
    std::fs::create_dir_all(&target).expect("创建迁移目标");
    let migrated = gate.migrate(&long_path(&target)).expect("迁移成功");
    assert_eq!(migrated["phase"], "ready");
    assert_eq!(migrated["data_dir_source"], "migrated");
    assert!(migrated["migration"]["entries"]
        .as_array()
        .is_some_and(|entries| entries.len() == 3));

    assert!(gate.ensure_ready().is_ok(), "迁移后业务命令应恢复");
    // 指针写入的是 canonicalize 长路径（CI 的 %TEMP% 为 8.3 短名，写入长路径）。
    assert_eq!(
        pointer::read_pointer(&pointer_file).expect("读取指针"),
        Some(PathBuf::from(long_path(&target))),
        "迁移成功后必须锁定新目录（指针原子替换）"
    );
    assert!(target.join("aether.db").is_file());

    let second = gate.migrate(&long_path(&target));
    assert_eq!(
        second.expect_err("重复迁移必须拒绝").code.as_str(),
        "invalid_value"
    );
}

#[test]
fn gate_rejects_sync_or_non_empty_migration_target() {
    let root = temp_dir("gate-targets");
    let source = make_source(&root);
    let sync_root = root.join("sync-root");
    let gate = StartupGate::bootstrap_at(
        source.clone(),
        DataDirSource::Default,
        sync_context(&sync_root),
        Some(root.join("config").join("data-location.json")),
    );

    let sync_target = sync_root.join("still-synced");
    std::fs::create_dir_all(&sync_target).expect("创建同步盘目标");
    let rejected = gate
        .migrate(&long_path(&sync_target))
        .expect_err("同步盘目标必须拒绝");
    assert_eq!(rejected.code.as_str(), "path_rejected");

    let occupied = root.join("occupied");
    std::fs::create_dir_all(&occupied).expect("创建非空目标");
    std::fs::write(occupied.join("x.txt"), b"x").expect("写入占用文件");
    let rejected = gate
        .migrate(&long_path(&occupied))
        .expect_err("非空目标必须拒绝");
    assert_eq!(rejected.code.as_str(), "path_rejected");

    let missing = root.join("missing-target");
    let rejected = gate
        .migrate(&long_path(&missing))
        .expect_err("不存在的目标必须拒绝");
    assert_eq!(rejected.code.as_str(), "path_rejected");

    // 失败不改变门状态：仍为阻塞，仍可重试迁移。
    assert_eq!(gate.snapshot().phase, StartupPhase::BlockedSyncDir);
}
