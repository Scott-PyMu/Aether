//! `aetherd` HTTP/WS 传输（M10-03；ADR-033 v1.1 D-033-1～D-033-6）。
//!
//! 路由：
//! - `POST /v1/rpc`：JSON-RPC 2.0 单请求（`params` = 内层 DTO，与 stdio 完全一致）；
//! - `POST /v1/rpc/cancel`：取消在途请求（未知 id → 204）；
//! - `GET /v1/meta`：只列已实现命令（不含内部路径/密钥）；
//! - `GET /v1/health`：`health` 命令 + `api` 状态；
//! - `GET /v1/events`：原生 WebSocket（首帧 auth + subscribe，`last_seq` 补读）；
//! - `POST /v1/admin/rotate-token`：轮换 token（需当前 token）。
//!
//! 安全口径：token 不走 URL/query；请求日志不记录 Authorization/body；CORS 默认拒绝；
//! body ≤2MiB；并发 ≤64；速率 100 req/s（突发 200）；业务错误不映射 4xx/5xx。

// `guard`/`authorize` 的 Err 为完整 HTTP 响应（构造一次、只走失败路径）；
// 与 aether-rpc `result_large_err` 同口径，不修改所有调用点签名。
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_app::ipc::dispatch::daemon_commands;
use aether_app::ipc::IpcState;
use aether_app::ipc::{is_shell_local, IpcErrorCode, COMMANDS};
use aether_core::{EventEnvelope, SessionId};
use aether_rpc::{code, Response, RpcFailure};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::{oneshot, OwnedSemaphorePermit};

use super::{
    command_timeout_ms, dispatch_command, ApiShared, BUSINESS_REQUEST_TIMEOUT,
    HTTP_TIMEOUT_SLACK_MS, MAX_BODY_BYTES,
};

/// 路由共享状态（`IpcState` + API 共享状态）。
#[derive(Clone)]
struct AppState {
    ipc: IpcState,
    shared: Arc<ApiShared>,
}

/// 启动 axum 服务（graceful shutdown 由 `oneshot` 触发）。
pub async fn serve(
    listener: tokio::net::TcpListener,
    ipc: IpcState,
    shared: Arc<ApiShared>,
    shutdown: oneshot::Receiver<()>,
) {
    let app = router(ipc, Arc::clone(&shared));
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown.await;
    });
    if let Err(error) = server.await {
        tracing::error!(error = %error, "HTTP/WS API 监听异常结束");
    }
    shared.remove_discovery();
}

fn router(ipc: IpcState, shared: Arc<ApiShared>) -> Router {
    let state = AppState { ipc, shared };
    Router::new()
        .route("/v1/rpc", post(rpc))
        .route("/v1/rpc/cancel", post(cancel))
        .route("/v1/meta", get(meta))
        .route("/v1/health", get(health))
        .route("/v1/events", get(events))
        .route("/v1/admin/rotate-token", post(rotate_token))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            cors_and_options,
        ))
        .with_state(state)
}

/// CORS 默认拒绝；仅显式 `AETHERD_HTTP_CORS_ORIGIN`（测试）返回单一来源。
async fn cors_and_options(
    State(app): State<AppState>,
    request: Request,
    next: Next,
) -> HttpResponse {
    if request.method() == Method::OPTIONS {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    if let Some(origin) = app.shared.cors_origin() {
        if let Ok(value) = header::HeaderValue::from_str(origin) {
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
    }
    response
}

async fn fallback() -> HttpResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"code": 404, "message": "未知路径"}})),
    )
        .into_response()
}

/// 并发/速率闸门（429）。
fn guard(app: &AppState) -> Result<OwnedSemaphorePermit, HttpResponse> {
    if !app.shared.allow_rate() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1")],
            Json(json!({"error": {"code": 429, "message": "速率限制"}})),
        )
            .into_response());
    }
    app.shared.try_acquire().ok_or_else(|| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": {"code": 429, "message": "并发连接上限"}})),
        )
            .into_response()
    })
}

/// `Authorization: Bearer <token>` 校验（401；token 不写日志/响应）。
fn authorize(app: &AppState, headers: &HeaderMap) -> Result<(), HttpResponse> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(token) = token else {
        return Err(unauthorized());
    };
    if !app.shared.token_matches(token) {
        return Err(unauthorized());
    }
    Ok(())
}

