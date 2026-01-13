use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
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
use codex_core::AuthManager;
use codex_core::ModelClient;
use codex_core::client_common::Prompt;
use codex_core::client_common::ResponseEvent;
use codex_core::config::Config;
use codex_core::model_provider_info::ModelProviderInfo;
use codex_otel::OtelManager;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::SessionSource;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    auth_manager: Arc<AuthManager>,
    otel_manager: OtelManager,
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

    let otel_manager = OtelManager::new();

    let state = AppState {
        config: Arc::new(config),
        auth_manager,
        otel_manager,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    info!("Static files directory: {:?}", static_dir);

    let router = Router::new()
        .route("/v1/models", get(handle_models))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/models", get(handle_models))
        .route("/chat/completions", post(handle_chat_completions))
        .nest_service("/static", ServeDir::new(&static_dir))
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = env::var("CODEX_OPENAI_PROXY_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:11435".to_string())
        .parse()
        .context("parse CODEX_OPENAI_PROXY_ADDR")?;

    info!("codex-openai-proxy (pure forwarding) listening on http://{addr}");

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
    info!("Chat completion request: model={}, stream={}", body.model, body.stream);

    if body.stream {
        return handle_stream(state, body.0).await;
    }
    handle_once(state, body.0).await
}

async fn handle_models() -> Response {
    info!("Models list request");

    let models = serde_json::json!({
        "object": "list",
        "data": [
            {"id": "xedoc-2.5-tpg", "object": "model", "owned_by": "codex"},
            {"id": "xam-xedoc-1.5-tpg", "object": "model", "owned_by": "codex"},
            {"id": "inim-xedoc-1.5-tpg", "object": "model", "owned_by": "codex"},
            {"id": "2.5-tpg", "object": "model", "owned_by": "codex"},
        ]
    });
    json_response(StatusCode::OK, models.to_string())
}

async fn handle_once(state: AppState, body: ChatCompletionRequest) -> Response {
    let original_model = body.model.clone();
    let reversed_model = map_model(&body.model);

    info!("Forwarding to Codex: {} -> {}", body.model, reversed_model);

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

    // Create Prompt for ModelClient
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: merged_text.clone(),
            }],
        }],
        tools: vec![],  // No tools for pure forwarding
        parallel_tool_calls: false,
        base_instructions_override: None,
        output_schema: None,
    };

    // Get model info
    let model_info = match get_model_info(&state, &reversed_model) {
        Ok(info) => info,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "internal_error",
            );
        }
    };

    // Get provider info
    let provider = match ModelProviderInfo::from_model_info(&model_info) {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "internal_error",
            );
        }
    };

    // Create ModelClient (pure API forwarding, no Agent)
    let conversation_id = ThreadId::new();
    let model_client = ModelClient::new(
        state.config.clone(),
        Some(state.auth_manager.clone()),
        model_info,
        state.otel_manager.clone(),
        provider,
        None,  // No reasoning effort override
        ReasoningSummary::Detailed,
        conversation_id,
        SessionSource::Exec,
    );

    // Stream from pure API
    let mut stream = match model_client.stream(&prompt).await {
        Ok(s) => s,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "internal_error",
            );
        }
    };

    // Collect all events
    let mut final_text = String::new();
    let mut tool_calls = Vec::new();

    while let Some(event) = stream.next().await {
        match event {
            ResponseEvent::ResponseItem(item) => {
                if let Some(tc) = map_tool_call(&item) {
                    tool_calls.push(tc);
                }
            }
            ResponseEvent::TextDelta(delta) => {
                final_text.push_str(&delta);
            }
            ResponseEvent::Error(err) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    err,
                    "api_error",
                );
            }
            _ => {}
        }
    }

    let resp = ChatCompletionResponse {
        id: format!("chatcmpl-codex-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: now_ts(),
        model: original_model,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessageResponse {
                role: "assistant".to_string(),
                content: final_text,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls.clone())
                },
            },
            finish_reason: if !tool_calls.is_empty() {
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

async fn handle_stream(state: AppState, body: ChatCompletionRequest) -> Response {
    let original_model = body.model.clone();
    let reversed_model = map_model(&body.model);

    info!("Streaming from Codex: {} -> {}", body.model, reversed_model);

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

    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: merged_text.clone(),
            }],
        }],
        tools: vec![],
        parallel_tool_calls: false,
        base_instructions_override: None,
        output_schema: None,
    };

    let model_info = match get_model_info(&state, &reversed_model) {
        Ok(info) => info,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "internal_error",
            );
        }
    };

    let provider = match ModelProviderInfo::from_model_info(&model_info) {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "internal_error",
            );
        }
    };

    let conversation_id = ThreadId::new();
    let model_client = ModelClient::new(
        state.config.clone(),
        Some(state.auth_manager.clone()),
        model_info,
        state.otel_manager.clone(),
        provider,
        None,
        ReasoningSummary::Detailed,
        conversation_id,
        SessionSource::Exec,
    );

    let api_stream = match model_client.stream(&prompt).await {
        Ok(s) => s,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "internal_error",
            );
        }
    };

    let (tx, rx) = mpsc::channel(16);
    let model_for_response = original_model.clone();

    tokio::spawn(async move {
        let mut stream = api_stream;
        let mut has_tool_calls = false;

        while let Some(event) = stream.next().await {
            match event {
                ResponseEvent::ResponseItem(item) => {
                    if let Some(tc) = map_tool_call(&item) {
                        has_tool_calls = true;
                        let chunk = stream_chunk(None, Some(tc), false, &model_for_response);
                        let _ = tx.send(Ok(chunk)).await;
                    }
                }
                ResponseEvent::TextDelta(delta) => {
                    let chunk = stream_chunk(Some(&delta), None, false, &model_for_response);
                    let _ = tx.send(Ok(chunk)).await;
                }
                ResponseEvent::Error(err) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
                _ => {}
            }
        }

        // Send final chunk with finish_reason
        let finish_reason = if has_tool_calls {
            "tool_calls"
        } else {
            "stop"
        };
        let chunk = stream_chunk_with_finish(None, None, finish_reason, &model_for_response);
        let _ = tx.send(Ok(chunk)).await;
        let _ = tx.send(Ok(serde_json::Value::String("[DONE]".to_string()))).await;
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

fn get_model_info(state: &AppState, model: &str) -> anyhow::Result<ModelInfo> {
    // Try to get model info from config
    codex_protocol::openai_models::get_model_info(model)
        .ok_or_else(|| anyhow::anyhow!("Unknown model: {}", model))
}

fn stream_chunk(
    content: Option<&str>,
    tool_call: Option<ToolCall>,
    _done: bool,
    model: &str,
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
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": null,
        }],
    })
}

fn stream_chunk_with_finish(
    content: Option<&str>,
    tool_call: Option<ToolCall>,
    finish_reason: &str,
    model: &str,
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
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
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
        _ => None,
    }
}

fn map_model(model: &str) -> String {
    model.chars().rev().collect()
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

fn merged_text_from_request(body: &ChatCompletionRequest) -> Option<String> {
    if let Some(msgs) = &body.messages {
        return merge_messages(msgs);
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
