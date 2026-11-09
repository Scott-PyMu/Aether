//! M10-02/ADR-032：子 Agent 完整结果与注入体持久化。
//!
//! 单写者约束（AGENTS §2.4）：全部写路径经 [`crate::WriteQueue::execute`] 进入
//! [`crate::StoreCommand::Subagent`]；本模块只提供事务实现与只读查询。
//!
//! 口径：
//! - 完整结果（≤64KiB canonical JSON）只存 `subagent_results`，**不进** `sessions.config`；
//! - compact envelope 与状态存 `subagent_injections`，`(parent_session_id,batch_id)` 唯一；
//! - `summary` 为 UTF-8 字节截断（≤512B），全量摘要仍在 `result` JSON；
//! - 父会话授权在命令层完成；本层仅按 `parent_session_id`/`child_session_id` 读写。

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::StoreError;
use crate::ops::{txn_failed, StoreOutcome};
use crate::write_queue::ReadPool;

/// `subagent_results` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentResultRecord {
    pub child_session_id: String,
    /// canonical JSON（UTF-8）。
    pub result: String,
    pub total_bytes: i64,
    pub truncated: bool,
    pub verdict: Option<String>,
    /// ≤512 字节（UTF-8）。
    pub summary: Option<String>,
    pub summary_truncated: bool,
    pub findings_count: i64,
    pub created_at: i64,
}

/// `subagent_injections` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentInjectionRecord {
    pub id: String,
    pub parent_session_id: String,
    pub batch_id: String,
    /// JSON array（child session ids）。
    pub child_session_ids: String,
    /// compact envelope canonical JSON。
    pub payload: String,
    /// JSON array（`subagent:<child>`）。
    pub result_refs: String,
    /// `pending|injected|folded|rejected|superseded`。
    pub state: String,
    /// `message_append|origin_fork|system_replace`。
    pub mode: String,
    pub supersedes_id: Option<String>,
    pub requires_approval: bool,
    pub context_version: i64,
    pub created_at: i64,
    pub injected_at: Option<i64>,
    pub folded_at: Option<i64>,
}

/// 子 Agent 写命令（嵌套于 [`crate::StoreCommand::Subagent`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentCommand {
    /// 写入完整结果；同 child 重复写入为幂等 no-op（`inserted=false`）。
    InsertResult { result: SubagentResultRecord },
    /// 写入注入行；`(parent,batch)` 冲突时返回既有行（`inserted=false`）。
    InsertInjection { injection: SubagentInjectionRecord },
    /// 更新注入状态（终态不可逆由应用层保证；本层只落指定字段）。
    UpdateInjectionState {
        id: String,
        state: String,
        injected_at: Option<i64>,
        folded_at: Option<i64>,
    },
    /// 单行 superseded（rerun 旧批次）。
    SupersedeInjection { id: String },
    /// 折叠旧注入行（保留 payload，仅置 `state=folded` + `folded_at`）。
    FoldInjections { ids: Vec<String>, folded_at: i64 },
}

