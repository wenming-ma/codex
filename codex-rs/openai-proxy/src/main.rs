use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::Event;
use axum::response::sse::Sse;
use axum::routing::{get, post};
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_core::auth::AuthManager;
use codex_core::config::Config;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol::Submission;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    thread_manager: Arc<ThreadManager>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionRequest {
    model: String,
    #[serde(default)]
    messages: Option<Vec<ChatMessage>>,
    #[serde(default)]
    input: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    conversation_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionResponse {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<ChatChoice>,
    usage: Usage,
}

#[derive(Debug, Serialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponseItem>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    conversation_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponsesResponse {
    id: String,
    object: String,
    created_at: u64,
    model: String,
    status: String,
    output: Vec<ResponseItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponseEventPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<ResponseSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<ResponseItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponseSummary {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatChoice {
    index: usize,
    message: ChatMessageResponse,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct ChatMessageResponse {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Serialize, Clone)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: ToolFunction,
}

#[derive(Debug, Serialize, Clone)]
struct ToolFunction {
    name: String,
    arguments: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let config = Config::load_with_cli_overrides(vec![])
        .await
        .context("load config")?;

    let auth_manager = Arc::new(AuthManager::new(
        config.codex_home.clone(),
        false,
        config.cli_auth_credentials_store_mode,
    ));

    let thread_manager = Arc::new(ThreadManager::new(
        config.codex_home.clone(),
        auth_manager,
        SessionSource::Exec,
    ));

    let state = AppState { thread_manager };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let router = Router::new()
        // With /v1 prefix
        .route("/v1/models", get(handle_models))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/responses", post(handle_responses))
        // Without /v1 prefix (for Cursor compatibility)
        .route("/models", get(handle_models))
        .route("/chat/completions", post(handle_chat_completions))
        .route("/responses", post(handle_responses))
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = env::var("CODEX_OPENAI_PROXY_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:11435".to_string())
        .parse()
        .context("parse CODEX_OPENAI_PROXY_ADDR")?;

    info!("codex-openai-proxy (Codex adapter) listening on http://{addr}");
    axum::serve(
        tokio::net::TcpListener::bind(addr)
            .await
            .context("bind listener")?,
        router,
    )
    .await
    .context("run server")?;

    Ok(())
}

async fn handle_chat_completions(
    State(state): State<AppState>,
    body: axum::Json<ChatCompletionRequest>,
) -> Response {
    if body.stream {
        return handle_stream(state, body.0).await;
    }
    handle_once(state, body.0).await
}

async fn handle_responses(
    State(state): State<AppState>,
    body: axum::Json<ResponsesRequest>,
) -> Response {
    if body.stream {
        return handle_responses_stream(state, body.0).await;
    }
    handle_responses_once(state, body.0).await
}

async fn handle_models() -> Response {
    let models = serde_json::json!({
        "object": "list",
        "data": [
            {"id": "gpt-4.1", "object": "model", "owned_by": "codex"},
            {"id": "gpt-4.1-mini", "object": "model", "owned_by": "codex"},
            {"id": "gpt-4o", "object": "model", "owned_by": "codex"},
            {"id": "gpt-4o-mini", "object": "model", "owned_by": "codex"},
            {"id": "gpt-5-mini", "object": "model", "owned_by": "codex"},
            {"id": "o3-mini", "object": "model", "owned_by": "codex"},
            {"id": "o1-mini", "object": "model", "owned_by": "codex"},
            {"id": "o1-preview", "object": "model", "owned_by": "codex"},
        ]
    });
    json_response(StatusCode::OK, models.to_string())
}

async fn handle_once(state: AppState, body: ChatCompletionRequest) -> Response {
    let merged_text = match merged_text_from_request(&body) {
        Some(text) => text,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "no user content found".to_string(),
                "invalid_request_error",
            );
        }
    };

    // Store original model name for response
    let original_model = body.model.clone();

    let (thread, conv_id) = match get_or_create_thread(&state, &body.model, body.conversation_id)
        .await
    {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e, "internal_error"),
    };

    let submission_id = uuid::Uuid::new_v4().to_string();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let payload_text = merged_text.clone();
    let model = map_model(&body.model);
    let tool_calls = Arc::new(Mutex::new(Vec::<ToolCall>::new()));
    let tool_calls_for_task = tool_calls.clone();
    let model_for_task = model.clone();

    let handle = tokio::spawn(async move {
        let submission = Submission {
            id: submission_id.clone(),
            op: Op::UserTurn {
                items: vec![UserInput::Text {
                    text: payload_text.clone(),
                }],
                cwd,
                approval_policy: AskForApproval::Never,
                sandbox_policy: SandboxPolicy::ReadOnly,
                model: model_for_task.clone(),
                effort: None,
                summary: ReasoningSummary::None,
                final_output_json_schema: None,
            },
        };

        thread
            .submit_with_id(submission)
            .await
            .map_err(|e| format!("submit error: {e}"))?;

        let mut final_text = String::new();
        loop {
            let ev = thread
                .next_event()
                .await
                .map_err(|e| format!("event error: {e}"))?;
            if ev.id != submission_id {
                continue;
            }
            match ev.msg {
                EventMsg::AgentMessage(m) => {
                    final_text.push_str(&m.message);
                    final_text.push('\n');
                }
                EventMsg::AgentMessageDelta(d) => final_text.push_str(&d.delta),
                EventMsg::RawResponseItem(raw) => {
                    if let Some(tc) = map_tool_call(&raw.item) {
                        tool_calls_for_task.lock().await.push(tc);
                    }
                }
                EventMsg::TurnComplete(done) => {
                    if let Some(msg) = done.last_agent_message {
                        final_text = msg;
                    }
                    break;
                }
                EventMsg::Error(err) => return Err(format!("Codex error: {}", err.message)),
                EventMsg::Warning(warn) => {
                    info!("warning from Codex: {}", warn.message);
                }
                EventMsg::TurnAborted(abort) => {
                    return Err(format!("Turn aborted: {:?}", abort.reason));
                }
                _ => {}
            }
        }

        Ok(final_text)
    });

    let final_text = match handle.await {
        Ok(Ok(text)) => text.trim().to_string(),
        Ok(Err(e)) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, e, "internal_error");
        }
        Err(join_err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                join_err.to_string(),
                "internal_error",
            );
        }
    };
    let tool_calls_snapshot = {
        let guard = tool_calls.lock().await;
        guard.clone()
    };

    let resp = ChatCompletionResponse {
        id: format!("chatcmpl-codex-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: now_ts(),
        model: original_model,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessageResponse {
                role: "assistant".to_string(),
                content: final_text.clone(),
                tool_calls: if tool_calls_snapshot.is_empty() {
                    None
                } else {
                    Some(tool_calls_snapshot.clone())
                },
            },
            finish_reason: if !tool_calls_snapshot.is_empty() {
                "tool_calls".to_string()
            } else {
                "stop".to_string()
            },
        }],
        usage: Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
    };

    let body = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
    json_response(StatusCode::OK, body)
}

