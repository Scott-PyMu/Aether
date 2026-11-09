//! M10-01/ADR-031：agent 节点执行器（真实会话后端）。
//!
//! 每个 agent 节点创建独立子会话（`sessions.parent_session_id` = 运行锚点会话），
//! 只下发显式 prompt + 节点输入；不继承锚点会话消息全文/工具状态。结果取子会话
//! 最后一条 assistant 消息作为节点输出。M10-02 在交付层补齐结果注入/隔离矩阵。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use aether_core::{MessageRole, RunStatus, SessionId};
use aether_store::StoreCommand;
use serde_json::{json, Value};

use super::engine::{AgentError, AgentExecutor, AgentOutcome, AgentRequest};
use crate::ipc::dto::{ReferenceEntry, SessionCreateRequest, SessionSendRequest};
use crate::ipc::IpcBackend;
use crate::session_backend::SessionBackend;

/// 真实会话后端 agent 执行器。
pub struct SessionAgentExecutor {
    backend: Arc<SessionBackend>,
}

impl SessionAgentExecutor {
    pub fn new(backend: Arc<SessionBackend>) -> Self {
        Self { backend }
    }

    async fn create_child(&self, request: &AgentRequest) -> Result<(String, String), AgentError> {
        let backend = Arc::clone(&self.backend);
        let anchor_id = SessionId::new(request.session_id.clone())
            .map_err(|_| AgentError::new("session_not_found", "锚点会话 id 非法", false))?;
        let reads = backend.read_pool().ok_or_else(|| {
            AgentError::new("core_not_ready", "会话后端未接线（读连接池缺失）", true)
        })?;
        let anchor = reads
            .session(&anchor_id)
            .await
            .map_err(|error| {
                AgentError::new("internal", format!("锚点会话读取失败：{error}"), true)
            })?
            .ok_or_else(|| AgentError::new("session_not_found", "锚点会话不存在", false))?;
        let provider_id = request.provider_id.clone().or_else(|| {
            anchor
                .config
                .get("aether")
                .and_then(|value| value.get("provider_id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let permission_mode = request
            .permission_mode
            .clone()
            .unwrap_or_else(|| anchor.permission_mode.as_str().to_owned());
        let create = SessionCreateRequest {
            runtime_id: Some(
                request
                    .runtime_id
                    .clone()
                    .unwrap_or_else(|| anchor.runtime_id.as_str().to_owned()),
            ),
            title: format!("workflow:{}:{}", request.workflow_id, request.node_id),
            workspace_id: anchor
                .workspace_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            model: request.model.clone().or_else(|| anchor.model.clone()),
            provider_id,
            thinking_depth: Some(i64::from(anchor.thinking_depth)),
            permission_mode: Some(permission_mode),
            preset_id: anchor.preset_id.as_ref().map(|id| id.as_str().to_owned()),
        };
        let created = tokio::task::spawn_blocking(move || backend.session_create(&create))
            .await
            .map_err(|error| {
                AgentError::new("internal", format!("会话创建任务失败：{error}"), true)
            })??;
        let child_id = created
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::new("internal", "会话创建响应缺少 id", false))?
            .to_owned();
        let child = SessionId::new(child_id.clone())
            .map_err(|_| AgentError::new("internal", "子会话 id 非法", false))?;
        let write = self.backend.write_queue().ok_or_else(|| {
            AgentError::new(
                "core_not_ready",
                "存储写路径未接线：无法登记子会话父指针",
                false,
            )
        })?;
        write
            .execute(StoreCommand::UpdateSessionParent {
                session_id: child,
                parent_session_id: anchor_id,
                updated_at: now_ms(),
            })
            .await
            .map_err(|error| {
                AgentError::new("internal", format!("子会话父指针登记失败：{error}"), true)
            })?;
        Ok((child_id, anchor.runtime_id.as_str().to_owned()))
    }

    async fn send_and_wait(
        &self,
        child_id: &str,
        prompt: &str,
        references: Vec<ReferenceEntry>,
        timeout_ms: i64,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<String, AgentError> {
        let backend = Arc::clone(&self.backend);
        let request = SessionSendRequest {
            session_id: child_id.to_owned(),
            text: prompt.to_owned(),
            client_msg_id: ulid::Ulid::new().to_string(),
            thinking_depth: None,
            references,
            provider_id: None,
            model: None,
        };
        let response = tokio::task::spawn_blocking(move || backend.session_send(&request))
            .await
            .map_err(|error| {
                AgentError::new("internal", format!("发送任务失败：{error}"), true)
            })??;
        let run_id = response
            .get("run_id")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::new("internal", "发送响应缺少 run_id", false))?
            .to_owned();
        let reads = self.backend.read_pool().ok_or_else(|| {
            AgentError::new("core_not_ready", "会话后端未接线（读连接池缺失）", true)
        })?;
        let run_id = aether_core::RunId::new(run_id)
            .map_err(|_| AgentError::new("internal", "run_id 非法", false))?;
        let deadline = now_ms().saturating_add(timeout_ms);
        loop {
            if cancel.load(Ordering::SeqCst) {
                let backend = Arc::clone(&self.backend);
                let session_id = child_id.to_owned();
                let _ = tokio::task::spawn_blocking(move || {
                    backend.session_interrupt(&crate::ipc::dto::SessionIdRequest { session_id })
                })
                .await;
                return Err(AgentError::new("cancelled", "工作流已取消", false));
            }
            if now_ms() >= deadline {
                return Err(AgentError::new("timeout", "agent 节点执行超时", true));
            }
            let run = reads
                .run(&run_id)
                .await
                .map_err(|error| {
                    AgentError::new("internal", format!("run 读取失败：{error}"), true)
                })?
                .ok_or_else(|| AgentError::new("internal", "run 不存在", false))?;
            match run.status {
                RunStatus::Succeeded => break,
                RunStatus::Failed => {
                    return Err(AgentError::new(
                        "agent_run_failed",
                        run.error.unwrap_or_else(|| "子会话 run 失败".to_owned()),
                        true,
                    ));
                }
                RunStatus::Cancelled => {
                    return Err(AgentError::new("cancelled", "子会话 run 已取消", false));
                }
                RunStatus::Timeout => {
                    return Err(AgentError::new("timeout", "子会话 run 超时", true));
                }
                RunStatus::Queued | RunStatus::Running => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let child = SessionId::new(child_id.to_owned())
            .map_err(|_| AgentError::new("internal", "子会话 id 非法", false))?;
        let messages = reads
            .messages_latest(&child, 10)
            .await
            .map_err(|error| AgentError::new("internal", format!("消息读取失败：{error}"), true))?;
        let content = messages
            .iter()
            .rev()
            .find(|message| matches!(message.role, MessageRole::Assistant))
            .map(|message| message.content.clone())
            .unwrap_or_default();
        Ok(content)
    }
}

impl AgentExecutor for SessionAgentExecutor {
    fn execute(
        &self,
        request: AgentRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AgentOutcome, AgentError>> + Send + '_>,
    > {
        Box::pin(async move {
            let (child_id, _runtime_id) = self.create_child(&request).await?;
            let references = request
                .context_refs
                .iter()
                .filter(|reference| matches!(reference.kind.as_str(), "file" | "directory"))
                .map(|reference| ReferenceEntry {
                    path: reference.path.clone(),
                    kind: reference.kind.clone(),
                })
                .collect::<Vec<_>>();
            let mut prompt = request.prompt.clone();
            if !request.input.is_null() {
                let input = serde_json::to_string(&request.input).unwrap_or_default();
                prompt.push_str("\n\n[workflow_input]\n");
                prompt.push_str(&input);
            }
            let content = self
                .send_and_wait(
                    &child_id,
                    &prompt,
                    references,
                    request.timeout_ms,
                    &request.cancel,
                )
                .await?;
            Ok(AgentOutcome {
                output: json!({
                    "session_id": child_id,
                    "content": content,
                }),
                session_id: Some(child_id),
            })
        })
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
