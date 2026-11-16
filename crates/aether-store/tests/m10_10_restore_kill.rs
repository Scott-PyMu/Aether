//! M10-10 恢复进程被 kill 的故障注入（测试专用子进程）。
//!
//! 父进程创建小型 generation + 备份集；子进程用测试闭包在复制主库后挂起，
//! 父进程 kill 子进程，模拟恢复 staging 中途崩溃。随后 `open_layout` 触发启动
//! 恢复：`CURRENT` 必须仍指向旧 generation，未引用 staging 必须清理。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::{Duration, Instant};

use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    cleanup_unreferenced_staging, create_backup_set, open_layout, read_current, stage_restore_with,
    validate_backup_set,
};

const NEW_GENERATION: &str = "gen-01J8ZQ0000000000000000NEW0";

fn child_stage(data_dir: &Path, set_path: &Path, marker: &Path) {
    let set = validate_backup_set(set_path).expect("validate set");
    std::fs::write(marker, b"started").expect("marker");
    stage_restore_with(data_dir, &set, NEW_GENERATION, 1_794_800_000_000, |source, dest| {
        std::fs::copy(source, dest).expect("copy");
        if dest.file_name().is_some_and(|name| name == "aether.db") {
            // 给父进程稳定窗口在 staging 中途 kill。
            std::thread::sleep(Duration::from_secs(60));
        }
        Ok(())
    })
    .expect("stage");
}

#[test]
#[ignore = "restore kill child worker"]
fn restore_kill_child_worker() {
    let Some(spec) = std::env::var_os("AETHER_RESTORE_KILL_CHILD") else {
        return;
    };
    let spec = std::path::PathBuf::from(spec);
    child_stage(&spec.join("data"), &spec.join("set"), &spec.join("started"));
}

#[test]
fn restore_kill_leaves_old_generation_and_cleans_staging() {
    let root = tempfile::tempdir().expect("temp");
    let data_dir = root.path().join("data");
    let layout = initialize_layout(&data_dir, "0.1.0", 1).expect("generation");
    let old = layout.generation().to_owned();
    let backup_id = "01J8ZQ0000000000000000KILB";
    create_backup_set(
        &layout,
        root.path(),
        backup_id,
        "manual",
        1_794_800_000_000,
        "0.1.0",
        false,
        "none",
    )
    .expect("backup set");
    let set_path = root.path().join("set");
    std::fs::rename(root.path().join(backup_id), &set_path).expect("move set");
    let marker = root.path().join("started");

    let mut child = std::process::Command::new(std::env::current_exe().expect("exe"))
        .args([
            "--exact",
            "restore_kill_child_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("AETHER_RESTORE_KILL_CHILD", root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(marker.is_file(), "子进程未进入 restore staging");
    std::thread::sleep(Duration::from_millis(500));
    child.kill().expect("kill child");
    let _ = child.wait();
    assert_eq!(
        read_current(&data_dir).expect("CURRENT").generation,
        old,
        "kill 后 CURRENT 不得切换"
    );

    let reopened = open_layout(&data_dir, "0.1.0", 1_794_800_000_001).expect("reopen");
    assert_eq!(reopened.generation(), old);
    cleanup_unreferenced_staging(&data_dir).expect("cleanup staging");
    let staging = data_dir.join("staging");
    let leftovers = std::fs::read_dir(&staging)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "恢复中断后 staging 必须清理");
    assert!(
        data_dir.join("generations").join(&old).join("aether.db").is_file(),
        "旧 generation 必须保留"
    );
}
