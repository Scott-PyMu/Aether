//! M1-05 DoD①/③/④/⑤：事件管线主路径集成测试（设计 D4）。
//!
//! 覆盖：
//! - DoD① 乱序/重复注入 10k 事件：seq 单调唯一、按到达顺序分配、`evt.id` 幂等；
//! - 先日志后广播：订阅者收件时事件必已在 journal（timeline 调用序断言）；
//! - DoD③ delta 合并：16ms 窗口/8KB 阈值生效；`message.completed` 终稿不受影响；
//! - DoD④ 补读：`last_seq` 缺口补齐；>10k 返回 `readback_gap_too_large`；
//! - DoD⑤ sequencer 崩溃恢复：重启后 seq = max+1，无重复无缺口；恢复期间事件排队。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashSet;
use std::time::Duration;

use aether_control::{
    PipelineConfig, PipelineError, ReadbackFrame, StorageState, SubmitOutcome, READBACK_MAX_GAP,
};
use aether_core::{EventPayload, EventType, SessionId};
use serde_json::{json, Value};

use common::{
    completed_event, delta_event, log_envelope, log_event, raw_event, run_completed_event,
    run_started_event, start_pipeline, start_pipeline_with, usage_event, FakeJournal, Rng,
    MESSAGE_1, SESSION_A, SESSION_B,
};

fn event_id(raw: &Value) -> String {
    raw["id"].as_str().unwrap().to_owned()
}

/// DoD①：乱序 + 重复注入 10k 事件，seq 单调唯一且按到达顺序分配。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dod1_out_of_order_and_duplicate_10k_events_keep_seq_monotonic_unique() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    let mut rng = Rng::new(0xA37E_1201);

    let ids: Vec<String> = (0..10_000).map(|index| format!("01J{index:023}")).collect();
    let mut submissions: Vec<Value> = ids.iter().map(|id| log_event(id, SESSION_A)).collect();
    common::shuffle(&mut submissions, &mut rng);
    // 重复注入 2k：随机挑已有 id 插入随机位置。
    for _ in 0..2_000 {
        let index = rng.below(ids.len());
        let position = rng.below(submissions.len());
        submissions.insert(position, log_event(&ids[index], SESSION_A));
    }

    let mut persisted = 0usize;
    let mut duplicates = 0usize;
    for raw in &submissions {
        match pipeline.submit(raw.clone()).await.unwrap() {
            SubmitOutcome::Persisted { .. } => persisted += 1,
            SubmitOutcome::Duplicate { .. } => duplicates += 1,
            other => panic!("非预期提交结果: {other:?}"),
        }
    }
    assert_eq!(persisted, 10_000, "10k 唯一事件必须全部落盘");
    assert_eq!(duplicates, 2_000, "重复事件必须按 evt.id 幂等丢弃");

    let stored = journal.persisted();
    assert_eq!(stored.len(), 10_000);
    let seqs: Vec<u64> = stored
        .iter()
        .filter(|event| event.session_id.as_str() == SESSION_A)
        .map(|event| event.seq)
        .collect();
    assert_eq!(seqs.len(), 10_000);
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seq 必须严格单调（乱序注入也必须保序分配）"
    );
    let mut unique_seqs = seqs.clone();
    unique_seqs.sort_unstable();
    unique_seqs.dedup();
    assert_eq!(unique_seqs.len(), 10_000, "seq 必须唯一");
    assert_eq!(seqs.first(), Some(&1), "seq 从 1 起");
    assert_eq!(seqs.last(), Some(&10_000), "seq 无缺口（本用例无丢弃路径）");

    let unique_ids: HashSet<String> = stored
        .iter()
        .map(|event| event.id.as_str().to_owned())
        .collect();
    assert_eq!(unique_ids.len(), 10_000, "落盘 id 必须唯一");

    // 到达顺序 = 落盘顺序（乱序注入不影响核心权威顺序）。
    let mut seen = HashSet::new();
    let expected: Vec<String> = submissions
        .iter()
        .map(event_id)
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let actual: Vec<String> = stored
        .iter()
        .map(|event| event.id.as_str().to_owned())
        .collect();
    assert_eq!(actual, expected, "seq 分配必须与到达顺序一致");

    let health = pipeline.health();
    assert_eq!(health.persisted_events, 10_000);
    assert_eq!(health.duplicate_events, 2_000);
    assert_eq!(health.dead_letter_events, 0);
    assert_eq!(health.duplicate_seq_bugs, 0, "DB UNIQUE 兜底不得被触发");
    assert_eq!(health.storage_state, StorageState::Normal);
}

