//! M10-02/ADR-032：子 Agent 派生、隔离、结果交付与父处置命令面。
//!
//! 装饰器位于会话后端外层：命令严格解析在 `ipc::handlers` 完成；本层负责上限/
//! 权限校验、子会话创建、结果落库、compact envelope 交付与审计。
//!
//! 关键不变量：
//! - 子会话只接收显式 context_refs/purpose；工作区继承父，权限只降不升；
//! - 完整结果只进 `subagent_results`；父上下文只接收 compact envelope；
//! - 同一 `(parent,batch)` 只一条 injection；终态不可重复注入；
//! - `message_append` 不改父 system prompt；`origin_fork` 使用冻结 origin + 独立
//!   ephemeral 上下文，不读取 live history。

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aether_control::{Clock, EventPipeline};
use aether_core::{
    session_status_is_terminal, truncate_utf8, EventType, Message, MessageId, MessageRole,
    PermissionMode, Session, SessionId, SubagentCompletedPayload, SubagentDeliveryMode,
    SubagentSpawnedPayload, SUBAGENT_SUMMARY_MAX_BYTES, SUBAGENT_TIMEOUT_DEFAULT_MS,
};
use aether_store::{
    AuditLogRecord, ReadPool, SessionQuery, StoreCommand, StoreOutcome, SubagentCommand,
    SubagentInjectionRecord, SubagentResultRecord, WriteQueue,
};
use serde_json::{json, Value};
use tokio::runtime::Handle;

use super::envelope::{build_envelope, fold_ids, wrap_block, ResultInput};
use super::result::{parse_child_result, ParsedResult};
use crate::ipc::dto::*;
use crate::ipc::path::ArtifactPath;
use crate::ipc::validate::CommandRequest;
use crate::ipc::{BusinessCode, IpcBackend, IpcError};

const SUBAGENT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

macro_rules! forward_request {
    ($($name:ident : $ty:ty),* $(,)?) => {
        $(fn $name(&self, request: &$ty) -> Result<Value, IpcError> { self.inner.$name(request) })*
    };
}

macro_rules! forward_none {
    ($($name:ident),* $(,)?) => {
        $(fn $name(&self) -> Result<Value, IpcError> { self.inner.$name() })*
    };
}

/// 子 Agent 命令后端（装饰器；最外层）。
#[derive(Clone)]
pub struct SubagentControlBackend {
    inner: Arc<dyn IpcBackend>,
    reads: ReadPool,
    write: WriteQueue,
    pipeline: Option<EventPipeline>,
    clock: Arc<dyn Clock>,
    handle: Handle,
    /// 进程内 origin fork（immutable origin snapshot；重启不可恢复 → 观测性拒绝）。
    origin_forks: Arc<std::sync::Mutex<HashMap<String, Value>>>,
}

/// 子 Agent 元数据（`sessions.config.aether.subagent`）。
#[derive(Debug, Clone, Default)]
struct SubagentMeta {
    role: String,
    mode: String,
    template: String,
    depth: u32,
    purpose: String,
    runtime_id: String,
    provider_id: Option<String>,
    model: Option<String>,
    permission_mode: String,
    timeout_ms: i64,
    batch_id: String,
    verdict: Option<String>,
    resolution: String,
    result_ref: Option<String>,
    callback: bool,
}

