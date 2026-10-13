//! 事件保留策略与清理任务（M5-04；设计 D4/D13、ADR-021）。
//!
//! 覆盖（ADR-021 §2.1）：
//! - **可清理白名单**：仅 `message.delta` / `usage` / `log` 三类超窗事件；其余事件
//!   类型（控制事件与审计相关信息）不清理；
//! - **窗口**：`events.retention.days`（缺省 30，7–365）；游标 = 事件 `ts`；
//! - **先归档后删除**：每批（≤5000 行）删除前写入归档（`events-archive-<yyyymmdd>.jsonl`，
//!   与 `session_export`/`archive_export` 同格式）并**逐批校验**（manifest 行数/哈希），
//!   归档或校验失败 → 该批跳过清理（失败计数 + 审计 + 不删除）；归档产物与保留窗口解耦，
//!   **不自动删除归档**；
//! - **阈值联动（策略启停）**：库 ≥2GB（警告档）由 UI 提示一键开启；库 ≥5GB（强提示档）
//!   或事件增长 >500MB/月（周采样拟合）→ **自动置 `true`**（审计 `retention.auto_armed`）；
//!   自动启停每进程至多触发一次，避免循环误触发（阈值误报回退条件见 ADR-021 §6-4）；
//! - **清理纪律**：批 ≤5000 行/事务、批间 yield、单事务 <500ms（D3 口径）；清理任务与
//!   容量巡检同频（每小时）限速执行；清理后执行 `wal_checkpoint(TRUNCATE)` 回收 WAL 空间；
//! - **审计不联动**：只触碰 `events` 表（`audit_log` 不清理、不级联）。
//!
//! 桥接口径与自动备份一致：调度器运行于核心 tokio 运行时；命令层经同步桥访问状态。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aether_control::SharedClock;
use aether_security::Redactor;
use aether_store::archive::{
    archive_retention_batch, cleanup_archive_temps, select_retention_batch, verify_archive_file,
    RETENTION_BATCH_MAX,
};
use aether_store::checkpoint::CheckpointConfig;
use aether_store::{AuditLogRecord, ReadPool, StoreCommand, StoreError, WriteQueue};
use serde_json::{json, Value};

use crate::backup_control::{file_len, wal_path, CapacityConfig};
use crate::diagnostics_control::{
    default_setting_value, EVENTS_RETENTION_DAYS_KEY, EVENTS_RETENTION_ENABLED_KEY,
};
use crate::ipc::error::IpcError;

/// 保留清理审计动作（ADR-023 附录 A-1：actor=`system`；result=`success`/`failed`）。
pub const AUDIT_ACTION_RETENTION_CLEANED: &str = "retention.cleaned";
/// 阈值自动启停审计动作（ADR-023 附录 A-1）。
pub const AUDIT_ACTION_RETENTION_ARMED: &str = "retention.auto_armed";
/// 清理周期（ADR-021 §2.1-5：与容量巡检同频，每小时）。
pub const RETENTION_TICK: Duration = Duration::from_secs(60 * 60);
/// 启动后首个 tick 延迟（避免与启动自检争用 IO）。
pub const RETENTION_START_DELAY: Duration = Duration::from_secs(5 * 60);
/// 增长统计窗口（ADR-021 附录 A-4：过去 30 天 `db+wal` 字节差拟合）。
pub const GROWTH_WINDOW_MS: i64 = 30 * DAY_MS;
/// 增长阈值（ADR-021 §2.1-3：>500MB/月 → 自动置 `true`）。
pub const GROWTH_THRESHOLD_BYTES: u64 = 500 * 1024 * 1024;
/// 增长拟合最小跨度（周采样语义：至少 7 天，避免短窗外推）。
pub const GROWTH_MIN_SPAN_MS: i64 = 7 * DAY_MS;
/// 采样点上限（每小时 1 点；30 天窗口内 ≤720）。
pub const RETENTION_SAMPLE_MAX: usize = 1_024;
/// 一天毫秒数。
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// 保留窗口下限（ADR-021 §2.1-2：7–365）。
pub const RETENTION_DAYS_MIN: u32 = 7;
/// 保留窗口上限。
pub const RETENTION_DAYS_MAX: u32 = 365;
/// 保留窗口缺省值。
pub const RETENTION_DAYS_DEFAULT: u32 = 30;