fn unauthorized() -> HttpResponse {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": {"code": 401, "message": "未授权（缺少或无效 token）"}})),
    )
        .into_response()
}

fn rpc_value(id: Value, failure: RpcFailure) -> Value {
    Response::error(id, failure.into_error_object()).to_value()
}

fn rpc_response(status: StatusCode, id: Value, failure: RpcFailure) -> HttpResponse {
    (status, Json(rpc_value(id, failure))).into_response()
}

fn id_key(id: &Value) -> String {
    let Ok(text) = serde_json::to_string(id) else {
        return String::new();
    };
    text
}

async fn rpc(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> HttpResponse {
    let permit = match guard(&app) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    if let Err(response) = authorize(&app, &headers) {
        return response;
    }

    let started = Instant::now();
    let raw = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => {
            return rpc_response(
                StatusCode::BAD_REQUEST,
                Value::Null,
                RpcFailure::transport(code::PARSE_ERROR, "invalid_json", "请求体不是合法 JSON"),
            )
        }
    };
    let request = match aether_rpc::Request::from_value(&raw) {
        Ok(request) => request,
        Err((id, reason)) => {
            return rpc_response(
                StatusCode::BAD_REQUEST,
                id.map(|id| id.to_value()).unwrap_or(Value::Null),
                RpcFailure::transport(code::INVALID_REQUEST, "invalid_json", reason),
            )
        }
    };
    let id = request.id.to_value();
    let key = id_key(&id);

    if is_shell_local(&request.method) {
        return rpc_response(
            StatusCode::OK,
            id,
            RpcFailure::method_not_found(&request.method).with_reason("shell_local"),
        );
    }
    if !COMMANDS.contains(&request.method.as_str()) {
        return rpc_response(
            StatusCode::OK,
            id,
            RpcFailure::method_not_found(&request.method),
        );
    }

    let entry = app.shared.begin_request(&key);
    let ipc = app.ipc.clone();
    let shared = Arc::clone(&app.shared);
    let method = request.method.clone();
    let params = request.params.clone();
    let timeout_ms = command_timeout_ms(&method).saturating_add(HTTP_TIMEOUT_SLACK_MS);
    let task =
        tokio::task::spawn_blocking(move || dispatch_command(&ipc, &shared, &method, params));
    let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), task).await;
    let canceled = entry.is_canceled();
    app.shared.finish_request(&key);
    drop(permit);

    let (status, failure) = match outcome {
        Ok(Ok(Ok(value))) => {
            if canceled {
                (
                    StatusCode::OK,
                    Some(RpcFailure::internal(
                        "请求已取消（仅取消在途响应；已落库状态不回滚）",
                    )),
                )
            } else {
                tracing::info!(
                    method = %request.method,
                    elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "HTTP RPC 完成"
                );
                return (StatusCode::OK, Json(Response::result(id, value).to_value()))
                    .into_response();
            }
        }
        Ok(Ok(Err(error))) => {
            let status = if error.code == IpcErrorCode::CoreNotReady {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            };
            (status, Some(super::super::serve::map_ipc_error(error)))
        }
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(RpcFailure::internal(format!("命令执行任务失败：{error}"))),
        ),
        Err(_) => (
            StatusCode::OK,
            Some(
                RpcFailure::transport(
                    code::INTERNAL_ERROR,
                    "internal",
                    format!("请求超时（{} ms）", timeout_ms),
                )
                .with_business(BUSINESS_REQUEST_TIMEOUT),
            ),
        ),
    };
    match failure {
        Some(failure) => rpc_response(status, id, failure),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn cancel(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> HttpResponse {
    let _permit = match guard(&app) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    if let Err(response) = authorize(&app, &headers) {
        return response;
    }
    let value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"code": 400, "message": "请求体不是合法 JSON"}})),
            )
                .into_response()
        }
    };
    let Some(request_id) = value.get("request_id") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"code": 400, "message": "缺少 request_id"}})),
        )
            .into_response();
    };
    let key = id_key(request_id);
    let found = app.shared.cancel_request(&key);
    tracing::info!(found, "HTTP RPC 取消请求（仅取消在途响应）");
    StatusCode::NO_CONTENT.into_response()
}

