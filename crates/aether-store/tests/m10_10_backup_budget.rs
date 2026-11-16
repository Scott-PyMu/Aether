//! M10-10 5.22GiB 整套备份/恢复预算复测（ADR-039 D-039-5）。
//!
//! 只测 store 层复制/校验/staging；加密、UI 窗口由 m5_03/M10-10 Electron 证据覆盖。
//! 默认 `#[ignore]`：
//! `AETHER_M10_10_DATA_DIR=<generation> cargo test -p aether-store --test m10_10_backup_budget -- --ignored --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Instant;

use aether_store::{
    create_backup_set, read_layout, stage_restore, validate_backup_set, verify_staged_generation,
    ValidatedBackupSet,
};

fn clean(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

#[test]
#[ignore = "M10-10 5.22GiB backup/restore benchmark"]
fn backup_and_restore_staging_within_budget() {
    let data_dir = std::env::var("AETHER_M10_10_DATA_DIR")
        .unwrap_or_else(|_| "dist-out/m10-09/generation".to_owned());
    let output = std::env::var("AETHER_M10_10_BACKUP_OUT")
        .unwrap_or_else(|_| "dist-out/m10-10/backup-budget.json".to_owned());
    let layout = read_layout(Path::new(&data_dir)).expect("generation layout");
    let scratch = std::env::temp_dir().join("opencode").join("m10-10-backup");
    let target = scratch.join("sets");
    let restore_root = scratch.join("restore-root");
    clean(&scratch);
    std::fs::create_dir_all(&target).expect("target");
    std::fs::create_dir_all(&restore_root).expect("restore root");

    let backup_id = "01J8ZQ0000000000000000M10B";
    let started = Instant::now();
    let outcome = create_backup_set(
        &layout,
        &target,
        backup_id,
        "manual",
        1_794_800_000_000,
        "0.1.0",
        false,
        "none",
    )
    .expect("create backup set");
    let backup_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(outcome.manifest.shard_count, 8);
    assert_eq!(outcome.manifest.files.len(), 10);

    let validate_started = Instant::now();
    let validated: ValidatedBackupSet =
        validate_backup_set(&target.join(backup_id)).expect("validate set");
    let validate_ms = validate_started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(validated.manifest.shard_count, 8);

    let restore_started = Instant::now();
    stage_restore(
        &restore_root,
        &validated,
        "gen-01J8ZQ0000000000000000M10R",
        1_794_800_000_000,
    )
    .expect("stage restore");
    let stage_ms = restore_started.elapsed().as_secs_f64() * 1000.0;
    verify_staged_generation(
        &restore_root,
        "restore",
        "gen-01J8ZQ0000000000000000M10R",
    )
    .expect("verify staged generation");

    let result = serde_json::json!({
        "task": "M10-10",
        "generation": layout.generation(),
        "backup_set_bytes": outcome.total_bytes,
        "backup_ms": backup_ms,
        "validate_ms": validate_ms,
        "stage_restore_ms": stage_ms,
        "budgets_ms": { "backup": 600_000, "restore": 900_000 },
        "manifest_files": outcome.manifest.files.len(),
        "shard_count": outcome.manifest.shard_count,
    });
    if let Some(parent) = Path::new(&output).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&output, serde_json::to_vec_pretty(&result).expect("json")).expect("write");
    println!("M10-10_BACKUP {}", serde_json::to_string(&result).expect("json"));
    clean(&scratch);
    assert!(backup_ms <= 600_000.0, "backup {backup_ms}ms > 600s");
    assert!(stage_ms <= 900_000.0, "restore {stage_ms}ms > 900s");
}
