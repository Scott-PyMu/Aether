//! 自动备份调度与执行（M5-02；设计 D13、ADR-020、ADR-018 附录 A-5）。
//!
//! 覆盖：
//! - 排期：进程内调度器按设置键 `backup.auto.frequency`（`daily` 缺省 / `weekly`）
//!   触发；总开关 `backup.auto.enabled`（缺省开启）；产物保留
//!   `backup.auto.retention`（缺省 10，仅 `kind='auto'`；手动不自动删除）；
//! - 错过窗口（ADR-020 §2.1-2）：应用未运行错过排期 → 启动后首个调度周期补备一次
//!   （`AUTO_BACKUP_START_DELAY` = 30s，处于 ADR 2 分钟窗口内；连续错过 ≥7 天仍
//!   只补一次——成功即刷新排期）；补备以 `missed_window_catchup` 标记进诊断包；
//! - 失败隔离（ADR-020 §2.1-5）：目标不可写/空间不足/写出失败 → 不产生半成品
//!   （临时文件清理）、不删除既有备份、审计 `backup.auto.run`（actor=system）+
//!   连续失败计数；`RETRY_COOLDOWN_MS`（10min，常量级调参记入任务证据）后重试；
//!   备份域失败**不得**触发 `persist_degraded`（D4 边界：不触碰存储状态机）；
//! - 执行路径与手动备份共用 [`execute_backup`]：`VACUUM INTO` 临时文件 +
//!   `quick_check` 校验 + 原子 rename（[`aether_store::create_backup_file_verified`]）；
//!   自动/手动经同一互斥锁串行（同一时刻最多 1 个备份任务）。
//!
//! 桥接口径：调度器运行于核心 tokio 运行时（async，不阻塞主流程）；命令层经
//! `BackupControlBackend` 的同步桥访问状态。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aether_control::SharedClock;
use aether_security::{
    derive_passphrase_key, encrypt_backup_file, load_or_create_master_key, master_key_available,
    BackupArtifactKey, BackupCryptoParams, SecretStore, SecretValue,
};
use aether_store::backup::{
    backup_file_name_for_with_extension, cleanup_backup_temps, create_backup_staging,
    prune_plan_for_kind, required_space_bytes, BackupRecord, ENCRYPTED_BACKUP_EXTENSION,
};
use aether_store::{AuditLogRecord, ReadPool, StoreCommand, WriteQueue};
use serde_json::{json, Value};

use crate::backup_control::{file_len, wal_path, SpaceProbe};
use crate::diagnostics_control::{
    default_setting_value, BACKUP_AUTO_ENABLED_KEY, BACKUP_AUTO_FREQUENCY_KEY,
    BACKUP_AUTO_RETENTION_KEY, BACKUP_ENCRYPTION_ENABLED_KEY,
};
use crate::ipc::error::IpcError;

/// 自动产物台账 `kind`（ADR-020 §2.1-1）。
pub const BACKUP_KIND_AUTO: &str = "auto";
/// 手动产物台账 `kind`（ADR-020 §2.1-1；位置语义由 `path` 判定）。
pub const BACKUP_KIND_MANUAL: &str = "manual";
/// 自动备份审计动作（ADR-023 附录 A-1：actor=`system`；result=`success`/`failed`）。
pub const AUDIT_ACTION_AUTO_BACKUP: &str = "backup.auto.run";
/// 每日周期（ADR-020 §2.1-1：`daily` 缺省）。
pub const FREQUENCY_DAILY_MS: i64 = 24 * 60 * 60 * 1000;
/// 每周周期（ADR-020 §2.1-1：`weekly`）。
pub const FREQUENCY_WEEKLY_MS: i64 = 7 * FREQUENCY_DAILY_MS;
/// 错过窗口判定阈值/启动补备窗口（ADR-020 §2.1-2：启动后 2 分钟内补备一次）。
pub const CATCHUP_WINDOW_MS: i64 = 2 * 60 * 1000;
/// 失败重试最小间隔（ADR-020「下次窗口重试」；常量级调参，记入 M5-02 证据）。
pub const RETRY_COOLDOWN_MS: i64 = 10 * 60 * 1000;
/// 调度器 tick 间隔（不阻塞主流程；单次 tick 仅在到期时执行备份）。
pub const AUTO_BACKUP_TICK: Duration = Duration::from_secs(60);
/// 启动后首个 tick 延迟（补备落在 ADR-020 §2.1-2 的 2 分钟窗口内；避免与启动自检争用 IO）。
pub const AUTO_BACKUP_START_DELAY: Duration = Duration::from_secs(30);

/// 自动备份频率（设置键枚举 `daily`/`weekly`，ADR-018 附录 A-6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoFrequency {
    Daily,
    Weekly,
}

