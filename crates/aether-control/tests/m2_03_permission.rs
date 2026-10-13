//! M2-03 集成测试：策略矩阵（**矩阵口径由 ADR-014 设计变更驱动（M6-01）**：
//! 会话级三档 `read_only` / `workspace_write`（缺省）/ `danger_full_access`）/
//! 路径逃逸（T7，形态违规样本 deny + 审计；纯逃逸三例区外读 allow）/ 审批持久化与
//! 重启恢复 / 300s 超时 deny / T6 100 并发 ask。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aether_control::{PermissionConfig, PermissionError, PermissionResolution};
use aether_core::{PermissionDecision, PermissionStatus};
use aether_security::{
    expand_t7_sample, MEMORY_FILE_MAX_BYTES, MEMORY_FILE_NAMES, T7_TEXTUAL_SAMPLES,
};
use m2_support::{
    build_manager, build_permission_service, create_session, insert_session_row, manual_clock,
    permission_request, set_session_permission_mode, wait_for, wait_permissions_pending,
    ScriptedExecutor, TestCore,
};
use serde_json::Value;

fn request_id(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("{label}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 测试工作区 + 外部目录 + 服务。
struct Harness {
    core: TestCore,
    workspace: tempfile::TempDir,
    outside: tempfile::TempDir,
    service: aether_control::PermissionService,
    clock: std::sync::Arc<aether_control::ManualClock>,
}

async fn harness() -> Harness {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    Harness {
        core,
        workspace,
        outside,
        service,
        clock,
    }
}

async fn shutdown(harness: Harness) {
    harness.core.pipeline.shutdown().await.unwrap();
    harness.core.storage.shutdown().await.unwrap();
}

async fn audit_actions(harness: &Harness) -> Vec<String> {
    harness
        .core
        .reads()
        .audit_log(5_000)
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.action)
        .collect()
}

/// 指定审计动作的条数（矩阵按会话档位三档断言用）。
async fn audit_count(harness: &Harness, action: &str) -> usize {
    audit_actions(harness)
        .await
        .iter()
        .filter(|record| record.as_str() == action)
        .count()
}

/// 区外目标（canonical 基准 + 剥离扩展前缀；规避 CI 临时目录 8.3 短名别名——
/// 区外路径检查对原始短名路径敏感，区外样本必须从 canonical 后的基准目录派生）。
fn outside_target(dir: &tempfile::TempDir, name: &str) -> String {
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let text = base.join(name).to_string_lossy().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// DoD1：策略矩阵断言（ADR-014 附录 B 会话级三档：缺省 `workspace_write`；
/// fs.read 区内/区外 allow、fs.write 区内免审批/区外 ask、exec/net 低档 deny、
/// 记忆白名单 1MB 上限；read_only 与 danger 分档对照）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_matrix_follows_three_modes() {
    let harness = harness().await;
    let workspace_file = harness.workspace.path().join("note.txt");
    std::fs::write(&workspace_file, b"x").unwrap();
    let inside = workspace_file.to_string_lossy().to_string();
    let outside = outside_target(&harness.outside, "secret.txt");
    std::fs::write(&outside, b"x").unwrap();

    // 无会话（缺省档 workspace_write）：fs.read 区内 / 区外均 allow（三档一致）。
    let allow = harness
        .service
        .request(permission_request(
            &request_id("read-in"),
            None,
            "fs.read",
            "read",
            Some(&inside),
            None,
        ))
        .await
        .unwrap();
    assert!(allow.is_allowed(), "fs.read 工作区内应 allow");

    let allow = harness
        .service
        .request(permission_request(
            &request_id("read-out"),
            None,
            "fs.read",
            "read",
            Some(&outside),
            None,
        ))
        .await
        .unwrap();
    assert!(allow.is_allowed(), "fs.read 区外（无重解析点）应 allow");

    // 缺省档 fs.write 区内 allow 且不进 pending（ADR-014：区内不再 ask）。
    let write_inside = harness
        .workspace
        .path()
        .join("new.txt")
        .to_string_lossy()
        .to_string();
    let allow = harness
        .service
        .request(permission_request(
            &request_id("write-in"),
            None,
            "fs.write",
            "write",
            Some(&write_inside),
            Some(10),
        ))
        .await
        .unwrap();
    assert!(
        allow.is_allowed(),
        "fs.write 工作区内应 allow（缺省档免逐次审批）"
    );
    assert!(
        harness.service.pending_list(None).is_empty(),
        "fs.write 工作区内不得进入待审批"
    );

    // 缺省档 fs.write 区外 ask → pending → once 授权 allow。
    let request = permission_request(
        &request_id("write-ask"),
        None,
        "fs.write",
        "write",
        Some(&outside),
        Some(10),
    );
    let service = harness.service.clone();
    let ask_task = tokio::spawn(async move { service.request(request).await.unwrap() });
    assert!(
        wait_for(
            || { !harness.service.pending_list(None).is_empty() },
            Duration::from_secs(3)
        )
        .await,
        "fs.write 区外应进入待审批"
    );
    let ticket = harness.service.pending_list(None).first().unwrap().clone();
    harness
        .service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(aether_core::PermissionScope::Once),
        )
        .await
        .unwrap();
    let resolution = ask_task.await.unwrap();
    assert!(resolution.is_allowed(), "once 授权应 allow");
    assert_eq!(resolution.scope, Some(aether_core::PermissionScope::Once));

    // 缺省档 exec / net 低档 deny。
    for (resource, action) in [("exec", "invoke"), ("net", "connect")] {
        let deny = harness
            .service
            .request(permission_request(
                &request_id(resource),
                None,
                resource,
                action,
                Some("anything"),
                None,
            ))
            .await
            .unwrap();
        assert!(!deny.is_allowed(), "缺省档 {resource} 应 deny");
    }

    // 缺省档记忆文件白名单（区内）：≤1MB allow；>1MB deny。
    for name in MEMORY_FILE_NAMES {
        let target = harness
            .workspace
            .path()
            .join(name)
            .to_string_lossy()
            .to_string();
        let allow = harness
            .service
            .request(permission_request(
                &request_id("memory-ok"),
                None,
                "fs.write",
                "write",
                Some(&target),
                Some(MEMORY_FILE_MAX_BYTES),
            ))
            .await
            .unwrap();
        assert!(allow.is_allowed(), "记忆文件 {name} ≤1MB 应 allow");
        let deny = harness
            .service
            .request(permission_request(
                &request_id("memory-over"),
                None,
                "fs.write",
                "write",
                Some(&target),
                Some(MEMORY_FILE_MAX_BYTES + 1),
            ))
            .await
            .unwrap();
        assert!(!deny.is_allowed(), "记忆文件 {name} >1MB 应 deny");
    }

    // read_only 会话：一切 fs.write deny（区内/区外/记忆文件）；exec/net deny；区外读 allow。
    let read_only = insert_session_row(&harness.core, "01J00000000000000000000M3A", None).await;
    set_session_permission_mode(
        &harness.core,
        &read_only.id,
        aether_core::PermissionMode::ReadOnly,
    )
    .await;
    for (target, label) in [(&inside, "区内"), (&outside, "区外")] {
        let deny = harness
            .service
            .request(permission_request(
                &request_id("ro-write"),
                Some(&read_only.id),
                "fs.write",
                "write",
                Some(target),
                Some(10),
            ))
            .await
            .unwrap();
        assert!(!deny.is_allowed(), "read_only 档 fs.write {label} 应 deny");
    }
    let memory = harness
        .workspace
        .path()
        .join(MEMORY_FILE_NAMES[0])
        .to_string_lossy()
        .to_string();
    let deny = harness
        .service
        .request(permission_request(
            &request_id("ro-memory"),
            Some(&read_only.id),
            "fs.write",
            "write",
            Some(&memory),
            Some(10),
        ))
        .await
        .unwrap();
    assert!(!deny.is_allowed(), "read_only 档记忆文件写应 deny");
    for (resource, action) in [("exec", "invoke"), ("net", "connect")] {
        let deny = harness
            .service
            .request(permission_request(
                &request_id("ro-shell"),
                Some(&read_only.id),
                resource,
                action,
                Some("anything"),
                None,
            ))
            .await
            .unwrap();
        assert!(!deny.is_allowed(), "read_only 档 {resource} 应 deny");
    }
    let allow = harness
        .service
        .request(permission_request(
            &request_id("ro-read-out"),
            Some(&read_only.id),
            "fs.read",
            "read",
            Some(&outside),
            None,
        ))
        .await
        .unwrap();
    assert!(allow.is_allowed(), "read_only 档区外读应 allow");

    // danger 会话：区外写 allow、exec/net allow（3 次 allow 各 +1 allowed_by_policy 审计）。
    let danger = insert_session_row(&harness.core, "01J00000000000000000000D9X", None).await;
    set_session_permission_mode(
        &harness.core,
        &danger.id,
        aether_core::PermissionMode::DangerFullAccess,
    )
    .await;
    let allowed_before = audit_count(&harness, "permission.allowed_by_policy").await;
    let allow = harness
        .service
        .request(permission_request(
            &request_id("danger-write-out"),
            Some(&danger.id),
            "fs.write",
            "write",
            Some(&outside),
            Some(10),
        ))
        .await
        .unwrap();
    assert!(allow.is_allowed(), "danger 档区外写应 allow");
    for (resource, action) in [("exec", "invoke"), ("net", "connect")] {
        let allow = harness
            .service
            .request(permission_request(
                &request_id("danger-shell"),
                Some(&danger.id),
                resource,
                action,
                Some("anything"),
                None,
            ))
            .await
            .unwrap();
        assert!(allow.is_allowed(), "danger 档 {resource} 应 allow");
    }
    let allowed_after = audit_count(&harness, "permission.allowed_by_policy").await;
    assert_eq!(
        allowed_after,
        allowed_before + 3,
        "danger 档 3 次 allow 必须各写一条 allowed_by_policy 审计"
    );

    let actions = audit_actions(&harness).await;
    assert!(actions.contains(&"permission.allowed_by_policy".to_owned()));
    assert!(actions.contains(&"permission.denied_by_policy".to_owned()));
    assert!(actions.contains(&"permission.requested".to_owned()));
    assert!(actions.contains(&"permission.resolved".to_owned()));
    shutdown(harness).await;
}

