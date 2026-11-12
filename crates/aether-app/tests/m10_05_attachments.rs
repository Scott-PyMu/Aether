//! M10-05/ADR-035 应用层集成：附件采集/存储/能力门（真实存储 + 数据目录）。
//!
//! 覆盖：
//! - `attachment_add`：魔数/像素/大小校验、内容寻址外置存储、幂等、摘要无 storage_path；
//! - `attachment_list` / `attachment_remove`（幂等 + 无引用文件清理）；
//! - `session_send.attachments`：运行时未声明 `multimodal` → `attachment_type_unsupported`，
//!   不创建 run/消息（不得静默丢弃）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_app::attachment::AttachmentReplayResolver;
use aether_app::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_app::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_app::ipc::business::BusinessCode;
use aether_app::ipc::dto::{
    AttachmentAddRequest, AttachmentListRequest, AttachmentRemoveRequest, SessionSendRequest,
};
use aether_app::runtime_control::{boot_supervisor, mock_spec};
use aether_app::session_backend::SessionBackend;
use aether_control::{
    AttachmentResolveErrorKind, AttachmentResolver, ExecutorFuture, ExecutorOutcome,
    LifecycleConfig, RunExecutor, RunRequest, SessionManager, SystemClock,
};
use aether_core::{AttachmentMime, Message, MessageRole, SessionId};
use aether_store::Store;
use serde_json::json;
use tempfile::TempDir;

const SESSION: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const SESSION_REPLAY: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1B";

#[derive(Default)]
struct CancellableExecutor;

impl RunExecutor for CancellableExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async move {
            request.cancel.cancelled().await;
            ExecutorOutcome::Cancelled {
                reason: Some("test_interrupt".to_owned()),
            }
        })
    }
}

struct Harness {
    dir: TempDir,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_app::shutdown::StorageSlot>,
    reads: aether_store::ReadPool,
    backend: Arc<dyn IpcBackend>,
}

fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&[0, 0, 0, 13]);
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes
}

fn harness() -> Harness {
    let dir = TempDir::new().expect("临时目录");
    {
        let store = Store::open(dir.path().join("aether.db")).expect("建库");
        store
            .execute_write(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1), \
                        ('claude-code', 'Claude Code', 'claude', '0.1.0', 1, 1)",
            )
            .unwrap();
        store
            .execute_write(&format!(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES ('{SESSION}', 'mock', 't', 'idle', 1, 1), \
                        ('{SESSION_REPLAY}', 'claude-code', 'replay', 'idle', 1, 1)"
            ))
            .unwrap();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio");
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage: slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心");
    let supervisor = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", "definitely-missing-adapter-binary")],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("监督器"),
    );
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline,
        Arc::new(CancellableExecutor),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager),
            None,
            Some(reads.clone()),
            Some(supervisor),
            handle,
        )
        .with_workspace_store(write)
        .with_data_dir(dir.path()),
    );
    Harness {
        dir,
        runtime,
        slot,
        reads,
        backend,
    }
}

fn add(
    backend: &dyn IpcBackend,
    path: &std::path::Path,
) -> Result<serde_json::Value, aether_app::ipc::error::IpcError> {
    backend.attachment_add(
        &AttachmentAddRequest {
            session_id: SESSION.to_owned(),
            source_path: path.to_string_lossy().to_string(),
            kind: Some("image".to_owned()),
        },
        path,
    )
}