impl AutoFrequency {
    /// 设置值文本（`backup.auto.frequency`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }

    /// 排期周期（毫秒）。
    pub fn period_ms(self) -> i64 {
        match self {
            Self::Daily => FREQUENCY_DAILY_MS,
            Self::Weekly => FREQUENCY_WEEKLY_MS,
        }
    }

    fn from_value(value: &Value) -> Self {
        match value.as_str() {
            Some("weekly") => Self::Weekly,
            _ => Self::Daily,
        }
    }
}

/// 自动备份有效设置（每 tick 从 settings 读取；UI 修改无需重启）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoBackupSettings {
    pub enabled: bool,
    pub frequency: AutoFrequency,
    pub retention: usize,
    /// M5-03/ADR-020：`backup.encryption.enabled`（缺省 `true`；`true` 且密钥不可用
    /// → 备份暂停并告警，禁止静默降级为明文）。
    pub encryption_enabled: bool,
}

impl Default for AutoBackupSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            frequency: AutoFrequency::Daily,
            retention: aether_store::BACKUP_RETENTION,
            encryption_enabled: true,
        }
    }
}

/// 运行触发来源（审计/诊断 detail）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoTrigger {
    /// 无任何自动产物历史的首次备份（新安装）。
    Initial,
    /// 正常排期（应用运行中到达排期窗口）。
    Scheduled,
    /// 错过窗口补备（应用未运行时错过排期；启动后一次）。
    MissedWindowCatchup,
}

impl AutoTrigger {
    /// 触发来源文本。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Scheduled => "scheduled",
            Self::MissedWindowCatchup => "catchup",
        }
    }
}

/// 排期决策（纯函数输出，便于时钟注入单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDecision {
    /// 未到期/已关闭/失败冷却中。
    Wait,
    /// 到达排期窗口，执行一次。
    Run(AutoTrigger),
}

/// 排期决策（ADR-020 §2.1-1/2 + 失败重试冷却）。
///
/// - 关闭 → 永不触发；
/// - 无成功历史 → 首次触发（`Initial`）；
/// - 上次成功 + 频率 ≤ now → 触发；晚于排期窗口（2 分钟）→ 补备（`MissedWindowCatchup`）；
/// - 上次失败且距上次尝试 < [`RETRY_COOLDOWN_MS`] → 等待（下次重试）。
pub fn decide(
    now_ms: i64,
    settings: &AutoBackupSettings,
    last_success_at: Option<i64>,
    consecutive_failures: u32,
    last_run_at: Option<i64>,
) -> AutoDecision {
    if !settings.enabled {
        return AutoDecision::Wait;
    }
    if consecutive_failures > 0 {
        if let Some(last) = last_run_at {
            if now_ms < last.saturating_add(RETRY_COOLDOWN_MS) {
                return AutoDecision::Wait;
            }
        }
    }
    match last_success_at {
        None => AutoDecision::Run(AutoTrigger::Initial),
        Some(last_success) => {
            let due_at = last_success.saturating_add(settings.frequency.period_ms());
            if now_ms < due_at {
                AutoDecision::Wait
            } else if now_ms.saturating_sub(due_at) > CATCHUP_WINDOW_MS {
                AutoDecision::Run(AutoTrigger::MissedWindowCatchup)
            } else {
                AutoDecision::Run(AutoTrigger::Scheduled)
            }
        }
    }
}

/// 下次排期（`backup_list.auto.next_run_at`；关闭 → `None`）。
pub fn compute_next_run_at(
    now_ms: i64,
    settings: &AutoBackupSettings,
    last_success_at: Option<i64>,
    consecutive_failures: u32,
    last_run_at: Option<i64>,
) -> Option<i64> {
    if !settings.enabled {
        return None;
    }
    if consecutive_failures > 0 {
        if let Some(last) = last_run_at {
            return Some(last.saturating_add(RETRY_COOLDOWN_MS).max(now_ms));
        }
    }
    Some(match last_success_at {
        None => now_ms,
        Some(last_success) => {
            let due_at = last_success.saturating_add(settings.frequency.period_ms());
            if due_at > now_ms {
                due_at
            } else {
                now_ms
            }
        }
    })
}

/// 单次运行回执（测试/证据投影）。
#[derive(Debug, Clone, PartialEq)]
pub struct AutoRunReport {
    pub trigger: AutoTrigger,
    /// `success` / `failed`（`backup_list.auto.last_result` 同源）。
    pub result: &'static str,
    pub backup_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub duration_ms: u64,
    pub pruned: Vec<String>,
    pub error: Option<String>,
}

