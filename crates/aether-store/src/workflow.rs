//! 编排引擎持久化（M10-01/ADR-031）。
//!
//! 单写者约束（AGENTS §2.4）：全部写路径经 [`crate::WriteQueue::execute`] 进入
//! [`crate::StoreCommand::Workflow`]；本模块只提供事务实现与只读查询。
//!
//! 关键不变量：
//! - `(workflow_run_id, node_id, attempt)` 唯一（唯一索引兜底）；重试必须新建 attempt；
//! - 运行/节点终态不可变（应用层状态机负责；本层按 id 更新时不改写历史 attempt）；
//! - daemon 重启 reconcile：`running` 运行 → `interrupted`，`running/queued` 节点与
//!   任务 → `cancelled`，不自动重放（避免重复副作用）。

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::StoreError;
use crate::ops::{txn_failed, StoreOutcome};
use crate::write_queue::ReadPool;

/// `workflows` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRecord {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub version: i64,
    pub definition: String,
    pub is_template: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

/// `workflow_runs` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunRecord {
    pub id: String,
    pub workflow_id: String,
    pub status: String,
    pub context: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub session_id: Option<String>,
    pub definition_version: i64,
    pub definition_hash: Option<String>,
    pub error: Option<String>,
    pub cancel_requested_at: Option<i64>,
}

/// `node_runs` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunRecord {
    pub id: String,
    pub workflow_run_id: String,
    pub node_id: String,
    pub session_id: Option<String>,
    pub status: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub attempt: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// `tasks` 行（编排任务台账；与 node_runs 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub id: String,
    pub session_id: Option<String>,
    pub workflow_run_id: Option<String>,
    pub node_id: Option<String>,
    pub task_type: String,
    pub status: String,
    pub priority: i64,
    pub dependencies: String,
    pub input: Option<String>,
    pub result: Option<String>,
    pub retry_count: i64,
    pub timeout_ms: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 编排写命令（嵌套于 [`crate::StoreCommand::Workflow`]）。
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowCommand {
    InsertWorkflow {
        workflow: WorkflowRecord,
    },
    /// 乐观并发：`expected_version` 不匹配时返回
    /// [`StoreOutcome::WorkflowVersionConflict`]（不覆盖）。
    UpdateWorkflow {
        workflow: WorkflowRecord,
        expected_version: i64,
    },
    /// 软删除；`force=false` 时有活动运行（`queued/running`）返回
    /// [`StoreOutcome::WorkflowRunActive`]（不修改任何行）。
    DeleteWorkflow {
        id: String,
        deleted_at: i64,
        force: bool,
    },
    InsertWorkflowRun {
        run: WorkflowRunRecord,
    },
    UpdateWorkflowRun {
        id: String,
        status: String,
        error: Option<String>,
        finished_at: Option<i64>,
        cancel_requested_at: Option<i64>,
    },
    /// 持久化运行上下文（`workflow_runs.context`；input/values/priority）。
    UpdateWorkflowRunContext {
        id: String,
        context: String,
    },
    /// 级联收口运行下的非终态节点与任务（取消/清理超时用）。
    CancelRunNodes {
        workflow_run_id: String,
        at: i64,
        reason: String,
    },
    /// 插入节点运行；`(workflow_run_id,node_id,attempt)` 冲突时返回既有行
    /// （幂等，不重复执行）。
    InsertNodeRun {
        node: NodeRunRecord,
    },
    UpdateNodeRun {
        id: String,
        status: String,
        session_id: Option<String>,
        output: Option<String>,
        error: Option<String>,
        started_at: Option<i64>,
        finished_at: Option<i64>,
    },
    InsertTask {
        task: TaskRecord,
    },
    UpdateTask {
        id: String,
        status: String,
        result: Option<String>,
        updated_at: i64,
    },
    /// daemon 重启 reconcile（不自动重放）。
    ReconcileRunning {
        reconciled_at: i64,
    },
}