async fn meta(State(app): State<AppState>, headers: HeaderMap) -> HttpResponse {
    let _permit = match guard(&app) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    if let Err(response) = authorize(&app, &headers) {
        return response;
    }
    let commands: Vec<Value> = daemon_commands()
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "timeout_ms": command_timeout_ms(name),
            })
        })
        .collect();
    Json(json!({
        "protocol": aether_rpc::PROTOCOL_VERSION,
        "app_version": aether_app::version(),
        "daemon_version": aether_app::version(),
        "core_version": aether_app::core_version(),
        "commands": commands,
        "capabilities": aether_rpc::DAEMON_CAPABILITIES,
    }))
    .into_response()
}

async fn health(State(app): State<AppState>, headers: HeaderMap) -> HttpResponse {
    let _permit = match guard(&app) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    if let Err(response) = authorize(&app, &headers) {
        return response;
    }
    let ipc = app.ipc.clone();
    let shared = Arc::clone(&app.shared);
    let outcome =
        tokio::task::spawn_blocking(move || dispatch_command(&ipc, &shared, "health", Value::Null))
            .await;
    match outcome {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(Err(error)) => {
            let status = if error.code == IpcErrorCode::CoreNotReady {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            let failure = super::super::serve::map_ipc_error(error);
            rpc_response(status, Value::Null, failure)
        }
        Err(error) => rpc_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            Value::Null,
            RpcFailure::internal(format!("health 任务失败：{error}")),
        ),
    }
}

async fn rotate_token(State(app): State<AppState>, headers: HeaderMap) -> HttpResponse {
    let _permit = match guard(&app) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    if let Err(response) = authorize(&app, &headers) {
        return response;
    }
    match app.shared.rotate_token() {
        Ok(token) => {
            tracing::info!("API token 已轮换（旧 token 立即失效；token 不入日志）");
            Json(json!({"token": token})).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"code": 500, "message": error}})),
        )
            .into_response(),
    }
}

async fn events(ws: WebSocketUpgrade, State(app): State<AppState>) -> HttpResponse {
    let permit = match guard(&app) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    ws.on_upgrade(move |socket| handle_events(socket, app, permit))
}