async fn handle_responses_once(state: AppState, body: ResponsesRequest) -> Response {
    let merged_text = match merge_responses_input(&body.input, body.instructions.as_deref()) {
        Some(text) => text,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "no user content found".to_string(),
                "invalid_request_error",
            );
        }
    };

    let (thread, conv_id) = match get_or_create_thread(&state, &body.model, body.conversation_id)
        .await
    {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e, "internal_error"),
    };

    let submission_id = uuid::Uuid::new_v4().to_string();
    let response_id = format!("resp-codex-{}", uuid::Uuid::new_v4());
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let payload_text = merged_text.clone();
    let model = map_model(&body.model);
    let response_model = model.clone();
    let output_items = Arc::new(Mutex::new(Vec::<ResponseItem>::new()));
    let output_items_for_task = output_items.clone();

    let handle = tokio::spawn(async move {
        let submission = Submission {
            id: submission_id.clone(),
            op: Op::UserTurn {
                items: vec![UserInput::Text {
                    text: payload_text.clone(),
                }],
                cwd,
                approval_policy: AskForApproval::Never,
                sandbox_policy: SandboxPolicy::ReadOnly,
                model: model.clone(),
                effort: None,
                summary: ReasoningSummary::None,
                final_output_json_schema: None,
            },
        };

        thread
            .submit_with_id(submission)
            .await
            .map_err(|e| format!("submit error: {e}"))?;

        let mut final_text = String::new();
        let mut text_seen = false;
        loop {
            let ev = thread
                .next_event()
                .await
                .map_err(|e| format!("event error: {e}"))?;
            if ev.id != submission_id {
                continue;
            }
            match ev.msg {
                EventMsg::AgentMessage(m) => {
                    final_text.push_str(&m.message);
                    final_text.push('\n');
                    text_seen = true;
                }
                EventMsg::AgentMessageDelta(d) => {
                    final_text.push_str(&d.delta);
                    text_seen = true;
                }
                EventMsg::RawResponseItem(raw) => {
                    output_items_for_task.lock().await.push(raw.item);
                }
                EventMsg::TurnComplete(done) => {
                    if let Some(msg) = done.last_agent_message {
                        final_text = msg;
                        text_seen = true;
                    }
                    break;
                }
                EventMsg::Error(err) => return Err(format!("Codex error: {}", err.message)),
                EventMsg::Warning(warn) => {
                    info!("warning from Codex: {}", warn.message);
                }
                EventMsg::TurnAborted(abort) => {
                    return Err(format!("Turn aborted: {:?}", abort.reason));
                }
                _ => {}
            }
        }

        if text_seen && !final_text.trim().is_empty() {
            output_items_for_task
                .lock()
                .await
                .push(ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: final_text.trim().to_string(),
                    }],
                });
        }

        Ok(())
    });

    match handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, e, "internal_error");
        }
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "internal_error",
            );
        }
    }

    let output_items_snapshot = {
        let guard = output_items.lock().await;
        guard.clone()
    };

    let resp = ResponsesResponse {
        id: response_id,
        object: "response".to_string(),
        created_at: now_ts(),
        model: response_model,
        status: "completed".to_string(),
        output: output_items_snapshot,
        conversation_id: Some(conv_id.to_string()),
    };

    let body = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
    json_response(StatusCode::OK, body)
}