/// 执行编排写命令（单事务；由单写任务调用）。
pub(crate) fn apply_command(
    connection: &mut Connection,
    command: &WorkflowCommand,
) -> Result<StoreOutcome, StoreError> {
    match command {
        WorkflowCommand::InsertWorkflow { workflow } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            transaction
                .execute(
                    "INSERT INTO workflows (id, name, description, version, definition, is_template, \
                     created_at, updated_at, deleted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        workflow.id,
                        workflow.name,
                        workflow.description,
                        workflow.version,
                        workflow.definition,
                        i64::from(workflow.is_template),
                        workflow.created_at,
                        workflow.updated_at,
                        workflow.deleted_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected: 1 })
        }
        WorkflowCommand::UpdateWorkflow {
            workflow,
            expected_version,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE workflows SET name = ?1, description = ?2, version = ?3, definition = ?4, \
                     is_template = ?5, updated_at = ?6 \
                     WHERE id = ?7 AND version = ?8 AND deleted_at IS NULL",
                    params![
                        workflow.name,
                        workflow.description,
                        workflow.version,
                        workflow.definition,
                        i64::from(workflow.is_template),
                        workflow.updated_at,
                        workflow.id,
                        expected_version,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            if affected == 0 {
                Ok(StoreOutcome::WorkflowVersionConflict)
            } else {
                Ok(StoreOutcome::Applied { affected })
            }
        }
        WorkflowCommand::DeleteWorkflow {
            id,
            deleted_at,
            force,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let active: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM workflow_runs \
                     WHERE workflow_id = ?1 AND status IN ('queued','running')",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            if active > 0 && !force {
                transaction.commit().map_err(txn_failed)?;
                return Ok(StoreOutcome::WorkflowRunActive {
                    runs: u64::try_from(active).unwrap_or(u64::MAX),
                });
            }
            let mut affected = 0;
            if active > 0 {
                affected += transaction
                    .execute(
                        "UPDATE workflow_runs SET status = 'cancelled', finished_at = ?1, \
                         cancel_requested_at = ?1, error = 'deleted' \
                         WHERE workflow_id = ?2 AND status IN ('queued','running')",
                        params![deleted_at, id.as_str()],
                    )
                    .map_err(txn_failed)?;
                affected += transaction
                    .execute(
                        "UPDATE node_runs SET status = 'cancelled', finished_at = ?1, error = 'deleted' \
                         WHERE workflow_run_id IN (SELECT id FROM workflow_runs WHERE workflow_id = ?2) \
                           AND status IN ('pending','queued','running')",
                        params![deleted_at, id.as_str()],
                    )
                    .map_err(txn_failed)?;
            }
            affected += transaction
                .execute(
                    "UPDATE workflows SET deleted_at = ?1, updated_at = ?1 \
                     WHERE id = ?2 AND deleted_at IS NULL",
                    params![deleted_at, id.as_str()],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        WorkflowCommand::InsertWorkflowRun { run } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            transaction
                .execute(
                    "INSERT INTO workflow_runs (id, workflow_id, status, context, started_at, finished_at, \
                     session_id, definition_version, definition_hash, error, cancel_requested_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        run.id,
                        run.workflow_id,
                        run.status,
                        run.context,
                        run.started_at,
                        run.finished_at,
                        run.session_id,
                        run.definition_version,
                        run.definition_hash,
                        run.error,
                        run.cancel_requested_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected: 1 })
        }
        WorkflowCommand::UpdateWorkflowRun {
            id,
            status,
            error,
            finished_at,
            cancel_requested_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE workflow_runs SET status = ?1, error = COALESCE(?2, error), \
                     finished_at = COALESCE(?3, finished_at), \
                     cancel_requested_at = COALESCE(?4, cancel_requested_at) WHERE id = ?5",
                    params![status, error, finished_at, cancel_requested_at, id.as_str()],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        WorkflowCommand::UpdateWorkflowRunContext { id, context } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE workflow_runs SET context = ?1 WHERE id = ?2",
                    params![context, id.as_str()],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        WorkflowCommand::CancelRunNodes {
            workflow_run_id,
            at,
            reason,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE node_runs SET status = 'cancelled', finished_at = ?2, error = ?3 \
                     WHERE workflow_run_id = ?1 AND status IN ('pending','queued','running')",
                    params![workflow_run_id.as_str(), at, reason],
                )
                .map_err(txn_failed)?;
            transaction
                .execute(
                    "UPDATE tasks SET status = 'cancelled', updated_at = ?2 \
                     WHERE workflow_run_id = ?1 AND status IN ('queued','running')",
                    params![workflow_run_id.as_str(), at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        WorkflowCommand::InsertNodeRun { node } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let inserted = transaction
                .execute(
                    "INSERT INTO node_runs (id, workflow_run_id, node_id, session_id, status, input, output, \
                     error, attempt, started_at, finished_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                     ON CONFLICT(workflow_run_id, node_id, attempt) DO NOTHING",
                    params![
                        node.id,
                        node.workflow_run_id,
                        node.node_id,
                        node.session_id,
                        node.status,
                        node.input,
                        node.output,
                        node.error,
                        node.attempt,
                        node.started_at,
                        node.finished_at,
                    ],
                )
                .map_err(txn_failed)?;
            let existing: String = transaction
                .query_row(
                    "SELECT id FROM node_runs WHERE workflow_run_id = ?1 AND node_id = ?2 AND attempt = ?3",
                    params![node.workflow_run_id, node.node_id, node.attempt],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::WorkflowNodeRun {
                id: existing,
                inserted: inserted == 1,
            })
        }
        WorkflowCommand::UpdateNodeRun {
            id,
            status,
            session_id,
            output,
            error,
            started_at,
            finished_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE node_runs SET status = ?1, session_id = COALESCE(?2, session_id), \
                     output = COALESCE(?3, output), error = COALESCE(?4, error), \
                     started_at = COALESCE(?5, started_at), finished_at = COALESCE(?6, finished_at) \
                     WHERE id = ?7",
                    params![status, session_id, output, error, started_at, finished_at, id],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        WorkflowCommand::InsertTask { task } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            transaction
                .execute(
                    "INSERT INTO tasks (id, session_id, workflow_run_id, node_id, type, status, priority, \
                     dependencies, input, result, retry_count, timeout_ms, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        task.id,
                        task.session_id,
                        task.workflow_run_id,
                        task.node_id,
                        task.task_type,
                        task.status,
                        task.priority,
                        task.dependencies,
                        task.input,
                        task.result,
                        task.retry_count,
                        task.timeout_ms,
                        task.created_at,
                        task.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected: 1 })
        }
        WorkflowCommand::UpdateTask {
            id,
            status,
            result,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE tasks SET status = ?1, result = COALESCE(?2, result), updated_at = ?3 WHERE id = ?4",
                    params![status, result, updated_at, id],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        WorkflowCommand::ReconcileRunning { reconciled_at } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let runs = transaction
                .execute(
                    "UPDATE workflow_runs SET status = 'interrupted', finished_at = ?1, \
                     error = 'daemon_restart' WHERE status IN ('queued','running')",
                    [reconciled_at],
                )
                .map_err(txn_failed)?;
            let nodes = transaction
                .execute(
                    "UPDATE node_runs SET status = 'cancelled', finished_at = ?1, error = 'daemon_restart' \
                     WHERE status IN ('pending','queued','running')",
                    [reconciled_at],
                )
                .map_err(txn_failed)?;
            transaction
                .execute(
                    "UPDATE tasks SET status = 'cancelled', updated_at = ?1 \
                     WHERE status IN ('queued','running')",
                    [reconciled_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::WorkflowReconciled {
                runs: u64::try_from(runs).unwrap_or(u64::MAX),
                nodes: u64::try_from(nodes).unwrap_or(u64::MAX),
            })
        }
    }
}

