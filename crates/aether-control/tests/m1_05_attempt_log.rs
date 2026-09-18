//! ADR-007 增量修订 1 决策 2：写失败尝试日志 `attempt=n/3` 的**测试侧 tracing 捕获**。
//!
//! `health` 是只读状态查询，不承载验证用日志内容；验证改由测试侧完成：
//! - dev-dependency `tracing-subscriber` 注册内存捕获层（不进入运行期依赖图）；
//! - 当前线程 runtime + 线程本地订阅器（`set_default`），捕获 actor 任务发出的
//!   `tracing::warn!`（生产路径原样，不引入测试专用代码分支）；
//! - 断言两场景：2 失败 1 成功 → 日志止于 `attempt=2/3`；连续 3 次失败 →
//!   `attempt=1/3`、`2/3`、`3/3`（含「进入 persist_degraded」标注）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};

use aether_control::{PipelineConfig, StorageState};
use tokio::runtime::Builder;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::Registry;

use common::{log_event, start_pipeline_with, Behavior, FakeJournal, SESSION_A};

/// 内存捕获层（收集事件 message 字段）。
#[derive(Clone, Default)]
struct CaptureLayer {
    lines: Arc<Mutex<Vec<String>>>,
}

struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(visitor.message);
        }
    }
}

/// 场景 A（2 失败 + 成功）与场景 B（连续 3 次失败）的 attempt 日志序列。
#[test]
fn attempt_sequence_is_captured_by_tracing() {
    let capture = CaptureLayer::default();
    let subscriber = Registry::default().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("构建 current_thread runtime");

    runtime.block_on(async {
        // 场景 A：第 1、2 次失败，第 3 次成功 → 不降级。
        let journal_a = FakeJournal::new();
        let pipeline_a = start_pipeline_with(&journal_a, PipelineConfig::default());
        journal_a.script([
            Behavior::fail("A 第一次失败"),
            Behavior::fail("A 第二次失败"),
            Behavior::Ok,
        ]);
        let outcome = pipeline_a
            .submit(log_event("01J00000000000000000000TA1", SESSION_A))
            .await
            .expect("提交成功");
        assert!(outcome.is_persisted(), "第 3 次尝试成功必须落盘");
        assert_eq!(pipeline_a.health().storage_state, StorageState::Normal);

        // 场景 B：连续 3 次失败 → 降级。
        let journal_b = FakeJournal::new();
        let pipeline_b = start_pipeline_with(&journal_b, PipelineConfig::default());
        journal_b.script_repeat(Behavior::fail("B 连续失败"), 3);
        let error = pipeline_b
            .submit(log_event("01J00000000000000000000TB1", SESSION_A))
            .await
            .expect_err("必须降级");
        assert_eq!(error.code(), "persist_degraded");
        assert_eq!(
            pipeline_b.health().storage_state,
            StorageState::PersistDegraded
        );
    });

    let lines = capture.lines.lock().expect("捕获锁").clone();
    for line in &lines {
        println!("captured attempt log: {line}");
    }
    let count = |needle: &str| lines.iter().filter(|line| line.contains(needle)).count();
    assert_eq!(count("attempt=1/3"), 2, "场景 A/B 各一次: {lines:?}");
    assert_eq!(count("attempt=2/3"), 2, "场景 A/B 各一次: {lines:?}");
    assert_eq!(count("attempt=3/3"), 1, "仅场景 B: {lines:?}");
    assert_eq!(
        count("attempt=4/3"),
        0,
        "总尝试次数恒为 3（含首次）: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("进入 persist_degraded")),
        "第 3 次失败需标注降级: {lines:?}"
    );
    assert!(
        !lines.is_empty(),
        "必须捕获到 tracing::warn!（运行期无 subscriber 时不影响业务）"
    );
}