/// 自动备份运行状态（进程内；`last_success_at` 以 `backups` 表为准）。
#[derive(Debug, Clone, Default)]
struct AutoBackupState {
    last_run_at: Option<i64>,
    last_result: Option<&'static str>,
    consecutive_failures: u32,
    last_trigger: Option<AutoTrigger>,
    last_duration_ms: Option<u64>,
    last_size_bytes: Option<u64>,
    last_error: Option<String>,
    /// 本进程是否执行过错过窗口补备（诊断包「补备标记」）。
    catchup_performed: bool,
}

/// 调度器依赖（组合根注入；测试可注入 `ManualClock` 与失败探针）。
pub struct AutoBackupDeps {
    pub data_dir: PathBuf,
    pub reads: ReadPool,
    pub write: WriteQueue,
    pub space: Arc<dyn SpaceProbe>,
    pub clock: SharedClock,
    /// 自动/手动备份共享互斥锁（同一时刻最多 1 个备份任务）。
    pub run_lock: Arc<tokio::sync::Mutex<()>>,
    /// M5-03/ADR-020：主密钥存储（Keychain；A3 降级为加密文件）。
    /// `None` = 安全存储不可用（加密开启时自动备份暂停并告警）。
    pub secrets: Option<Arc<dyn SecretStore>>,
    /// 口令模式 KDF 参数（生产固定 A3 冻结值；测试/演练注入弱参数）。
    pub crypto_params: BackupCryptoParams,
}

/// 自动备份调度器（后台任务 + 状态投影；`Clone` 共享状态）。
#[derive(Clone)]
pub struct AutoBackup {
    data_dir: PathBuf,
    reads: ReadPool,
    write: WriteQueue,
    space: Arc<Mutex<Arc<dyn SpaceProbe>>>,
    clock: SharedClock,
    run_lock: Arc<tokio::sync::Mutex<()>>,
    secrets: Option<Arc<dyn SecretStore>>,
    crypto_params: BackupCryptoParams,
    state: Arc<Mutex<AutoBackupState>>,
}

impl AutoBackup {
    pub fn new(deps: AutoBackupDeps) -> Self {
        Self {
            data_dir: deps.data_dir,
            reads: deps.reads,
            write: deps.write,
            space: Arc::new(Mutex::new(deps.space)),
            clock: deps.clock,
            run_lock: deps.run_lock,
            secrets: deps.secrets,
            crypto_params: deps.crypto_params,
            state: Arc::new(Mutex::new(AutoBackupState::default())),
        }
    }

    /// 替换空间探针（测试/故障注入：与手动路径同源——`with_space_probe` 转发）。
    pub fn set_space_probe(&self, space: Arc<dyn SpaceProbe>) {
        let mut slot = self.lock_space();
        *slot = space;
    }

    /// 替换主密钥存储（M5-03；测试/演练注入）。
    pub fn set_secrets(&mut self, secrets: Option<Arc<dyn SecretStore>>) {
        self.secrets = secrets;
    }

    /// 以新口令模式 KDF 参数构造克隆（测试/演练；状态共享）。
    #[must_use]
    pub fn with_crypto_params(mut self, params: BackupCryptoParams) -> Self {
        self.crypto_params = params;
        self
    }

    /// 以新时钟构造克隆（测试注入 `ManualClock`；状态共享）。
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    fn lock_space(&self) -> MutexGuard<'_, Arc<dyn SpaceProbe>> {
        match self.space.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn space(&self) -> Arc<dyn SpaceProbe> {
        self.lock_space().clone()
    }

