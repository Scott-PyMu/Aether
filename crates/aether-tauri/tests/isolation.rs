//! M2-04 DoD⑤（组合层）：`IsolationSink` 生产桥接到 M1-10 监督器。
//!
//! 覆盖：
//! - `isolate` → 监督器 `degraded + status_reason=storage_backpressure`（进程终止）；
//! - `release` → `degraded → starting → ready`（自动解除）。
//!
//! 夹具：`AETHER_ADAPTER_FIXTURE` 指向 `aether-adapter-fixture` 可执行文件
//! （由 `pnpm verify:m2-04` 构建并注入）；未设置时显式 SKIP，设置
//! `AETHER_REQUIRE_ADAPTER_FIXTURE=1` 时缺路径直接失败（禁止静默跳过）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_adapters::protocol::DisabledReason;
use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, StartOutcome};
use aether_control::{IsolationReason, IsolationSink};
use aether_core::{RuntimeId, RuntimeStatus};
use aether_tauri::isolation::SupervisorIsolationSink;
use aether_tauri::runtime_control::boot_supervisor;

fn fixture_binary() -> Option<String> {
    match std::env::var("AETHER_ADAPTER_FIXTURE") {
        Ok(path) if !path.is_empty() => Some(path),
        _ => {
            if std::env::var("AETHER_REQUIRE_ADAPTER_FIXTURE").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_ADAPTER_FIXTURE=1 但 AETHER_ADAPTER_FIXTURE 未设置");
            }
            eprintln!(
                "SKIP：AETHER_ADAPTER_FIXTURE 未设置（运行 pnpm verify:m2-04 构建夹具后执行）"
            );
            None
        }
    }
}

#[test]
fn supervisor_isolation_sink_isolates_and_releases() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let manifest = RuntimeManifest::new("mock", "Mock Fixture", fixture)
        .official(true)
        .with_args([
            "--mode".to_owned(),
            "deaf".to_owned(),
            "--seconds".to_owned(),
            "120".to_owned(),
        ]);
    let supervisor = Arc::new(
        boot_supervisor(
            vec![RuntimeSpec::with_fresh_token(manifest)],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("构造监督器"),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时");
    let handle = runtime.handle().clone();

    let outcomes = runtime.block_on(supervisor.warmup_all());
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1, StartOutcome::Ready, "预热必须 Ready");

    let sink = SupervisorIsolationSink::new(
        Arc::clone(&supervisor),
        handle,
        aether_tauri::isolation::ISOLATION_TIMEOUT,
    );
    let runtime_id = RuntimeId::new("mock").expect("runtime id");

    // 隔离：degraded + storage_backpressure。
    assert!(sink.isolate(
        &runtime_id,
        IsolationReason::StorageBackpressure,
        "写队列临时高水位 >4096（测试注入）"
    ));
    let target = supervisor.get("mock").expect("白名单命中");
    assert_eq!(runtime.block_on(target.status()), RuntimeStatus::Degraded);
    assert_eq!(
        runtime.block_on(target.status_reason()),
        Some(DisabledReason::StorageBackpressure)
    );

    // 解除：回到 ready。
    assert!(sink.release(&runtime_id));
    assert_eq!(runtime.block_on(target.status()), RuntimeStatus::Ready);
    assert_eq!(runtime.block_on(target.status_reason()), None);

    runtime.block_on(supervisor.shutdown_all());
}
