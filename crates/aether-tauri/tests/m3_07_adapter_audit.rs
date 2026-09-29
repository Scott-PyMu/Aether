//! M3-07 集成测试：适配器状态变化审计落库（设计 D9 / SE-03；M1-10 观察者契约）。
//!
//! 覆盖：
//! - DoD1（注入动作 ↔ 审计条数一一对应）：注入「启动失败」与「非官方 manifest 拒绝」
//!   两类适配器动作，断言 `audit_log` 追加条数与监督器回调一一对应
//!   （状态转移各 1 条；准入拒绝另 1 条）；
//! - DoD3（字段齐备）：`actor` / `resource` / `result` / `ts` 均非空且语义正确；
//! - 组合根装配：延迟观察者（`DeferredObserver`）在存储就绪前不落库、注入后转发。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aether_adapters::supervisor::{
    RuntimeManifest, RuntimeSpec, StartOutcome, StatusChange, SupervisorObserver,
};
use aether_core::{RuntimeId, RuntimeStatus};
use aether_store::{AuditLogRecord, ReadPool, StoreRuntime, WriteQueueConfig};
use aether_tauri::audit_bridge::{DeferredObserver, StoreAuditObserver};
use aether_tauri::runtime_control::boot_supervisor_with_observer;
use serde_json::{json, Value};
use tokio::runtime::Handle;

fn write_evidence(name: &str, value: &Value) {
    println!("[m3-07] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_07_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{name}.json"));
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let _ = std::fs::write(&path, text);
}

