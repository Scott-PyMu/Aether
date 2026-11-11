//! M10-05/ADR-035 控制层集成：附件随消息进入 `content_parts` 与 `RunRequest`。
//!
//! 覆盖：
//! - `send_with_attachments` 在**同一存储事务**内绑定 `attachments.message_id`；
//! - 用户消息 `content_parts` 写入附件引用（含 sha256，renderer 缩略图协议需要）；
//! - 执行器 `RunRequest.attachments` 携带引用帧（D6：数据体不进线协议）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::time::Duration;

use aether_core::{AttachmentMime, OutboundAttachment};
use aether_store::{AttachmentRecord, StoreCommand};
use m2_support::{
    build_manager, create_session, system_clock, wait_for, ScriptedExecutor, TestCore,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_with_attachment_persists_content_parts_and_binds_message() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(1);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "m10-05").await;
    let attachment_id = "01J000000000000000000000A1";
    let sha = "b".repeat(64);
    core.write()
        .execute(StoreCommand::InsertAttachment {
            attachment: AttachmentRecord {
                id: attachment_id.to_owned(),
                session_id: session.id.as_str().to_owned(),
                message_id: None,
                sha256: sha.clone(),
                mime: "image/png".to_owned(),
                size_bytes: 2048,
                width: Some(64),
                height: Some(48),
                storage_path: format!("attachments/bb/{sha}.png"),
                created_at: 1,
            },
        })
        .await
        .unwrap();

    let ack = manager
        .send_with_attachments(
            &session.id,
            "看图",
            "01J00000000000000000000C05",
            None,
            Vec::new(),
            vec![OutboundAttachment {
                attachment_id: attachment_id.to_owned(),
                mime: AttachmentMime::Png,
                width: Some(64),
                height: Some(48),
                path: "C:\\tmp\\pic.png".to_owned(),
                size_bytes: 2048,
                sha256: sha.clone(),
            }],
            None,
        )
        .await
        .unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "执行器必须收到 run"
    );
    let request = executor.calls.lock().unwrap()[0].clone();
    assert_eq!(request.attachments.len(), 1);
    assert_eq!(request.attachments[0].attachment_id, attachment_id);
    assert_eq!(request.attachments[0].mime, AttachmentMime::Png);
    assert_eq!(request.attachments[0].path, "C:\\tmp\\pic.png");
    let wire = request.attachments[0].wire();
    assert!(wire.artifact_ref, "线协议必须为引用帧");
    assert_eq!(wire.sha256, sha);

    let message = core
        .reads()
        .message(&ack.message_id)
        .await
        .unwrap()
        .expect("消息落库");
    let parts = message.content_parts.as_ref().expect("content_parts");
    assert_eq!(parts[0]["kind"], "image");
    assert_eq!(parts[0]["attachment_id"], attachment_id);
    assert_eq!(parts[0]["width"], 64);

    let stored = core
        .reads()
        .attachment(attachment_id)
        .await
        .unwrap()
        .expect("附件行");
    assert_eq!(
        stored.message_id.as_deref(),
        Some(ack.message_id.as_str()),
        "附件必须绑定到消息"
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