/// 阈值参数（2GB 警告 / 5GB 强提示 / 增长 >500MB/月；容量两档与 M3-05 同源注入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionThresholds {
    pub warn_bytes: u64,
    pub critical_bytes: u64,
    pub growth_bytes_per_30d: u64,
}

impl Default for RetentionThresholds {
    fn default() -> Self {
        let capacity = CapacityConfig::default();
        Self {
            warn_bytes: capacity.warn_bytes,
            critical_bytes: capacity.critical_bytes,
            growth_bytes_per_30d: GROWTH_THRESHOLD_BYTES,
        }
    }
}

impl RetentionThresholds {
    /// 显式构造（测试/演练参数注入；常量级调参记录于任务证据）。
    pub fn with_limits(warn_bytes: u64, critical_bytes: u64, growth_bytes_per_30d: u64) -> Self {
        Self {
            warn_bytes,
            critical_bytes,
            growth_bytes_per_30d,
        }
    }

    /// 环境变量覆盖（复用 M3-05 容量阈值参数化；仅调参，不改变语义）。
    pub fn from_env() -> Self {
        let capacity = CapacityConfig::from_env();
        Self {
            warn_bytes: capacity.warn_bytes,
            critical_bytes: capacity.critical_bytes,
            growth_bytes_per_30d: GROWTH_THRESHOLD_BYTES,
        }
    }

    /// 容量等级（`ok`/`warn`/`critical`；与 `backup_list.capacity` 同口径）。
    pub fn level(&self, total_bytes: u64) -> &'static str {
        if total_bytes >= self.critical_bytes {
            "critical"
        } else if total_bytes >= self.warn_bytes {
            "warn"
        } else {
            "ok"
        }
    }
}

/// 保留策略设置（每 tick 从 settings 读取；UI 修改无需重启）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSettings {
    pub enabled: bool,
    pub days: u32,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            days: RETENTION_DAYS_DEFAULT,
        }
    }
}

/// 清理游标（ADR-021 §2.1-2：`ts < now - days`）。
pub fn retention_cutoff_ms(now_ms: i64, days: u32) -> i64 {
    now_ms.saturating_sub(i64::from(days).saturating_mul(DAY_MS))
}

/// 增长估计（ADR-021 附录 A-4）：过去 30 天 `db+wal` 采样最小二乘拟合，
/// 斜率（字节/天）× 30 天；采样跨度 <7 天返回 `None`（不外推）。
pub fn estimate_growth_bytes_per_30d(samples: &[(i64, u64)]) -> Option<i64> {
    if samples.len() < 2 {
        return None;
    }
    let mut ordered: Vec<(i64, u64)> = samples.to_vec();
    ordered.sort_by_key(|(at, _)| *at);
    let last_at = ordered.last().map(|(at, _)| *at)?;
    let first_at = ordered.first().map(|(at, _)| *at)?;
    if last_at.saturating_sub(first_at) < GROWTH_MIN_SPAN_MS {
        return None;
    }
    let window_start = last_at.saturating_sub(GROWTH_WINDOW_MS);
    let points: Vec<(f64, f64)> = ordered
        .iter()
        .filter(|(at, _)| *at >= window_start)
        .map(|(at, bytes)| {
            let days = (at - window_start) as f64 / DAY_MS as f64;
            (days, *bytes as f64)
        })
        .collect();
    if points.len() < 2 {
        return None;
    }
    let count = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / count;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / count;
    let mut numerator = 0.0f64;
    let mut denominator = 0.0f64;
    for (x, y) in &points {
        numerator += (x - mean_x) * (y - mean_y);
        denominator += (x - mean_x) * (x - mean_x);
    }
    if denominator <= f64::EPSILON {
        return None;
    }
    let slope_per_day = numerator / denominator;
    let growth = slope_per_day * 30.0;
    if !growth.is_finite() {
        return None;
    }
    Some(growth.round() as i64)
}