/// 轮询等待审计行数达到期望（观察者写入经单写队列异步落地）。
async fn wait_for_audits(reads: &ReadPool, expected: usize) -> Vec<AuditLogRecord> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows = reads.audit_log(200).await.unwrap();
        if rows.len() >= expected || tokio::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn row_json(record: &AuditLogRecord) -> Value {
    json!({
        "actor": record.actor,
        "action": record.action,
        "resource": record.resource,
        "result": record.result,
        "ts": record.ts,
        "runtime_id": record.runtime_id,
        "detail": record.detail,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_start_failure_is_audited_one_to_one() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("aether.db");
    let storage =
        StoreRuntime::open(&db_path, WriteQueueConfig::default(), &Handle::current()).unwrap();
    let reads = storage.reads().clone();

    // 组合根装配口径：监督器先构造（未注入审计出口），存储就绪后注入。
    let deferred = Arc::new(DeferredObserver::new());
    // 未注入期间回调为 no-op（组合根保证此时监督器无活动；此处显式调用验证丢弃）。
    deferred.on_status_changed(&StatusChange {
        runtime_id: RuntimeId::new("mock").unwrap(),
        from: RuntimeStatus::Cold,
        to: RuntimeStatus::Ready,
        reason: None,
        detail: Some("注入前回调（必须丢弃）".to_owned()),
        at_ms: 1,
    });
    deferred.set(Arc::new(StoreAuditObserver::new(
        storage.queue().clone(),
        Handle::current(),
    )));

    // 缺二进制：cold→starting→disabled(start_failed)（恰好 2 条状态审计）。
    // 使用官方白名单 id `mock`（准入放行，故障发生在 spawn）。
    let missing = temp.path().join("definitely-missing-adapter-binary");
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", &missing).official(true),
    );
    let supervisor = Arc::new(
        boot_supervisor_with_observer(
            vec![spec],
            Some(&temp.path().join("adapters.json")),
            deferred.clone(),
        )
        .expect("监督器构造"),
    );
    let outcomes = supervisor.warmup_all().await;
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0].1, StartOutcome::Failed { .. }),
        "缺二进制必须失败：{:?}",
        outcomes[0].1
    );

    let rows = wait_for_audits(&reads, 2).await;
    assert_eq!(
        rows.len(),
        2,
        "注入前回调必须丢弃：只允许 2 条审计；实际 {rows:?}"
    );
    let actual: Vec<(String, String, String)> = rows
        .iter()
        .map(|record| {
            (
                record.runtime_id.clone().unwrap_or_default(),
                record.action.clone(),
                record.result.clone().unwrap_or_default(),
            )
        })
        .collect();
    let expected: Vec<(&str, &str, &str)> = vec![
        ("mock", "runtime.status_changed", "cold→starting"),
        ("mock", "runtime.status_changed", "starting→disabled"),
    ];
    let expected_owned: Vec<(String, String, String)> = expected
        .iter()
        .map(|(runtime_id, action, result)| {
            (
                (*runtime_id).to_owned(),
                (*action).to_owned(),
                (*result).to_owned(),
            )
        })
        .collect();
    assert_eq!(
        actual, expected_owned,
        "启动失败的状态转移与审计条数必须一一对应"
    );
    assert_audit_fields(&rows);
    write_evidence(
        "dod1_adapter_start_failed",
        &json!({
            "task": "M3-07 DoD1 适配器状态变化（启动失败）1:1",
            "injected": ["runtime.start_failed(missing binary)"],
            "expected": expected
                .iter()
                .map(|(runtime_id, action, result)| json!({
                    "runtime_id": runtime_id, "action": action, "result": result,
                }))
                .collect::<Vec<_>>(),
            "actual": actual
                .iter()
                .map(|(runtime_id, action, result)| json!({
                    "runtime_id": runtime_id, "action": action, "result": result,
                }))
                .collect::<Vec<_>>(),
            "pre_injection_callback_dropped": rows.len() == 2,
            "rows": rows.iter().map(row_json).collect::<Vec<_>>(),
            "one_to_one": actual == expected_owned,
        }),
    );

    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_admission_rejection_is_audited_one_to_one() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("aether.db");
    let storage =
        StoreRuntime::open(&db_path, WriteQueueConfig::default(), &Handle::current()).unwrap();
    let reads = storage.reads().clone();

    let deferred = Arc::new(DeferredObserver::new());
    deferred.set(Arc::new(StoreAuditObserver::new(
        storage.queue().clone(),
        Handle::current(),
    )));

    // 非官方 manifest：cold→disabled(untrusted) + admission_rejected（恰好 2 条）。
    let missing = temp.path().join("definitely-missing-adapter-binary");
    let spec =
        RuntimeSpec::with_fresh_token(RuntimeManifest::new("unofficial", "Unofficial", &missing));
    let supervisor = Arc::new(
        boot_supervisor_with_observer(
            vec![spec],
            Some(&temp.path().join("adapters.json")),
            deferred.clone(),
        )
        .expect("监督器构造"),
    );
    let outcomes = supervisor.warmup_all().await;
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0].1, StartOutcome::Rejected { .. }),
        "非官方 manifest 必须被拒：{:?}",
        outcomes[0].1
    );

    let rows = wait_for_audits(&reads, 2).await;
    assert_eq!(rows.len(), 2, "只允许 2 条审计；实际 {rows:?}");
    let actual: Vec<(String, String, String)> = rows
        .iter()
        .map(|record| {
            (
                record.runtime_id.clone().unwrap_or_default(),
                record.action.clone(),
                record.result.clone().unwrap_or_default(),
            )
        })
        .collect();
    let expected: Vec<(&str, &str, &str)> = vec![
        ("unofficial", "runtime.status_changed", "cold→disabled"),
        (
            "unofficial",
            "runtime.admission_rejected",
            "admission_rejected",
        ),
    ];
    let expected_owned: Vec<(String, String, String)> = expected
        .iter()
        .map(|(runtime_id, action, result)| {
            (
                (*runtime_id).to_owned(),
                (*action).to_owned(),
                (*result).to_owned(),
            )
        })
        .collect();
    assert_eq!(
        actual, expected_owned,
        "准入拒绝的状态转移/审计记录与落库条数必须一一对应"
    );
    assert_audit_fields(&rows);
    write_evidence(
        "dod1_adapter_admission_rejected",
        &json!({
            "task": "M3-07 DoD1 适配器状态变化（准入拒绝）1:1",
            "injected": ["runtime.admission_rejected(unofficial manifest)"],
            "expected": expected
                .iter()
                .map(|(runtime_id, action, result)| json!({
                    "runtime_id": runtime_id, "action": action, "result": result,
                }))
                .collect::<Vec<_>>(),
            "actual": actual
                .iter()
                .map(|(runtime_id, action, result)| json!({
                    "runtime_id": runtime_id, "action": action, "result": result,
                }))
                .collect::<Vec<_>>(),
            "rows": rows.iter().map(row_json).collect::<Vec<_>>(),
            "one_to_one": actual == expected_owned,
        }),
    );

    storage.shutdown().await.unwrap();
}

/// DoD3：适配器审计行字段齐备（actor/resource/result/ts/detail）。
fn assert_audit_fields(rows: &[AuditLogRecord]) {
    for record in rows {
        assert_eq!(record.actor, "system", "适配器审计 actor=system");
        assert!(
            record
                .resource
                .as_deref()
                .is_some_and(|value| value.starts_with("runtime:")),
            "resource 必须指向 runtime"
        );
        assert!(record
            .result
            .as_deref()
            .is_some_and(|value| !value.is_empty()));
        assert!(record.ts > 0, "ts 必须为有效时间戳");
        assert!(record
            .detail
            .as_deref()
            .is_some_and(|value| !value.is_empty()));
    }
}