/// 执行子 Agent 写命令（单事务；由单写任务调用）。
pub(crate) fn apply_command(
    connection: &mut Connection,
    command: &SubagentCommand,
) -> Result<StoreOutcome, StoreError> {
    match command {
        SubagentCommand::InsertResult { result } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO subagent_results (child_session_id, result, total_bytes, truncated, \
                     verdict, summary, summary_truncated, findings_count, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                     ON CONFLICT(child_session_id) DO NOTHING",
                    params![
                        result.child_session_id,
                        result.result,
                        result.total_bytes,
                        i64::from(result.truncated),
                        result.verdict,
                        result.summary,
                        i64::from(result.summary_truncated),
                        result.findings_count,
                        result.created_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::SubagentResultRecorded {
                inserted: affected > 0,
            })
        }
        SubagentCommand::InsertInjection { injection } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO subagent_injections (id, parent_session_id, batch_id, child_session_ids, \
                     payload, result_refs, state, mode, supersedes_id, requires_approval, \
                     context_version, created_at, injected_at, folded_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
                     ON CONFLICT(parent_session_id, batch_id) DO NOTHING",
                    params![
                        injection.id,
                        injection.parent_session_id,
                        injection.batch_id,
                        injection.child_session_ids,
                        injection.payload,
                        injection.result_refs,
                        injection.state,
                        injection.mode,
                        injection.supersedes_id,
                        i64::from(injection.requires_approval),
                        injection.context_version,
                        injection.created_at,
                        injection.injected_at,
                        injection.folded_at,
                    ],
                )
                .map_err(txn_failed)?;
            let existing = transaction
                .query_row(
                    &format!(
                        "SELECT {INJECTION_COLUMNS} FROM subagent_injections \
                         WHERE parent_session_id = ?1 AND batch_id = ?2"
                    ),
                    params![injection.parent_session_id, injection.batch_id],
                    read_injection_row,
                )
                .optional()
                .map_err(txn_failed)?
                .ok_or_else(|| StoreError::WriteTransactionFailed {
                    code: None,
                    message: "subagent_injections 回读失败（不变量破坏）".to_owned(),
                })?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::SubagentInjectionRecorded {
                injection: existing,
                inserted: affected > 0,
            })
        }
        SubagentCommand::UpdateInjectionState {
            id,
            state,
            injected_at,
            folded_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE subagent_injections SET state = ?2, injected_at = COALESCE(?3, injected_at), \
                     folded_at = COALESCE(?4, folded_at) WHERE id = ?1",
                    params![id, state, injected_at, folded_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        SubagentCommand::SupersedeInjection { id } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE subagent_injections SET state = 'superseded' WHERE id = ?1",
                    params![id],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        SubagentCommand::FoldInjections { ids, folded_at } => {
            if ids.is_empty() {
                return Ok(StoreOutcome::Applied { affected: 0 });
            }
            let transaction = connection.transaction().map_err(txn_failed)?;
            let mut affected = 0usize;
            for id in ids {
                affected += transaction
                    .execute(
                        "UPDATE subagent_injections SET state = 'folded', folded_at = ?2 \
                         WHERE id = ?1 AND state IN ('injected','pending')",
                        params![id, folded_at],
                    )
                    .map_err(txn_failed)?;
            }
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
    }
}

const INJECTION_COLUMNS: &str = "id, parent_session_id, batch_id, child_session_ids, payload, \
                                 result_refs, state, mode, supersedes_id, requires_approval, \
                                 context_version, created_at, injected_at, folded_at";

fn read_injection_row(row: &Row<'_>) -> rusqlite::Result<SubagentInjectionRecord> {
    Ok(SubagentInjectionRecord {
        id: row.get(0)?,
        parent_session_id: row.get(1)?,
        batch_id: row.get(2)?,
        child_session_ids: row.get(3)?,
        payload: row.get(4)?,
        result_refs: row.get(5)?,
        state: row.get(6)?,
        mode: row.get(7)?,
        supersedes_id: row.get(8)?,
        requires_approval: row.get::<_, i64>(9)? != 0,
        context_version: row.get(10)?,
        created_at: row.get(11)?,
        injected_at: row.get(12)?,
        folded_at: row.get(13)?,
    })
}

fn read_result_row(row: &Row<'_>) -> rusqlite::Result<SubagentResultRecord> {
    Ok(SubagentResultRecord {
        child_session_id: row.get(0)?,
        result: row.get(1)?,
        total_bytes: row.get(2)?,
        truncated: row.get::<_, i64>(3)? != 0,
        verdict: row.get(4)?,
        summary: row.get(5)?,
        summary_truncated: row.get::<_, i64>(6)? != 0,
        findings_count: row.get(7)?,
        created_at: row.get(8)?,
    })
}

const RESULT_COLUMNS: &str =
    "child_session_id, result, total_bytes, truncated, verdict, summary, summary_truncated, \
     findings_count, created_at";

impl ReadPool {
    /// 读取子 Agent 完整结果（父授权由命令层校验）。
    pub async fn subagent_result(
        &self,
        child_session_id: &str,
    ) -> Result<Option<SubagentResultRecord>, StoreError> {
        let child = child_session_id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    &format!(
                        "SELECT {RESULT_COLUMNS} FROM subagent_results WHERE child_session_id = ?1"
                    ),
                    [child.as_str()],
                    read_result_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 按 id 读取注入行。
    pub async fn subagent_injection(
        &self,
        id: &str,
    ) -> Result<Option<SubagentInjectionRecord>, StoreError> {
        let id = id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    &format!("SELECT {INJECTION_COLUMNS} FROM subagent_injections WHERE id = ?1"),
                    [id.as_str()],
                    read_injection_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 按 `(parent,batch)` 读取注入行（唯一）。
    pub async fn subagent_injection_for_batch(
        &self,
        parent_session_id: &str,
        batch_id: &str,
    ) -> Result<Option<SubagentInjectionRecord>, StoreError> {
        let parent = parent_session_id.to_owned();
        let batch = batch_id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    &format!(
                        "SELECT {INJECTION_COLUMNS} FROM subagent_injections \
                         WHERE parent_session_id = ?1 AND batch_id = ?2"
                    ),
                    params![parent, batch],
                    read_injection_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 父会话全部注入行（按创建时间升序；组装层折叠/快照用）。
    pub async fn subagent_injections(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SubagentInjectionRecord>, StoreError> {
        let parent = parent_session_id.to_owned();
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {INJECTION_COLUMNS} FROM subagent_injections \
                 WHERE parent_session_id = ?1 ORDER BY created_at ASC, id ASC"
            ))?;
            let rows = statement.query_map([parent.as_str()], read_injection_row)?;
            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }
            Ok(items)
        })
        .await
    }

    /// 父会话当前上下文版本（`MAX(context_version)`；无行 = 0）。
    pub async fn subagent_context_version(
        &self,
        parent_session_id: &str,
    ) -> Result<i64, StoreError> {
        let parent = parent_session_id.to_owned();
        self.with_connection(move |connection| {
            let version: Option<i64> = connection.query_row(
                "SELECT MAX(context_version) FROM subagent_injections WHERE parent_session_id = ?1",
                [parent.as_str()],
                |row| row.get(0),
            )?;
            Ok(version.unwrap_or(0))
        })
        .await
    }

    /// 子会话总数（含已完成/失败；父会话删除前置检查）。
    pub async fn child_session_count(&self, parent_session_id: &str) -> Result<u64, StoreError> {
        let parent = parent_session_id.to_owned();
        self.with_connection(move |connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sessions WHERE parent_session_id = ?1",
                [parent.as_str()],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(u64::MAX))
        })
        .await
    }

    /// 单父会话**活动**子 Agent 数（`creating|idle|running|waiting_permission`）。
    pub async fn active_child_session_count(
        &self,
        parent_session_id: &str,
    ) -> Result<u64, StoreError> {
        let parent = parent_session_id.to_owned();
        self.with_connection(move |connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sessions WHERE parent_session_id = ?1 AND status IN \
                 ('creating','idle','running','waiting_permission')",
                [parent.as_str()],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(u64::MAX))
        })
        .await
    }

    /// 全局活动子 Agent 数（并发闸门：≤8）。
    pub async fn global_active_child_session_count(&self) -> Result<u64, StoreError> {
        self.with_connection(|connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sessions WHERE parent_session_id IS NOT NULL AND status IN \
                 ('creating','idle','running','waiting_permission')",
                [],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(u64::MAX))
        })
        .await
    }

    /// 全部子会话（daemon 重启 reconcile 用；按创建时间升序）。
    pub async fn all_child_sessions(&self) -> Result<Vec<aether_core::Session>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, runtime_id, workspace_id, parent_session_id, title, status, model, \
                 system_prompt, config, token_usage, created_at, updated_at, closed_at, \
                 thinking_depth, permission_mode, preset_id \
                 FROM sessions WHERE parent_session_id IS NOT NULL \
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = statement.query_map([], crate::ops::read_session_row)?;
            let mut sessions = Vec::new();
            for row in rows {
                sessions.push(row?);
            }
            Ok(sessions)
        })
        .await
    }

    /// 父会话 → 子会话数（`session_list.has_children`；单查询聚合，避免 N+1）。
    pub async fn child_session_counts(
        &self,
    ) -> Result<std::collections::BTreeMap<String, u64>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT parent_session_id, COUNT(*) FROM sessions \
                 WHERE parent_session_id IS NOT NULL GROUP BY parent_session_id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut counts = std::collections::BTreeMap::new();
            for row in rows {
                let (parent, count) = row?;
                counts.insert(parent, u64::try_from(count).unwrap_or(u64::MAX));
            }
            Ok(counts)
        })
        .await
    }
}
