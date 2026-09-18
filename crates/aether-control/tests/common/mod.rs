//! M1-05 集成测试公共夹具：内存 journal/source（故障注入接缝）与确定性随机。
//!
//! 夹具约束（防止测试自欺）：
//! - `FakeJournal::append` 在 `Behavior::Ok` 分支**复刻 DB 约束**：`events.id` 主键冲突返回
//!   [`JournalError::DuplicateEventId`]、`UNIQUE(session_id, seq)` 冲突返回
//!   [`JournalError::DuplicateSeq`]——sequencer/补读断言因此具备真实兜底语义；
//! - 调用序断言经共享 `timeline`：journal 落盘与订阅者收件写同一时间线，测试比对先后。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_control::{
    EventPipeline, EventSource, JournalError, JournalFuture, JournalMetrics, JournalReceipt,
    JournalWriter, PipelineConfig, SourceFuture, StartupSelfCheckReport,
};
use aether_core::{
    ErrorInfo, EventEnvelope, EventId, EventPayload, LogLevel, LogPayload, MessageDeltaPayload,
    MessageId, MessageSummary, RunId, RuntimeId, SessionId, EVENT_ENVELOPE_VERSION,
};
use serde_json::{json, Value};
use tokio::runtime::Handle;

/// 测试会话 A（26 字符，ULID 形状）。
pub const SESSION_A: &str = "01J0000000000000000000000A";
/// 测试会话 B。
pub const SESSION_B: &str = "01J0000000000000000000000B";
/// 测试 run。
pub const RUN_1: &str = "01J000000000000000000000R1";
/// 测试消息。
pub const MESSAGE_1: &str = "01J000000000000000000000M1";

/// journal 行为脚本（耗尽后默认 `Ok`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Behavior {
    /// 正常落盘。
    Ok,
    /// 写事务失败（`code` 为 SQLite 扩展码）。
    Fail { code: Option<i32>, message: String },
    /// `UNIQUE(session_id, seq)` 兜底命中。
    DuplicateSeq { message: String },
    /// `events.id` 主键冲突。
    DuplicateId,
    /// 写队列临时高水位。
    Backpressure { depth: usize, threshold: usize },
}

impl Behavior {
    /// 写事务失败（盘满/损坏等）。
    pub fn fail(message: &str) -> Self {
        Self::Fail {
            code: Some(13),
            message: message.to_owned(),
        }
    }
}

/// 共享事件存储（journal 落盘与 source 读取共用，模拟同一 SQLite 库）。
#[derive(Default)]
pub struct FakeStore {
    pub events: Mutex<Vec<EventEnvelope>>,
}

/// 内存 journal（故障注入接缝；实现 [`JournalWriter`]）。
#[derive(Clone)]
pub struct FakeJournal {
    store: Arc<FakeStore>,
    script: Arc<Mutex<VecDeque<Behavior>>>,
    calls: Arc<Mutex<Vec<Vec<EventEnvelope>>>>,
    timeline: Arc<Mutex<Vec<String>>>,
    admission: Arc<Mutex<Option<JournalError>>>,
    metrics: Arc<Mutex<JournalMetrics>>,
    max_seq_delay: Arc<Mutex<Option<Duration>>>,
}