/// DoD①（第二组种子，属性化）：较小规模复跑，防单次随机偶然通过。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dod1_property_second_seed_keeps_seq_contiguous() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    let mut rng = Rng::new(0x51E3_0BEE);

    let ids: Vec<String> = (0..3_000).map(|index| format!("01K{index:023}")).collect();
    let mut submissions: Vec<Value> = ids.iter().map(|id| log_event(id, SESSION_B)).collect();
    common::shuffle(&mut submissions, &mut rng);
    for _ in 0..600 {
        let index = rng.below(ids.len());
        let position = rng.below(ids.len());
        submissions.insert(position, log_event(&ids[index], SESSION_B));
    }
    let mut persisting = 0usize;
    for raw in &submissions {
        if let SubmitOutcome::Persisted { .. } = pipeline.submit(raw.clone()).await.unwrap() {
            persisting += 1;
        }
    }
    assert_eq!(persisting, 3_000);
    let stored = journal.persisted();
    let mut seqs: Vec<u64> = stored.iter().map(|event| event.seq).collect();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        (1..=3_000).collect::<Vec<u64>>(),
        "seq 必须连续无缺口"
    );
}

/// 先日志后广播：订阅者收件时事件必已在 journal（调用序断言）。
#[tokio::test]
async fn write_before_broadcast_is_asserted_by_timeline() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    let mut subscriber = pipeline.subscribe();

    let raw = log_event("01J0000000000000000000BRD1", SESSION_A);
    let id = event_id(&raw);
    let outcome = pipeline.submit(raw).await.unwrap();
    let seq = outcome.seq().expect("必须落盘");

    let received = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("2s 内必须收到广播")
        .expect("广播通道可用");
    assert_eq!(received.id.as_str(), id);
    assert_eq!(received.seq, seq);
    // 收件即证明落盘：journal 中必有该事件。
    assert!(
        journal
            .persisted()
            .iter()
            .any(|event| event.id == received.id),
        "广播事件必须已落盘"
    );

    journal.mark(format!("recv:{}", received.id));
    let timeline = journal.timeline();
    let append_index = timeline
        .iter()
        .position(|entry| entry == &format!("append:{}", received.id))
        .expect("journal 必须记录 append");
    let recv_index = timeline
        .iter()
        .position(|entry| entry == &format!("recv:{}", received.id))
        .expect("测试必须记录 recv");
    assert!(
        append_index < recv_index,
        "先日志后广播被破坏: {timeline:?}"
    );
}

/// 严格校验失败：死信计数、不落库、不阻断会话（D4）。
#[tokio::test]
async fn dead_letter_validation_failures_do_not_block_pipeline() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);

    let unknown_type = raw_event(
        "01J0000000000000000000BAD1",
        SESSION_A,
        "message.self_made",
        json!({}),
    );
    let outcome = pipeline.submit(unknown_type).await.unwrap();
    match outcome {
        SubmitOutcome::DeadLettered { code, .. } => assert_eq!(code, "unknown_event_type"),
        other => panic!("非预期结果: {other:?}"),
    }

    let bad_payload = raw_event(
        "01J0000000000000000000BAD2",
        SESSION_A,
        "log",
        json!({"level": "nope", "message": "x"}),
    );
    let outcome = pipeline.submit(bad_payload).await.unwrap();
    match outcome {
        SubmitOutcome::DeadLettered { code, .. } => assert_eq!(code, "invalid_event_payload"),
        other => panic!("非预期结果: {other:?}"),
    }

    assert_eq!(journal.call_count(), 0, "死信不得触发 journal 写入");
    assert_eq!(pipeline.health().dead_letter_events, 2);
    assert_eq!(pipeline.health().storage_state, StorageState::Normal);

    // 后续合法事件不受影响。
    let outcome = pipeline
        .submit(log_event("01J0000000000000000000OK1", SESSION_A))
        .await
        .unwrap();
    assert_eq!(outcome.seq(), Some(1));
}