/// DoD2（T7）：路径逃逸样本审计与矩阵口径（ADR-014 设计变更驱动）——纯逃逸三例
/// （`parent_traversal` / `parent_traversal_forward` / `relative_traversal`）在 fs.read 下
/// 按矩阵区外读 **allow** 并各 +1 `permission.allowed_by_policy`；其余形态违规样本仍
/// **deny** 并各 +1 `permission.denied_by_policy`；动态软链接/Junction 逃逸样本维持
/// deny（区外重解析点）。文本样本全平台；文件系统样本按平台可创建性显式记录。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t7_path_escape_samples_all_denied_and_audited() {
    let harness = harness().await;
    // canonical 基准 + 剥离扩展前缀（区外样本的 8.3 短名规避，见 m2_support::outside_target）。
    let root_base = std::fs::canonicalize(harness.workspace.path()).unwrap();
    let root_text = root_base.to_string_lossy().to_string();
    let root = root_text
        .strip_prefix(r"\\?\")
        .unwrap_or(&root_text)
        .to_owned();
    let escape_only = [
        "parent_traversal",
        "parent_traversal_forward",
        "relative_traversal",
    ];
    let mut denied = 0usize;
    let mut allowed = 0usize;
    for (label, template) in T7_TEXTUAL_SAMPLES {
        let raw = expand_t7_sample(template, &root);
        let expected_allow = escape_only.contains(label);
        let audit_action = if expected_allow {
            "permission.allowed_by_policy"
        } else {
            "permission.denied_by_policy"
        };
        let before = audit_count(&harness, audit_action).await;
        let resolution = harness
            .service
            .request(permission_request(
                &request_id(label),
                None,
                "fs.read",
                "read",
                Some(&raw),
                None,
            ))
            .await
            .unwrap();
        if expected_allow {
            assert!(
                resolution.is_allowed(),
                "T7 纯逃逸样本 {label} 按矩阵区外读应 allow（raw={raw}）"
            );
            allowed += 1;
        } else {
            assert!(
                !resolution.is_allowed(),
                "T7 样本 {label} 必须 deny（raw={raw}）"
            );
            denied += 1;
        }
        let after = audit_count(&harness, audit_action).await;
        assert_eq!(after, before + 1, "样本 {label} 必须写审计 {audit_action}");
    }

    // 文件系统样本：Junction（Windows 无特权可建）/ 软链接（Unix 可建；Windows 需开发者模式）。
    let outside = harness.outside.path();
    std::fs::write(outside.join("secret.txt"), b"top-secret").unwrap();
    let mut fs_samples = 0usize;
    let mut fs_denied = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    let junction = harness.workspace.path().join("junction-escape");
    match create_junction(&junction, outside) {
        Ok(()) => {
            fs_samples += 1;
            let resolution = harness
                .service
                .request(permission_request(
                    &request_id("junction"),
                    None,
                    "fs.read",
                    "read",
                    Some(&junction.join("secret.txt").to_string_lossy()),
                    None,
                ))
                .await
                .unwrap();
            assert!(!resolution.is_allowed(), "Junction 逃逸必须 deny");
            fs_denied += 1;
        }
        Err(error) => skipped.push(format!("junction 创建不可用: {error}")),
    }

    let symlink = harness.workspace.path().join("symlink-escape");
    match create_symlink_dir(&symlink, outside) {
        Ok(()) => {
            fs_samples += 1;
            let resolution = harness
                .service
                .request(permission_request(
                    &request_id("symlink"),
                    None,
                    "fs.read",
                    "read",
                    Some(&symlink.join("secret.txt").to_string_lossy()),
                    None,
                ))
                .await
                .unwrap();
            assert!(!resolution.is_allowed(), "软链接逃逸必须 deny");
            fs_denied += 1;
        }
        Err(error) => skipped.push(format!("symlink 创建不可用: {error}")),
    }

    println!(
        "[m2-03-t7] textual={} denied={} allowed={} fs_samples={} fs_denied={} skipped={:?} fs_denied_ratio=100%",
        T7_TEXTUAL_SAMPLES.len(),
        denied,
        allowed,
        fs_samples,
        fs_denied,
        skipped
    );
    assert_eq!(
        denied,
        T7_TEXTUAL_SAMPLES.len() - 3,
        "文本样本除 3 例纯逃逸（区外读 allow）外必须 100% deny"
    );
    assert_eq!(allowed, 3, "纯逃逸三例按矩阵区外读必须 allow");
    assert_eq!(fs_denied, fs_samples, "文件系统样本必须 100% deny");
    assert!(
        fs_samples >= 1,
        "至少应有 1 个文件系统逃逸样本可执行（Junction 或 symlink）"
    );
    shutdown(harness).await;
}