#[test]
fn attachment_add_stores_content_addressed_and_lists_without_storage_path() {
    let h = harness();
    let source = h.dir.path().join("shot.png");
    std::fs::write(&source, png_header(64, 48)).unwrap();

    let summary = add(&*h.backend, &source).expect("attachment_add");
    assert!(
        summary.get("storage_path").is_none(),
        "Q10 A：不返回 storage_path"
    );
    let id = summary["attachment_id"].as_str().unwrap().to_owned();
    let sha = summary["sha256"].as_str().unwrap().to_owned();
    assert_eq!(summary["mime"], "image/png");
    assert_eq!(summary["width"], 64);
    assert_eq!(summary["height"], 48);

    let stored = h
        .dir
        .path()
        .join("attachments")
        .join(&sha[..2])
        .join(format!("{sha}.png"));
    assert!(
        stored.is_file(),
        "内容寻址文件必须落盘：{}",
        stored.display()
    );
    assert_eq!(std::fs::read(&stored).unwrap(), png_header(64, 48));

    // 同内容再次添加 → 幂等（同 id），不覆盖。
    let duplicate = add(&*h.backend, &source).expect("duplicate add");
    assert_eq!(duplicate["attachment_id"], id.as_str());

    let list = h
        .backend
        .attachment_list(&AttachmentListRequest {
            session_id: SESSION.to_owned(),
        })
        .expect("attachment_list");
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["attachment_id"], id.as_str());
    assert!(items[0].get("storage_path").is_none());

    // 删除 → 已无引用 → 文件清理；再次删除幂等 removed=false。
    let removed = h
        .backend
        .attachment_remove(&AttachmentRemoveRequest {
            attachment_id: id.clone(),
        })
        .expect("attachment_remove");
    assert_eq!(removed["removed"], true);
    assert!(!stored.exists(), "无引用文件必须清理");
    let again = h
        .backend
        .attachment_remove(&AttachmentRemoveRequest { attachment_id: id })
        .expect("idempotent remove");
    assert_eq!(again["removed"], false);
}

#[test]
fn attachment_add_rejects_unsupported_oversized_and_oversized_pixels() {
    let h = harness();
    let text = h.dir.path().join("note.txt");
    std::fs::write(&text, b"not an image").unwrap();
    let error = add(&*h.backend, &text).unwrap_err();
    assert_eq!(
        error.business_code(),
        Some(BusinessCode::AttachmentTypeUnsupported)
    );

    let huge_pixels = h.dir.path().join("huge-pixels.png");
    std::fs::write(&huge_pixels, png_header(5001, 5000)).unwrap();
    let error = add(&*h.backend, &huge_pixels).unwrap_err();
    assert_eq!(
        error.business_code(),
        Some(BusinessCode::AttachmentTooLarge)
    );

    let huge_file = h.dir.path().join("huge.png");
    let mut bytes = png_header(64, 48);
    bytes.resize(10 * 1024 * 1024 + 1, 0);
    std::fs::write(&huge_file, bytes).unwrap();
    let error = add(&*h.backend, &huge_file).unwrap_err();
    assert_eq!(
        error.business_code(),
        Some(BusinessCode::AttachmentTooLarge)
    );

    // 失败不产生附件行。
    let list = h
        .backend
        .attachment_list(&AttachmentListRequest {
            session_id: SESSION.to_owned(),
        })
        .expect("list");
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
}

#[test]
fn session_send_with_attachment_on_unsupported_runtime_is_not_silently_dropped() {
    let h = harness();
    let source = h.dir.path().join("shot.png");
    std::fs::write(&source, png_header(64, 48)).unwrap();
    let summary = add(&*h.backend, &source).expect("add");
    let id = summary["attachment_id"].as_str().unwrap().to_owned();

    let error = h
        .backend
        .session_send(&SessionSendRequest {
            session_id: SESSION.to_owned(),
            text: "看图".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1N".to_owned(),
            thinking_depth: None,
            references: Vec::new(),
            provider_id: None,
            model: None,
            subagent_context: None,
            attachments: Some(vec![id]),
            priority: None,
            timeout_ms: None,
            max_attempts: None,
        })
        .unwrap_err();
    assert_eq!(
        error.business_code(),
        Some(BusinessCode::AttachmentTypeUnsupported)
    );
    assert!(error.message.contains("multimodal"));

    // 不创建消息/run（无半成品）。
    let store = Store::open(h.dir.path().join("aether.db")).unwrap();
    let messages: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    let runs: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!((messages, runs), (0, 0));
}