    fn state_guard(&self) -> MutexGuard<'_, AutoBackupState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn snapshot_state(&self) -> AutoBackupState {
        self.state_guard().clone()
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("aether.db")
    }

    /// 后台调度任务（首个 tick 于 [`AUTO_BACKUP_START_DELAY`] 后执行补备检查；
    /// 退出序列由组合根经 `AppShutdown` 停止）。
    pub fn spawn(self: &Arc<Self>, handle: &tokio::runtime::Handle) -> tokio::task::JoinHandle<()> {
        let scheduler = Arc::clone(self);
        handle.spawn(async move {
            tokio::time::sleep(AUTO_BACKUP_START_DELAY).await;
            let _ = scheduler.tick().await;
            loop {
                tokio::time::sleep(AUTO_BACKUP_TICK).await;
                let _ = scheduler.tick().await;
            }
        })
    }

    /// 读取有效设置（缺省与登记一致；外部改库导致的非法值按缺省回退并留诊断）。
    pub async fn load_settings(&self) -> Result<AutoBackupSettings, IpcError> {
        let enabled = match self.setting_value(BACKUP_AUTO_ENABLED_KEY).await? {
            Value::Bool(value) => value,
            other => {
                tracing::warn!(value = %other, "设置 {} 非布尔：按缺省（开启）处理", BACKUP_AUTO_ENABLED_KEY);
                true
            }
        };
        let frequency =
            AutoFrequency::from_value(&self.setting_value(BACKUP_AUTO_FREQUENCY_KEY).await?);
        let retention = match self.setting_value(BACKUP_AUTO_RETENTION_KEY).await? {
            Value::Number(ref number) => match number.as_i64() {
                Some(value) => usize::try_from(value.clamp(1, 100)).unwrap_or(10),
                None => aether_store::BACKUP_RETENTION,
            },
            _ => aether_store::BACKUP_RETENTION,
        };
        let encryption_enabled = match self.setting_value(BACKUP_ENCRYPTION_ENABLED_KEY).await? {
            Value::Bool(value) => value,
            other => {
                tracing::warn!(value = %other, "设置 {} 非布尔：按缺省（加密开启）处理", BACKUP_ENCRYPTION_ENABLED_KEY);
                true
            }
        };
        Ok(AutoBackupSettings {
            enabled,
            frequency,
            retention,
            encryption_enabled,
        })
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

    /// 最近一次成功的自动产出时间（`backups` 表 `kind='auto'` 最大值）。
    async fn last_auto_success_at(&self) -> Result<Option<i64>, IpcError> {
        let reads = self.reads.clone();
        let all = reads
            .backups()
            .await
            .map_err(|error| IpcError::internal(format!("备份清单读取失败：{error}")))?;
        Ok(all
            .iter()
            .filter(|record| record.kind == BACKUP_KIND_AUTO)
            .map(|record| record.created_at)
            .max())
    }

    /// 调度一次：读取设置与历史 → [`decide`] → 到期则执行。
    pub async fn tick(&self) -> Option<AutoRunReport> {
        let settings = match self.load_settings().await {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(error = %error, "自动备份调度：设置读取失败（跳过本 tick）");
                return None;
            }
        };
        if !settings.enabled {
            return None;
        }
        let last_success = match self.last_auto_success_at().await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(error = %error, "自动备份调度：历史读取失败（跳过本 tick）");
                return None;
            }
        };
        let now = self.clock.now_ms();
        let snapshot = self.snapshot_state();
        match decide(
            now,
            &settings,
            last_success,
            snapshot.consecutive_failures,
            snapshot.last_run_at,
        ) {
            AutoDecision::Wait => None,
            AutoDecision::Run(trigger) => Some(self.run_with_settings(settings, trigger).await),
        }
    }

    /// 强制执行一次（测试/演练；排期语义由 [`tick`] 承担，本入口不检查到期）。
    pub async fn run_once(&self, trigger: AutoTrigger) -> AutoRunReport {
        let settings = match self.load_settings().await {
            Ok(settings) => settings,
            Err(error) => {
                return self
                    .record_failure(trigger, None, 0, None, format!("设置读取失败：{error}"))
                    .await;
            }
        };
        self.run_with_settings(settings, trigger).await
    }

    async fn run_with_settings(
        &self,
        settings: AutoBackupSettings,
        trigger: AutoTrigger,
    ) -> AutoRunReport {
        let now = self.clock.now_ms();
        let target = self.data_dir.join("backups");
        if let Err(error) = std::fs::create_dir_all(&target) {
            return self
                .record_failure(
                    trigger,
                    Some(now),
                    0,
                    None,
                    format!("创建默认备份目录失败（{}）：{error}", target.display()),
                )
                .await;
        }
        let id = ulid::Ulid::new().to_string();
        let started = Instant::now();
        let space = self.space();
        let job = BackupJob {
            target: &target,
            kind: BACKUP_KIND_AUTO,
            auto_retention: Some(settings.retention),
            created_at: now,
            id: &id,
            // M5-03/ADR-020：加密缺省开启；密钥不可用 → 失败并告警（不产出明文）。
            encryption: if settings.encryption_enabled {
                BackupEncryption::Keychain
            } else {
                BackupEncryption::None
            },
            secrets: self.secrets.as_deref(),
            crypto_params: self.crypto_params,
        };
        let outcome = execute_backup(
            &self.reads,
            &self.write,
            &self.db_path(),
            &self.run_lock,
            space.as_ref(),
            &job,
        )
        .await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(outcome) => {
                self.record_success(trigger, now, &outcome, duration_ms)
                    .await;
                AutoRunReport {
                    trigger,
                    result: "success",
                    backup_id: Some(outcome.record.id.clone()),
                    size_bytes: Some(outcome.record.size_bytes),
                    duration_ms,
                    pruned: outcome.pruned,
                    error: None,
                }
            }
            Err(error) => {
                self.record_failure(trigger, Some(now), duration_ms, None, error.message.clone())
                    .await
            }
        }
    }

    async fn record_success(
        &self,
        trigger: AutoTrigger,
        at_ms: i64,
        outcome: &BackupOutcome,
        duration_ms: u64,
    ) {
        {
            let mut state = self.state_guard();
            state.last_run_at = Some(at_ms);
            state.last_result = Some("success");
            state.consecutive_failures = 0;
            state.last_trigger = Some(trigger);
            state.last_duration_ms = Some(duration_ms);
            state.last_size_bytes = Some(outcome.record.size_bytes);
            state.last_error = None;
            if trigger == AutoTrigger::MissedWindowCatchup {
                state.catchup_performed = true;
            }
        }
        let detail = format!(
            "trigger={}; id={}; size_bytes={}; duration_ms={}; pruned={}",
            trigger.as_str(),
            outcome.record.id,
            outcome.record.size_bytes,
            duration_ms,
            outcome.pruned.len()
        );
        self.write_audit("success", &detail, Some(&outcome.record.path))
            .await;
    }

    async fn record_failure(
        &self,
        trigger: AutoTrigger,
        at_ms: Option<i64>,
        duration_ms: u64,
        size_bytes: Option<u64>,
        error: String,
    ) -> AutoRunReport {
        {
            let mut state = self.state_guard();
            state.last_run_at = at_ms;
            state.last_result = Some("failed");
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            state.last_trigger = Some(trigger);
            state.last_duration_ms = Some(duration_ms);
            state.last_size_bytes = size_bytes;
            state.last_error = Some(error.clone());
            if trigger == AutoTrigger::MissedWindowCatchup {
                state.catchup_performed = true;
            }
        }
        let detail = format!("trigger={}; error={error}", trigger.as_str());
        self.write_audit("failed", &detail, None).await;
        AutoRunReport {
            trigger,
            result: "failed",
            backup_id: None,
            size_bytes,
            duration_ms,
            pruned: Vec::new(),
            error: Some(error),
        }
    }

    /// 审计 `backup.auto.run`（ADR-023；写失败只告警，不阻断运行记录）。
    async fn write_audit(&self, result: &str, detail: &str, resource: Option<&Path>) {
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: None,
            runtime_id: None,
            actor: "system".to_owned(),
            action: AUDIT_ACTION_AUTO_BACKUP.to_owned(),
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
            tracing::warn!(error = %error, "自动备份审计写入失败（不阻断）");
        }
    }

    /// 完整运行状态（`backup_list.auto` 超集；诊断包 `backup_auto` 段）。
    pub async fn status(&self) -> Result<Value, IpcError> {
        let settings = self.load_settings().await?;
        let now = self.clock.now_ms();
        let last_success = self.last_auto_success_at().await?;
        let snapshot = self.snapshot_state();
        let (last_run_at, last_result) = match (snapshot.last_run_at, snapshot.last_result) {
            (Some(ts), Some(result)) => (Some(ts), Some(result)),
            _ => match last_success {
                Some(ts) => (Some(ts), Some("success")),
                None => (None, None),
            },
        };
        let next_run_at = compute_next_run_at(
            now,
            &settings,
            last_success,
            snapshot.consecutive_failures,
            snapshot.last_run_at,
        );
        Ok(json!({
            "enabled": settings.enabled,
            "frequency": settings.frequency.as_str(),
            "retention": settings.retention,
            "last_run_at": last_run_at,
            "last_result": last_result,
            "consecutive_failures": snapshot.consecutive_failures,
            "next_run_at": next_run_at,
            "missed_window_catchup": snapshot.catchup_performed,
            "last_trigger": snapshot.last_trigger.map(AutoTrigger::as_str),
            "last_duration_ms": snapshot.last_duration_ms,
            "last_size_bytes": snapshot.last_size_bytes,
            "last_error": snapshot.last_error,
            "encryption_enabled": settings.encryption_enabled,
            "encryption_key_available": self.encryption_key_available(),
        }))
    }

    /// 主密钥可用性（M5-03：加密开启且安全存储可用；诊断/状态投影，不创建密钥）。
    fn encryption_key_available(&self) -> bool {
        self.secrets
            .as_ref()
            .is_some_and(|store| master_key_available(store.as_ref()))
    }

    /// `backup_list.auto` 段（ADR-018 附录 A-5：7 字段冻结）。
    pub async fn auto_segment(&self) -> Result<Value, IpcError> {
        let status = self.status().await?;
        Ok(json!({
            "enabled": status["enabled"],
            "frequency": status["frequency"],
            "retention": status["retention"],
            "last_run_at": status["last_run_at"],
            "last_result": status["last_result"],
            "consecutive_failures": status["consecutive_failures"],
            "next_run_at": status["next_run_at"],
        }))
    }
}

