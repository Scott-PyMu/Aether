//! M2-05 DoD3（按 bug 上报）：任务 dump 的 `tracing::error!` 上报由测试侧捕获
//! （D8「看门狗记录任务 dump 并按 bug 上报」；M2-07 运行期日志汇聚端与
//! M3-05 诊断包的输入口径）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_control::LifecycleConfig;
use m2_support::{
    build_manager, create_session, manual_clock, wait_for, TestCore, UnresponsiveExecutor,
};
use tokio::runtime::Builder;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::Registry;

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

/// 不响应任务 → 10s 看门狗 dump 必须经 `tracing::error!` 上报（运行期无 subscriber
/// 时不影响业务；M2-07 汇聚端接线后进入诊断包）。
#[test]
fn watchdog_dump_is_reported_via_tracing() {
    let capture = CaptureLayer::default();
    let subscriber = Registry::default().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("构建 current_thread runtime");

    runtime.block_on(async {
        let core = TestCore::open().await;
        let clock = manual_clock(5_000);
        let executor = UnresponsiveExecutor::new();
        let manager = build_manager(
            &core,
            clock.clone(),
            executor.clone(),
            LifecycleConfig::default(),
        );
        let session = create_session(&manager, "watchdog-log").await;
        let _ack = manager.send(&session.id, "hang", "log-1").await.unwrap();
        assert!(
            wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
            "不响应执行器应被派发"
        );
        manager.interrupt(&session.id).await.unwrap();
        clock.advance(10_000);
        let dumps = manager.sweep_tasks_once();
        assert_eq!(dumps.len(), 1, "10s 必须记录 dump");
        // 等待 abort 生效，避免任务悬挂到测试结束。
        let _ = wait_for(
            || {
                manager.sweep_tasks_once();
                manager.active_task_count() == 0
            },
            Duration::from_secs(5),
        )
        .await;
        core.pipeline.shutdown().await.unwrap();
        core.storage.shutdown().await.unwrap();
    });

    let lines = capture.lines.lock().expect("捕获锁").clone();
    for line in &lines {
        println!("captured watchdog log: {line}");
    }
    assert!(
        lines.iter().any(|line| {
            line.contains("会话任务 dump") && line.contains("强制清理") && line.contains("10000")
        }),
        "必须捕获 dump 上报日志（含阈值与耗时）：{lines:?}"
    );
}