fn read_workflow_row(row: &Row<'_>) -> rusqlite::Result<WorkflowRecord> {
    Ok(WorkflowRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        version: row.get(3)?,
        definition: row.get(4)?,
        is_template: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        deleted_at: row.get(8)?,
    })
}

fn read_run_row(row: &Row<'_>) -> rusqlite::Result<WorkflowRunRecord> {
    Ok(WorkflowRunRecord {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        status: row.get(2)?,
        context: row.get(3)?,
        started_at: row.get(4)?,
        finished_at: row.get(5)?,
        session_id: row.get(6)?,
        definition_version: row.get(7)?,
        definition_hash: row.get(8)?,
        error: row.get(9)?,
        cancel_requested_at: row.get(10)?,
    })
}

fn read_node_row(row: &Row<'_>) -> rusqlite::Result<NodeRunRecord> {
    Ok(NodeRunRecord {
        id: row.get(0)?,
        workflow_run_id: row.get(1)?,
        node_id: row.get(2)?,
        session_id: row.get(3)?,
        status: row.get(4)?,
        input: row.get(5)?,
        output: row.get(6)?,
        error: row.get(7)?,
        attempt: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

const WORKFLOW_COLUMNS: &str =
    "id, name, description, version, definition, is_template, created_at, updated_at, deleted_at";
const RUN_COLUMNS: &str = "id, workflow_id, status, context, started_at, finished_at, session_id, \
                           definition_version, definition_hash, error, cancel_requested_at";
const NODE_COLUMNS: &str =
    "id, workflow_run_id, node_id, session_id, status, input, output, error, \
                            attempt, started_at, finished_at";

impl ReadPool {
    /// 按 id 读取工作流（默认排除软删除；`include_deleted=true` 用于运行历史）。
    pub async fn workflow(
        &self,
        workflow_id: &str,
        include_deleted: bool,
    ) -> Result<Option<WorkflowRecord>, StoreError> {
        let id = workflow_id.to_owned();
        self.with_connection(move |connection| {
            let sql = if include_deleted {
                format!("SELECT {WORKFLOW_COLUMNS} FROM workflows WHERE id = ?1")
            } else {
                format!(
                    "SELECT {WORKFLOW_COLUMNS} FROM workflows WHERE id = ?1 AND deleted_at IS NULL"
                )
            };
            connection
                .query_row(&sql, [id.as_str()], read_workflow_row)
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 工作流列表（默认排除模板与软删除）。
    pub async fn workflows(
        &self,
        include_templates: bool,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<WorkflowRecord>, StoreError> {
        let limit = i64::from(limit);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {WORKFLOW_COLUMNS} FROM workflows \
                 WHERE deleted_at IS NULL AND (?1 = 1 OR is_template = 0) \
                 ORDER BY updated_at DESC, id DESC LIMIT ?2 OFFSET ?3"
            ))?;
            let rows = statement.query_map(
                params![i64::from(include_templates), limit, offset],
                read_workflow_row,
            )?;
            let mut workflows = Vec::new();
            for row in rows {
                workflows.push(row?);
            }
            Ok(workflows)
        })
        .await
    }

    /// 工作流总数（与 [`Self::workflows`] 同过滤条件）。
    pub async fn workflow_count(&self, include_templates: bool) -> Result<u64, StoreError> {
        self.with_connection(move |connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM workflows \
                 WHERE deleted_at IS NULL AND (?1 = 1 OR is_template = 0)",
                [i64::from(include_templates)],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(u64::MAX))
        })
        .await
    }

    /// 按 id 读取运行。
    pub async fn workflow_run(
        &self,
        workflow_run_id: &str,
    ) -> Result<Option<WorkflowRunRecord>, StoreError> {
        let id = workflow_run_id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    &format!("SELECT {RUN_COLUMNS} FROM workflow_runs WHERE id = ?1"),
                    [id.as_str()],
                    read_run_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 运行列表（可选过滤；`statuses` 为空 = 不过滤；按开始时间倒序）。
    pub async fn workflow_runs(
        &self,
        workflow_id: Option<&str>,
        session_id: Option<&str>,
        statuses: &[String],
        limit: u32,
        offset: u64,
    ) -> Result<Vec<WorkflowRunRecord>, StoreError> {
        let workflow_id = workflow_id.map(str::to_owned);
        let session_id = session_id.map(str::to_owned);
        let statuses = statuses.to_vec();
        let limit = i64::from(limit);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        self.with_connection(move |connection| {
            let status_clause = if statuses.is_empty() {
                "1 = 1".to_owned()
            } else {
                let placeholders = (0..statuses.len())
                    .map(|index| format!("?{}", index + 5))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("status IN ({placeholders})")
            };
            let sql = format!(
                "SELECT {RUN_COLUMNS} FROM workflow_runs \
                 WHERE (?1 IS NULL OR workflow_id = ?1) \
                   AND (?2 IS NULL OR session_id = ?2) \
                   AND ({status_clause}) \
                 ORDER BY started_at DESC, id DESC LIMIT ?3 OFFSET ?4"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(workflow_id.clone()),
                Box::new(session_id.clone()),
                Box::new(limit),
                Box::new(offset),
            ];
            for status in &statuses {
                values.push(Box::new(status.clone()));
            }
            let bound: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|value| value.as_ref()).collect();
            let rows = statement.query_map(bound.as_slice(), read_run_row)?;
            let mut runs = Vec::new();
            for row in rows {
                runs.push(row?);
            }
            Ok(runs)
        })
        .await
    }

    /// 运行总数（与 [`Self::workflow_runs`] 同过滤条件）。
    pub async fn workflow_run_count(
        &self,
        workflow_id: Option<&str>,
        session_id: Option<&str>,
        statuses: &[String],
    ) -> Result<u64, StoreError> {
        let workflow_id = workflow_id.map(str::to_owned);
        let session_id = session_id.map(str::to_owned);
        let statuses = statuses.to_vec();
        self.with_connection(move |connection| {
            let status_clause = if statuses.is_empty() {
                "1 = 1".to_owned()
            } else {
                let placeholders = (0..statuses.len())
                    .map(|index| format!("?{}", index + 3))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("status IN ({placeholders})")
            };
            let sql = format!(
                "SELECT COUNT(*) FROM workflow_runs \
                 WHERE (?1 IS NULL OR workflow_id = ?1) AND (?2 IS NULL OR session_id = ?2) \
                   AND ({status_clause})"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            values.push(Box::new(workflow_id));
            values.push(Box::new(session_id));
            for status in &statuses {
                values.push(Box::new(status.clone()));
            }
            let bound: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|value| value.as_ref()).collect();
            let count: i64 = statement.query_row(bound.as_slice(), |row| row.get(0))?;
            Ok(u64::try_from(count).unwrap_or(u64::MAX))
        })
        .await
    }

    /// 节点运行列表（按 node_id、attempt 排序；运行视图）。
    pub async fn node_runs(&self, workflow_run_id: &str) -> Result<Vec<NodeRunRecord>, StoreError> {
        let id = workflow_run_id.to_owned();
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {NODE_COLUMNS} FROM node_runs WHERE workflow_run_id = ?1 \
                 ORDER BY node_id ASC, attempt ASC"
            ))?;
            let rows = statement.query_map([id.as_str()], read_node_row)?;
            let mut nodes = Vec::new();
            for row in rows {
                nodes.push(row?);
            }
            Ok(nodes)
        })
        .await
    }

    /// 按 id 读取节点运行。
    pub async fn node_run(&self, node_run_id: &str) -> Result<Option<NodeRunRecord>, StoreError> {
        let id = node_run_id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    &format!("SELECT {NODE_COLUMNS} FROM node_runs WHERE id = ?1"),
                    [id.as_str()],
                    read_node_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 未收口运行（daemon 启动 reconcile 前的诊断读取）。
    pub async fn active_workflow_runs(&self) -> Result<Vec<WorkflowRunRecord>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM workflow_runs WHERE status IN ('queued','running') \
                 ORDER BY started_at ASC, id ASC"
            ))?;
            let rows = statement.query_map([], read_run_row)?;
            let mut runs = Vec::new();
            for row in rows {
                runs.push(row?);
            }
            Ok(runs)
        })
        .await
    }
}