/// 备份加密选择（M5-03/ADR-020）。
pub enum BackupEncryption<'a> {
    /// 不加密（`backup.encryption.enabled=false`；产物保持 P0 `.db` 形态）。
    None,
    /// Keychain 主密钥模式（缺省；密钥不可用 → 失败，不产出明文）。
    Keychain,
    /// 口令模式（显式提供口令；Argon2id 派生）。
    Passphrase(&'a SecretValue),
}

/// 备份写出任务（手动/自动共用；ADR-020 实现要点 1）。
pub struct BackupJob<'a> {
    pub target: &'a Path,
    /// `manual`/`auto`（[`BACKUP_KIND_MANUAL`]/[`BACKUP_KIND_AUTO`]）。
    pub kind: &'a str,
    /// `Some(n)` = 按 `kind` 滚动保留 n 份（仅自动）；`None` = 不滚动（手动）。
    pub auto_retention: Option<usize>,
    pub created_at: i64,
    pub id: &'a str,
    /// M5-03：加密选择。
    pub encryption: BackupEncryption<'a>,
    /// M5-03：主密钥存储（Keychain/A3；口令模式不使用）。
    pub secrets: Option<&'a dyn SecretStore>,
    /// M5-03：口令模式 KDF 参数。
    pub crypto_params: BackupCryptoParams,
}