fn create_junction(link: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("mklink /J 退出码 {status}"))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (link, target);
        Err("非 Windows 平台不适用".to_owned())
    }
}

fn create_symlink_dir(link: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).map_err(|error| error.to_string())
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).map_err(|error| error.to_string())
    }
}

/// DoD3：pending 持久化 + 核心重启后恢复 + 300s 超时 deny + 审计（时钟注入）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_survives_restart_and_times_out_after_300s() {
    let harness = harness().await;
    // 区外写（缺省档 ask）制造 pending；区内写免审批不再进入待审批路径。
    let target = outside_target(&harness.outside, "pending.txt");

    // 真实等待者路径：请求进入 pending 后由超时巡检判 deny。
    let service = harness.service.clone();
    let request = permission_request(
        &request_id("timeout-waiter"),
        None,
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let waiter = tokio::spawn(async move { service.request(request).await });

    assert!(
        wait_for(
            || { !harness.service.pending_list(None).is_empty() },
            Duration::from_secs(3)
        )
        .await
    );
    // 内存入队先于持久化（写队列异步提交）；等待 DB 行落库后再断言，消除
    // CI 高负载/覆盖率插桩下的时序竞态（断言语义不变：pending 必须已持久化）。
    assert!(
        wait_permissions_pending(&harness.core.reads(), 1).await,
        "pending 必须已持久化"
    );
    let pending_rows = harness
        .core
        .reads()
        .permissions_pending(None)
        .await
        .unwrap();
    assert_eq!(pending_rows.len(), 1, "pending 必须已持久化");
    assert_eq!(pending_rows[0].status, PermissionStatus::Pending);

    // 核心重启（新服务实例读同一库恢复 pending）。
    let restarted = build_permission_service(
        &harness.core,
        harness.workspace.path(),
        harness.clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let restored = restarted.restore_pending().await.unwrap();
    assert_eq!(restored, 1, "重启后待审批必须恢复");

    // 299.999s 不超时；300.000s 判 deny。
    harness.clock.advance(299_999);
    assert!(restarted.sweep_timeouts_once().await.is_empty());
    harness.clock.advance(1);
    let timed_out = restarted.sweep_timeouts_once().await;
    assert_eq!(timed_out.len(), 1, "恰好 300s 应判超时");

    let row = harness
        .core
        .reads()
        .permissions_pending(None)
        .await
        .unwrap();
    assert!(row.is_empty(), "超时后不得再出现在 pending 清单");
    let actions = audit_actions(&harness).await;
    assert!(
        actions.contains(&"permission.timeout".to_owned()),
        "超时必须写审计: {actions:?}"
    );

    // 重启语义：原等待者随旧核心退出（不再被通知）；此处显式收尾避免悬挂。
    waiter.abort();
    let _ = waiter.await;
    shutdown(harness).await;
}

/// DoD3（等待者路径，同核心）：300s 超时后等待者收到 deny + timed_out。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_waiter_receives_deny_within_same_core() {
    let harness = harness().await;
    // 区外写（缺省档 ask）制造 pending。
    let target = outside_target(&harness.outside, "same-core-timeout.txt");
    let service = harness.service.clone();
    let request = permission_request(
        &request_id("same-core"),
        None,
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let waiter = tokio::spawn(async move { service.request(request).await.unwrap() });
    assert!(
        wait_for(
            || { !harness.service.pending_list(None).is_empty() },
            Duration::from_secs(3)
        )
        .await
    );
    harness.clock.advance(300_000);
    let timed_out = harness.service.sweep_timeouts_once().await;
    assert_eq!(timed_out.len(), 1);
    let resolution: PermissionResolution = waiter.await.unwrap();
    assert!(!resolution.is_allowed());
    assert!(resolution.timed_out, "等待者应收到超时 deny");
    assert!(harness
        .core
        .reads()
        .permissions_pending(None)
        .await
        .unwrap()
        .is_empty());
    shutdown(harness).await;
}

/// DoD4（T6）：100 次并发 ask 无丢失/重复/死锁；决议后行为正确。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t6_100_concurrent_asks_no_loss_no_duplication() {
    let harness = harness().await;
    // 区外写（缺省档 ask）制造 100 个独立 pending。
    let target = outside_target(&harness.outside, "t6.txt");
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..100 {
        let service = harness.service.clone();
        let target = target.clone();
        let request = permission_request(
            &format!("t6-{index}"),
            None,
            "fs.write",
            "write",
            Some(&target),
            Some(10),
        );
        tasks.spawn(async move { (index, service.request(request).await) });
    }

    assert!(
        wait_for(
            || { harness.service.pending_list(None).len() == 100 },
            Duration::from_secs(10)
        )
        .await,
        "100 个请求必须全部进入 pending（无丢失）"
    );
    assert!(
        m2_support::wait_permissions_pending(&harness.core.reads(), 100).await,
        "DB pending 行数必须为 100（无丢失）"
    );

    // 并发决议：50 allow once / 50 deny。
    let pending = harness.service.pending_list(None);
    assert_eq!(pending.len(), 100);
    let mut resolve_tasks = tokio::task::JoinSet::new();
    for (index, ticket) in pending.into_iter().enumerate() {
        let service = harness.service.clone();
        let decision = if index % 2 == 0 {
            (
                PermissionDecision::Allow,
                Some(aether_core::PermissionScope::Once),
            )
        } else {
            (PermissionDecision::Deny, None)
        };
        resolve_tasks.spawn(async move {
            service
                .resolve(&ticket.request_id, decision.0, decision.1)
                .await
        });
    }
    while let Some(result) = resolve_tasks.join_next().await {
        result.unwrap().unwrap();
    }

    let mut allowed = 0usize;
    let mut denied = 0usize;
    let mut seen = std::collections::HashSet::new();
    while let Some(result) = tasks.join_next().await {
        let (index, resolution) = result.unwrap();
        assert!(seen.insert(index), "重复结果");
        let resolution = resolution.unwrap();
        if resolution.is_allowed() {
            allowed += 1;
        } else {
            denied += 1;
        }
    }
    assert_eq!(allowed, 50, "50 次 allow");
    assert_eq!(denied, 50, "50 次 deny");
    assert!(harness.service.pending_list(None).is_empty());
    assert_eq!(
        harness
            .core
            .reads()
            .permissions_pending(None)
            .await
            .unwrap()
            .len(),
        0
    );
    // 重复决议被拒绝（无重复）。
    let error = harness
        .service
        .resolve("t6-0", PermissionDecision::Allow, None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "permission_not_pending");
    shutdown(harness).await;
}