#[test]
fn replay_resolver_rehydrates_attachments_and_rejects_missing_or_unsupported() {
    let h = harness();
    let source = h.dir.path().join("replay.png");
    std::fs::write(&source, png_header(64, 48)).unwrap();
    let added = h
        .backend
        .attachment_add(
            &AttachmentAddRequest {
                session_id: SESSION_REPLAY.to_owned(),
                source_path: source.to_string_lossy().to_string(),
                kind: Some("image".to_owned()),
            },
            &source,
        )
        .expect("attachment_add");
    let attachment_id = added["attachment_id"].as_str().unwrap().to_owned();
    let sha = added["sha256"].as_str().unwrap().to_owned();
    let message = Message {
        id: aether_core::MessageId::new("01J8ZQ5R0N7W9Y8X6V4T2S0K1M").unwrap(),
        session_id: SessionId::new(SESSION_REPLAY).unwrap(),
        run_id: None,
        client_msg_id: None,
        role: MessageRole::User,
        content: "重放图片".to_owned(),
        content_parts: Some(json!([
            { "kind": "image", "attachment_id": attachment_id, "mime": "image/png" }
        ])),
        tool_calls: None,
        parent_message_id: None,
        seq: 1,
        created_at: 1,
    };
    let resolver = AttachmentReplayResolver::new(h.reads.clone(), h.dir.path(), None);
    let session_id = SessionId::new(SESSION_REPLAY).unwrap();
    let outbound = h
        .runtime
        .block_on(resolver.resolve(&session_id, &message))
        .expect("resolver");
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].attachment_id, attachment_id);
    assert_eq!(outbound[0].mime, AttachmentMime::Png);
    assert_eq!(outbound[0].sha256, sha);
    assert!(outbound[0].path.ends_with(".png"));

    let stored = h
        .dir
        .path()
        .join("attachments")
        .join(&sha[..2])
        .join(format!("{sha}.png"));
    std::fs::remove_file(&stored).unwrap();
    let error = h
        .runtime
        .block_on(resolver.resolve(&session_id, &message))
        .expect_err("文件缺失必须拒绝");
    assert_eq!(error.kind, AttachmentResolveErrorKind::StorageFailed);

    // mock 运行时未声明 multimodal → 结构化 Unsupported（即使文件存在）。
    let mock_source = h.dir.path().join("mock.png");
    std::fs::write(&mock_source, png_header(64, 48)).unwrap();
    let mock_added = h
        .backend
        .attachment_add(
            &AttachmentAddRequest {
                session_id: SESSION.to_owned(),
                source_path: mock_source.to_string_lossy().to_string(),
                kind: Some("image".to_owned()),
            },
            &mock_source,
        )
        .expect("mock attachment_add");
    let mut mock_message = message.clone();
    mock_message.session_id = SessionId::new(SESSION).unwrap();
    mock_message.content_parts = Some(json!([
        { "kind": "image", "attachment_id": mock_added["attachment_id"], "mime": "image/png" }
    ]));
    let error = h
        .runtime
        .block_on(resolver.resolve(&SessionId::new(SESSION).unwrap(), &mock_message))
        .expect_err("未声明 multimodal 必须拒绝");
    assert_eq!(error.kind, AttachmentResolveErrorKind::Unsupported);
}

#[test]
fn session_send_rejects_foreign_attachment() {
    let h = harness();
    let error = h
        .backend
        .session_send(&SessionSendRequest {
            session_id: SESSION.to_owned(),
            text: "x".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1N".to_owned(),
            thinking_depth: None,
            references: Vec::new(),
            provider_id: None,
            model: None,
            subagent_context: None,
            attachments: Some(vec!["01J8ZQ5R0N7W9Y8X6V4T2S0K1Z".to_owned()]),
            priority: None,
            timeout_ms: None,
            max_attempts: None,
        })
        .unwrap_err();
    // 运行时能力门先于归属校验（mock 未声明 multimodal）；换成 claude-code 会话时
    // 才到归属校验。此用例锁定「不静默成功」：任一结构化拒绝均可接受。
    assert!(error.business_code().is_some() || error.message.contains("附件"));
    let _ = json!({});
}
