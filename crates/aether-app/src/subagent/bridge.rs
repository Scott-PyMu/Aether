//! M10-02/ADR-032：适配器 → 核心子 Agent 请求桥（D6 增量）。
//!
//! 适配器 SDK 的 `subagent_spawn` 工具调用经 JSON-RPC 请求进入本桥；与 IPC
//! `subagent_*` 共用同一核心校验/审计/上限，`actor=agent`。
//! 启动顺序：桥先注入监督器，控制后端构建完成后再绑定（OnceLock）。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use aether_adapters::connection::{CatalogError, SubagentRequestSource};
use aether_core::SessionId;
use serde_json::Value;

use super::control::SubagentControlBackend;
use crate::ipc::dto::{
    SubagentCancelRequest, SubagentListRequest, SubagentResultGetRequest, SubagentSpawnRequest,
};
use crate::ipc::validate::CommandRequest;
use crate::ipc::{IpcBackend, IpcError, IpcErrorCode};

/// 适配器协议桥（延迟绑定控制后端）。
#[derive(Default)]
pub struct SubagentBridge {
    control: Arc<OnceLock<Arc<SubagentControlBackend>>>,
}

impl SubagentBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// 绑定控制后端（启动尾段；重复绑定忽略）。
    pub fn bind(&self, control: Arc<SubagentControlBackend>) {
        let _ = self.control.set(control);
    }

    fn control(&self) -> Result<&Arc<SubagentControlBackend>, CatalogError> {
        self.control
            .get()
            .ok_or_else(|| CatalogError::internal("子 Agent 控制后端未就绪"))
    }
}

impl SubagentRequestSource for SubagentBridge {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CatalogError>> + Send + 'a>> {
        Box::pin(async move { self.dispatch(method, params) })
    }
}

impl SubagentBridge {
    fn dispatch(&self, method: &str, params: Value) -> Result<Value, CatalogError> {
        match method {
            "subagent.spawn" => {
                let request: SubagentSpawnRequest = parse(params)?;
                request.validate().map_err(ipc_to_catalog)?;
                self.control()?
                    .spawn_from_adapter(&request)
                    .map_err(ipc_to_catalog)
            }
            "subagent.cancel" => {
                let request: SubagentCancelRequest = parse(params)?;
                request.validate().map_err(ipc_to_catalog)?;
                self.control()?
                    .subagent_cancel(&request)
                    .map_err(ipc_to_catalog)
            }
            "subagent.status" => {
                if params.get("child_session_id").is_some() {
                    let child_id = params
                        .get("child_session_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            CatalogError::invalid_params("child_session_id 必须是字符串")
                        })?;
                    let child = SessionId::new(child_id.to_owned()).map_err(|_| {
                        CatalogError::invalid_params("child_session_id 必须是 26 位 ULID")
                    })?;
                    self.control()?.status_of(&child).map_err(ipc_to_catalog)
                } else {
                    let request: SubagentListRequest = parse(params)?;
                    request.validate().map_err(ipc_to_catalog)?;
                    self.control()?
                        .subagent_list(&request)
                        .map_err(ipc_to_catalog)
                }
            }
            "subagent.result.get" => {
                let request: SubagentResultGetRequest = parse(params)?;
                request.validate().map_err(ipc_to_catalog)?;
                self.control()?
                    .subagent_result_get(&request)
                    .map_err(ipc_to_catalog)
            }
            other => Err(CatalogError::method_not_found(format!(
                "未知子 Agent 方法：{other}"
            ))),
        }
    }
}

fn parse<T: for<'de> serde::Deserialize<'de>>(params: Value) -> Result<T, CatalogError> {
    serde_json::from_value(params)
        .map_err(|error| CatalogError::invalid_params(format!("参数解析失败：{error}")))
}

fn ipc_to_catalog(error: IpcError) -> CatalogError {
    let aether_code = match error.code {
        IpcErrorCode::InvalidValue
        | IpcErrorCode::InvalidEnum
        | IpcErrorCode::InvalidType
        | IpcErrorCode::InvalidFormat
        | IpcErrorCode::MissingField
        | IpcErrorCode::UnknownField
        | IpcErrorCode::TooLarge
        | IpcErrorCode::OutOfRange
        | IpcErrorCode::SessionNotFound => "invalid_value",
        _ => "core_not_ready",
    };
    let code = if aether_code == "invalid_value" {
        -32602
    } else {
        -32603
    };
    CatalogError {
        code,
        aether_code,
        message: error.message,
    }
}