async fn handle_events(mut socket: WebSocket, app: AppState, _permit: OwnedSemaphorePermit) {
    // 首帧 auth（5s；失败/超时 close 4401；token 不进 URL）。
    let token = match tokio::time::timeout(super::AUTH_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.get("type").and_then(Value::as_str) == Some("auth") => value
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        },
        _ => None,
    };
    let Some(token) = token else {
        close(&mut socket, 4401, "auth required").await;
        return;
    };
    if !app.shared.token_matches(&token) {
        close(&mut socket, 4401, "invalid token").await;
        return;
    }

    // subscribe（10s 内；session_id 可选、last_seq 可选）。
    let subscribe = match tokio::time::timeout(Duration::from_secs(10), socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<Value>(&text).ok(),
        _ => None,
    };
    let Some(subscribe) = subscribe else {
        close(&mut socket, 4400, "subscribe required").await;
        return;
    };
    if subscribe.get("type").and_then(Value::as_str) != Some("subscribe") {
        close(&mut socket, 4400, "subscribe required").await;
        return;
    }
    let session = match subscribe.get("session_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if !is_ulid(text) {
                close(&mut socket, 4400, "invalid session_id").await;
                return;
            }
            match SessionId::new(text.clone()) {
                Ok(session) => Some(session),
                Err(_) => {
                    close(&mut socket, 4400, "invalid session_id").await;
                    return;
                }
            }
        }
        Some(_) => {
            close(&mut socket, 4400, "invalid session_id").await;
            return;
        }
    };
    let last_seq = subscribe.get("last_seq").and_then(Value::as_u64);

    let Some(pipeline) = app.shared.pipeline() else {
        let _ = send_json(
            &mut socket,
            &json!({"type": "error", "code": "core_not_ready"}),
        )
        .await;
        close(&mut socket, 1013, "core_not_ready").await;
        return;
    };
    let mut receiver = pipeline.subscribe();
    let mut shutdown = app.shared.shutdown_receiver();
    let mut last_sent = last_seq.unwrap_or(0);
    let mut bytes_sent = 0_usize;

    // 先订阅再补读：补读期间到达的实时事件由 receiver 缓冲，随后按 seq 去重。
    if let (Some(session), Some(last_seq)) = (&session, last_seq) {
        match pipeline.readback(session, last_seq).await {
            Ok(frame) => {
                for event in frame.events {
                    if !send_event(&mut socket, &event, &mut bytes_sent).await {
                        return;
                    }
                    last_sent = last_sent.max(event.seq);
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "WS 补读失败（发送 gap 并继续实时流）");
                if !send_gap(&mut socket, session, last_sent).await {
                    return;
                }
            }
        }
    }

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                let closed = changed.is_err();
                let shutting_down = *shutdown.borrow();
                if closed || shutting_down {
                    close(&mut socket, 1012, "daemon shutdown").await;
                    return;
                }
            }
            received = receiver.recv() => {
                match received {
                    Ok(event) => {
                        if let Some(session) = &session {
                            if &event.session_id != session {
                                continue;
                            }
                        }
                        if event.seq <= last_sent {
                            continue;
                        }
                        if !send_event(&mut socket, &event, &mut bytes_sent).await {
                            return;
                        }
                        last_sent = event.seq;
                        if bytes_sent > super::WS_BYTES_LIMIT {
                            close(&mut socket, 1013, "slow consumer").await;
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        if skipped as usize > super::WS_QUEUE_LIMIT {
                            close(&mut socket, 1013, "lagged").await;
                            return;
                        }
                        let Some(session) = &session else {
                            if !send_gap_without_session(&mut socket, last_sent).await {
                                return;
                            }
                            continue;
                        };
                        if !send_gap(&mut socket, session, last_sent).await {
                            return;
                        }
                        match pipeline.readback(session, last_sent).await {
                            Ok(frame) => {
                                for event in frame.events {
                                    if !send_event(&mut socket, &event, &mut bytes_sent).await {
                                        return;
                                    }
                                    last_sent = last_sent.max(event.seq);
                                }
                            }
                            Err(error) => {
                                tracing::warn!(error = %error, "WS 补读失败（保持 gap 标记）");
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        close(&mut socket, 1012, "event stream closed").await;
                        return;
                    }
                }
            }
        }
    }
}

async fn send_event(socket: &mut WebSocket, event: &EventEnvelope, bytes: &mut usize) -> bool {
    let value = match serde_json::to_value(event) {
        Ok(value) => json!({"type": "event", "envelope": value}),
        Err(error) => {
            tracing::error!(error = %error, "事件序列化失败（跳过该事件）");
            return true;
        }
    };
    match send_json(socket, &value).await {
        Ok(size) => {
            *bytes = bytes.saturating_add(size);
            true
        }
        Err(()) => false,
    }
}

async fn send_gap(socket: &mut WebSocket, session: &SessionId, from_seq: u64) -> bool {
    send_json(
        socket,
        &json!({
            "type": "gap",
            "session_id": session.as_str(),
            "from_seq": from_seq,
        }),
    )
    .await
    .is_ok()
}

async fn send_gap_without_session(socket: &mut WebSocket, from_seq: u64) -> bool {
    send_json(socket, &json!({"type": "gap", "from_seq": from_seq}))
        .await
        .is_ok()
}

async fn send_json(socket: &mut WebSocket, value: &Value) -> Result<usize, ()> {
    let text = serde_json::to_string(value).map_err(|_| ())?;
    socket
        .send(Message::Text(text.clone().into()))
        .await
        .map_err(|_| ())?;
    Ok(text.len())
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_owned().into(),
        })))
        .await;
}

/// ULID 形状（26 字符 Crockford Base32；与 `client_msg_id`/`session_id` 校验一致）。
fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value
            .chars()
            .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_key_serializes_numbers_and_strings_distinctly() {
        assert_eq!(id_key(&json!(1)), "1");
        assert_eq!(id_key(&json!("1")), "\"1\"");
    }

    #[test]
    fn cancellation_flag_is_observable() {
        let shared = ApiShared::disabled();
        let entry = shared.begin_request("1");
        assert!(!entry.is_canceled());
        assert!(shared.cancel_request("1"));
        assert!(entry.is_canceled());
        // 取消后 finish 不得再生效（map 已移除）。
        shared.finish_request("1");
        assert!(!shared.cancel_request("1"));
    }
}