impl Default for FakeJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeJournal {
    pub fn new() -> Self {
        Self {
            store: Arc::new(FakeStore::default()),
            script: Arc::new(Mutex::new(VecDeque::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
            timeline: Arc::new(Mutex::new(Vec::new())),
            admission: Arc::new(Mutex::new(None)),
            metrics: Arc::new(Mutex::new(JournalMetrics::default())),
            max_seq_delay: Arc::new(Mutex::new(None)),
        }
    }

    /// 与 journal 共享存储的读取源。
    pub fn source(&self) -> FakeSource {
        FakeSource {
            store: Arc::clone(&self.store),
            max_seq_delay: Arc::clone(&self.max_seq_delay),
        }
    }

    /// 设置行为脚本（按调用顺序消费；耗尽后默认 `Ok`）。
    pub fn script(&self, behaviors: impl IntoIterator<Item = Behavior>) {
        let mut script = self.script.lock().unwrap();
        script.clear();
        script.extend(behaviors);
    }

    /// 重复同一行为 `count` 次（脚本快捷方式）。
    pub fn script_repeat(&self, behavior: Behavior, count: usize) {
        self.script(std::iter::repeat(behavior).take(count));
    }

    /// 已落盘事件（顺序即事务提交顺序）。
    pub fn persisted(&self) -> Vec<EventEnvelope> {
        self.store.events.lock().unwrap().clone()
    }

    /// 每次 append 调用的入参（含失败尝试）。
    pub fn calls(&self) -> Vec<Vec<EventEnvelope>> {
        self.calls.lock().unwrap().clone()
    }

    /// append 调用次数。
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// 共享时间线（`append:<id>` 由夹具写入；`recv:<id>` 由测试在订阅者收件时写入）。
    pub fn timeline(&self) -> Vec<String> {
        self.timeline.lock().unwrap().clone()
    }

    /// 测试向共享时间线追加标记（调用序断言）。
    pub fn mark(&self, entry: impl Into<String>) {
        self.timeline.lock().unwrap().push(entry.into());
    }

    /// 注入准入错误（`admission()`）。
    pub fn set_admission(&self, error: Option<JournalError>) {
        *self.admission.lock().unwrap() = error;
    }

    /// 设置诊断快照。
    pub fn set_metrics(&self, metrics: JournalMetrics) {
        *self.metrics.lock().unwrap() = metrics;
    }

    /// 设置 `max_seq` 读取延迟（sequencer 恢复期间「事件排队」断言用）。
    pub fn set_max_seq_delay(&self, delay: Option<Duration>) {
        *self.max_seq_delay.lock().unwrap() = delay;
    }

    /// 直接写入事件（预置历史数据，模拟既有库）。
    pub fn seed(&self, events: impl IntoIterator<Item = EventEnvelope>) {
        self.store.events.lock().unwrap().extend(events);
    }
}

impl JournalWriter for FakeJournal {
    fn append(&self, events: Vec<EventEnvelope>) -> JournalFuture<'_> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(events.clone());
            for event in &events {
                self.timeline
                    .lock()
                    .unwrap()
                    .push(format!("append:{}", event.id));
            }
            let behavior = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Behavior::Ok);
            match behavior {
                Behavior::Ok => {
                    let mut store = self.store.events.lock().unwrap();
                    for event in &events {
                        if store.iter().any(|existing| existing.id == event.id) {
                            return Err(JournalError::DuplicateEventId);
                        }
                        if store.iter().any(|existing| {
                            existing.session_id == event.session_id && existing.seq == event.seq
                        }) {
                            return Err(JournalError::DuplicateSeq {
                                message: format!(
                                    "UNIQUE constraint failed: events.session_id={}, events.seq={}",
                                    event.session_id, event.seq
                                ),
                            });
                        }
                    }
                    store.extend(events.iter().cloned());
                    Ok(JournalReceipt {
                        entries: events.len(),
                        batch_entries: events.len(),
                        commit_ms: 0,
                    })
                }
                Behavior::Fail { code, message } => {
                    Err(JournalError::TransactionFailed { code, message })
                }
                Behavior::DuplicateSeq { message } => Err(JournalError::DuplicateSeq { message }),
                Behavior::DuplicateId => Err(JournalError::DuplicateEventId),
                Behavior::Backpressure { depth, threshold } => {
                    Err(JournalError::Backpressure { depth, threshold })
                }
            }
        })
    }

    fn admission(&self) -> Result<(), JournalError> {
        match self.admission.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn metrics(&self) -> JournalMetrics {
        *self.metrics.lock().unwrap()
    }
}

/// 内存读取源（实现 [`EventSource`]）。
#[derive(Clone)]
pub struct FakeSource {
    store: Arc<FakeStore>,
    max_seq_delay: Arc<Mutex<Option<Duration>>>,
}

impl EventSource for FakeSource {
    fn max_seq(&self, session_id: &SessionId) -> SourceFuture<'_, Option<u64>> {
        let session = session_id.as_str().to_owned();
        let delay = *self.max_seq_delay.lock().unwrap();
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let events = store.events.lock().unwrap();
            Ok(events
                .iter()
                .filter(|event| event.session_id.as_str() == session)
                .map(|event| event.seq)
                .max())
        })
    }

    fn events_after(
        &self,
        session_id: &SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> SourceFuture<'_, Vec<EventEnvelope>> {
        let session = session_id.as_str().to_owned();
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let events = store.events.lock().unwrap();
            let mut page: Vec<EventEnvelope> = events
                .iter()
                .filter(|event| event.session_id.as_str() == session)
                .filter(|event| after_seq.map_or(true, |after| event.seq > after))
                .cloned()
                .collect();
            page.sort_by_key(|event| event.seq);
            page.truncate(limit);
            Ok(page)
        })
    }
}

/// 启动管线（默认通过的自检报告；journal 与 source 共享同一存储）。
pub fn start_pipeline(journal: &FakeJournal) -> EventPipeline {
    start_pipeline_with(journal, PipelineConfig::default())
}

/// 以自定义配置启动管线（故障注入用；重试间隔置 0 以免拖慢测试）。
pub fn start_pipeline_with(journal: &FakeJournal, config: PipelineConfig) -> EventPipeline {
    let config = PipelineConfig {
        persist_retry_delay: Duration::ZERO,
        ..config
    };
    EventPipeline::start(
        config,
        Arc::new(journal.clone()),
        Arc::new(journal.source()),
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &Handle::current(),
    )
    .expect("管线启动失败")
}