/// DoD③（计时路径）：16ms 合并窗口到期后合并为一条持久化。
#[tokio::test]
async fn dod3_delta_merge_window_flushes_merged_event() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline_with(
        &journal,
        PipelineConfig {
            delta_flush_interval: Duration::from_millis(100),
            ..PipelineConfig::default()
        },
    );
    // 并发提交两条 delta（同一窗口内到达）。
    let first = delta_event("01J0000000000000000000DLT1", SESSION_A, MESSAGE_1, "Hello ");
    let second = delta_event("01J0000000000000000000DLT2", SESSION_A, MESSAGE_1, "world");
    let (a, b) = tokio::join!(pipeline.submit(first), pipeline.submit(second));
    assert_eq!(a.unwrap(), SubmitOutcome::Buffered);
    assert_eq!(b.unwrap(), SubmitOutcome::Buffered);
    assert_eq!(journal.persisted().len(), 0, "窗口期内不落盘");

    assert!(
        common::wait_for(|| journal.persisted().len() == 1, Duration::from_secs(3)).await,
        "100ms 窗口到期必须冲刷"
    );
    let stored = journal.persisted();
    match &stored[0].payload {
        EventPayload::MessageDelta(delta) => {
            assert_eq!(delta.text, "Hello world", "合并文本必须按到达顺序拼接");
            assert_eq!(delta.message_id.as_str(), MESSAGE_1);
        }
        other => panic!("类型不符: {other:?}"),
    }
    assert_eq!(pipeline.health().delta_input_events, 2);
    assert_eq!(
        pipeline.health().delta_persisted_events,
        1,
        "2 条输入合并为 1 条"
    );
}

/// DoD③（阈值路径 + 非 delta 冲刷）：8KB 立即冲刷；`message.completed` 终稿不受影响。
#[tokio::test]
async fn dod3_delta_threshold_and_completed_final_are_intact() {
    // 阈值路径：默认 8KB 阈值。
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    let big = "x".repeat(8 * 1024);
    pipeline
        .submit(delta_event(
            "01J0000000000000000000BIG1",
            SESSION_A,
            MESSAGE_1,
            &big,
        ))
        .await
        .unwrap();
    assert_eq!(
        journal.persisted().len(),
        1,
        "累计 ≥8KB 必须立即落盘（不等窗口）"
    );

    // 非 delta 冲刷 + 终稿：窗口设为 10s（不可能自然到期），由 usage/completed 触发冲刷。
    let journal2 = FakeJournal::new();
    let pipeline2 = start_pipeline_with(
        &journal2,
        PipelineConfig {
            delta_flush_interval: Duration::from_secs(10),
            ..PipelineConfig::default()
        },
    );
    let mut subscriber = pipeline2.subscribe();
    pipeline2
        .submit(delta_event(
            "01J0000000000000000000FIN1",
            SESSION_A,
            MESSAGE_1,
            "Hel",
        ))
        .await
        .unwrap();
    pipeline2
        .submit(delta_event(
            "01J0000000000000000000FIN2",
            SESSION_A,
            MESSAGE_1,
            "lo ",
        ))
        .await
        .unwrap();
    pipeline2
        .submit(usage_event("01J0000000000000000000FIN3", SESSION_A))
        .await
        .unwrap();
    pipeline2
        .submit(completed_event(
            "01J0000000000000000000FIN4",
            SESSION_A,
            MESSAGE_1,
            "Hello world",
        ))
        .await
        .unwrap();

    let stored = journal2.persisted();
    assert_eq!(stored.len(), 3, "delta/usage/completed 各一条: {stored:?}");
    match &stored[0].payload {
        EventPayload::MessageDelta(delta) => assert_eq!(delta.text, "Hello "),
        other => panic!("第 1 条应为合并 delta: {other:?}"),
    }
    assert_eq!(stored[1].event_type(), EventType::Usage);
    match &stored[2].payload {
        EventPayload::MessageCompleted(completed) => {
            assert_eq!(
                completed.message.content, "Hello world",
                "completed 终稿不得被 delta 合并改动"
            );
            assert_eq!(completed.message.id.as_str(), MESSAGE_1);
        }
        other => panic!("第 3 条应为 completed: {other:?}"),
    }
    assert!(
        stored[0].seq < stored[1].seq && stored[1].seq < stored[2].seq,
        "seq 顺序必须保持 delta → usage → completed"
    );

    let mut received = Vec::new();
    while let Ok(event) = subscriber.try_recv() {
        received.push(event);
    }
    assert_eq!(received.len(), 3, "落盘后才广播，且顺序一致");
    assert_eq!(received[0].event_type(), EventType::MessageDelta);
    assert_eq!(received[2].event_type(), EventType::MessageCompleted);
}