impl SubagentControlBackend {
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        reads: ReadPool,
        write: WriteQueue,
        pipeline: Option<EventPipeline>,
        clock: Arc<dyn Clock>,
        handle: Handle,
    ) -> Self {
        Self {
            inner,
            reads,
            write,
            pipeline,
            clock,
            handle,
            origin_forks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    fn call<T, F>(&self, future: F) -> Result<T, IpcError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, IpcError>> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver
            .recv_timeout(SUBAGENT_COMMAND_TIMEOUT)
            .map_err(|_| IpcError::internal("子 Agent 命令超时（核心未在预算内返回）"))?
    }

    async fn inner_create(&self, request: SessionCreateRequest) -> Result<Value, IpcError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || inner.session_create(&request))
            .await
            .map_err(|error| IpcError::internal(format!("会话创建任务失败：{error}")))?
    }

    async fn inner_send(&self, request: SessionSendRequest) -> Result<Value, IpcError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || inner.session_send(&request))
            .await
            .map_err(|error| IpcError::internal(format!("会话发送任务失败：{error}")))?
    }

    async fn inner_interrupt(&self, session_id: String) {
        let inner = Arc::clone(&self.inner);
        let _ = tokio::task::spawn_blocking(move || {
            inner.session_interrupt(&SessionIdRequest { session_id })
        })
        .await;
    }

    async fn inner_delete(&self, session_id: String) {
        let inner = Arc::clone(&self.inner);
        let _ = tokio::task::spawn_blocking(move || {
            inner.session_delete(&SessionDeleteRequest { session_id })
        })
        .await;
    }

    /// 适配器→核心入口（`subagent.spawn` 等；actor=agent）。
    pub fn spawn_from_adapter(&self, request: &SubagentSpawnRequest) -> Result<Value, IpcError> {
        request.validate()?;
        let request = request.clone();
        let this = self.clone();
        self.call(async move { this.spawn_inner(request, "agent").await })
    }

    /// `subagent.status`：按 child 读取状态/结果摘要。
    pub fn status_of(&self, child_id: &SessionId) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let child_id = child_id.clone();
        self.call(async move {
            let session = reads
                .session(&child_id)
                .await
                .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?
                .ok_or_else(|| subagent_not_found(child_id.as_str()))?;
            if session.parent_session_id.is_none() {
                return Err(subagent_not_found(child_id.as_str()));
            }
            let meta = meta_of(&session);
            let result = reads
                .subagent_result(child_id.as_str())
                .await
                .map_err(|error| IpcError::internal(format!("结果读取失败：{error}")))?;
            Ok(json!({
                "child_session_id": child_id.as_str(),
                "parent_session_id": session.parent_session_id,
                "status": session.status.as_str(),
                "role": meta.role,
                "mode": meta.mode,
                "depth": meta.depth,
                "verdict": result.as_ref().and_then(|result| result.verdict.clone()),
                "result_summary": result.as_ref().and_then(|result| result.summary.clone()),
                "result_ref": meta.result_ref,
                "has_more_details": result.is_some(),
            }))
        })
    }

    /// daemon 启动 reconcile：补齐终态子会话缺失的结果（不重放运行、不伪造事件）。
    pub async fn reconcile(&self) -> Result<(u64, u64), IpcError> {
        let children = self
            .reads
            .all_child_sessions()
            .await
            .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?;
        let mut scanned = 0u64;
        let mut repaired = 0u64;
        for child in children {
            if meta_of(&child).callback {
                continue;
            }
            scanned = scanned.saturating_add(1);
            if self
                .reads
                .subagent_result(child.id.as_str())
                .await
                .map_err(|error| IpcError::internal(format!("结果读取失败：{error}")))?
                .is_some()
            {
                continue;
            }
            if !session_status_is_terminal(child.status) {
                continue;
            }
            let messages = self
                .reads
                .messages_latest(&child.id, 10)
                .await
                .map_err(|error| IpcError::internal(format!("消息读取失败：{error}")))?;
            let content = messages
                .iter()
                .rev()
                .find(|message| matches!(message.role, MessageRole::Assistant))
                .map(|message| message.content.clone());
            let Some(content) = content else {
                continue;
            };
            let parsed = parse_child_result(&content);
            self.persist_result(&child, &parsed).await?;
            repaired = repaired.saturating_add(1);
        }
        Ok((scanned, repaired))
    }

    async fn spawn_inner(
        &self,
        request: SubagentSpawnRequest,
        actor: &'static str,
    ) -> Result<Value, IpcError> {
        let parent_id = SessionId::new(request.parent_session_id.clone())
            .map_err(|_| IpcError::invalid_format("parent_session_id", "必须是 26 位 ULID"))?;
        let parent = self
            .reads
            .session(&parent_id)
            .await
            .map_err(|error| IpcError::internal(format!("父会话读取失败：{error}")))?
            .ok_or_else(|| IpcError::session_not_found(parent_id.as_str()))?;
        let parent_meta = meta_of(&parent);
        let depth = if parent_meta.depth > 0 {
            parent_meta.depth.saturating_add(1)
        } else if parent.parent_session_id.is_some() {
            2
        } else {
            1
        };
        if depth > aether_core::SUBAGENT_DEPTH_MAX {
            return Err(limit_exceeded(
                "depth",
                format!(
                    "派生深度超过上限 {}（父 depth={}）",
                    aether_core::SUBAGENT_DEPTH_MAX,
                    depth.saturating_sub(1)
                ),
            ));
        }
        let fanout = self
            .reads
            .active_child_session_count(parent_id.as_str())
            .await
            .map_err(|error| IpcError::internal(format!("子 Agent 计数失败：{error}")))?;
        if fanout >= aether_core::SUBAGENT_FANOUT_MAX {
            return Err(limit_exceeded(
                "fanout",
                format!(
                    "单父会话活动子 Agent 上限 {}（当前 {fanout}）",
                    aether_core::SUBAGENT_FANOUT_MAX
                ),
            ));
        }
        let global = self
            .reads
            .global_active_child_session_count()
            .await
            .map_err(|error| IpcError::internal(format!("全局子 Agent 计数失败：{error}")))?;
        if global >= aether_core::SUBAGENT_CONCURRENCY_MAX {
            return Err(limit_exceeded(
                "concurrency",
                format!(
                    "全局并发子 Agent 上限 {}（当前 {global}）",
                    aether_core::SUBAGENT_CONCURRENCY_MAX
                ),
            ));
        }
        let spawn_count = self
            .reads
            .child_session_count(parent_id.as_str())
            .await
            .map_err(|error| IpcError::internal(format!("累计派生计数失败：{error}")))?;
        if spawn_count >= aether_core::SUBAGENT_SPAWN_COUNT_MAX {
            return Err(limit_exceeded(
                "spawn_count",
                format!(
                    "单父会话累计派生上限 {}（当前 {spawn_count}）",
                    aether_core::SUBAGENT_SPAWN_COUNT_MAX
                ),
            ));
        }

        // 权限只降不升。
        let requested_mode = request.permission_mode_value()?;
        let child_mode = requested_mode.unwrap_or(parent.permission_mode);
        if permission_rank(child_mode) > permission_rank(parent.permission_mode) {
            return Err(context_rejected(format!(
                "禁止升权：父档位 {}，请求 {}",
                parent.permission_mode.as_str(),
                child_mode.as_str()
            )));
        }

        let runtime_id = request
            .runtime_id
            .clone()
            .unwrap_or_else(|| parent.runtime_id.as_str().to_owned());
        self.ensure_subagent_capability(&runtime_id).await?;

        // provider/model：缺省继承父；父 provider 失效时不得为子选新 provider。
        let parent_provider = parent
            .config
            .get("aether")
            .and_then(|value| value.get("provider_id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let providers = self
            .reads
            .providers()
            .await
            .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?;
        let models = self
            .reads
            .provider_models()
            .await
            .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?;
        if let Some(provider_id) = &parent_provider {
            let invalid = providers
                .iter()
                .find(|provider| &provider.id == provider_id)
                .is_none_or(|provider| !provider.enabled);
            if invalid {
                return Err(context_rejected(format!(
                    "父会话 provider 已失效（{provider_id}）；子会话只读，不得选择新 provider"
                )));
            }
        }
        if let Some(provider_id) = &request.provider_id {
            if providers
                .iter()
                .find(|provider| &provider.id == provider_id)
                .is_none_or(|provider| !provider.enabled)
            {
                return Err(IpcError::provider_not_found(provider_id));
            }
        }
        let provider_id = request.provider_id.clone().or(parent_provider);
        let model = request.model.clone().or_else(|| parent.model.clone());
        if let (Some(provider_id), Some(model)) = (&provider_id, &model) {
            let exists = models.iter().any(|candidate| {
                candidate.provider_id == *provider_id
                    && candidate.model_id == *model
                    && candidate.enabled
            });
            if !exists {
                return Err(IpcError::invalid_value(format!(
                    "模型 {model} 不属于启用供应商 {provider_id}"
                )));
            }
        }

        let mode = request.mode_value().to_owned();
        let template = request
            .template
            .clone()
            .unwrap_or_else(|| match mode.as_str() {
                "explore" => "parallel_explore".to_owned(),
                "review" => "adversarial_review".to_owned(),
                _ => "custom".to_owned(),
            });
        let title = subagent_title(&request.role, &request.purpose);
        let batch_id = ulid::Ulid::new().to_string();
        let timeout_ms = request
            .timeout_ms
            .unwrap_or(aether_core::SUBAGENT_TIMEOUT_DEFAULT_MS);
        let references = request
            .context_refs
            .iter()
            .filter(|reference| matches!(reference.kind.as_str(), "file" | "directory"))
            .filter_map(|reference| {
                reference.path.as_ref().map(|path| ReferenceEntry {
                    path: path.clone(),
                    kind: reference.kind.clone(),
                })
            })
            .collect::<Vec<_>>();
        let context_texts = request
            .context_refs
            .iter()
            .filter(|reference| reference.kind == "text")
            .filter_map(|reference| reference.text.clone())
            .collect::<Vec<_>>();

        let create = SessionCreateRequest {
            runtime_id: Some(runtime_id.clone()),
            title: title.clone(),
            workspace_id: parent
                .workspace_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            model: model.clone(),
            provider_id: provider_id.clone(),
            thinking_depth: Some(i64::from(parent.thinking_depth)),
            permission_mode: Some(child_mode.as_str().to_owned()),
            preset_id: parent.preset_id.as_ref().map(|id| id.as_str().to_owned()),
        };
        let created = self.inner_create(create).await?;
        let child_id = created
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| IpcError::internal("会话创建响应缺少 id"))?
            .to_owned();
        let child_session_id =
            SessionId::new(child_id.clone()).map_err(|_| IpcError::internal("子会话 id 非法"))?;
        self.write
            .execute(StoreCommand::UpdateSessionParent {
                session_id: child_session_id.clone(),
                parent_session_id: parent_id.clone(),
                updated_at: self.clock.now_ms(),
            })
            .await
            .map_err(|error| IpcError::internal(format!("子会话父指针登记失败：{error}")))?;

        let child = self
            .reads
            .session(&child_session_id)
            .await
            .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?
            .ok_or_else(|| IpcError::internal("子会话创建后不可见"))?;
        let meta = SubagentMeta {
            role: request.role.clone(),
            mode: mode.clone(),
            template: template.clone(),
            depth,
            purpose: request.purpose.clone(),
            runtime_id: runtime_id.clone(),
            provider_id: provider_id.clone(),
            model: model.clone(),
            permission_mode: child_mode.as_str().to_owned(),
            timeout_ms,
            batch_id: batch_id.clone(),
            verdict: None,
            resolution: "spawned".to_owned(),
            result_ref: None,
            callback: false,
        };
        self.write_session_meta(&child, &meta).await?;

        // origin fork：在 delegation boundary 冻结父消息快照（进程内，不落库）。
        if let Ok(messages) = self.reads.messages_latest(&parent_id, 20).await {
            let snapshot: Vec<Value> = messages
                .iter()
                .map(|message| {
                    json!({
                        "role": message.role,
                        "content": truncate_utf8(&message.content, 4_096).0,
                        "created_at": message.created_at,
                    })
                })
                .collect();
            if let Ok(mut forks) = self.origin_forks.lock() {
                forks.insert(
                    child_id.clone(),
                    json!({"parent_session_id": parent_id, "messages": snapshot}),
                );
            }
        }

        let prompt = build_prompt(&template, &mode, &request.purpose, depth, &context_texts);
        let send = SessionSendRequest {
            session_id: child_id.clone(),
            text: prompt,
            client_msg_id: ulid::Ulid::new().to_string(),
            thinking_depth: None,
            references,
            provider_id: None,
            model: None,
            subagent_context: None,
        };
        let sent = match self.inner_send(send).await {
            Ok(value) => value,
            Err(error) => {
                let mut failed_meta = meta;
                failed_meta.resolution = "failed".to_owned();
                let _ = self.write_session_meta(&child, &failed_meta).await;
                return Err(IpcError::internal(format!(
                    "子 Agent 首轮发送失败（已保留子会话供诊断）：{error}"
                )));
            }
        };
        let run_id = sent
            .get("run_id")
            .and_then(Value::as_str)
            .ok_or_else(|| IpcError::internal("子会话发送响应缺少 run_id"))?
            .to_owned();

        self.emit(
            &parent_id,
            parent.runtime_id.as_str(),
            EventType::SubagentSpawned,
            serde_json::to_value(SubagentSpawnedPayload {
                child_session_id: child_session_id.clone(),
                parent_session_id: parent_id.clone(),
                role: request.role.clone(),
                mode: mode.clone(),
                depth,
                context_refs_count: u32::try_from(request.context_refs.len()).unwrap_or(u32::MAX),
            })
            .unwrap_or(Value::Null),
        )
        .await;
        self.audit(
            actor,
            "subagent.spawn",
            &parent_id,
            Some(&child_id),
            Some(json!({
                "role": request.role,
                "mode": mode,
                "template": template,
                "depth": depth,
                "verdict": Value::Null,
            })),
            "allowed",
        )
        .await;

        // 后台观察子 run 终态（超时/取消同样收口；不阻塞父命令）。
        let this = self.clone();
        let watch = WatchContext {
            parent_id: parent_id.clone(),
            child_id: child_id.clone(),
            run_id,
            runtime_id: parent.runtime_id.as_str().to_owned(),
            meta,
        };
        self.handle.spawn(async move {
            this.watch_child(watch).await;
        });

        Ok(json!({
            "child_session_id": child_id,
            "status": "running",
            "depth": depth,
            "mode": mode,
            "batch_id": batch_id,
        }))
    }

    async fn ensure_subagent_capability(&self, runtime_id: &str) -> Result<(), IpcError> {
        // 静态清单为三官方运行时的唯一判定源；其余运行时要求 hello/initialize
        // 声明 `subagent`（占位实现/测试可显式声明）。
        if aether_core::static_capabilities(runtime_id).contains(&"subagent") {
            return Ok(());
        }
        let inner = Arc::clone(&self.inner);
        let runtimes = tokio::task::spawn_blocking(move || inner.runtimes_list())
            .await
            .map_err(|error| IpcError::internal(format!("运行时列表任务失败：{error}")))??;
        let declared = runtimes
            .as_array()
            .and_then(|items| items.iter().find(|item| item["id"] == runtime_id))
            .and_then(|item| item["capabilities"].as_array())
            .is_some_and(|items| items.iter().any(|item| item == "subagent"));
        if declared {
            Ok(())
        } else {
            Err(context_rejected(format!(
                "运行时不支持子 Agent（{runtime_id}；未声明 subagent 能力）"
            )))
        }
    }

    async fn write_session_meta(
        &self,
        session: &Session,
        meta: &SubagentMeta,
    ) -> Result<(), IpcError> {
        let mut config = session.config.clone();
        let Some(object) = config.as_object_mut() else {
            return Err(IpcError::internal(
                "会话 config 不是 JSON 对象（不变量破坏）",
            ));
        };
        let aether = object
            .entry("aether".to_owned())
            .or_insert_with(|| json!({}));
        let Some(aether) = aether.as_object_mut() else {
            return Err(IpcError::internal(
                "会话 config.aether 不是 JSON 对象（不变量破坏）",
            ));
        };
        aether.insert(
            "subagent".to_owned(),
            json!({
                "role": meta.role,
                "mode": meta.mode,
                "template": meta.template,
                "depth": meta.depth,
                "purpose": meta.purpose,
                "runtime_id": meta.runtime_id,
                "provider_id": meta.provider_id,
                "model": meta.model,
                "permission_mode": meta.permission_mode,
                "timeout_ms": meta.timeout_ms,
                "batch_id": meta.batch_id,
                "verdict": meta.verdict,
                "resolution": meta.resolution,
                "result_ref": meta.result_ref,
                "callback": meta.callback,
            }),
        );
        self.write
            .execute(StoreCommand::UpdateSessionConfig {
                session_id: session.id.clone(),
                config,
                updated_at: self.clock.now_ms(),
            })
            .await
            .map_err(|error| IpcError::internal(format!("子会话元数据落库失败：{error}")))?;
        Ok(())
    }

    async fn persist_result(&self, child: &Session, parsed: &ParsedResult) -> Result<(), IpcError> {
        let result_ref = format!("subagent:{}", child.id.as_str());
        let record = SubagentResultRecord {
            child_session_id: child.id.as_str().to_owned(),
            result: parsed.canonical.clone(),
            total_bytes: i64::try_from(parsed.total_bytes).unwrap_or(i64::MAX),
            truncated: parsed.truncated,
            verdict: Some(parsed.result.verdict.as_str().to_owned()),
            summary: Some(parsed.summary_512.clone()),
            summary_truncated: parsed.summary_truncated,
            findings_count: i64::try_from(parsed.result.findings.len()).unwrap_or(i64::MAX),
            created_at: self.clock.now_ms(),
        };
        self.write
            .execute(StoreCommand::Subagent {
                command: SubagentCommand::InsertResult { result: record },
            })
            .await
            .map_err(|error| IpcError::internal(format!("子结果落库失败：{error}")))?;
        let mut meta = meta_of(child);
        meta.verdict = Some(parsed.result.verdict.as_str().to_owned());
        meta.resolution = "result_ready".to_owned();
        meta.result_ref = Some(result_ref);
        self.write_session_meta(child, &meta).await
    }

    async fn watch_child(&self, context: WatchContext) {
        let deadline = self
            .clock
            .now_ms()
            .saturating_add(context.meta.timeout_ms.max(1));
        let run_id = match aether_core::RunId::new(context.run_id.clone()) {
            Ok(run_id) => run_id,
            Err(_) => return,
        };
        let status = loop {
            if self.clock.now_ms() >= deadline {
                let _ = self.inner_interrupt(context.child_id.clone()).await;
                break "timeout".to_owned();
            }
            match self.reads.run(&run_id).await {
                Ok(Some(run)) => match run.status {
                    aether_core::RunStatus::Succeeded => break "succeeded".to_owned(),
                    aether_core::RunStatus::Failed => break "failed".to_owned(),
                    aether_core::RunStatus::Cancelled => break "cancelled".to_owned(),
                    aether_core::RunStatus::Timeout => break "timeout".to_owned(),
                    _ => {}
                },
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(child = %context.child_id, error = %error, "子 Agent run 读取失败");
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let child_session_id = match SessionId::new(context.child_id.clone()) {
            Ok(id) => id,
            Err(_) => return,
        };
        let child = match self.reads.session(&child_session_id).await {
            Ok(Some(child)) => child,
            _ => return,
        };

        let mut parsed = None;
        if status == "succeeded" {
            if let Ok(messages) = self.reads.messages_latest(&child_session_id, 10).await {
                if let Some(content) = messages
                    .iter()
                    .rev()
                    .find(|message| matches!(message.role, MessageRole::Assistant))
                    .map(|message| message.content.clone())
                {
                    let value = parse_child_result(&content);
                    if let Err(error) = self.persist_result(&child, &value).await {
                        tracing::warn!(child = %context.child_id, error = %error, "子结果持久化失败");
                    }
                    parsed = Some(value);
                }
            }
        }
        if parsed.is_none() {
            let mut meta = context.meta.clone();
            meta.resolution = status.clone();
            let _ = self.write_session_meta(&child, &meta).await;
        }

        let verdict = parsed
            .as_ref()
            .map(|value| value.result.verdict.as_str().to_owned());
        let result_ref = parsed
            .as_ref()
            .map(|_| format!("subagent:{}", context.child_id));
        let _ = self
            .emit(
                &context.parent_id,
                &context.runtime_id,
                EventType::SubagentCompleted,
                serde_json::to_value(SubagentCompletedPayload {
                    child_session_id: child_session_id.clone(),
                    parent_session_id: context.parent_id.clone(),
                    status: if status == "succeeded" {
                        "succeeded".to_owned()
                    } else {
                        status.clone()
                    },
                    verdict: verdict.clone(),
                    result_ref: result_ref.clone(),
                    batch_id: context.meta.batch_id.clone(),
                    injected: false,
                })
                .unwrap_or(Value::Null),
            )
            .await;

        let Some(parsed) = parsed else {
            return;
        };
        let input = ResultInput {
            child_session_id: context.child_id.clone(),
            verdict: parsed.result.verdict,
            summary: parsed.result.summary.clone(),
            findings: parsed.result.findings.clone(),
            result_ref: result_ref.unwrap_or_default(),
            failed: false,
        };
        let payload = build_envelope(&[input]);
        let auto = context.meta.template == "parallel_explore";
        let requires_approval = !auto;
        let version = match self
            .reads
            .subagent_context_version(context.parent_id.as_str())
            .await
        {
            Ok(version) => version.saturating_add(1),
            Err(_) => 1,
        };
        let injection = SubagentInjectionRecord {
            id: ulid::Ulid::new().to_string(),
            parent_session_id: context.parent_id.as_str().to_owned(),
            batch_id: context.meta.batch_id.clone(),
            child_session_ids: json!([context.child_id]).to_string(),
            payload: payload.to_string(),
            result_refs: json!([format!("subagent:{}", context.child_id)]).to_string(),
            state: "pending".to_owned(),
            mode: mode_for_parent(self, &context.parent_id, &context.meta).await,
            supersedes_id: None,
            requires_approval,
            context_version: version,
            created_at: self.clock.now_ms(),
            injected_at: None,
            folded_at: None,
        };
        let recorded = match self
            .write
            .execute(StoreCommand::Subagent {
                command: SubagentCommand::InsertInjection {
                    injection: injection.clone(),
                },
            })
            .await
        {
            Ok(StoreOutcome::SubagentInjectionRecorded { injection, .. }) => injection,
            _ => injection,
        };
        if auto {
            let _ = self.deliver(&recorded.id, "agent").await;
        }
    }

    async fn deliver(
        &self,
        injection_id: &str,
        actor: &str,
    ) -> Result<(bool, u64, String), IpcError> {
        let Some(injection) = self
            .reads
            .subagent_injection(injection_id)
            .await
            .map_err(|error| IpcError::internal(format!("注入行读取失败：{error}")))?
        else {
            return Err(subagent_not_found(injection_id));
        };
        if matches!(
            injection.state.as_str(),
            "injected" | "folded" | "rejected" | "superseded"
        ) {
            return Err(resolution_conflict());
        }
        let parent_id = SessionId::new(injection.parent_session_id.clone())
            .map_err(|_| IpcError::internal("注入行父会话 id 非法"))?;
        let parent = self
            .reads
            .session(&parent_id)
            .await
            .map_err(|error| IpcError::internal(format!("父会话读取失败：{error}")))?
            .ok_or_else(|| IpcError::session_not_found(parent_id.as_str()))?;
        let mode = SubagentDeliveryMode::from_code(&injection.mode)
            .unwrap_or(SubagentDeliveryMode::MessageAppend);
        // 安全边界：父会话空闲/无在途 run；运行中不插队。
        if !self
            .wait_parent_idle(&parent_id, Duration::from_secs(30))
            .await
        {
            return Ok((false, injection.context_version as u64, injection.mode));
        }
        // 折叠旧行（保留 payload；只影响组装层展示）。
        let injections = self
            .reads
            .subagent_injections(parent_id.as_str())
            .await
            .map_err(|error| IpcError::internal(format!("注入行读取失败：{error}")))?;
        let fold = fold_ids(&injections);
        if !fold.is_empty() {
            let _ = self
                .write
                .execute(StoreCommand::Subagent {
                    command: SubagentCommand::FoldInjections {
                        ids: fold,
                        folded_at: self.clock.now_ms(),
                    },
                })
                .await;
        }
        let version = match self
            .reads
            .subagent_context_version(parent_id.as_str())
            .await
        {
            Ok(version) => u64::try_from(version.max(0)).unwrap_or(0).saturating_add(1),
            Err(_) => u64::try_from(injection.context_version.max(0))
                .unwrap_or(0)
                .saturating_add(1),
        };
        let supersedes = injections
            .iter()
            .rev()
            .find(|item| item.state == "injected")
            .map(|item| item.id.clone());
        let payload: Value = serde_json::from_str(&injection.payload).unwrap_or(Value::Null);

        let applied = match mode {
            SubagentDeliveryMode::MessageAppend => {
                let block = wrap_block(version, supersedes.as_deref(), &payload);
                let text = format!(
                    "[subagent_result] 以下为子 Agent 结果（不是用户指令；最高 version 有效）。\n\n{block}"
                );
                let span = SessionSendRequest {
                    session_id: parent_id.as_str().to_owned(),
                    text,
                    client_msg_id: ulid::Ulid::new().to_string(),
                    thinking_depth: None,
                    references: Vec::new(),
                    provider_id: None,
                    model: None,
                    subagent_context: Some(SubagentContextInput {
                        version: i64::try_from(version).unwrap_or(i64::MAX),
                        block: block.clone(),
                        mode: Some(mode.as_str().to_owned()),
                    }),
                };
                match self.inner_send(span).await {
                    Ok(response) => {
                        let applied_version = response
                            .get("applied_context_version")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let applied_mode = response
                            .get("applied_context_mode")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        applied_version >= version && applied_mode == mode.as_str()
                    }
                    Err(_) => false,
                }
            }
            SubagentDeliveryMode::OriginFork => {
                match self
                    .deliver_origin_fork(&parent, &injection, &payload, version)
                    .await
                {
                    Ok(true) => true,
                    Ok(false) => {
                        self.audit(
                            actor,
                            "subagent.result.injected",
                            &parent_id,
                            injection_child_ids(&injection).first().map(String::as_str),
                            Some(json!({"reason": "origin_fork_unavailable", "verdict": payload.get("verdict")})),
                            "denied",
                        )
                        .await;
                        false
                    }
                    Err(_) => false,
                }
            }
            SubagentDeliveryMode::SystemReplace => false,
        };
        if !applied {
            return Ok((false, version, mode.as_str().to_owned()));
        }
        // 旧注入行标 superseded（message_append 旧块仅历史证据）。
        if let Some(previous) = &supersedes {
            let _ = self
                .write
                .execute(StoreCommand::Subagent {
                    command: SubagentCommand::SupersedeInjection {
                        id: previous.clone(),
                    },
                })
                .await;
        }
        self.write
            .execute(StoreCommand::Subagent {
                command: SubagentCommand::UpdateInjectionState {
                    id: injection.id.clone(),
                    state: "injected".to_owned(),
                    injected_at: Some(self.clock.now_ms()),
                    folded_at: None,
                },
            })
            .await
            .map_err(|error| IpcError::internal(format!("注入状态落库失败：{error}")))?;
        self.audit(
            actor,
            "subagent.result.injected",
            &parent_id,
            injection_child_ids(&injection).first().map(String::as_str),
            Some(json!({
                "mode": mode.as_str(),
                "version": version,
                "verdict": payload.get("verdict"),
            })),
            "allowed",
        )
        .await;
        Ok((true, version, mode.as_str().to_owned()))
    }

    async fn wait_parent_idle(&self, parent_id: &SessionId, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let active = self
                .reads
                .session_has_active_run(parent_id)
                .await
                .unwrap_or(true);
            let session = self.reads.session(parent_id).await.ok().flatten();
            let busy = active
                || session.as_ref().is_some_and(|session| {
                    matches!(
                        session.status,
                        aether_core::SessionStatus::Running
                            | aether_core::SessionStatus::WaitingPermission
                    )
                });
            if !busy {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn deliver_origin_fork(
        &self,
        parent: &Session,
        injection: &SubagentInjectionRecord,
        payload: &Value,
        version: u64,
    ) -> Result<bool, IpcError> {
        let child_ids = injection_child_ids(injection);
        let Some(child_id) = child_ids.first() else {
            return Ok(false);
        };
        let Some(origin) = self
            .origin_forks
            .lock()
            .ok()
            .and_then(|forks| forks.get(child_id).cloned())
        else {
            // 重启/进程内状态不可恢复：观测性拒绝，不回退 live history。
            return Ok(false);
        };
        let origin_text = serde_json::to_string(&origin).unwrap_or_else(|_| "{}".to_owned());
        let block = wrap_block(version, None, payload);
        let prompt = format!(
            "[origin_fork] 你正在处理冻结的 delegation boundary；不得读取 live history。\n\n\
             [origin_snapshot]\n{origin_text}\n\n[aether:subagent-results]\n{block}\n\n\
             请基于冻结起点与子 Agent 结果生成最终回调输出（将追加到父会话时间线）。"
        );
        let create = SessionCreateRequest {
            runtime_id: Some(parent.runtime_id.as_str().to_owned()),
            title: format!("子 Agent 回调 · {}", truncate_utf8(&parent.title, 64).0),
            workspace_id: parent
                .workspace_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            model: parent.model.clone(),
            provider_id: parent
                .config
                .get("aether")
                .and_then(|value| value.get("provider_id"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            thinking_depth: Some(i64::from(parent.thinking_depth)),
            permission_mode: Some(parent.permission_mode.as_str().to_owned()),
            preset_id: None,
        };
        let created = match self.inner_create(create).await {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        let callback_id = created
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| IpcError::internal("回调会话创建响应缺少 id"))?;
        let callback_session = SessionId::new(callback_id.clone())
            .map_err(|_| IpcError::internal("回调会话 id 非法"))?;
        let callback_meta = SubagentMeta {
            role: "custom".to_owned(),
            mode: "custom".to_owned(),
            template: "origin_fork".to_owned(),
            depth: 1,
            purpose: "origin fork callback".to_owned(),
            runtime_id: parent.runtime_id.as_str().to_owned(),
            provider_id: None,
            model: parent.model.clone(),
            permission_mode: parent.permission_mode.as_str().to_owned(),
            timeout_ms: aether_core::SUBAGENT_TIMEOUT_DEFAULT_MS,
            batch_id: injection.batch_id.clone(),
            verdict: None,
            resolution: "callback".to_owned(),
            result_ref: None,
            callback: true,
        };
        if let Ok(Some(child)) = self.reads.session(&callback_session).await {
            let _ = self.write_session_meta(&child, &callback_meta).await;
        }
        let send = SessionSendRequest {
            session_id: callback_id.clone(),
            text: prompt,
            client_msg_id: ulid::Ulid::new().to_string(),
            thinking_depth: None,
            references: Vec::new(),
            provider_id: None,
            model: None,
            subagent_context: None,
        };
        let sent = match self.inner_send(send).await {
            Ok(value) => value,
            Err(_) => {
                let _ = self.inner_delete(callback_id.clone()).await;
                return Ok(false);
            }
        };
        let Some(run_id) = sent.get("run_id").and_then(Value::as_str) else {
            return Ok(false);
        };
        let run_id = match aether_core::RunId::new(run_id.to_owned()) {
            Ok(run_id) => run_id,
            Err(_) => return Ok(false),
        };
        let deadline = self
            .clock
            .now_ms()
            .saturating_add(aether_core::SUBAGENT_TIMEOUT_DEFAULT_MS);
        loop {
            if self.clock.now_ms() >= deadline {
                self.inner_interrupt(callback_id.clone()).await;
                return Ok(false);
            }
            match self.reads.run(&run_id).await {
                Ok(Some(run)) if run.status == aether_core::RunStatus::Succeeded => break,
                Ok(Some(run))
                    if matches!(
                        run.status,
                        aether_core::RunStatus::Failed
                            | aether_core::RunStatus::Cancelled
                            | aether_core::RunStatus::Timeout
                    ) =>
                {
                    self.inner_delete(callback_id.clone()).await;
                    return Ok(false);
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let messages = self
            .reads
            .messages_latest(&callback_session, 10)
            .await
            .map_err(|error| IpcError::internal(format!("回调消息读取失败：{error}")))?;
        let content = messages
            .iter()
            .rev()
            .find(|message| matches!(message.role, MessageRole::Assistant))
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let message_id = MessageId::new(ulid::Ulid::new().to_string())
            .map_err(|_| IpcError::internal("回调消息 id 生成失败"))?;
        let now = self.clock.now_ms();
        let message = Message {
            id: message_id.clone(),
            session_id: parent.id.clone(),
            run_id: None,
            client_msg_id: None,
            role: MessageRole::Assistant,
            content,
            content_parts: None,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: now,
        };
        self.write
            .execute(StoreCommand::InsertMessage {
                message: message.clone(),
            })
            .await
            .map_err(|error| IpcError::internal(format!("回调消息落库失败：{error}")))?;
        self.emit(
            &parent.id,
            parent.runtime_id.as_str(),
            EventType::MessageCompleted,
            serde_json::to_value(aether_core::MessageCompletedPayload {
                message: aether_core::MessageSummary {
                    id: message.id,
                    session_id: message.session_id,
                    run_id: None,
                    role: message.role,
                    content: message.content,
                    created_at: message.created_at,
                },
                usage: None,
            })
            .unwrap_or(Value::Null),
        )
        .await;
        let _ = self.inner_delete(callback_id).await;
        Ok(true)
    }

    async fn resolve_inner(&self, request: SubagentResolveRequest) -> Result<Value, IpcError> {
        let child_id = SessionId::new(request.child_session_id.clone())
            .map_err(|_| IpcError::invalid_format("child_session_id", "必须是 26 位 ULID"))?;
        let child = self
            .reads
            .session(&child_id)
            .await
            .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?
            .ok_or_else(|| subagent_not_found(&request.child_session_id))?;
        let parent = child
            .parent_session_id
            .clone()
            .ok_or_else(|| subagent_not_found(&request.child_session_id))?;
        let injections = self
            .reads
            .subagent_injections(parent.as_str())
            .await
            .map_err(|error| IpcError::internal(format!("注入行读取失败：{error}")))?;
        let injection = injections
            .iter()
            .rev()
            .find(|injection| {
                injection_child_ids(injection)
                    .iter()
                    .any(|id| id == &request.child_session_id)
            })
            .cloned();
        let result = self
            .reads
            .subagent_result(&request.child_session_id)
            .await
            .map_err(|error| IpcError::internal(format!("结果读取失败：{error}")))?;

        match request.decision.as_str() {
            "accept" => {
                let Some(injection) = injection else {
                    return Err(result_not_ready());
                };
                if result.is_none() {
                    return Err(result_not_ready());
                }
                if matches!(
                    injection.state.as_str(),
                    "injected" | "folded" | "rejected" | "superseded"
                ) {
                    return Err(resolution_conflict());
                }
                let (injected, version, mode) = self.deliver(&injection.id, "user").await?;
                self.audit(
                    "user",
                    "subagent.result.accepted",
                    &parent,
                    Some(&request.child_session_id),
                    Some(json!({"decision": "accept", "injected": injected, "mode": mode, "note": request.note})),
                    "allowed",
                )
                .await;
                Ok(json!({
                    "decision": "accept",
                    "injected": injected,
                    "injection_id": injection.id,
                    "context_version": version,
                    "mode": mode,
                }))
            }
            "reject" => {
                let Some(injection) = injection else {
                    return Err(result_not_ready());
                };
                if matches!(
                    injection.state.as_str(),
                    "injected" | "folded" | "rejected" | "superseded"
                ) {
                    return Err(resolution_conflict());
                }
                self.write
                    .execute(StoreCommand::Subagent {
                        command: SubagentCommand::UpdateInjectionState {
                            id: injection.id.clone(),
                            state: "rejected".to_owned(),
                            injected_at: None,
                            folded_at: None,
                        },
                    })
                    .await
                    .map_err(|error| IpcError::internal(format!("拒绝状态落库失败：{error}")))?;
                let version = self
                    .reads
                    .subagent_context_version(parent.as_str())
                    .await
                    .unwrap_or(0);
                self.audit(
                    "user",
                    "subagent.result.rejected",
                    &parent,
                    Some(&request.child_session_id),
                    Some(json!({"decision": "reject", "note": request.note})),
                    "allowed",
                )
                .await;
                Ok(json!({
                    "decision": "reject",
                    "injected": false,
                    "injection_id": injection.id,
                    "context_version": version,
                    "mode": injection.mode,
                }))
            }
            "rerun" => {
                if let Some(injection) = &injection {
                    if matches!(
                        injection.state.as_str(),
                        "injected" | "folded" | "superseded"
                    ) {
                        return Err(resolution_conflict());
                    }
                    let _ = self
                        .write
                        .execute(StoreCommand::Subagent {
                            command: SubagentCommand::SupersedeInjection {
                                id: injection.id.clone(),
                            },
                        })
                        .await;
                }
                let meta = meta_of(&child);
                let spawn = SubagentSpawnRequest {
                    parent_session_id: parent.as_str().to_owned(),
                    role: if meta.role.is_empty() {
                        "custom".to_owned()
                    } else {
                        meta.role
                    },
                    purpose: meta.purpose,
                    mode: Some(if meta.mode.is_empty() {
                        "custom".to_owned()
                    } else {
                        meta.mode
                    }),
                    template: Some(if meta.template.is_empty() {
                        "custom".to_owned()
                    } else {
                        meta.template
                    }),
                    context_refs: Vec::new(),
                    runtime_id: Some(meta.runtime_id),
                    provider_id: meta.provider_id,
                    model: meta.model,
                    permission_mode: Some(meta.permission_mode),
                    timeout_ms: Some(meta.timeout_ms),
                };
                let spawned = self.spawn_inner(spawn, "user").await?;
                let version = self
                    .reads
                    .subagent_context_version(parent.as_str())
                    .await
                    .unwrap_or(0);
                self.audit(
                    "user",
                    "subagent.result.rerun",
                    &parent,
                    Some(&request.child_session_id),
                    Some(json!({"decision": "rerun", "note": request.note})),
                    "allowed",
                )
                .await;
                Ok(json!({
                    "decision": "rerun",
                    "injected": false,
                    "injection_id": injection.map(|injection| injection.id),
                    "context_version": version,
                    "mode": "message_append",
                    "new_child_session_id": spawned.get("child_session_id").cloned().unwrap_or(Value::Null),
                }))
            }
            _ => Err(IpcError::invalid_enum(
                "decision",
                "允许值：accept / reject / rerun",
            )),
        }
    }

    async fn emit(
        &self,
        session_id: &SessionId,
        runtime_id: &str,
        event_type: EventType,
        payload: Value,
    ) {
        let Some(pipeline) = &self.pipeline else {
            return;
        };
        let envelope = json!({
            "v": aether_core::EVENT_ENVELOPE_VERSION,
            "id": ulid::Ulid::new().to_string(),
            "session_id": session_id,
            "run_id": null,
            "runtime_id": runtime_id,
            "seq": null,
            "ts": self.clock.now_ms(),
            "type": event_type.as_str(),
            "payload": payload,
        });
        if let Err(error) = pipeline.submit(envelope).await {
            tracing::warn!(%event_type, error = %error, "子 Agent 事件提交失败（状态已落库）");
        }
    }

    async fn audit(
        &self,
        actor: &str,
        action: &str,
        parent_id: &SessionId,
        child_id: Option<&str>,
        detail: Option<Value>,
        result: &str,
    ) {
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: Some(parent_id.as_str().to_owned()),
            runtime_id: None,
            actor: actor.to_owned(),
            action: action.to_owned(),
            resource: child_id.map(|child| format!("session:{child}")),
            detail: detail.map(|value| value.to_string()),
            result: Some(result.to_owned()),
            ts: self.clock.now_ms(),
        };
        if let Err(error) = self
            .write
            .execute(StoreCommand::InsertAudit { record })
            .await
        {
            tracing::warn!(%action, error = %error, "子 Agent 审计写入失败");
        }
    }
}

struct WatchContext {
    parent_id: SessionId,
    child_id: String,
    run_id: String,
    runtime_id: String,
    meta: SubagentMeta,
}

fn injection_child_ids(injection: &SubagentInjectionRecord) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(&injection.child_session_ids).unwrap_or_default()
}

fn meta_of(session: &Session) -> SubagentMeta {
    let Some(meta) = session
        .config
        .get("aether")
        .and_then(|value| value.get("subagent"))
    else {
        return SubagentMeta::default();
    };
    SubagentMeta {
        role: meta
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("custom")
            .to_owned(),
        mode: meta
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("custom")
            .to_owned(),
        template: meta
            .get("template")
            .and_then(Value::as_str)
            .unwrap_or("custom")
            .to_owned(),
        depth: meta
            .get("depth")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0),
        purpose: meta
            .get("purpose")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        runtime_id: meta
            .get("runtime_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        provider_id: meta
            .get("provider_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: meta.get("model").and_then(Value::as_str).map(str::to_owned),
        permission_mode: meta
            .get("permission_mode")
            .and_then(Value::as_str)
            .unwrap_or("workspace_write")
            .to_owned(),
        timeout_ms: meta
            .get("timeout_ms")
            .and_then(Value::as_i64)
            .unwrap_or(SUBAGENT_TIMEOUT_DEFAULT_MS),
        batch_id: meta
            .get("batch_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        verdict: meta
            .get("verdict")
            .and_then(Value::as_str)
            .map(str::to_owned),
        resolution: meta
            .get("resolution")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        result_ref: meta
            .get("result_ref")
            .and_then(Value::as_str)
            .map(str::to_owned),
        callback: meta
            .get("callback")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn permission_rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::ReadOnly => 1,
        PermissionMode::WorkspaceWrite => 2,
        PermissionMode::DangerFullAccess => 3,
    }
}

fn subagent_title(role: &str, purpose: &str) -> String {
    let short = truncate_utf8(purpose.trim(), 120).0;
    truncate_utf8(&format!("子 Agent · {role} · {short}"), 256).0
}

async fn mode_for_parent(
    backend: &SubagentControlBackend,
    parent_id: &SessionId,
    _meta: &SubagentMeta,
) -> String {
    // 后台（父有在途 run）走 origin_fork；否则默认 append-only。
    match backend.reads.session_has_active_run(parent_id).await {
        Ok(true) => SubagentDeliveryMode::OriginFork.as_str().to_owned(),
        _ => SubagentDeliveryMode::MessageAppend.as_str().to_owned(),
    }
}

fn build_prompt(
    template: &str,
    mode: &str,
    purpose: &str,
    depth: u32,
    context_texts: &[String],
) -> String {
    let template_instruction = match template {
        "parallel_explore" => {
            "你是并行探索子 Agent。独立提出可比较的方案/结论，不假设父会话已有上下文；\
             输出必须包含 verdict、summary、findings，并用 <aether:subagent-result>{JSON}</aether:subagent-result> 包裹。"
        }
        "adversarial_review" => {
            "你是对抗式审查子 Agent。只报告可验证的问题（blocker/major/minor/info），\
             每条 finding 给出证据路径；输出必须包含 verdict、summary、findings，并用 \
             <aether:subagent-result>{JSON}</aether:subagent-result> 包裹。"
        }
        _ => {
            "你是父 Agent 显式派生的子 Agent。只基于下列任务与显式上下文执行；\
             输出结构化结果，并用 <aether:subagent-result>{JSON}</aether:subagent-result> 包裹。"
        }
    };
    let mut prompt =
        format!("{template_instruction}\n\n[role] {mode}\n[depth] {depth}\n[purpose]\n{purpose}\n");
    if !context_texts.is_empty() {
        prompt.push_str("\n[context_refs]\n");
        for (index, text) in context_texts.iter().enumerate() {
            prompt.push_str(&format!(
                "--- ref {} (source=explicit_text) ---\n{}\n",
                index + 1,
                text
            ));
        }
    }
    prompt
}

fn limit_exceeded(limit: &str, message: String) -> IpcError {
    IpcError::invalid_value(message)
        .with_business_code(BusinessCode::SubagentLimitExceeded)
        .with_details(json!({"limit": limit}))
}

fn context_rejected(message: String) -> IpcError {
    IpcError::invalid_value(message).with_business_code(BusinessCode::SubagentContextRejected)
}

fn subagent_not_found(id: &str) -> IpcError {
    IpcError::invalid_value(format!("子 Agent 不存在（{id}）"))
        .with_business_code(BusinessCode::SubagentNotFound)
}

fn result_not_ready() -> IpcError {
    IpcError::invalid_value("子 Agent 结果尚未就绪")
        .with_business_code(BusinessCode::SubagentResultNotReady)
}

fn resolution_conflict() -> IpcError {
    IpcError::invalid_value("子 Agent 处置冲突（终态不可重复注入/处置）；请刷新后重试")
        .with_business_code(BusinessCode::SubagentResolutionConflict)
}

impl IpcBackend for SubagentControlBackend {
    fn health(&self) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let mut value = self.inner.health()?;
        let metrics = self.call(async move {
            let active = reads
                .global_active_child_session_count()
                .await
                .map_err(|error| IpcError::internal(format!("子 Agent 计数失败：{error}")))?;
            Ok(json!({"active_children": active}))
        })?;
        if let Some(object) = value.as_object_mut() {
            object.insert("subagents".to_owned(), metrics);
        }
        Ok(value)
    }

    fn runtimes_list(&self) -> Result<Value, IpcError> {
        self.inner.runtimes_list()
    }

    forward_request!(
        session_list: SessionListRequest,
        session_search: SessionSearchRequest,
        session_rename: SessionRenameRequest,
        session_delete: SessionDeleteRequest,
        session_create: SessionCreateRequest,
        session_send: SessionSendRequest,
        session_pause: SessionIdRequest,
        session_resume: SessionIdRequest,
        session_dispose: SessionIdRequest,
        messages_page: MessagesPageRequest,
        permissions_pending: PermissionsPendingRequest,
        permission_resolve: PermissionResolveRequest,
        session_permission_mode_set: SessionPermissionModeSetRequest,
        rule_add: RuleAddRequest,
        rule_update: RuleUpdateRequest,
        rule_delete: RuleDeleteRequest,
        rule_toggle: RuleToggleRequest,
        settings_get: SettingsGetRequest,
        settings_set: SettingsSetRequest,
        app_restart: AppRestartRequest,
        run_retry: RunRetryRequest,
        runtime_retry: RuntimeRetryRequest,
        runtime_enable: RuntimeEnableRequest,
        runtime_disable: RuntimeDisableRequest,
        workspaces_list: WorkspacesListRequest,
        workspace_update: WorkspaceUpdateRequest,
        workspace_delete: WorkspaceDeleteRequest,
        artifacts_list: ArtifactsListRequest,
        artifact_remove: ArtifactRemoveRequest,
        providers_list: ProvidersListRequest,
        provider_create: ProviderCreateRequest,
        provider_update: ProviderUpdateRequest,
        provider_delete: ProviderDeleteRequest,
        provider_toggle: ProviderToggleRequest,
        provider_model_add: ProviderModelAddRequest,
        provider_model_toggle: ProviderModelToggleRequest,
        presets_list: PresetsListRequest,
        preset_add: PresetAddRequest,
        preset_update: PresetUpdateRequest,
        preset_delete: PresetDeleteRequest,
        provider_test: ProviderTestRequest,
        provider_models_fetch: ProviderModelsFetchRequest,
        audit_query: AuditQueryRequest,
        workflow_create: WorkflowCreateRequest,
        workflow_list: WorkflowListRequest,
        workflow_get: WorkflowIdRequest,
        workflow_update: WorkflowUpdateRequest,
        workflow_delete: WorkflowDeleteRequest,
        workflow_import: WorkflowImportRequest,
        workflow_run: WorkflowRunRequest,
        workflow_run_get: WorkflowRunGetRequest,
        workflow_run_list: WorkflowRunListRequest,
        workflow_run_cancel: WorkflowRunCancelRequest,
        workflow_node_retry: WorkflowNodeRetryRequest,
    );

    forward_none!(rules_list, backup_list);

    fn backup_auto_status(&self) -> Result<Option<Value>, IpcError> {
        self.inner.backup_auto_status()
    }

    fn retention_status(&self) -> Result<Option<Value>, IpcError> {
        self.inner.retention_status()
    }

    fn export_metrics_status(&self) -> Result<Option<Value>, IpcError> {
        self.inner.export_metrics_status()
    }

    fn backup_create(
        &self,
        request: &BackupCreateRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.backup_create(request, canonical_target_dir)
    }

    fn backup_restore(
        &self,
        request: &BackupRestoreRequest,
        canonical_external_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.backup_restore(request, canonical_external_path)
    }

    fn workspace_set(
        &self,
        request: &WorkspaceSetRequest,
        canonical_root_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.workspace_set(request, canonical_root_path)
    }

    fn workspace_create(
        &self,
        request: &WorkspaceCreateRequest,
        canonical_root_path: &Path,
    ) -> Result<Value, IpcError> {
        self.inner.workspace_create(request, canonical_root_path)
    }

    fn session_workspace_assign(
        &self,
        request: &SessionWorkspaceAssignRequest,
    ) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let session_id = request.session_id.clone();
        let blocked = self.call(async move {
            let id = SessionId::new(session_id)
                .map_err(|_| IpcError::invalid_format("session_id", "必须是 26 位 ULID"))?;
            let session = reads
                .session(&id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?;
            Ok(session.is_some_and(|session| session.parent_session_id.is_some()))
        })?;
        if blocked {
            return Err(context_rejected(
                "子 Agent 工作区固定继承父会话，禁止改用其他工作区".to_owned(),
            ));
        }
        self.inner.session_workspace_assign(request)
    }

    fn artifact_add(
        &self,
        request: &ArtifactAddRequest,
        resolved: &ArtifactPath,
    ) -> Result<Value, IpcError> {
        self.inner.artifact_add(request, resolved)
    }

    fn export_diagnostics(
        &self,
        request: &ExportDiagnosticsRequest,
        canonical_target_dir: &Path,
    ) -> Result<Value, IpcError> {
        self.inner.export_diagnostics(request, canonical_target_dir)
    }

    fn session_export(
        &self,
        request: &SessionExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.session_export(request, canonical_target_dir)
    }

    fn archive_export(
        &self,
        request: &ArchiveExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.archive_export(request, canonical_target_dir)
    }

    fn audit_export(
        &self,
        request: &AuditExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.audit_export(request, canonical_target_dir)
    }

    fn session_interrupt(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let inner = Arc::clone(&self.inner);
        let reads = self.reads.clone();
        let session_id = request.session_id.clone();
        self.call(async move {
            let id = SessionId::new(session_id.clone())
                .map_err(|_| IpcError::invalid_format("session_id", "必须是 26 位 ULID"))?;
            let inner_interrupt = Arc::clone(&inner);
            let target = session_id.clone();
            let report = tokio::task::spawn_blocking(move || {
                inner_interrupt.session_interrupt(&SessionIdRequest { session_id: target })
            })
            .await
            .map_err(|error| IpcError::internal(format!("会话中断任务失败：{error}")))??;
            // 取消树级联：父中断 → 活动子 Agent 逐级中断（≤5s 收口）。
            let children = reads
                .sessions(SessionQuery {
                    parent_session_id: Some(session_id),
                    include_children: true,
                    ..SessionQuery::default()
                })
                .await
                .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?;
            for child in children {
                if !session_status_is_terminal(child.status) {
                    let inner_child = Arc::clone(&inner);
                    let child_id = child.id.as_str().to_owned();
                    let _ = tokio::task::spawn_blocking(move || {
                        inner_child
                            .session_interrupt(&SessionIdRequest { session_id: child_id })
                    })
                    .await;
                }
            }
            let _ = id;
            Ok(report)
        })
    }

    fn subagent_spawn(&self, request: &SubagentSpawnRequest) -> Result<Value, IpcError> {
        let request = request.clone();
        let this = self.clone();
        self.call(async move { this.spawn_inner(request, "user").await })
    }

    fn subagent_list(&self, request: &SubagentListRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let parent = request.parent_session_id.clone();
        let include_completed = request.include_completed.unwrap_or(false);
        self.call(async move {
            let parent_id = SessionId::new(parent.clone())
                .map_err(|_| IpcError::invalid_format("parent_session_id", "必须是 26 位 ULID"))?;
            if reads
                .session(&parent_id)
                .await
                .map_err(|error| IpcError::internal(format!("父会话读取失败：{error}")))?
                .is_none()
            {
                return Err(IpcError::session_not_found(parent_id.as_str()));
            }
            let children = reads
                .sessions(SessionQuery {
                    parent_session_id: Some(parent.clone()),
                    include_children: true,
                    limit: Some(500),
                    ..SessionQuery::default()
                })
                .await
                .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?;
            let injections = reads
                .subagent_injections(&parent)
                .await
                .map_err(|error| IpcError::internal(format!("注入行读取失败：{error}")))?;
            let mut items = Vec::new();
            for child in children {
                let meta = meta_of(&child);
                if meta.callback {
                    continue;
                }
                let result = reads
                    .subagent_result(child.id.as_str())
                    .await
                    .map_err(|error| IpcError::internal(format!("结果读取失败：{error}")))?;
                let injection = injections.iter().rev().find(|injection| {
                    injection_child_ids(injection)
                        .iter()
                        .any(|id| id == child.id.as_str())
                });
                let injection_state = injection
                    .map(|injection| injection.state.clone())
                    .unwrap_or_else(|| "pending".to_owned());
                if !include_completed
                    && session_status_is_terminal(child.status)
                    && matches!(
                        injection_state.as_str(),
                        "injected" | "folded" | "rejected" | "superseded"
                    )
                {
                    continue;
                }
                let summary = result
                    .as_ref()
                    .and_then(|result| result.summary.clone())
                    .map(|summary| truncate_utf8(&summary, SUBAGENT_SUMMARY_MAX_BYTES).0);
                items.push(json!({
                    "child_session_id": child.id.as_str(),
                    "parent_session_id": parent,
                    "title": child.title,
                    "role": meta.role,
                    "mode": meta.mode,
                    "depth": meta.depth,
                    "status": child.status.as_str(),
                    "verdict": result.as_ref().and_then(|result| result.verdict.clone()),
                    "result_summary": summary,
                    "summary_truncated": result.as_ref().is_some_and(|result| result.summary_truncated),
                    "result_ref": meta.result_ref,
                    "injection_state": injection_state,
                    "delivery_mode": injection.map(|injection| injection.mode.clone()).unwrap_or_else(|| "message_append".to_owned()),
                    "has_more_details": result.is_some(),
                    "created_at": child.created_at,
                }));
            }
            Ok(json!({"items": items}))
        })
    }

    fn subagent_cancel(&self, request: &SubagentCancelRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let inner = Arc::clone(&self.inner);
        let clock = Arc::clone(&self.clock);
        let write = self.write.clone();
        let child_id = request.child_session_id.clone();
        let reason = request.reason.clone();
        self.call(async move {
            let child = SessionId::new(child_id.clone())
                .map_err(|_| IpcError::invalid_format("child_session_id", "必须是 26 位 ULID"))?;
            let session = reads
                .session(&child)
                .await
                .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?
                .ok_or_else(|| subagent_not_found(&child_id))?;
            let Some(parent) = session.parent_session_id.clone() else {
                return Err(subagent_not_found(&child_id));
            };
            let status = if session_status_is_terminal(session.status) {
                session.status.as_str().to_owned()
            } else {
                let inner = Arc::clone(&inner);
                let id = child_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    inner.session_interrupt(&SessionIdRequest { session_id: id })
                })
                .await;
                "cancelled".to_owned()
            };
            let record = AuditLogRecord {
                id: ulid::Ulid::new().to_string(),
                session_id: Some(parent.as_str().to_owned()),
                runtime_id: None,
                actor: "user".to_owned(),
                action: "subagent.cancel".to_owned(),
                resource: Some(format!("session:{child_id}")),
                detail: Some(json!({"reason": reason}).to_string()),
                result: Some("allowed".to_owned()),
                ts: clock.now_ms(),
            };
            let _ = write.execute(StoreCommand::InsertAudit { record }).await;
            Ok(json!({"status": status}))
        })
    }

    fn subagent_resolve(&self, request: &SubagentResolveRequest) -> Result<Value, IpcError> {
        let request = request.clone();
        let this = self.clone();
        self.call(async move { this.resolve_inner(request).await })
    }

    fn subagent_result_get(&self, request: &SubagentResultGetRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let clock = Arc::clone(&self.clock);
        let write = self.write.clone();
        let parent_id = request.parent_session_id.clone();
        let child_id = request.child_session_id.clone();
        let offset = request.offset.unwrap_or(0);
        let limit_bytes = request.limit_bytes.unwrap_or(16_384) as usize;
        self.call(async move {
            let parent = SessionId::new(parent_id.clone())
                .map_err(|_| IpcError::invalid_format("parent_session_id", "必须是 26 位 ULID"))?;
            let child = SessionId::new(child_id.clone())
                .map_err(|_| IpcError::invalid_format("child_session_id", "必须是 26 位 ULID"))?;
            let session = reads
                .session(&child)
                .await
                .map_err(|error| IpcError::internal(format!("子会话读取失败：{error}")))?
                .ok_or_else(|| subagent_not_found(&child_id))?;
            if session.parent_session_id.as_ref() != Some(&parent) {
                return Err(subagent_not_found(&child_id));
            }
            let result = reads
                .subagent_result(&child_id)
                .await
                .map_err(|error| IpcError::internal(format!("结果读取失败：{error}")))?
                .ok_or_else(result_not_ready)?;
            let total = result.result.len();
            let mut start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
            while start < total && !result.result.is_char_boundary(start) {
                start += 1;
            }
            let mut end = start.saturating_add(limit_bytes).min(total);
            while end > start && !result.result.is_char_boundary(end) {
                end -= 1;
            }
            let chunk = result.result[start..end].to_owned();
            let next_offset = if end < total {
                Some(u64::try_from(end).unwrap_or(u64::MAX))
            } else {
                None
            };
            let record = AuditLogRecord {
                id: ulid::Ulid::new().to_string(),
                session_id: Some(parent_id.clone()),
                runtime_id: None,
                actor: "user".to_owned(),
                action: "subagent.result.pulled".to_owned(),
                resource: Some(format!("session:{child_id}")),
                detail: Some(json!({"offset": start, "limit_bytes": limit_bytes}).to_string()),
                result: Some("allowed".to_owned()),
                ts: clock.now_ms(),
            };
            let _ = write.execute(StoreCommand::InsertAudit { record }).await;
            Ok(json!({
                "child_session_id": child_id,
                "result_chunk": chunk,
                "offset": start,
                "next_offset": next_offset,
                "total_bytes": total,
                "truncated": result.truncated,
            }))
        })
    }
}