/// 适配器事件 JSON（信封 9 字段 + 适配器 seq 位）。
pub fn raw_event(id: &str, session: &str, event_type: &str, payload: Value) -> Value {
    json!({
        "v": EVENT_ENVELOPE_VERSION,
        "id": id,
        "session_id": session,
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 1_700_000_000_000i64,
        "type": event_type,
        "payload": payload,
    })
}

pub fn log_event(id: &str, session: &str) -> Value {
    raw_event(
        id,
        session,
        "log",
        json!({"level": "info", "message": format!("事件 {id}")}),
    )
}

pub fn delta_event(id: &str, session: &str, message_id: &str, text: &str) -> Value {
    raw_event(
        id,
        session,
        "message.delta",
        json!({"message_id": message_id, "text": text}),
    )
}

pub fn completed_event(id: &str, session: &str, message_id: &str, content: &str) -> Value {
    raw_event(
        id,
        session,
        "message.completed",
        json!({
            "message": {
                "id": message_id,
                "session_id": session,
                "run_id": null,
                "role": "assistant",
                "content": content,
                "created_at": 1_700_000_000_000i64,
            },
            "usage": null,
        }),
    )
}

pub fn usage_event(id: &str, session: &str) -> Value {
    raw_event(
        id,
        session,
        "usage",
        json!({"tokens": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}}),
    )
}

pub fn run_started_event(id: &str, session: &str, run_id: &str) -> Value {
    raw_event(id, session, "run.started", json!({"run_id": run_id}))
}

pub fn run_completed_event(id: &str, session: &str, run_id: &str) -> Value {
    raw_event(
        id,
        session,
        "run.completed",
        json!({"run_id": run_id, "usage": null}),
    )
}

pub fn error_envelope(id: &str, session: &str, seq: u64) -> EventEnvelope {
    EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new(id).unwrap(),
        session_id: SessionId::new(session).unwrap(),
        run_id: None,
        runtime_id: RuntimeId::new("mock").unwrap(),
        seq,
        ts: 1,
        payload: EventPayload::Error(ErrorInfo {
            code: "seed".to_owned(),
            message: format!("预置事件 {id}"),
            recoverable: true,
        }),
    }
}

pub fn log_envelope(id: &str, session: &str, seq: u64) -> EventEnvelope {
    EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new(id).unwrap(),
        session_id: SessionId::new(session).unwrap(),
        run_id: None,
        runtime_id: RuntimeId::new("mock").unwrap(),
        seq,
        ts: 1,
        payload: EventPayload::Log(LogPayload {
            level: LogLevel::Info,
            message: format!("预置事件 {id}"),
        }),
    }
}

pub fn delta_envelope(
    id: &str,
    session: &str,
    seq: u64,
    message_id: &str,
    text: &str,
) -> EventEnvelope {
    EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new(id).unwrap(),
        session_id: SessionId::new(session).unwrap(),
        run_id: None,
        runtime_id: RuntimeId::new("mock").unwrap(),
        seq,
        ts: 1,
        payload: EventPayload::MessageDelta(MessageDeltaPayload {
            message_id: MessageId::new(message_id).unwrap(),
            text: text.to_owned(),
        }),
    }
}

pub fn completed_envelope(
    id: &str,
    session: &str,
    seq: u64,
    message_id: &str,
    content: &str,
) -> EventEnvelope {
    EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new(id).unwrap(),
        session_id: SessionId::new(session).unwrap(),
        run_id: Some(RunId::new(RUN_1).unwrap()),
        runtime_id: RuntimeId::new("mock").unwrap(),
        seq,
        ts: 1,
        payload: EventPayload::MessageCompleted(aether_core::MessageCompletedPayload {
            message: MessageSummary {
                id: MessageId::new(message_id).unwrap(),
                session_id: SessionId::new(session).unwrap(),
                run_id: Some(RunId::new(RUN_1).unwrap()),
                role: aether_core::MessageRole::Assistant,
                content: content.to_owned(),
                created_at: 1,
            },
            usage: None,
        }),
    }
}

/// 确定性伪随机（xorshift64*；不引入依赖，测试可复现）。
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        self.0 = state;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0, bound)` 均匀取值。
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }
}

/// Fisher–Yates 洗牌（确定性）。
pub fn shuffle<T>(items: &mut [T], rng: &mut Rng) {
    if items.len() < 2 {
        return;
    }
    for index in (1..items.len()).rev() {
        let swap_with = rng.below(index + 1);
        items.swap(index, swap_with);
    }
}

/// 轮询等待条件成立（合并窗口/异步冲刷断言用）。
pub async fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    condition()
}