async fn handle_stream(state: AppState, body: ChatCompletionRequest) -> Response {
    let merged_text = match merged_text_from_request(&body) {
        Some(text) => text,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "no user content found".to_string(),
                "invalid_request_error",
            );
        }
    };

    let (thread, conv_id) = match get_or_create_thread(&state, &body.model, body.conversation_id)
        .await
    {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e, "internal_error"),
    };

    let submission_id = uuid::Uuid::new_v4().to_string();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let payload_text = merged_text.clone();
    let model = map_model(&body.model);
    let conv_id_clone = conv_id.clone();
    let tool_seen = Arc::new(AtomicBool::new(false));
    let tool_seen_for_task = tool_seen.clone();

    let (tx, rx) = mpsc::channel(16);

    tokio::spawn(async move {
        let submission = Submission {
            id: submission_id.clone(),
            op: Op::UserTurn {
                items: vec![UserInput::Text {
                    text: payload_text.clone(),
                }],
                cwd,
                approval_policy: AskForApproval::Never,
                sandbox_policy: SandboxPolicy::ReadOnly,
                model: model.clone(),
                effort: None,
                summary: ReasoningSummary::None,
                final_output_json_schema: None,
            },
        };

        if let Err(e) = thread.submit_with_id(submission).await {
            let _ = tx.send(Err(format!("submit error: {e}"))).await;
            return;
        }

        loop {
            let ev = match thread.next_event().await {
                Ok(ev) => ev,
                Err(e) => {
                    let _ = tx.send(Err(format!("event error: {e}"))).await;
                    return;
                }
            };
            if ev.id != submission_id {
                continue;
            }
            match ev.msg {
                EventMsg::AgentMessage(m) => {
                    let chunk =
                        stream_chunk(Some(&m.message), None, false, Some(conv_id_clone.clone()));
                    let _ = tx.send(Ok(chunk)).await;
                }
                EventMsg::AgentMessageDelta(d) => {
                    let chunk =
                        stream_chunk(Some(&d.delta), None, false, Some(conv_id_clone.clone()));
                    let _ = tx.send(Ok(chunk)).await;
                }
                EventMsg::RawResponseItem(raw) => {
                    if let Some(tc) = map_tool_call(&raw.item) {
                        tool_seen_for_task.store(true, Ordering::Relaxed);
                        let chunk =
                            stream_chunk(None, Some(tc), false, Some(conv_id_clone.clone()));
                        let _ = tx.send(Ok(chunk)).await;
                    }
                }
                EventMsg::TurnComplete(done) => {
                    if let Some(msg) = done.last_agent_message {
                        let chunk =
                            stream_chunk(Some(&msg), None, false, Some(conv_id_clone.clone()));
                        let _ = tx.send(Ok(chunk)).await;
                    }
                    let finish_reason = if tool_seen_for_task.load(Ordering::Relaxed) {
                        "tool_calls"
                    } else {
                        "stop"
                    };
                    let chunk = stream_chunk_with_finish(
                        None,
                        None,
                        finish_reason,
                        Some(conv_id_clone.clone()),
                    );
                    let _ = tx.send(Ok(chunk)).await;
                    let _ = tx
                        .send(Ok(serde_json::Value::String("[DONE]".to_string())))
                        .await;
                    break;
                }
                EventMsg::Error(err) => {
                    let _ = tx.send(Err(format!("Codex error: {}", err.message))).await;
                    break;
                }
                EventMsg::Warning(warn) => {
                    info!("warning from Codex: {}", warn.message);
                }
                EventMsg::TurnAborted(abort) => {
                    let _ = tx
                        .send(Err(format!("Turn aborted: {:?}", abort.reason)))
                        .await;
                    break;
                }
                _ => {}
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|msg| match msg {
        Ok(json_val) => match json_val {
            serde_json::Value::String(s) if s == "[DONE]" => {
                Ok::<Event, std::convert::Infallible>(Event::default().data(s))
            }
            other => {
                let data = serde_json::to_string(&other).unwrap_or_else(|_| "{}".to_string());
                Ok::<Event, std::convert::Infallible>(Event::default().data(data))
            }
        },
        Err(err) => Ok(Event::default().data(
            serde_json::to_string(&serde_json::json!({
                "error": err,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
        )),
    });

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

async fn handle_responses_stream(state: AppState, body: ResponsesRequest) -> Response {
    let merged_text = match merge_responses_input(&body.input, body.instructions.as_deref()) {
        Some(text) => text,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "no user content found".to_string(),
                "invalid_request_error",
            );
        }
    };

    let (thread, conv_id) = match get_or_create_thread(&state, &body.model, body.conversation_id)
        .await
    {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e, "internal_error"),
    };

    let submission_id = uuid::Uuid::new_v4().to_string();
    let response_id = format!("resp-codex-{}", uuid::Uuid::new_v4());
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let payload_text = merged_text.clone();
    let model = map_model(&body.model);
    let conv_id_clone = conv_id.clone();
    let text_seen = Arc::new(AtomicBool::new(false));
    let text_seen_for_task = text_seen.clone();

    let (tx, rx) = mpsc::channel(16);

    let created_event = response_created_event(&response_id, Some(conv_id_clone.clone()));
    let _ = tx.send(Ok(created_event)).await;

    tokio::spawn(async move {
        let submission = Submission {
            id: submission_id.clone(),
            op: Op::UserTurn {
                items: vec![UserInput::Text {
                    text: payload_text.clone(),
                }],
                cwd,
                approval_policy: AskForApproval::Never,
                sandbox_policy: SandboxPolicy::ReadOnly,
                model: model.clone(),
                effort: None,
                summary: ReasoningSummary::None,
                final_output_json_schema: None,
            },
        };

        if let Err(e) = thread.submit_with_id(submission).await {
            let _ = tx.send(Err(format!("submit error: {e}"))).await;
            return;
        }

        loop {
            let ev = match thread.next_event().await {
                Ok(ev) => ev,
                Err(e) => {
                    let _ = tx.send(Err(format!("event error: {e}"))).await;
                    return;
                }
            };
            if ev.id != submission_id {
                continue;
            }
            match ev.msg {
                EventMsg::AgentMessage(m) => {
                    text_seen_for_task.store(true, Ordering::Relaxed);
                    let chunk = response_output_text_delta_event(&m.message);
                    let _ = tx.send(Ok(chunk)).await;
                }
                EventMsg::AgentMessageDelta(d) => {
                    text_seen_for_task.store(true, Ordering::Relaxed);
                    let chunk = response_output_text_delta_event(&d.delta);
                    let _ = tx.send(Ok(chunk)).await;
                }
                EventMsg::RawResponseItem(raw) => {
                    let chunk = response_output_item_done_event(raw.item);
                    let _ = tx.send(Ok(chunk)).await;
                }
                EventMsg::TurnComplete(done) => {
                    if let Some(msg) = done.last_agent_message
                        && !text_seen_for_task.load(Ordering::Relaxed)
                    {
                        text_seen_for_task.store(true, Ordering::Relaxed);
                        let chunk = response_output_text_delta_event(&msg);
                        let _ = tx.send(Ok(chunk)).await;
                    }
                    let chunk = response_completed_event(&response_id, Some(conv_id_clone.clone()));
                    let _ = tx.send(Ok(chunk)).await;
                    let _ = tx
                        .send(Ok(serde_json::Value::String("[DONE]".to_string())))
                        .await;
                    break;
                }
                EventMsg::Error(err) => {
                    let _ = tx.send(Err(format!("Codex error: {}", err.message))).await;
                    break;
                }
                EventMsg::Warning(warn) => {
                    info!("warning from Codex: {}", warn.message);
                }
                EventMsg::TurnAborted(abort) => {
                    let _ = tx
                        .send(Err(format!("Turn aborted: {:?}", abort.reason)))
                        .await;
                    break;
                }
                _ => {}
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|msg| match msg {
        Ok(json_val) => {
            let data = serde_json::to_string(&json_val).unwrap_or_else(|_| "{}".to_string());
            Ok::<Event, std::convert::Infallible>(Event::default().data(data))
        }
        Err(err) => Ok(Event::default().data(
            serde_json::to_string(&serde_json::json!({
                "error": err,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
        )),
    });

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn stream_chunk(
    content: Option<&str>,
    tool_call: Option<ToolCall>,
    done: bool,
    conversation_id: Option<String>,
) -> serde_json::Value {
    let mut delta = serde_json::Map::new();
    if let Some(text) = content {
        delta.insert(
            "content".to_string(),
            serde_json::Value::String(text.to_string()),
        );
    }
    if let Some(tc) = tool_call {
        delta.insert(
            "tool_calls".to_string(),
            serde_json::json!([{
                "index": 0,
                "id": tc.id,
                "type": tc.kind,
                "function": {
                    "name": tc.function.name,
                    "arguments": tc.function.arguments,
                }
            }]),
        );
    }

    let finish_json = if done {
        serde_json::Value::String(if delta.contains_key("tool_calls") {
            "tool_calls".to_string()
        } else {
            "stop".to_string()
        })
    } else {
        serde_json::Value::Null
    };

    serde_json::json!({
        "id": format!("chatcmpl-codex-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion.chunk",
        "created": now_ts(),
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_json,
        }],
        "conversation_id": conversation_id,
    })
}

fn response_created_event(response_id: &str, conversation_id: Option<String>) -> serde_json::Value {
    serde_json::to_value(ResponseEventPayload {
        kind: "response.created".to_string(),
        response: Some(ResponseSummary {
            id: response_id.to_string(),
            conversation_id,
        }),
        item: None,
        delta: None,
    })
    .unwrap_or_else(|_| serde_json::json!({}))
}

fn response_output_text_delta_event(delta: &str) -> serde_json::Value {
    serde_json::to_value(ResponseEventPayload {
        kind: "response.output_text.delta".to_string(),
        response: None,
        item: None,
        delta: Some(delta.to_string()),
    })
    .unwrap_or_else(|_| serde_json::json!({}))
}

fn response_output_item_done_event(item: ResponseItem) -> serde_json::Value {
    serde_json::to_value(ResponseEventPayload {
        kind: "response.output_item.done".to_string(),
        response: None,
        item: Some(item),
        delta: None,
    })
    .unwrap_or_else(|_| serde_json::json!({}))
}

fn response_completed_event(
    response_id: &str,
    conversation_id: Option<String>,
) -> serde_json::Value {
    serde_json::to_value(ResponseEventPayload {
        kind: "response.completed".to_string(),
        response: Some(ResponseSummary {
            id: response_id.to_string(),
            conversation_id,
        }),
        item: None,
        delta: None,
    })
    .unwrap_or_else(|_| serde_json::json!({}))
}

async fn get_or_create_thread(
    state: &AppState,
    model: &str,
    conversation_id: Option<String>,
) -> Result<(Arc<CodexThread>, String), String> {
    let overrides = vec![
        ("model".to_string(), toml::Value::String(map_model(model))),
        (
            "approval_policy".to_string(),
            toml::Value::String("never".to_string()),
        ),
        (
            "sandbox_mode".to_string(),
            toml::Value::String("read-only".to_string()),
        ),
    ];

    let config = Config::load_with_cli_overrides(overrides)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(cid) = conversation_id {
        let tid = codex_protocol::ThreadId::from_string(&cid)
            .map_err(|e| format!("invalid conversation_id: {e}"))?;
        let thread = state
            .thread_manager
            .get_thread(tid)
            .await
            .map_err(|e| format!("thread not found: {e}"))?;
        return Ok((thread, cid));
    }

    let new_thread = state
        .thread_manager
        .start_thread(config)
        .await
        .map_err(|e| e.to_string())?;
    Ok((new_thread.thread, new_thread.thread_id.to_string()))
}

fn stream_chunk_with_finish(
    content: Option<&str>,
    tool_call: Option<ToolCall>,
    finish_reason: &str,
    conversation_id: Option<String>,
) -> serde_json::Value {
    let mut delta = serde_json::Map::new();
    if let Some(text) = content {
        delta.insert(
            "content".to_string(),
            serde_json::Value::String(text.to_string()),
        );
    }
    if let Some(tc) = tool_call {
        delta.insert(
            "tool_calls".to_string(),
            serde_json::json!([{
                "index": 0,
                "id": tc.id,
                "type": tc.kind,
                "function": {
                    "name": tc.function.name,
                    "arguments": tc.function.arguments,
                }
            }]),
        );
    }

    serde_json::json!({
        "id": format!("chatcmpl-codex-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion.chunk",
        "created": now_ts(),
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
        "conversation_id": conversation_id,
    })
}

fn map_tool_call(item: &ResponseItem) -> Option<ToolCall> {
    match item {
        ResponseItem::FunctionCall {
            call_id,
            name,
            arguments,
            ..
        } => Some(ToolCall {
            id: call_id.clone(),
            kind: "function".to_string(),
            function: ToolFunction {
                name: name.clone(),
                arguments: arguments.clone(),
            },
        }),
        ResponseItem::CustomToolCall {
            call_id,
            name,
            input,
            ..
        } => Some(ToolCall {
            id: call_id.clone(),
            kind: "function".to_string(),
            function: ToolFunction {
                name: name.clone(),
                arguments: input.clone(),
            },
        }),
        _ => None,
    }
}

fn map_model(model: &str) -> String {
    let normalized = model.to_lowercase();
    let aliases = [
        ("gpt-4.1", "gpt-4.1"),
        ("gpt-4.1-mini", "gpt-4.1-mini"),
        ("gpt-4o", "gpt-4o"),
        ("gpt-4o-mini", "gpt-4o-mini"),
        ("o3-mini", "o3-mini"),
        ("o1-mini", "o1-mini"),
        ("o1-preview", "o1-preview"),
    ];

    for (k, v) in aliases {
        if normalized == k {
            return v.to_string();
        }
    }

    model.to_string()
}

fn merge_messages(msgs: &[ChatMessage]) -> Option<String> {
    let mut parts = Vec::new();
    for m in msgs {
        let content = match &m.content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.get("text").or_else(|| v.get("content")))
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if content.trim().is_empty() {
            continue;
        }
        parts.push(format!("{}: {}", m.role, content));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn merge_responses_input_values(
    input: &[serde_json::Value],
    instructions: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(instr) = instructions
        && !instr.trim().is_empty()
    {
        parts.push(format!("system: {}", instr.trim()));
    }

    for item in input {
        let Some(obj) = item.as_object() else {
            continue;
        };

        let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("user");

        let content_val = obj
            .get("content")
            .or_else(|| obj.get("text"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let content_text = match content_val {
            serde_json::Value::String(s) => s,
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.get("text").or_else(|| v.get("content")))
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };

        if !content_text.trim().is_empty() {
            parts.push(format!("{role}: {}", content_text.trim()));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn merge_responses_input(input: &[ResponseItem], instructions: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(instr) = instructions
        && !instr.trim().is_empty()
    {
        parts.push(format!("system: {}", instr.trim()));
    }

    for item in input {
        if let ResponseItem::Message { role, content, .. } = item {
            let text = content
                .iter()
                .filter_map(|c| match c {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                parts.push(format!("{role}: {}", text.trim()));
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn merged_text_from_request(body: &ChatCompletionRequest) -> Option<String> {
    if let Some(msgs) = &body.messages
        && let Some(text) = merge_messages(msgs)
    {
        return Some(text);
    }

    if let Some(items) = &body.input {
        return merge_responses_input_values(items, body.instructions.as_deref());
    }

    None
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn error_response(status: StatusCode, msg: String, kind: &str) -> Response {
    json_response(
        status,
        serde_json::json!({
            "error": {
                "message": msg,
                "type": kind,
            }
        })
        .to_string(),
    )
}

fn json_response(status: StatusCode, body: String) -> Response {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::from("internal error"))
                .unwrap()
        })
}