/// 备份执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupOutcome {
    pub record: BackupRecord,
    pub pruned: Vec<String>,
    pub duration_ms: u64,
}

/// 备份写出（手动/自动共用执行路径；ADR-020 §2.1）：
/// 互斥锁串行 → 空间护栏（可写 + 可用 ≥ `db+wal` ×1.2）→ 清理历史临时残留 →
/// `VACUUM INTO` 暂存文件 + `quick_check` 校验（[`create_backup_staging`]）→
/// （M5-03）加密开启时流式加密（`AEBK1`，单一 AEAD tag）→ 原子 rename →
/// 台账登记（失败删除产物，保持台账/磁盘一致）→ 自动类滚动（新产物已校验再删最旧）。
///
/// 加密边界（ADR-020）：加密开启且密钥不可用（Keychain 失败且未提供口令）→
/// **失败并告警，禁止静默降级为明文**；`backup.encryption.enabled=false` 才允许
/// 不加密产物。失败路径不产生半成品、不删除既有备份；不触碰存储状态机（D4 边界）。
pub async fn execute_backup(
    reads: &ReadPool,
    write: &WriteQueue,
    db_path: &Path,
    lock: &tokio::sync::Mutex<()>,
    space: &dyn SpaceProbe,
    job: &BackupJob<'_>,
) -> Result<BackupOutcome, IpcError> {
    let _guard = lock.lock().await;
    let started = Instant::now();

    // 空间护栏（ADR-003 决策 19）：可写 + 可用空间 ≥ 当前 db+wal ×1.2，不足拒绝并提示。
    space.ensure_writable(job.target).map_err(|message| {
        IpcError::invalid_value(format!("{message}（backup_target_not_writable）"))
    })?;
    let required = required_space_bytes(file_len(db_path), file_len(&wal_path(db_path)));
    let available = space.available_bytes(job.target).map_err(|message| {
        IpcError::invalid_value(format!(
            "目标空间探测失败（backup_space_unknown）：{message}"
        ))
    })?;
    if available < required {
        return Err(IpcError::invalid_value(format!(
            "目标可用空间不足（backup_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节（当前 db+wal 的 1.2 倍）；请选择空间充足的目录"
        )));
    }

    // 崩溃/中断残留的临时产物清理（不阻断本次备份）。
    let cleaned = cleanup_backup_temps(job.target);
    if cleaned > 0 {
        tracing::warn!(cleaned, "清理历史备份临时文件（崩溃/中断残留）");
    }

    let encrypted = !matches!(job.encryption, BackupEncryption::None);
    let extension = if encrypted {
        ENCRYPTED_BACKUP_EXTENSION
    } else {
        "db"
    };
    let file_name =
        backup_file_name_for_with_extension(job.target, job.created_at, job.id, extension);
    let dest = job.target.join(&file_name);
    let staging = aether_store::backup::backup_temp_path(&dest, job.created_at, job.id);

    // 暂存写出（VACUUM INTO + quick_check + schema 校验 + fsync；不 rename）。
    {
        let staging_for_task = staging.clone();
        let reads_for_task = reads.clone();
        let outcome = reads_for_task
            .with_connection(move |connection| create_backup_staging(connection, &staging_for_task))
            .await;
        if let Err(error) = outcome {
            let _ = std::fs::remove_file(&staging);
            return Err(IpcError::internal(format!("备份写出失败：{error}")));
        }
    }

    let size_bytes = if encrypted {
        match encrypt_staged_backup(job, &staging, &dest).await {
            Ok(size) => size,
            Err(error) => {
                // 加密任一步失败：明文暂存必须清除（不产出明文/半成品）。
                let _ = std::fs::remove_file(&staging);
                return Err(error);
            }
        }
    } else {
        let outcome = std::fs::rename(&staging, &dest);
        if let Err(error) = outcome {
            let _ = std::fs::remove_file(&staging);
            return Err(IpcError::internal(format!("备份就位失败：{error}")));
        }
        file_len(&dest)
    };
    // 暂存文件（明文）在成功路径删除；失败路径由加密分支清理。
    let _ = std::fs::remove_file(&staging);

    let record = BackupRecord {
        id: job.id.to_owned(),
        path: dest,
        size_bytes,
        encrypted,
        kind: job.kind.to_owned(),
        created_at: job.created_at,
    };

    // 台账登记（经单写队列；AGENTS §2.4）；失败删除刚写出的产物，保持台账与磁盘一致。
    let record_for_task = record.clone();
    let write_for_task = write.clone();
    if let Err(error) = write_for_task
        .execute(StoreCommand::InsertBackup {
            record: record_for_task,
        })
        .await
    {
        if let Err(remove_error) = std::fs::remove_file(&record.path) {
            tracing::warn!(path = %record.path.display(), error = %remove_error, "台账登记失败后的产物清理失败");
        }
        return Err(IpcError::internal(format!("备份台账登记失败：{error}")));
    }

    // 自动产物滚动（仅 `kind='auto'`；新产物已通过 quick_check 校验 → 再删最旧）。
    let mut pruned: Vec<String> = Vec::new();
    if let Some(retention) = job.auto_retention {
        let reads_for_list = reads.clone();
        match reads_for_list.backups().await {
            Ok(all) => {
                for old in prune_plan_for_kind(&all, job.kind, retention) {
                    let write_for_delete = write.clone();
                    match write_for_delete
                        .execute(StoreCommand::DeleteBackup { id: old.id.clone() })
                        .await
                    {
                        Ok(_) => {
                            if old.path.exists() {
                                if let Err(error) = std::fs::remove_file(&old.path) {
                                    tracing::warn!(
                                        path = %old.path.display(),
                                        error = %error,
                                        "滚动清理删除备份文件失败（台账已清理）"
                                    );
                                }
                            }
                            pruned.push(old.id);
                        }
                        Err(error) => {
                            tracing::warn!(id = %old.id, error = %error, "滚动清理台账删除失败（保留该产物）");
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "滚动清理清单读取失败（跳过本次清理）");
            }
        }
    }

    Ok(BackupOutcome {
        record,
        pruned,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// 加密暂存文件 → 产物（M5-03；失败清理暂存/密文临时文件，不产生半成品）。
async fn encrypt_staged_backup(
    job: &BackupJob<'_>,
    staging: &Path,
    dest: &Path,
) -> Result<u64, IpcError> {
    let key = match &job.encryption {
        BackupEncryption::None => {
            return Err(IpcError::internal("加密路径选择不一致（不变量破坏）"))
        }
        BackupEncryption::Keychain => {
            let store = job.secrets.ok_or_else(|| {
                IpcError::invalid_value(
                    "备份加密不可用（backup_key_unavailable）：OS 凭据库不可用且未提供口令；已按 ADR-020 暂停加密备份（不产出不加密产物；可在设置中显式关闭加密）",
                )
            })?;
            let master = load_or_create_master_key(store).map_err(|error| {
                IpcError::invalid_value(format!(
                    "备份加密不可用（backup_key_unavailable）：主密钥读取失败：{error}"
                ))
            })?;
            BackupArtifactKey::Keychain(master)
        }
        BackupEncryption::Passphrase(passphrase) => {
            let (key, salt) = derive_passphrase_key(job.crypto_params, passphrase)
                .map_err(|error| IpcError::internal(format!("备份口令派生失败：{error}")))?;
            BackupArtifactKey::Passphrase { key, salt }
        }
    };

    let cipher_temp = append_suffix(staging, ".cipher");
    let _ = std::fs::remove_file(&cipher_temp);
    let staging_for_task = staging.to_path_buf();
    let cipher_for_task = cipher_temp.clone();
    let source_id = job.id.to_owned();
    let created_at = job.created_at;
    let outcome = tokio::task::spawn_blocking(move || {
        encrypt_backup_file(
            &staging_for_task,
            &cipher_for_task,
            &source_id,
            created_at,
            &key,
        )
    })
    .await
    .map_err(|error| IpcError::internal(format!("备份加密任务失败：{error}")))?;

    let size = match outcome {
        Ok(size) => size,
        Err(error) => {
            let _ = std::fs::remove_file(&cipher_temp);
            return Err(IpcError::internal(format!(
                "备份加密失败（backup_encrypt_failed）：{error}"
            )));
        }
    };
    if let Err(error) = std::fs::rename(&cipher_temp, dest) {
        let _ = std::fs::remove_file(&cipher_temp);
        return Err(IpcError::internal(format!("加密产物就位失败：{error}")));
    }
    Ok(size)
}

/// 追加路径后缀（`OsString` 级拼接，避免 lossy 转换）。
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn settings(enabled: bool, frequency: AutoFrequency) -> AutoBackupSettings {
        AutoBackupSettings {
            enabled,
            frequency,
            retention: 10,
            encryption_enabled: true,
        }
    }

    // ===== DoD1：排期单测（纯决策函数；时钟注入的端到端断言见
    // tests/m5_02_auto_backup.rs）=====

    #[test]
    fn daily_frequency_triggers_at_window_boundary() {
        let settings = settings(true, AutoFrequency::Daily);
        let now = 100 * FREQUENCY_DAILY_MS;
        assert_eq!(
            decide(now, &settings, Some(now - FREQUENCY_DAILY_MS + 1), 0, None),
            AutoDecision::Wait,
            "末次+频率之前不得触发"
        );
        assert_eq!(
            decide(now, &settings, Some(now - FREQUENCY_DAILY_MS), 0, None),
            AutoDecision::Run(AutoTrigger::Scheduled)
        );
        assert_eq!(
            decide(
                now,
                &settings,
                Some(now - FREQUENCY_DAILY_MS - CATCHUP_WINDOW_MS - 1),
                0,
                None
            ),
            AutoDecision::Run(AutoTrigger::MissedWindowCatchup),
            "超出 2 分钟窗口 → 补备"
        );
    }

    #[test]
    fn weekly_frequency_triggers_after_seven_days() {
        let settings = settings(true, AutoFrequency::Weekly);
        let now = 100 * FREQUENCY_DAILY_MS;
        assert_eq!(
            decide(now, &settings, Some(now - 6 * FREQUENCY_DAILY_MS), 0, None),
            AutoDecision::Wait
        );
        assert_eq!(
            decide(now, &settings, Some(now - FREQUENCY_WEEKLY_MS), 0, None),
            AutoDecision::Run(AutoTrigger::Scheduled)
        );
    }

    #[test]
    fn initial_backup_without_history_is_due() {
        let settings = settings(true, AutoFrequency::Daily);
        assert_eq!(
            decide(1_000, &settings, None, 0, None),
            AutoDecision::Run(AutoTrigger::Initial)
        );
    }

    #[test]
    fn disabled_switch_never_triggers() {
        let settings = settings(false, AutoFrequency::Daily);
        assert_eq!(
            decide(i64::MAX, &settings, None, 0, None),
            AutoDecision::Wait
        );
        assert_eq!(
            compute_next_run_at(i64::MAX, &settings, None, 0, None),
            None
        );
    }

    #[test]
    fn failure_retry_respects_cooldown_then_retries() {
        let settings = settings(true, AutoFrequency::Daily);
        let now = 100 * FREQUENCY_DAILY_MS;
        let last_success = now - 5 * FREQUENCY_DAILY_MS;
        let last_run = now - RETRY_COOLDOWN_MS + 1;
        assert_eq!(
            decide(now, &settings, Some(last_success), 1, Some(last_run)),
            AutoDecision::Wait,
            "冷却期内不重试"
        );
        assert_eq!(
            decide(
                now,
                &settings,
                Some(last_success),
                1,
                Some(now - RETRY_COOLDOWN_MS)
            ),
            AutoDecision::Run(AutoTrigger::MissedWindowCatchup),
            "冷却期满重试（仅一次补备——成功后排期刷新）"
        );
    }

    #[test]
    fn next_run_at_tracks_schedule_retry_and_disable() {
        let settings = settings(true, AutoFrequency::Daily);
        let now = 100 * FREQUENCY_DAILY_MS;
        assert_eq!(
            compute_next_run_at(now, &settings, None, 0, None),
            Some(now)
        );
        assert_eq!(
            compute_next_run_at(now, &settings, Some(now - 1_000), 0, None),
            Some(now - 1_000 + FREQUENCY_DAILY_MS)
        );
        assert_eq!(
            compute_next_run_at(
                now,
                &settings,
                Some(now - FREQUENCY_DAILY_MS - 5_000),
                0,
                None
            ),
            Some(now),
            "已过期 → 下次调度检查即触发"
        );
        assert_eq!(
            compute_next_run_at(now, &settings, Some(now - 1_000), 1, Some(now - 100)),
            Some(now - 100 + RETRY_COOLDOWN_MS)
        );
    }
}