/// DoD④：`last_seq` 缺口补齐；>10k 拒绝自动补发并返回明确错误码。
#[tokio::test]
async fn dod4_readback_fills_gap_and_rejects_gap_over_10k() {
    let journal = FakeJournal::new();
    // 预置 25k 会话 A 事件（连续 seq）；会话 B 存在缺口（1,2,5,6）。
    journal.seed((1..=25_000u64).map(|seq| log_envelope(&format!("01J{seq:023}"), SESSION_A, seq)));
    journal.seed([
        log_envelope("01J00000000000000000000B01", SESSION_B, 1),
        log_envelope("01J00000000000000000000B02", SESSION_B, 2),
        log_envelope("01J00000000000000000000B05", SESSION_B, 5),
        log_envelope("01J00000000000000000000B06", SESSION_B, 6),
    ]);
    let pipeline = start_pipeline(&journal);
    let session_a = SessionId::new(SESSION_A).unwrap();
    let session_b = SessionId::new(SESSION_B).unwrap();

    // 缺口 10_000：允许补发。
    let frame: ReadbackFrame = pipeline.readback(&session_a, 15_000).await.unwrap();
    assert_eq!(frame.events.len(), 10_000);
    assert_eq!(frame.events.first().map(|event| event.seq), Some(15_001));
    assert_eq!(frame.events.last().map(|event| event.seq), Some(25_000));
    assert_eq!(frame.max_seq, Some(25_000));
    assert!(frame.complete);
    assert!(
        frame
            .events
            .windows(2)
            .all(|pair| pair[0].seq < pair[1].seq),
        "补读必须升序"
    );

    // 缺口 10_001：拒绝自动补发（明确错误码）。
    let error = pipeline.readback(&session_a, 14_999).await.unwrap_err();
    assert_eq!(error.code(), "readback_gap_too_large");
    match error {
        PipelineError::ReadbackGapTooLarge { gap, limit } => {
            assert_eq!(gap, 10_001);
            assert_eq!(limit, READBACK_MAX_GAP);
        }
        other => panic!("非预期错误: {other:?}"),
    }

    // 已到最新 / 超前：空补读。
    let frame = pipeline.readback(&session_a, 25_000).await.unwrap();
    assert!(frame.events.is_empty() && frame.complete);
    let frame = pipeline.readback(&session_a, 99_999).await.unwrap();
    assert!(frame.events.is_empty() && frame.complete);

    // 会话内 seq 缺口（delta 丢弃场景）同样按 `> last_seq` 补齐。
    let frame = pipeline.readback(&session_b, 2).await.unwrap();
    let seqs: Vec<u64> = frame.events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![5, 6]);
    assert!(frame.complete);

    // 会话起点补读（last_seq=0）：缺口 6 ≤ 10k，补齐全部已有 seq。
    let frame = pipeline.readback(&session_b, 0).await.unwrap();
    let seqs: Vec<u64> = frame.events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![1, 2, 5, 6]);
    assert!(frame.complete);

    // 未知会话：空补读。
    let unknown = SessionId::new("01J000000000000000000000ZZ").unwrap();
    let frame = pipeline.readback(&unknown, 0).await.unwrap();
    assert!(frame.events.is_empty());
    assert_eq!(frame.max_seq, None);
}

/// DoD⑤：sequencer 崩溃重启后 seq = max+1，无重复无缺口。
#[tokio::test]
async fn dod5_sequencer_restart_resumes_at_max_plus_one() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    for index in 1..=5 {
        let outcome = pipeline
            .submit(log_event(&format!("01J{index:023}"), SESSION_A))
            .await
            .unwrap();
        assert_eq!(outcome.seq(), Some(index));
    }
    let session = SessionId::new(SESSION_A).unwrap();
    pipeline.restart_session(session.clone()).await.unwrap();

    let outcome = pipeline
        .submit(log_event("01J00000000000000000000901", SESSION_A))
        .await
        .unwrap();
    assert_eq!(outcome.seq(), Some(6), "重启后 seq = max(seq)+1");
    let outcome = pipeline
        .submit(log_event("01J00000000000000000000902", SESSION_A))
        .await
        .unwrap();
    assert_eq!(outcome.seq(), Some(7));

    let seqs: Vec<u64> = journal.persisted().iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7], "无重复无缺口");
    assert_eq!(pipeline.health().sequencer_restarts, 1);
    assert_eq!(pipeline.health().duplicate_seq_bugs, 0);
}