/// 自动启停原因（审计/状态投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmReason {
    /// 事件增长 >500MB/月（周采样）。
    Growth,
    /// 库 ≥5GB（强提示档）。
    Size,
}

impl ArmReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Growth => "growth",
            Self::Size => "size",
        }
    }
}

/// 自动启停决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmDecision {
    /// 保持现状（未触发或已启用/已自动启用过）。
    Keep,
    /// 自动置 `events.retention.enabled=true`。
    Arm(ArmReason),
}

/// 纯决策（时钟/参数注入单测）：仅当当前关闭且本进程未自动启用过时触发；
/// 强提示档（≥5GB）优先于增长档。
pub fn decide_auto_arm(
    enabled: bool,
    total_bytes: u64,
    growth_bytes_per_30d: Option<i64>,
    thresholds: &RetentionThresholds,
    already_armed_in_process: bool,
) -> ArmDecision {
    if enabled || already_armed_in_process {
        return ArmDecision::Keep;
    }
    if total_bytes >= thresholds.critical_bytes {
        return ArmDecision::Arm(ArmReason::Size);
    }
    if let Some(growth) = growth_bytes_per_30d {
        let threshold = i64::try_from(thresholds.growth_bytes_per_30d).unwrap_or(i64::MAX);
        if growth > threshold {
            return ArmDecision::Arm(ArmReason::Growth);
        }
    }
    ArmDecision::Keep
}

/// 单次 tick 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionOutcome {
    /// 策略关闭（未启用；可能同时触发自动启停）。
    Disabled,
    /// 启用但无可清理事件。
    NothingToClean,
    /// 阈值自动启用。
    Armed(ArmReason),
    /// 清理完成（`rows`/`freed_bytes`/`archives` 为本次汇总）。
    Cleaned {
        rows: u64,
        freed_bytes: u64,
        archives: u64,
    },
    /// 归档或删除失败：该批跳过清理（失败计数 + 审计）。
    Failed { error: String },
}

/// tick 报告（测试/证据投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionTickReport {
    pub outcome: RetentionOutcome,
    pub at_ms: i64,
}

impl RetentionTickReport {
    /// 结果文本（状态投影）。
    pub fn result(&self) -> &'static str {
        match self.outcome {
            RetentionOutcome::Disabled => "disabled",
            RetentionOutcome::NothingToClean => "nothing",
            RetentionOutcome::Armed(_) => "armed",
            RetentionOutcome::Cleaned { .. } => "success",
            RetentionOutcome::Failed { .. } => "failed",
        }
    }
}

/// 运行状态（进程内；重启后采样史清零，阈值判断按新采样重新积累）。
#[derive(Debug, Clone, Default)]
struct RetentionState {
    samples: VecDeque<(i64, u64)>,
    last_run_at: Option<i64>,
    last_result: Option<&'static str>,
    last_cleaned_rows: u64,
    last_freed_bytes: u64,
    last_archive_failures: u64,
    archive_failures_total: u64,
    auto_armed_at: Option<i64>,
    auto_arm_reason: Option<&'static str>,
    armed_in_process: bool,
    last_error: Option<String>,
}

/// 保留任务依赖（组合根注入；测试注入 `ManualClock`/参数化阈值）。
pub struct RetentionDeps {
    pub data_dir: PathBuf,
    pub reads: ReadPool,
    pub write: WriteQueue,
    pub clock: SharedClock,
    pub thresholds: RetentionThresholds,
}

/// 保留策略与清理任务（后台 tick + 状态投影；`Clone` 共享状态）。
#[derive(Clone)]
pub struct RetentionTask {
    data_dir: PathBuf,
    reads: ReadPool,
    write: WriteQueue,
    clock: SharedClock,
    thresholds: Arc<Mutex<RetentionThresholds>>,
    state: Arc<Mutex<RetentionState>>,
}

