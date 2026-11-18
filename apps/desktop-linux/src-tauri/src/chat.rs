//! Real DeepSeek AI Streaming Inference Engine
//!
//! Streams live reasoning (chain-of-thought) and response text directly from
//! the official DeepSeek API (https://api.deepseek.com/v1/chat/completions)
//! using stored DeepSeek Account session tokens or API keys.

use futures_util::StreamExt;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

pub static LAST_STREAM_ID: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();

pub fn get_last_stream_id() -> &'static Mutex<Option<String>> {
    LAST_STREAM_ID.get_or_init(|| Mutex::new(None))
}

pub async fn set_active_stream_id(id: String) {
    let mut guard = get_last_stream_id().lock().await;
    *guard = Some(id);
}

pub async fn get_active_stream_id() -> Option<String> {
    let guard = get_last_stream_id().lock().await;
    guard.clone()
}

/// Dispatches a live stream frame to the webview UI via dsh://ws-message.
pub fn emit_ws_message(app: &AppHandle, payload: serde_json::Value) {
    let s = payload.to_string();
    let _ = app.emit("dsh://ws-message", &s);
}

/// Executes prompt streaming asynchronously against DeepSeek API.
pub fn spawn_chat_stream(
    prompt_text: String,
    session_id: String,
    stream_id: Option<String>,
    app: AppHandle,
) {
    tokio::spawn(async move {
        let stream_id = match stream_id {
            Some(id) => id,
            None => get_active_stream_id().await.unwrap_or_else(|| "default-stream".to_string()),
        };

        info!("Starting DeepSeek AI chat stream for session {session_id} (streamId={stream_id})");

        // 1. Resolve credentials (stored OAuth account grant or API key)
        let mut auth_token = None;
        let mut is_account_token = false;

        if let Some((token, _, _)) = crate::oauth::read_stored_account_grant() {
            if !token.is_empty() {
                auth_token = Some(token);
                is_account_token = true;
            }
        }

        if auth_token.is_none() {
            let cred_file = crate::get_user_home().join(".dsh/.credentials.yaml");
            if cred_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&cred_file) {
                    for line in content.lines() {
                        if line.trim_start().starts_with("DEEPSEEK_API_KEY:") {
                            let k = line.trim_start().trim_start_matches("DEEPSEEK_API_KEY:").trim().trim_matches('"');
                            if !k.is_empty() {
                                auth_token = Some(k.to_string());
                                is_account_token = false;
                                break;
                            }
                        }
                    }
                }
            }
        }

        if auth_token.is_none() {
            if let Ok(env_key) = std::env::var("DEEPSEEK_API_KEY") {
                if !env_key.trim().is_empty() {
                    auth_token = Some(env_key.trim().to_string());
                    is_account_token = false;
                }
            }
        }

        let token = match auth_token {
            Some(t) => t,
            None => {
                warn!("No DeepSeek credentials found for prompt");
                emit_ws_message(&app, serde_json::json!({
                    "type": "item",
                    "streamId": stream_id,
                    "value": {
                        "type": "assistant-stream",
                        "frame": {
                            "type": "chunk",
                            "chunk": {
                                "type": "text-delta",
                                "text": "⚠️ Please sign in with your DeepSeek Account or provide an API Key to chat with DeepSeek AI."
                            }
                        }
                    }
                }));
                emit_ws_message(&app, serde_json::json!({
                    "type": "end",
                    "streamId": stream_id
                }));
                return;
            }
        };

        // 2. Build HTTP request
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to create reqwest client: {e}");
                return;
            }
        };

        let req_body = serde_json::json!({
            "model": "deepseek-chat",
            "messages": [
                {
                    "role": "user",
                    "content": prompt_text
                }
            ],
            "stream": true
        });

        let mut req_builder = client
            .post("https://api.deepseek.com/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"));

        if is_account_token {
            req_builder = req_builder.header("x-dsh-auth-token", &token);
        }

        let resp = match req_builder.json(&req_body).send().await {
            Ok(r) => r,
            Err(e) => {
                error!("DeepSeek chat request failed: {e}");
                emit_ws_message(&app, serde_json::json!({
                    "type": "error",
                    "streamId": stream_id,
                    "error": { "message": format!("Network error: {e}") }
                }));
                return;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            error!("DeepSeek API returned error HTTP {status}: {err_body}");
            emit_ws_message(&app, serde_json::json!({
                "type": "item",
                "streamId": stream_id,
                "value": {
                    "type": "assistant-stream",
                    "frame": {
                        "type": "chunk",
                        "chunk": {
                            "type": "text-delta",
                            "text": format!("Error from DeepSeek API ({status}): {err_body}")
                        }
                    }
                }
            }));
            emit_ws_message(&app, serde_json::json!({
                "type": "end",
                "streamId": stream_id
            }));
            return;
        }

        // 3. Stream Server-Sent Events (SSE)
        let mut byte_stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_res) = byte_stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    warn!("Stream chunk error: {e}");
                    break;
                }
            };

            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(idx) = buffer.find('\n') {
                let line = buffer[..idx].trim().to_string();
                buffer = buffer[idx + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    let trimmed = data.trim();
                    if trimmed == "[DONE]" {
                        break;
                    }

                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
                            if let Some(delta) = choices.first().and_then(|c| c.get("delta")) {
                                // 1. Chain-of-thought Reasoning tokens
                                if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
                                    if !reasoning.is_empty() {
                                        emit_ws_message(&app, serde_json::json!({
                                            "type": "item",
                                            "streamId": stream_id,
                                            "value": {
                                                "type": "assistant-stream",
                                                "frame": {
                                                    "type": "chunk",
                                                    "chunk": {
                                                        "type": "reasoning-delta",
                                                        "text": reasoning
                                                    }
                                                }
                                            }
                                        }));
                                    }
                                }

                                // 2. Content answer tokens
                                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                    if !content.is_empty() {
                                        emit_ws_message(&app, serde_json::json!({
                                            "type": "item",
                                            "streamId": stream_id,
                                            "value": {
                                                "type": "assistant-stream",
                                                "frame": {
                                                    "type": "chunk",
                                                    "chunk": {
                                                        "type": "text-delta",
                                                        "text": content
                                                    }
                                                }
                                            }
                                        }));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 4. Finish the assistant turn
        info!("DeepSeek AI chat stream completed for {session_id}");
        emit_ws_message(&app, serde_json::json!({
            "type": "item",
            "streamId": stream_id,
            "value": {
                "type": "turn/end",
                "data": { "status": "completed" }
            }
        }));
        emit_ws_message(&app, serde_json::json!({
            "type": "end",
            "streamId": stream_id
        }));
    });
}