/// DoD⑤：崩溃时未落盘 delta 丢弃（不广播；允许缺口），恢复期间提交在队列排队。
#[tokio::test]
async fn dod5_restart_discards_unflushed_deltas_and_queues_submissions() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline_with(
        &journal,
        PipelineConfig {
            delta_flush_interval: Duration::from_secs(10),
            ..PipelineConfig::default()
        },
    );
    pipeline
        .submit(log_event("01J00000000000000000000R01", SESSION_A))
        .await
        .unwrap();
    pipeline
        .submit(delta_event(
            "01J00000000000000000000R02",
            SESSION_A,
            MESSAGE_1,
            "ab",
        ))
        .await
        .unwrap();
    pipeline
        .submit(delta_event(
            "01J00000000000000000000R03",
            SESSION_A,
            MESSAGE_1,
            "cd",
        ))
        .await
        .unwrap();
    assert_eq!(journal.persisted().len(), 1, "delta 仅在缓冲中");

    // 恢复读取 max_seq 延迟 50ms：期间提交在入站队列排队，恢复后按到达顺序分配。
    journal.set_max_seq_delay(Some(Duration::from_millis(50)));
    let session = SessionId::new(SESSION_A).unwrap();
    pipeline.restart_session(session).await.unwrap();

    let first = pipeline
        .submit(log_event("01J00000000000000000000R04", SESSION_A))
        .await
        .unwrap();
    let second = pipeline
        .submit(log_event("01J00000000000000000000R05", SESSION_A))
        .await
        .unwrap();
    assert_eq!(first.seq(), Some(2), "max=1 → 恢复后从 2 起");
    assert_eq!(second.seq(), Some(3));

    let stored = journal.persisted();
    let types: Vec<EventType> = stored.iter().map(|event| event.event_type()).collect();
    assert_eq!(
        types,
        vec![EventType::Log, EventType::Log, EventType::Log],
        "未落盘 delta 必须丢弃（无 delta 行）"
    );
    let health = pipeline.health();
    assert_eq!(health.delta_buffers_discarded, 2, "丢弃的输入 delta 计数");
    assert_eq!(health.sequencer_restarts, 1);
    assert_eq!(
        health.dropped_events, 0,
        "丢弃 delta 只计 delta_buffers_discarded"
    );
}

/// run 终态解除在途台账（为降级取消断言提供基线）。
#[tokio::test]
async fn run_terminal_state_clears_in_flight_tracking() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    pipeline
        .submit(run_started_event(
            "01J00000000000000000000N01",
            SESSION_A,
            common::RUN_1,
        ))
        .await
        .unwrap();
    assert_eq!(pipeline.health().in_flight_runs, 1);
    pipeline
        .submit(run_completed_event(
            "01J00000000000000000000N02",
            SESSION_A,
            common::RUN_1,
        ))
        .await
        .unwrap();
    assert_eq!(pipeline.health().in_flight_runs, 0);
}

/// 关闭序列：放弃未落盘 delta（D2：drain 限时放弃 delta，保留控制事件）。
#[tokio::test]
async fn shutdown_discards_unflushed_deltas() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline_with(
        &journal,
        PipelineConfig {
            delta_flush_interval: Duration::from_secs(10),
            ..PipelineConfig::default()
        },
    );
    pipeline
        .submit(log_event("01J00000000000000000000D01", SESSION_A))
        .await
        .unwrap();
    pipeline
        .submit(delta_event(
            "01J00000000000000000000D02",
            SESSION_A,
            MESSAGE_1,
            "未落盘",
        ))
        .await
        .unwrap();
    pipeline.shutdown().await.unwrap();

    let stored = journal.persisted();
    assert_eq!(stored.len(), 1, "关闭后仅保留已落盘控制事件");
    assert_eq!(stored[0].event_type(), EventType::Log);
    assert_eq!(pipeline.health().delta_buffers_discarded, 1);
    // 关停后提交被拒绝（管线已关闭）。
    let error = pipeline
        .submit(log_event("01J00000000000000000000D03", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "pipeline_closed");
}