/// DoD5（审计字段）：原始 target 与规范化结果并存；审计携带 request_id/target；
/// 已拒绝请求不得进入 pending。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_preserves_raw_and_canonical_target() {
    let harness = harness().await;
    // 区外写（缺省档 ask）制造 pending。
    let raw = outside_target(&harness.outside, "raw-and-canonical.txt");
    let request = permission_request(
        &request_id("audit"),
        None,
        "fs.write",
        "write",
        Some(&raw),
        Some(5),
    );
    let service = harness.service.clone();
    let task = tokio::spawn(async move { service.request(request).await.unwrap() });
    assert!(
        wait_for(
            || { !harness.service.pending_list(None).is_empty() },
            Duration::from_secs(3)
        )
        .await
    );
    let ticket = harness.service.pending_list(None).first().unwrap().clone();
    assert_eq!(
        ticket.target.as_deref(),
        Some(raw.as_str()),
        "原始 target 保留"
    );
    assert!(
        ticket.canonical_target.is_some(),
        "规范化结果保留（UI 对照展示，D9）"
    );

    let audit = harness.core.reads().audit_log(100).await.unwrap();
    let requested = audit
        .iter()
        .find(|record| record.action == "permission.requested")
        .expect("permission.requested 审计必须存在");
    let detail: Value = serde_json::from_str(requested.detail.as_deref().unwrap()).unwrap();
    assert!(detail.get("request_id").is_some());
    assert!(detail.get("target").is_some());
    assert!(detail.get("runtime_id").is_some());

    harness
        .service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(aether_core::PermissionScope::Once),
        )
        .await
        .unwrap();
    let resolution = task.await.unwrap();
    assert!(resolution.is_allowed());
    shutdown(harness).await;
}