impl RetentionTask {
    pub fn new(deps: RetentionDeps) -> Self {
        Self {
            data_dir: deps.data_dir,
            reads: deps.reads,
            write: deps.write,
            clock: deps.clock,
            thresholds: Arc::new(Mutex::new(deps.thresholds)),
            state: Arc::new(Mutex::new(RetentionState::default())),
        }
    }

    /// 注入阈值（测试/演练参数化；容量两档与增长档）。
    pub fn set_thresholds(&self, thresholds: RetentionThresholds) {
        let mut slot = self.lock_thresholds();
        *slot = thresholds;
    }

    /// 当前阈值（状态投影/诊断复用）。
    pub fn thresholds(&self) -> RetentionThresholds {
        *self.lock_thresholds()
    }

    fn lock_thresholds(&self) -> MutexGuard<'_, RetentionThresholds> {
        match self.thresholds.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn state_guard(&self) -> MutexGuard<'_, RetentionState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn snapshot_state(&self) -> RetentionState {
        self.state_guard().clone()
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("aether.db")
    }

    /// 归档目录（数据目录下 `archives/`；ADR-021 实现要点 3）。
    pub fn archives_dir(&self) -> PathBuf {
        self.data_dir.join("archives")
    }

    /// 当前 `db+wal` 总字节数（增长采样/空间回收证据）。
    pub fn total_bytes(&self) -> u64 {
        let db = self.db_path();
        file_len(&db).saturating_add(file_len(&wal_path(&db)))
    }

    /// 记录一个采样点（生产 tick 自动记录；测试注入历史样本）。
    pub fn push_sample(&self, at_ms: i64, total_bytes: u64) {
        let mut state = self.state_guard();
        state.samples.push_back((at_ms, total_bytes));
        while state.samples.len() > RETENTION_SAMPLE_MAX {
            state.samples.pop_front();
        }
    }

    /// 当前样本（升序副本）。
    pub fn samples(&self) -> Vec<(i64, u64)> {
        self.snapshot_state().samples.into_iter().collect()
    }

    /// 后台调度任务（首个 tick 于 [`RETENTION_START_DELAY`] 后执行；
    /// 退出序列由组合根经 `AppShutdown` 停止）。
    pub fn spawn(self: &Arc<Self>, handle: &tokio::runtime::Handle) -> tokio::task::JoinHandle<()> {
        let scheduler = Arc::clone(self);
        handle.spawn(async move {
            tokio::time::sleep(RETENTION_START_DELAY).await;
            let _ = scheduler.tick().await;
            loop {
                tokio::time::sleep(RETENTION_TICK).await;
                let _ = scheduler.tick().await;
            }
        })
    }

    /// 读取有效设置（缺省与登记一致；外部改库导致的非法值按缺省回退并留诊断）。
    pub async fn load_settings(&self) -> Result<RetentionSettings, IpcError> {
        let enabled = match self.setting_value(EVENTS_RETENTION_ENABLED_KEY).await? {
            Value::Bool(value) => value,
            other => {
                tracing::warn!(
                    value = %other,
                    "设置 {} 非布尔：按缺省（关闭）处理",
                    EVENTS_RETENTION_ENABLED_KEY
                );
                false
            }
        };
        let days = match self.setting_value(EVENTS_RETENTION_DAYS_KEY).await? {
            Value::Number(number) => match number.as_u64() {
                Some(value) => u32::try_from(
                    value.clamp(u64::from(RETENTION_DAYS_MIN), u64::from(RETENTION_DAYS_MAX)),
                )
                .unwrap_or(RETENTION_DAYS_DEFAULT),
                None => RETENTION_DAYS_DEFAULT,
            },
            _ => RETENTION_DAYS_DEFAULT,
        };
        Ok(RetentionSettings { enabled, days })
    }

    async fn setting_value(&self, key: &str) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let lookup = key.to_owned();
        let raw = reads
            .setting(&lookup)
            .await
            .map_err(|error| IpcError::internal(format!("设置读取失败：{error}")))?;
        Ok(match raw {
            Some(text) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(key = %key, error = %error, "设置值不是合法 JSON：按缺省语义处理");
                    default_setting_value(key)
                }
            },
            None => default_setting_value(key),
        })
    }

    /// 一次 tick：记录采样 → 自动启停评估 → （启用时）清理。
    pub async fn tick(&self) -> RetentionTickReport {
        let at_ms = self.clock.now_ms();
        self.push_sample(at_ms, self.total_bytes());
        match self.evaluate_auto_arm().await {
            Ok(Some(reason)) => {
                return RetentionTickReport {
                    outcome: RetentionOutcome::Armed(reason),
                    at_ms,
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "保留策略自动启停评估失败（跳过本 tick 启停）");
            }
        }
        self.run_cleanup(at_ms).await
    }

    /// 强制执行一次清理（测试/演练；仍按 `enabled` 设置门控）。
    pub async fn run_once(&self) -> RetentionTickReport {
        let at_ms = self.clock.now_ms();
        self.run_cleanup(at_ms).await
    }

    /// 自动启停评估（纯决策 [`decide_auto_arm`] + 落设置 + 审计）。
    pub async fn evaluate_auto_arm(&self) -> Result<Option<ArmReason>, IpcError> {
        let settings = self.load_settings().await?;
        if settings.enabled {
            return Ok(None);
        }
        let state = self.snapshot_state();
        let total = self.total_bytes();
        let samples: Vec<(i64, u64)> = state.samples.iter().copied().collect();
        let growth = estimate_growth_bytes_per_30d(&samples);
        let thresholds = self.thresholds();
        let decision = decide_auto_arm(
            settings.enabled,
            total,
            growth,
            &thresholds,
            state.armed_in_process,
        );
        let ArmDecision::Arm(reason) = decision else {
            return Ok(None);
        };
        let at_ms = self.clock.now_ms();
        let write = self.write.clone();
        write
            .execute(StoreCommand::UpsertSetting {
                key: EVENTS_RETENTION_ENABLED_KEY.to_owned(),
                value: "true".to_owned(),
                updated_at: at_ms,
            })
            .await
            .map_err(|error| IpcError::internal(format!("保留策略自动启用写入失败：{error}")))?;
        {
            let mut state = self.state_guard();
            state.auto_armed_at = Some(at_ms);
            state.auto_arm_reason = Some(reason.as_str());
            state.armed_in_process = true;
        }
        self.write_audit(
            AUDIT_ACTION_RETENTION_ARMED,
            "success",
            None,
            &format!(
                "reason={}; total_bytes={total}; growth_bytes_per_month={}",
                reason.as_str(),
                growth.map_or("unknown".to_owned(), |value| value.to_string())
            ),
        )
        .await;
        Ok(Some(reason))
    }

    /// 清理一轮：批循环（先归档 + 逐批校验 → 删除）；任一失败跳过该批并停止。
    async fn run_cleanup(&self, at_ms: i64) -> RetentionTickReport {
        let settings = match self.load_settings().await {
            Ok(settings) => settings,
            Err(error) => {
                let report = RetentionTickReport {
                    outcome: RetentionOutcome::Failed {
                        error: format!("设置读取失败：{error}"),
                    },
                    at_ms,
                };
                self.record_failure(at_ms, &report).await;
                return report;
            }
        };
        if !settings.enabled {
            return RetentionTickReport {
                outcome: RetentionOutcome::Disabled,
                at_ms,
            };
        }

        let archives_dir = self.archives_dir();
        if let Err(error) = std::fs::create_dir_all(&archives_dir) {
            let report = RetentionTickReport {
                outcome: RetentionOutcome::Failed {
                    error: format!("创建归档目录失败（{}）：{error}", archives_dir.display()),
                },
                at_ms,
            };
            self.record_failure(at_ms, &report).await;
            return report;
        }
        let stale = cleanup_archive_temps(&archives_dir);
        if stale > 0 {
            tracing::warn!(stale, "清理历史归档临时残片（崩溃/中断残留）");
        }

        let cutoff = retention_cutoff_ms(at_ms, settings.days);
        let before = self.total_bytes();
        let mut cleaned_rows = 0u64;
        let mut archives = 0u64;

        loop {
            let reads = self.reads.clone();
            let batch = match reads
                .with_connection(move |connection| {
                    select_retention_batch(connection, cutoff, RETENTION_BATCH_MAX)
                })
                .await
            {
                Ok(batch) => batch,
                Err(error) => {
                    let report = RetentionTickReport {
                        outcome: RetentionOutcome::Failed {
                            error: format!("保留候选读取失败：{error}"),
                        },
                        at_ms,
                    };
                    self.record_failure(at_ms, &report).await;
                    return report;
                }
            };
            if batch.is_empty() {
                break;
            }
            let rowids: Vec<i64> = batch.iter().map(|row| row.rowid).collect();
            let row_count = batch.len() as u64;
            let archive_dir = archives_dir.clone();
            let archive_result =
                tokio::task::spawn_blocking(move || -> Result<PathBuf, StoreError> {
                    let redactor = Redactor::new().map_err(|error| StoreError::Internal {
                        reason: format!("脱敏器初始化失败：{error}"),
                    })?;
                    let sanitize = |value: &Value| -> Result<String, StoreError> {
                        let redacted = redactor.redact_json(value);
                        let line = serde_json::to_string(&redacted).map_err(|error| {
                            StoreError::Internal {
                                reason: format!("归档行序列化失败：{error}"),
                            }
                        })?;
                        if !redactor.is_clean(&line) {
                            return Err(StoreError::Internal {
                                reason: "归档行脱敏后仍命中密钥模式，已拒绝写出（D10）".to_owned(),
                            });
                        }
                        Ok(line)
                    };
                    let report = archive_retention_batch(&archive_dir, &batch, at_ms, &sanitize)?;
                    let verification = verify_archive_file(&report.path)?;
                    if !verification.matches {
                        return Err(StoreError::Internal {
                            reason: format!(
                                "归档产物校验失败（rows={} vs {}；sha256 不一致），跳过该批清理",
                                verification.rows, verification.manifest.rows
                            ),
                        });
                    }
                    Ok(report.path)
                })
                .await
                .map_err(|error| StoreError::Internal {
                    reason: format!("归档任务异常退出：{error}"),
                })
                .and_then(|result| result);

            let archive_path = match archive_result {
                Ok(path) => path,
                Err(error) => {
                    let report = RetentionTickReport {
                        outcome: RetentionOutcome::Failed {
                            error: format!("归档失败，跳过该批清理：{error}"),
                        },
                        at_ms,
                    };
                    self.record_failure(at_ms, &report).await;
                    return report;
                }
            };

            let write = self.write.clone();
            if let Err(error) = write
                .execute(StoreCommand::DeleteEventsByIds { rowids })
                .await
            {
                let report = RetentionTickReport {
                    outcome: RetentionOutcome::Failed {
                        error: format!(
                            "已归档但删除失败（下轮重试；归档 {archive_path:?}）：{error}"
                        ),
                    },
                    at_ms,
                };
                self.record_failure(at_ms, &report).await;
                return report;
            }
            cleaned_rows = cleaned_rows.saturating_add(row_count);
            archives = archives.saturating_add(1);
            tokio::task::yield_now().await;
        }

        // 空间回收：清理后 checkpoint（WAL 截断；失败仅诊断，不失败本轮）。
        if cleaned_rows > 0 {
            if let Err(error) = self
                .write
                .checkpoint_truncate(CheckpointConfig::default())
                .await
            {
                tracing::warn!(error = %error, "保留清理后的 WAL checkpoint 失败（仅诊断）");
            }
        }

        // 审计先写（审计行本身也会写 WAL），再采样 `after` → 释放空间记录与运行结束后
        // 的外部观测一致（证据口径：清理与回收动作后 db+wal 变化）。
        if cleaned_rows > 0 {
            self.write_audit(
                AUDIT_ACTION_RETENTION_CLEANED,
                "success",
                Some(&archives_dir),
                &format!("rows={cleaned_rows}; archives={archives}; cutoff_ts={cutoff}"),
            )
            .await;
        }
        let after = self.total_bytes();
        let freed_bytes = before.saturating_sub(after);

        {
            let mut state = self.state_guard();
            state.last_run_at = Some(at_ms);
            state.last_result = Some("success");
            state.last_cleaned_rows = cleaned_rows;
            state.last_freed_bytes = freed_bytes;
            state.last_archive_failures = 0;
            state.last_error = None;
        }
        if cleaned_rows > 0 {
            RetentionTickReport {
                outcome: RetentionOutcome::Cleaned {
                    rows: cleaned_rows,
                    freed_bytes,
                    archives,
                },
                at_ms,
            }
        } else {
            RetentionTickReport {
                outcome: RetentionOutcome::NothingToClean,
                at_ms,
            }
        }
    }

    /// 失败记录（计数 + 审计；不触碰存储状态机，D4 边界）。
    async fn record_failure(&self, at_ms: i64, report: &RetentionTickReport) {
        let error = match &report.outcome {
            RetentionOutcome::Failed { error } => error.clone(),
            _ => "未知失败".to_owned(),
        };
        {
            let mut state = self.state_guard();
            state.last_run_at = Some(at_ms);
            state.last_result = Some("failed");
            state.last_archive_failures = state.last_archive_failures.saturating_add(1);
            state.archive_failures_total = state.archive_failures_total.saturating_add(1);
            state.last_error = Some(error.clone());
        }
        self.write_audit(AUDIT_ACTION_RETENTION_CLEANED, "failed", None, &error)
            .await;
    }

    /// 审计（写失败只告警，不阻断运行记录）。
    async fn write_audit(
        &self,
        action: &str,
        result: &str,
        resource: Option<&std::path::Path>,
        detail: &str,
    ) {
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: None,
            runtime_id: None,
            actor: "system".to_owned(),
            action: action.to_owned(),
            resource: resource.map(|path| path.to_string_lossy().to_string()),
            detail: Some(detail.to_owned()),
            result: Some(result.to_owned()),
            ts: self.clock.now_ms(),
        };
        if let Err(error) = self
            .write
            .execute(StoreCommand::InsertAudit { record })
            .await
        {
            tracing::warn!(error = %error, "保留策略审计写入失败（不阻断）");
        }
    }

    /// 状态投影（诊断包 `retention` 段与 UI）。
    pub async fn status(&self) -> Result<Value, IpcError> {
        let settings = self.load_settings().await?;
        let now = self.clock.now_ms();
        let total = self.total_bytes();
        let thresholds = self.thresholds();
        let snapshot = self.snapshot_state();
        let samples: Vec<(i64, u64)> = snapshot.samples.iter().copied().collect();
        let growth = estimate_growth_bytes_per_30d(&samples);
        let cutoff = settings
            .enabled
            .then(|| retention_cutoff_ms(now, settings.days));
        Ok(json!({
            "enabled": settings.enabled,
            "days": settings.days,
            "cutoff_ts": cutoff,
            "capacity": {
                "level": thresholds.level(total),
                "total_bytes": total,
                "warn_bytes": thresholds.warn_bytes,
                "critical_bytes": thresholds.critical_bytes,
            },
            "growth": {
                "bytes_per_month": growth,
                "threshold_bytes": thresholds.growth_bytes_per_30d,
                "samples": snapshot.samples.len(),
            },
            "auto_arm": {
                "armed": snapshot.auto_armed_at.is_some(),
                "at": snapshot.auto_armed_at,
                "reason": snapshot.auto_arm_reason,
            },
            "last_run_at": snapshot.last_run_at,
            "last_result": snapshot.last_result,
            "last_cleaned_rows": snapshot.last_cleaned_rows,
            "last_freed_bytes": snapshot.last_freed_bytes,
            "last_archive_failures": snapshot.last_archive_failures,
            "archive_failures_total": snapshot.archive_failures_total,
            "last_error": snapshot.last_error,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    // ===== 策略单测（纯函数；时钟/参数注入的端到端断言见 tests/m5_04_retention.rs）=====

    #[test]
    fn cutoff_uses_days_window() {
        let now = 100 * DAY_MS;
        assert_eq!(retention_cutoff_ms(now, 30), 70 * DAY_MS);
        assert_eq!(retention_cutoff_ms(now, 7), 93 * DAY_MS);
        assert_eq!(retention_cutoff_ms(0, 30), -30 * DAY_MS);
    }

    #[test]
    fn growth_estimate_fits_weekly_samples() {
        // 4 周采样：每周 +150MB → 拟合斜率 ≈ 150MB/周 = 642.86MB/30 天。
        let mb = 1024 * 1024u64;
        let samples = vec![
            (0, 1000 * mb),
            (7 * DAY_MS, 1150 * mb),
            (14 * DAY_MS, 1300 * mb),
            (21 * DAY_MS, 1450 * mb),
            (28 * DAY_MS, 1600 * mb),
        ];
        let growth = estimate_growth_bytes_per_30d(&samples).expect("足够样本必须可拟合");
        let expected = 600 * mb as i64 * 30 / 28;
        let delta = (growth - expected).abs();
        assert!(
            delta < i64::from(10 * 1024 * 1024),
            "30 天增长估计 {growth} 应接近 {expected}（±10MiB）"
        );
        assert!(
            growth > i64::try_from(GROWTH_THRESHOLD_BYTES).unwrap(),
            "周 +150MB 应超过 500MB/月阈值：{growth}"
        );
    }

    #[test]
    fn growth_estimate_requires_week_span_and_two_points() {
        let mb = 1024 * 1024u64;
        assert_eq!(estimate_growth_bytes_per_30d(&[]), None);
        assert_eq!(estimate_growth_bytes_per_30d(&[(0, mb)]), None);
        assert_eq!(
            estimate_growth_bytes_per_30d(&[(0, mb), (DAY_MS, 2 * mb)]),
            None,
            "跨度 <7 天不外推"
        );
    }

    #[test]
    fn growth_estimate_ignores_samples_older_than_window() {
        let mb = 1024 * 1024u64;
        let stale = 100 * DAY_MS;
        let samples = vec![
            (stale - 40 * DAY_MS, mb),
            (stale - 30 * DAY_MS, 100 * mb),
            (stale - 14 * DAY_MS, 200 * mb),
            (stale, 300 * mb),
        ];
        let growth = estimate_growth_bytes_per_30d(&samples).unwrap();
        assert!(growth > 0);
    }

    #[test]
    fn auto_arm_decision_prefers_size_then_growth() {
        let thresholds = RetentionThresholds::with_limits(100, 500, 50);
        assert_eq!(
            decide_auto_arm(false, 10, None, &thresholds, false),
            ArmDecision::Keep
        );
        assert_eq!(
            decide_auto_arm(false, 200, None, &thresholds, false),
            ArmDecision::Keep,
            "仅 2GB 警告档不自动启用（UI 提示一键开启）"
        );
        assert_eq!(
            decide_auto_arm(false, 200, Some(60), &thresholds, false),
            ArmDecision::Arm(ArmReason::Growth)
        );
        assert_eq!(
            decide_auto_arm(false, 600, Some(60), &thresholds, false),
            ArmDecision::Arm(ArmReason::Size),
            "强提示档优先"
        );
        assert_eq!(
            decide_auto_arm(true, 600, Some(60), &thresholds, false),
            ArmDecision::Keep,
            "已启用不重复触发"
        );
        assert_eq!(
            decide_auto_arm(false, 600, Some(60), &thresholds, true),
            ArmDecision::Keep,
            "本进程已自动启用过（防循环误触发）"
        );
        assert_eq!(
            decide_auto_arm(false, 200, Some(50), &thresholds, false),
            ArmDecision::Keep,
            "恰好等于阈值不触发（严格大于）"
        );
    }
}