/// 无效资源被拒绝（不进入策略/happy path）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_resource_is_rejected() {
    let harness = harness().await;
    let error = harness
        .service
        .request(permission_request(
            &request_id("bogus"),
            None,
            "fs.teleport",
            "teleport",
            Some("x"),
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "invalid_permission_resource");
    shutdown(harness).await;
}

/// 会话级授权（session 作用域）命中：同 target 后续请求直接 allow。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_scope_grant_applies_to_same_target() {
    let harness = harness().await;
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(
        &harness.core,
        harness.clock.clone(),
        executor,
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "grant").await;
    // 区外写（缺省档 ask）制造 pending；授权命中按 canonical target。
    let target = outside_target(&harness.outside, "grant.txt");

    let first_request = permission_request(
        &request_id("grant-1"),
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(5),
    );
    let service = harness.service.clone();
    let first = tokio::spawn(async move { service.request(first_request).await.unwrap() });
    assert!(
        wait_for(
            || { !harness.service.pending_list(None).is_empty() },
            Duration::from_secs(3)
        )
        .await
    );
    let ticket = harness.service.pending_list(None).first().unwrap().clone();
    harness
        .service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(aether_core::PermissionScope::Session),
        )
        .await
        .unwrap();
    assert!(first.await.unwrap().is_allowed());

    let second = harness
        .service
        .request(permission_request(
            &request_id("grant-2"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(second.is_allowed(), "同 target 会话级授权应直接 allow");
    assert_eq!(second.scope, Some(aether_core::PermissionScope::Session));
    assert!(harness.service.pending_list(None).is_empty());
    shutdown(harness).await;
}

/// 决议不存在/重复决议 → `permission_not_pending`（无重复语义）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolving_unknown_request_is_rejected() {
    let harness = harness().await;
    let error: PermissionError = harness
        .service
        .resolve("missing", PermissionDecision::Allow, None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "permission_not_pending");
    shutdown(harness).await;
}

/// 后台巡检（真实循环）+ 访问器 + 错误展示 + 路径违规码。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_sweeper_times_out_and_accessors_work() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(9_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            sweep_tick: Duration::from_millis(20),
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    assert_eq!(service.config().sweep_tick, Duration::from_millis(20));
    assert!(service.workspace_root_path().is_dir());
    assert_eq!(
        service.spawn_background(&tokio::runtime::Handle::current()),
        1
    );

    // 区外写（缺省档 ask）制造 pending。
    let target = outside_target(&outside, "bg.txt");
    let request = permission_request(
        &request_id("bg-timeout"),
        None,
        "fs.write",
        "write",
        Some(&target),
        Some(1),
    );
    let service_clone = service.clone();
    let waiter = tokio::spawn(async move { service_clone.request(request).await.unwrap() });
    assert!(
        wait_for(
            || { !service.pending_list(None).is_empty() },
            Duration::from_secs(3)
        )
        .await
    );
    clock.advance(300_000);
    assert!(
        wait_for(
            || { service.pending_list(None).is_empty() },
            Duration::from_secs(5)
        )
        .await,
        "后台巡检应在 20ms 周期内判超时"
    );
    let resolution = waiter.await.unwrap();
    assert!(resolution.timed_out);
    service.shutdown_background().await;

    // 错误展示与路径违规码（审计字段口径）。
    let error = PermissionError::NotPending {
        request_id: "r".to_owned(),
    };
    assert!(error.to_string().contains("r"));
    assert_eq!(
        PermissionError::InvalidResource {
            resource: "fs.teleport".to_owned()
        }
        .to_string(),
        "权限资源不在 D9 清单内：fs.teleport"
    );
    let violation = aether_security::PathViolation::Malformed {
        reason: "空".to_owned(),
    };
    assert_eq!(
        aether_control::path_violation_code(&violation),
        "malformed_path"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// 会话绑定请求：`permission.requested/resolved` 事件落库；`pending_list` 按会话过滤；
/// `resolve` 传入 `Ask` 按 deny 处理（无直通）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_bound_events_and_pending_filter() {
    let harness = harness().await;
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(
        &harness.core,
        harness.clock.clone(),
        executor,
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "perm-events").await;
    // 区外写（缺省档 ask）制造 pending。
    let target = outside_target(&harness.outside, "perm-events.txt");
    let request = permission_request(
        &request_id("sess-events"),
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(1),
    );
    let service = harness.service.clone();
    let waiter = tokio::spawn(async move { service.request(request).await.unwrap() });
    assert!(
        wait_for(
            || { !harness.service.pending_list(Some(&session.id)).is_empty() },
            Duration::from_secs(3)
        )
        .await
    );
    let other = aether_core::SessionId::new("01J0000000000000000000000Z").unwrap();
    assert!(harness.service.pending_list(Some(&other)).is_empty());
    let ticket = harness
        .service
        .pending_list(Some(&session.id))
        .first()
        .unwrap()
        .clone();

    // Ask 决议 → 按 deny 处理。
    harness
        .service
        .resolve(&ticket.request_id, PermissionDecision::Ask, None)
        .await
        .unwrap();
    let resolution = waiter.await.unwrap();
    assert!(!resolution.is_allowed());

    let readback = harness
        .core
        .pipeline
        .readback(&session.id, 0)
        .await
        .unwrap();
    let types: Vec<&str> = readback
        .events
        .iter()
        .map(|event| event.event_type().as_str())
        .collect();
    assert!(types.contains(&"permission.requested"), "{types:?}");
    assert!(types.contains(&"permission.resolved"), "{types:?}");
    shutdown(harness).await;
}

/// 会话绑定超时：`permission.resolved`（deny）事件 + 审计字段。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_bound_timeout_emits_deny_event() {
    let harness = harness().await;
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(
        &harness.core,
        harness.clock.clone(),
        executor,
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "perm-timeout").await;
    // 区外写（缺省档 ask）制造 pending。
    let target = outside_target(&harness.outside, "perm-timeout.txt");
    let request = permission_request(
        &request_id("sess-timeout"),
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(1),
    );
    let service = harness.service.clone();
    let waiter = tokio::spawn(async move { service.request(request).await.unwrap() });
    assert!(
        wait_for(
            || { !harness.service.pending_list(None).is_empty() },
            Duration::from_secs(3)
        )
        .await
    );
    harness.clock.advance(300_000);
    let timed_out = harness.service.sweep_timeouts_once().await;
    assert_eq!(timed_out.len(), 1);
    let resolution = waiter.await.unwrap();
    assert!(resolution.timed_out);

    let readback = harness
        .core
        .pipeline
        .readback(&session.id, 0)
        .await
        .unwrap();
    let resolved = readback
        .events
        .iter()
        .find(|event| event.event_type().as_str() == "permission.resolved")
        .expect("超时应广播 permission.resolved");
    let payload = resolved.payload.to_value().unwrap();
    assert_eq!(payload["decision"], "deny");
    shutdown(harness).await;
}
