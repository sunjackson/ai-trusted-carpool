use crate::models::ToolKind;
use crate::runtime::{HostSeatBinding, RuntimeState};
use crate::usage::{apply_usage, extract_usage};
use crate::usage_history::{
    append as append_usage_history, UsageHistoryContext, UsageHistoryRecord,
};
use base64::{engine::general_purpose, Engine as _};
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, Mutex};

pub const RELAY_REQUEST_EVENT: &str = "trusted-carpool:relay-request";
pub const RELAY_STREAM_EVENT: &str = "trusted-carpool:relay-stream-event";
const MAX_RELAY_BODY_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_RELAY_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const RELAY_TTL_MS: i64 = 5 * 60 * 1_000;
const RELAY_TIMEOUT_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RelayHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayRequest {
    pub request_id: String,
    pub access_id: String,
    pub tool: ToolKind,
    pub method: String,
    pub path: String,
    pub headers: Vec<RelayHeader>,
    pub body_base64: String,
    pub body_sha256: String,
    pub timestamp_ms: i64,
    pub auth_proof: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayResponse {
    pub request_id: String,
    pub status_code: u16,
    pub headers: Vec<RelayHeader>,
    pub body_base64: String,
    pub body_sha256: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RelayStreamKind {
    Start,
    Chunk,
    End,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayStreamEvent {
    pub request_id: String,
    pub kind: RelayStreamKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<RelayHeader>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RelayStreamEvent {
    pub fn error(request_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            kind: RelayStreamKind::Error,
            status_code: None,
            headers: Vec::new(),
            chunk_base64: None,
            body_sha256: None,
            latency_ms: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayBridgeRequestEvent {
    pub request_id: String,
    pub access_id: String,
    pub owner_peer_id: String,
    pub payload_json: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Serialize)]
struct SignableRelayRequest<'a> {
    request_id: &'a str,
    access_id: &'a str,
    tool: ToolKind,
    method: &'a str,
    path: &'a str,
    headers: &'a [RelayHeader],
    body_sha256: &'a str,
    timestamp_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCredentialKind {
    ApiKey,
    ClaudeOAuth,
    CodexChatGptOAuth,
}

#[derive(Clone)]
pub struct HostCredential {
    pub secret: String,
    pub account_id: Option<String>,
    pub kind: HostCredentialKind,
    pub source: String,
}

pub struct RelayBridge {
    app_handle: Mutex<Option<AppHandle>>,
    pending: Mutex<HashMap<String, mpsc::Sender<RelayStreamEvent>>>,
}

impl RelayBridge {
    pub fn global() -> Arc<Self> {
        static INSTANCE: OnceLock<Arc<RelayBridge>> = OnceLock::new();
        INSTANCE
            .get_or_init(|| {
                Arc::new(Self {
                    app_handle: Mutex::new(None),
                    pending: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    pub async fn set_app_handle(&self, app_handle: AppHandle) {
        *self.app_handle.lock().await = Some(app_handle);
    }

    pub async fn relay_stream(
        &self,
        access_id: String,
        owner_peer_id: String,
        request: &RelayRequest,
    ) -> Result<mpsc::Receiver<RelayStreamEvent>, String> {
        let app_handle = self
            .app_handle
            .lock()
            .await
            .clone()
            .ok_or_else(|| "本地中转尚未初始化，请重启应用".to_string())?;
        let payload_json =
            serde_json::to_string(request).map_err(|error| format!("无法编码中转请求: {error}"))?;
        let event = RelayBridgeRequestEvent {
            request_id: request.request_id.clone(),
            access_id,
            owner_peer_id,
            payload_json,
            timeout_ms: RELAY_TIMEOUT_MS,
        };
        let (sender, receiver) = mpsc::channel(64);
        {
            let mut pending = self.pending.lock().await;
            if pending.insert(request.request_id.clone(), sender).is_some() {
                return Err("中转请求编号重复".to_string());
            }
        }
        if let Err(error) = app_handle.emit(RELAY_REQUEST_EVENT, event) {
            self.pending.lock().await.remove(&request.request_id);
            return Err(format!("无法交给安全连接发送: {error}"));
        }
        Ok(receiver)
    }

    pub async fn submit_stream_event(&self, event: RelayStreamEvent) -> Result<bool, String> {
        validate_stream_event(&event)?;
        let terminal = matches!(event.kind, RelayStreamKind::End | RelayStreamKind::Error);
        let sender = if terminal {
            self.pending.lock().await.remove(&event.request_id)
        } else {
            self.pending.lock().await.get(&event.request_id).cloned()
        };
        if let Some(sender) = sender {
            let request_id = event.request_id.clone();
            if sender.send(event).await.is_err() {
                self.pending.lock().await.remove(&request_id);
                return Err("本地模型请求已经结束".to_string());
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn submit(&self, request_id: String, payload_json: String) -> Result<bool, String> {
        if request_id.trim().is_empty() || payload_json.len() > MAX_RELAY_RESPONSE_BYTES * 2 {
            return Err("中转响应编号为空或数据过大".to_string());
        }
        let value: Value = serde_json::from_str(&payload_json)
            .map_err(|error| format!("车主响应格式无效: {error}"))?;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return self
                .submit_stream_event(RelayStreamEvent::error(request_id, error))
                .await;
        }
        let response: RelayResponse =
            serde_json::from_value(value).map_err(|error| format!("车主响应格式无效: {error}"))?;
        if response.request_id != request_id {
            return Err("车主响应与当前请求不匹配".to_string());
        }
        if !self
            .submit_stream_event(RelayStreamEvent {
                request_id: request_id.clone(),
                kind: RelayStreamKind::Start,
                status_code: Some(response.status_code),
                headers: response.headers,
                chunk_base64: None,
                body_sha256: None,
                latency_ms: None,
                error: None,
            })
            .await?
        {
            return Ok(false);
        }
        self.submit_stream_event(RelayStreamEvent {
            request_id: request_id.clone(),
            kind: RelayStreamKind::Chunk,
            status_code: None,
            headers: Vec::new(),
            chunk_base64: Some(response.body_base64),
            body_sha256: None,
            latency_ms: None,
            error: None,
        })
        .await?;
        self.submit_stream_event(RelayStreamEvent {
            request_id,
            kind: RelayStreamKind::End,
            status_code: None,
            headers: Vec::new(),
            chunk_base64: None,
            body_sha256: Some(response.body_sha256),
            latency_ms: Some(response.latency_ms),
            error: None,
        })
        .await
    }
}

fn validate_stream_event(event: &RelayStreamEvent) -> Result<(), String> {
    if event.request_id.trim().is_empty() || event.request_id.len() > 128 {
        return Err("中转响应编号无效".to_string());
    }
    match event.kind {
        RelayStreamKind::Start if event.status_code.is_none() => {
            Err("流式响应缺少状态码".to_string())
        }
        RelayStreamKind::Chunk => {
            let encoded = event
                .chunk_base64
                .as_deref()
                .ok_or_else(|| "流式响应分块为空".to_string())?;
            if encoded.len() > MAX_RELAY_RESPONSE_BYTES * 2 {
                return Err("流式响应分块过大".to_string());
            }
            Ok(())
        }
        RelayStreamKind::End if event.body_sha256.is_none() => {
            Err("流式响应缺少完整性摘要".to_string())
        }
        RelayStreamKind::Error if event.error.as_deref().unwrap_or_default().is_empty() => {
            Err("流式响应错误信息为空".to_string())
        }
        _ => Ok(()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

pub fn sha256_label(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        general_purpose::URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, bytes).as_ref())
    )
}

fn session_key(secret: &str) -> Result<Vec<u8>, String> {
    general_purpose::URL_SAFE_NO_PAD
        .decode(secret.trim())
        .map_err(|_| "会话密钥格式无效".to_string())
}

fn signable_bytes(request: &RelayRequest) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&SignableRelayRequest {
        request_id: &request.request_id,
        access_id: &request.access_id,
        tool: request.tool,
        method: &request.method,
        path: &request.path,
        headers: &request.headers,
        body_sha256: &request.body_sha256,
        timestamp_ms: request.timestamp_ms,
    })
    .map_err(|error| format!("无法编码请求签名内容: {error}"))
}

pub fn sign_request(request: &mut RelayRequest, session_secret: &str) -> Result<(), String> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, &session_key(session_secret)?);
    request.auth_proof = general_purpose::URL_SAFE_NO_PAD
        .encode(hmac::sign(&key, &signable_bytes(request)?).as_ref());
    Ok(())
}

fn verify_request(request: &RelayRequest, session_secret: &str) -> Result<(), String> {
    let supplied = general_purpose::URL_SAFE_NO_PAD
        .decode(request.auth_proof.trim())
        .map_err(|_| "请求授权证明格式无效".to_string())?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, &session_key(session_secret)?);
    hmac::verify(&key, &signable_bytes(request)?, &supplied)
        .map_err(|_| "请求授权证明无效".to_string())
}

pub fn decode_body(encoded: &str, limit: usize, label: &str) -> Result<Vec<u8>, String> {
    let body = general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|error| format!("{label}内容编码无效: {error}"))?;
    if body.len() > limit {
        return Err(format!("{label}内容过大，最多 {} MiB", limit / 1024 / 1024));
    }
    Ok(body)
}

pub fn allowed_path(tool: ToolKind, path: &str) -> bool {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains("://")
        || path.contains('\\')
        || path.contains('#')
        || path.contains('?')
        || path.contains('%')
        || path.chars().any(char::is_whitespace)
    {
        return false;
    }
    match tool {
        ToolKind::Claude => matches!(
            path,
            "/v1/messages" | "/v1/messages/count_tokens" | "/v1/models"
        ),
        ToolKind::Codex => {
            matches!(
                path,
                "/v1/responses" | "/v1/chat/completions" | "/v1/models" | "/v1/embeddings"
            ) || path
                .strip_prefix("/v1/models/")
                .map(|model| {
                    !model.is_empty()
                        && model.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'-' | b'_' | b'.' | b':')
                        })
                })
                .unwrap_or(false)
        }
    }
}

pub fn allowed_relay_header(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "accept"
            | "content-type"
            | "idempotency-key"
            | "x-request-id"
            | "x-client-request-id"
            | "anthropic-beta"
            | "anthropic-version"
            | "openai-beta"
    )
}

fn json_api_key(path: &Path, pointers: &[&str]) -> Option<String> {
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    pointers.iter().find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty() && !key.starts_with("trusted-carpool-"))
            .map(str::to_string)
    })
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn nonempty_string(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn codex_oauth_credential(path: &Path) -> Option<HostCredential> {
    let value = read_json(path)?;
    Some(HostCredential {
        secret: nonempty_string(&value, "/tokens/access_token")?,
        account_id: nonempty_string(&value, "/tokens/account_id"),
        kind: HostCredentialKind::CodexChatGptOAuth,
        source: path.to_string_lossy().into_owned(),
    })
}

fn claude_oauth_from_value(value: &Value, source: String) -> Option<HostCredential> {
    let expires_at = value
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(Value::as_i64);
    if expires_at.is_some_and(|expires_at| expires_at <= now_ms().saturating_add(60_000)) {
        return None;
    }
    Some(HostCredential {
        secret: nonempty_string(value, "/claudeAiOauth/accessToken")?,
        account_id: None,
        kind: HostCredentialKind::ClaudeOAuth,
        source,
    })
}

fn claude_settings_bearer(path: &Path) -> Option<HostCredential> {
    let value = read_json(path)?;
    let configured_base = nonempty_string(&value, "/env/ANTHROPIC_BASE_URL");
    if configured_base
        .as_deref()
        .is_some_and(|base| base.trim_end_matches('/') != "https://api.anthropic.com")
    {
        return None;
    }
    let secret = nonempty_string(&value, "/env/CLAUDE_CODE_OAUTH_TOKEN")
        .or_else(|| nonempty_string(&value, "/env/ANTHROPIC_AUTH_TOKEN"))?;
    Some(HostCredential {
        secret,
        account_id: None,
        kind: HostCredentialKind::ClaudeOAuth,
        source: path.to_string_lossy().into_owned(),
    })
}

#[cfg(target_os = "macos")]
fn claude_keychain_oauth() -> Option<HostCredential> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: Value = serde_json::from_slice(&output.stdout).ok()?;
    claude_oauth_from_value(
        &value,
        "macOS Keychain: Claude Code-credentials".to_string(),
    )
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_oauth() -> Option<HostCredential> {
    None
}

fn credential_candidates(kind: ToolKind, home: &Path) -> Vec<(PathBuf, &'static [&'static str])> {
    match kind {
        ToolKind::Claude => vec![
            (
                home.join(".claude/settings.json"),
                &["/env/ANTHROPIC_API_KEY", "/ANTHROPIC_API_KEY"],
            ),
            (
                home.join(".claude/settings.local.json"),
                &["/env/ANTHROPIC_API_KEY", "/ANTHROPIC_API_KEY"],
            ),
            (
                home.join(".claude.json"),
                &["/env/ANTHROPIC_API_KEY", "/ANTHROPIC_API_KEY"],
            ),
        ],
        ToolKind::Codex => vec![(home.join(".codex/auth.json"), &["/OPENAI_API_KEY"])],
    }
}

pub fn load_host_credential(kind: ToolKind) -> Option<HostCredential> {
    let env_name = match kind {
        ToolKind::Claude => "ANTHROPIC_API_KEY",
        ToolKind::Codex => "OPENAI_API_KEY",
    };
    if let Ok(value) = std::env::var(env_name) {
        let value = value.trim();
        if !value.is_empty() && !value.starts_with("trusted-carpool-") {
            return Some(HostCredential {
                secret: value.to_string(),
                account_id: None,
                kind: HostCredentialKind::ApiKey,
                source: format!("环境变量 {env_name}"),
            });
        }
    }
    if kind == ToolKind::Claude {
        let configured_base = std::env::var("ANTHROPIC_BASE_URL").ok();
        if configured_base
            .as_deref()
            .is_none_or(|base| base.trim_end_matches('/') == "https://api.anthropic.com")
        {
            for env_name in ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN"] {
                if let Ok(value) = std::env::var(env_name) {
                    if !value.trim().is_empty() {
                        return Some(HostCredential {
                            secret: value.trim().to_string(),
                            account_id: None,
                            kind: HostCredentialKind::ClaudeOAuth,
                            source: format!("环境变量 {env_name}"),
                        });
                    }
                }
            }
        }
    }
    let home = dirs::home_dir()?;
    if let Some(credential) =
        credential_candidates(kind, &home)
            .into_iter()
            .find_map(|(path, pointers)| {
                json_api_key(&path, pointers).map(|api_key| HostCredential {
                    secret: api_key,
                    account_id: None,
                    kind: HostCredentialKind::ApiKey,
                    source: path.to_string_lossy().into_owned(),
                })
            })
    {
        return Some(credential);
    }
    match kind {
        ToolKind::Claude => {
            let path = home.join(".claude/.credentials.json");
            read_json(&path)
                .and_then(|value| {
                    claude_oauth_from_value(&value, path.to_string_lossy().into_owned())
                })
                .or_else(|| claude_settings_bearer(&home.join(".claude/settings.json")))
                .or_else(claude_keychain_oauth)
        }
        ToolKind::Codex => codex_oauth_credential(&home.join(".codex/auth.json")),
    }
}

fn binding_for_request(
    state: &RuntimeState,
    request: &RelayRequest,
    current_ms: i64,
) -> Result<(HostSeatBinding, String), String> {
    if request.timestamp_ms > current_ms.saturating_add(60_000)
        || current_ms > request.timestamp_ms.saturating_add(RELAY_TTL_MS)
    {
        return Err("请求已过期，请重新发送".to_string());
    }
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    runtime
        .relay_request_seen_at
        .retain(|_, timestamp| current_ms.saturating_sub(*timestamp) <= RELAY_TTL_MS);
    if runtime
        .relay_request_seen_at
        .contains_key(&request.request_id)
    {
        return Err("检测到重复请求，已拒绝重放".to_string());
    }
    let binding = runtime
        .host_bindings
        .values()
        .find(|binding| binding.access_id == request.access_id)
        .cloned()
        .ok_or_else(|| "上车授权不存在或已经失效".to_string())?;
    verify_request(request, &binding.session_secret)?;
    let car = runtime
        .active_car
        .as_ref()
        .ok_or_else(|| "车主已经停止发车".to_string())?;
    if car.started_at > current_ms || car.expires_at <= current_ms {
        return Err("当前不在发车开放时间内".to_string());
    }
    if !car.enabled_tools.contains(&request.tool) {
        return Err("车主没有开放这个工具".to_string());
    }
    if !matches!(request.method.as_str(), "GET" | "POST")
        || !allowed_path(request.tool, &request.path)
    {
        return Err("请求方法或地址不在官方 API 白名单内".to_string());
    }
    runtime
        .relay_request_seen_at
        .insert(request.request_id.clone(), current_ms);
    Ok((binding.clone(), binding.code))
}

fn official_endpoint(tool: ToolKind, credential: &HostCredential) -> Result<&'static str, String> {
    match (tool, credential.kind) {
        (ToolKind::Claude, HostCredentialKind::ApiKey | HostCredentialKind::ClaudeOAuth) => {
            Ok("https://api.anthropic.com")
        }
        (ToolKind::Codex, HostCredentialKind::ApiKey) => Ok("https://api.openai.com"),
        (ToolKind::Codex, HostCredentialKind::CodexChatGptOAuth) => {
            Ok("https://chatgpt.com/backend-api/codex")
        }
        _ => Err("本地认证类型与所选工具不匹配".to_string()),
    }
}

fn upstream_request(
    request: &RelayRequest,
    body: Vec<u8>,
    credential: &HostCredential,
    endpoint: &str,
) -> Result<reqwest::RequestBuilder, String> {
    let upstream_path = if credential.kind == HostCredentialKind::CodexChatGptOAuth {
        request.path.strip_prefix("/v1").unwrap_or(&request.path)
    } else {
        &request.path
    };
    let url = format!("{}{}", endpoint.trim_end_matches('/'), upstream_path);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_millis(RELAY_TIMEOUT_MS))
        .build()
        .map_err(|error| format!("无法创建官方 API 客户端: {error}"))?;
    let method = request
        .method
        .parse::<reqwest::Method>()
        .map_err(|error| format!("请求方法无效: {error}"))?;
    let mut builder = client.request(method, &url);
    builder = match (request.tool, credential.kind) {
        (ToolKind::Claude, HostCredentialKind::ApiKey) => builder
            .header("x-api-key", &credential.secret)
            .header("anthropic-version", "2023-06-01"),
        (ToolKind::Claude, HostCredentialKind::ClaudeOAuth) => builder
            .bearer_auth(&credential.secret)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20"),
        (ToolKind::Codex, HostCredentialKind::ApiKey) => builder.bearer_auth(&credential.secret),
        (ToolKind::Codex, HostCredentialKind::CodexChatGptOAuth) => {
            let builder = builder.bearer_auth(&credential.secret);
            if let Some(account_id) = credential.account_id.as_deref() {
                builder.header("ChatGPT-Account-ID", account_id)
            } else {
                builder
            }
        }
        _ => return Err("本地认证类型与所选工具不匹配".to_string()),
    };
    for header in &request.headers {
        if allowed_relay_header(&header.name)
            && !matches!(
                header.name.trim().to_ascii_lowercase().as_str(),
                "anthropic-version"
            )
        {
            builder = builder.header(header.name.trim(), header.value.trim());
        }
    }
    Ok(builder.body(body))
}

fn response_headers(response: &reqwest::Response) -> Vec<RelayHeader> {
    response
        .headers()
        .iter()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "content-type" | "request-id" | "x-request-id" | "openai-processing-ms"
            )
        })
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|value| RelayHeader {
                name: name.as_str().to_string(),
                value: value.chars().take(4096).collect(),
            })
        })
        .collect()
}

async fn execute_upstream(
    request: &RelayRequest,
    body: Vec<u8>,
    credential: &HostCredential,
    endpoint: &str,
) -> Result<RelayResponse, String> {
    let started = std::time::Instant::now();
    let response = upstream_request(request, body, credential, endpoint)?
        .send()
        .await
        .map_err(|error| format!("车主连接官方 API 失败: {error}"))?;
    let status_code = response.status().as_u16();
    let headers = response_headers(&response);
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("读取官方 API 响应失败: {error}"))?;
    if bytes.len() > MAX_RELAY_RESPONSE_BYTES {
        return Err("官方 API 响应过大，已安全中止".to_string());
    }
    Ok(RelayResponse {
        request_id: request.request_id.clone(),
        status_code,
        headers,
        body_base64: general_purpose::STANDARD.encode(&bytes),
        body_sha256: sha256_label(&bytes),
        latency_ms: started.elapsed().as_millis() as u64,
    })
}

fn should_record_usage(request: &RelayRequest, status_code: u16) -> bool {
    status_code < 400
        && matches!(
            (
                request.tool,
                request.path.split('?').next().unwrap_or_default()
            ),
            (ToolKind::Claude, "/v1/messages")
                | (ToolKind::Codex, "/v1/responses")
                | (ToolKind::Codex, "/v1/chat/completions")
        )
}

fn record_usage(
    state: &RuntimeState,
    request: &RelayRequest,
    code: &str,
    request_body: &[u8],
    response_body: &[u8],
    status_code: u16,
    current_ms: i64,
) -> Result<(), String> {
    if !should_record_usage(request, status_code) {
        return Ok(());
    }
    let Ok(delta) = extract_usage(request.tool, request_body, response_body, current_ms) else {
        return Ok(());
    };
    let history = {
        let mut runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        let passenger_peer_id = runtime
            .host_bindings
            .get(code)
            .map(|binding| binding.passenger_peer_id.clone())
            .unwrap_or_default();
        let history_path = runtime.usage_history_path.clone();
        if let Some(car) = runtime.active_car.as_mut() {
            let seat = car
                .seats
                .iter()
                .find(|seat| seat.code == code)
                .ok_or_else(|| "用量记录对应的座位不存在".to_string())?;
            let car_id = car.car_id.clone();
            let car_name = car.car_name.clone();
            let seat_no = seat.seat_no;
            let nickname = seat
                .nickname
                .clone()
                .unwrap_or_else(|| "未命名乘员".to_string());
            let record = UsageHistoryRecord::from_usage(
                UsageHistoryContext {
                    car_id: &car_id,
                    car_name: &car_name,
                    seat_no,
                    nickname: &nickname,
                    passenger_peer_id: &passenger_peer_id,
                },
                &delta,
            );
            apply_usage(car, code, delta)?;
            history_path.map(|path| (path, record))
        } else {
            None
        }
    };
    if let Some((path, record)) = history {
        if let Err(error) = append_usage_history(&path, &record) {
            eprintln!("usage history append failed: {error}");
        }
    }
    Ok(())
}

pub fn start_host_request_stream(
    app: AppHandle,
    state: RuntimeState,
    request: RelayRequest,
) -> bool {
    let request_id = request.request_id.clone();
    tauri::async_runtime::spawn(async move {
        let result = execute_host_request_stream(&state, request, |event| {
            app.emit(RELAY_STREAM_EVENT, event)
                .map_err(|error| format!("无法发送流式响应: {error}"))
        })
        .await;
        if let Err(error) = result {
            let _ = app.emit(
                RELAY_STREAM_EVENT,
                RelayStreamEvent::error(request_id, error),
            );
        }
    });
    true
}

async fn execute_host_request_stream<F>(
    state: &RuntimeState,
    request: RelayRequest,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(RelayStreamEvent) -> Result<(), String>,
{
    let credential = load_host_credential(request.tool).ok_or_else(|| match request.tool {
        ToolKind::Claude => {
            "本机没有可用的 Claude 官方 API Key 或未过期的官方 OAuth 授权".to_string()
        }
        ToolKind::Codex => {
            "本机没有可用的 OpenAI API Key 或 ChatGPT Codex 官方 OAuth 授权".to_string()
        }
    })?;
    let endpoint = official_endpoint(request.tool, &credential)?;
    execute_host_request_stream_with(state, request, &credential, endpoint, &mut emit).await
}

pub(crate) async fn execute_host_request_stream_with<F>(
    state: &RuntimeState,
    request: RelayRequest,
    credential: &HostCredential,
    endpoint: &str,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(RelayStreamEvent) -> Result<(), String>,
{
    let current_ms = now_ms();
    let (_binding, code) = binding_for_request(state, &request, current_ms)?;
    let body = decode_body(&request.body_base64, MAX_RELAY_BODY_BYTES, "请求")?;
    if sha256_label(&body) != request.body_sha256 {
        return Err("请求内容完整性校验失败".to_string());
    }
    let started = std::time::Instant::now();
    let mut response = upstream_request(&request, body.clone(), credential, endpoint)?
        .send()
        .await
        .map_err(|error| format!("车主连接官方 API 失败: {error}"))?;
    let status_code = response.status().as_u16();
    emit(RelayStreamEvent {
        request_id: request.request_id.clone(),
        kind: RelayStreamKind::Start,
        status_code: Some(status_code),
        headers: response_headers(&response),
        chunk_base64: None,
        body_sha256: None,
        latency_ms: None,
        error: None,
    })?;
    let mut response_body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取官方 API 流式响应失败: {error}"))?
    {
        if response_body.len().saturating_add(chunk.len()) > MAX_RELAY_RESPONSE_BYTES {
            return Err("官方 API 响应过大，已安全中止".to_string());
        }
        response_body.extend_from_slice(&chunk);
        emit(RelayStreamEvent {
            request_id: request.request_id.clone(),
            kind: RelayStreamKind::Chunk,
            status_code: None,
            headers: Vec::new(),
            chunk_base64: Some(general_purpose::STANDARD.encode(&chunk)),
            body_sha256: None,
            latency_ms: None,
            error: None,
        })?;
    }
    let body_sha256 = sha256_label(&response_body);
    record_usage(
        state,
        &request,
        &code,
        &body,
        &response_body,
        status_code,
        current_ms,
    )?;
    emit(RelayStreamEvent {
        request_id: request.request_id,
        kind: RelayStreamKind::End,
        status_code: None,
        headers: Vec::new(),
        chunk_base64: None,
        body_sha256: Some(body_sha256),
        latency_ms: Some(started.elapsed().as_millis() as u64),
        error: None,
    })?;
    Ok(())
}

pub async fn execute_host_request(
    state: &RuntimeState,
    request: RelayRequest,
) -> Result<RelayResponse, String> {
    let credential = load_host_credential(request.tool).ok_or_else(|| match request.tool {
        ToolKind::Claude => {
            "本机没有可用的 Claude 官方 API Key 或未过期的官方 OAuth 授权".to_string()
        }
        ToolKind::Codex => {
            "本机没有可用的 OpenAI API Key 或 ChatGPT Codex 官方 OAuth 授权".to_string()
        }
    })?;
    let endpoint = official_endpoint(request.tool, &credential)?;
    execute_host_request_with(state, request, &credential, endpoint).await
}

pub(crate) async fn execute_host_request_with(
    state: &RuntimeState,
    request: RelayRequest,
    credential: &HostCredential,
    endpoint: &str,
) -> Result<RelayResponse, String> {
    let current_ms = now_ms();
    let (_binding, code) = binding_for_request(state, &request, current_ms)?;
    let body = decode_body(&request.body_base64, MAX_RELAY_BODY_BYTES, "请求")?;
    if sha256_label(&body) != request.body_sha256 {
        return Err("请求内容完整性校验失败".to_string());
    }
    let response = execute_upstream(&request, body.clone(), credential, endpoint).await?;
    if let Ok(response_body) = decode_body(&response.body_base64, MAX_RELAY_RESPONSE_BYTES, "响应")
    {
        record_usage(
            state,
            &request,
            &code,
            &body,
            &response_body,
            response.status_code,
            current_ms,
        )?;
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CarSession, Seat, SeatState, SeatUsageSummary};
    use crate::runtime::HostSeatBinding;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    fn request(secret: &str) -> RelayRequest {
        let body = br#"{"model":"gpt-5.6-luna","input":"hello"}"#;
        let mut request = RelayRequest {
            request_id: Uuid::new_v4().to_string(),
            access_id: Uuid::new_v4().to_string(),
            tool: ToolKind::Codex,
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            headers: vec![RelayHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            }],
            body_base64: general_purpose::STANDARD.encode(body),
            body_sha256: sha256_label(body),
            timestamp_ms: now_ms(),
            auth_proof: String::new(),
        };
        sign_request(&mut request, secret).expect("sign");
        request
    }

    fn state_for(request: &RelayRequest, secret: &str) -> RuntimeState {
        let state = RuntimeState::default();
        let now = now_ms();
        let code = "7G2K5LQ8M4TZ".to_string();
        let mut runtime = state.inner.lock().expect("runtime");
        runtime.active_car = Some(CarSession {
            car_id: "car".to_string(),
            car_name: "friends".to_string(),
            owner_peer_id: "owner".to_string(),
            started_at: now - 1_000,
            expires_at: now + 60_000,
            enabled_tools: vec![ToolKind::Codex],
            seats: vec![Seat {
                seat_no: 1,
                code: code.clone(),
                nickname: Some("friend".to_string()),
                state: SeatState::Connected,
                tool: None,
                usage: SeatUsageSummary::default(),
            }],
        });
        runtime.host_bindings.insert(
            code.clone(),
            HostSeatBinding {
                code,
                claim_id: "claim".to_string(),
                passenger_peer_id: "passenger".to_string(),
                passenger_encryption_public_key: "key".to_string(),
                access_id: request.access_id.clone(),
                session_secret: secret.to_string(),
                issued_at_ms: now,
            },
        );
        drop(runtime);
        state
    }

    #[test]
    fn session_hmac_detects_tampering_and_replay() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let signed_request = request(&secret);
        let state = state_for(&signed_request, &secret);
        binding_for_request(&state, &signed_request, now_ms()).expect("first request");
        assert!(binding_for_request(&state, &signed_request, now_ms()).is_err());

        let mut tampered = request(&secret);
        tampered.path = "/v1/models".to_string();
        let tampered_state = state_for(&tampered, &secret);
        assert!(binding_for_request(&tampered_state, &tampered, now_ms()).is_err());
    }

    #[test]
    fn official_path_allowlist_rejects_arbitrary_urls_and_traversal() {
        assert!(allowed_path(ToolKind::Claude, "/v1/messages"));
        assert!(allowed_path(ToolKind::Codex, "/v1/responses"));
        assert!(!allowed_path(
            ToolKind::Codex,
            "https://evil.example/v1/responses"
        ));
        assert!(!allowed_path(ToolKind::Claude, "/v1/../admin"));
        assert!(!allowed_path(ToolKind::Claude, "/internal/account"));
    }

    #[test]
    fn oauth_only_codex_file_is_not_misread_as_an_api_key() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"tokens":{"access_token":"oauth-official","account_id":"account-1"}}"#,
        )
        .expect("write");
        assert!(json_api_key(&path, &["/OPENAI_API_KEY"]).is_none());
        let oauth = codex_oauth_credential(&path).expect("official OAuth");
        assert_eq!(oauth.kind, HostCredentialKind::CodexChatGptOAuth);
        assert_eq!(oauth.account_id.as_deref(), Some("account-1"));
        assert_eq!(
            official_endpoint(ToolKind::Codex, &oauth).expect("endpoint"),
            "https://chatgpt.com/backend-api/codex"
        );
        std::fs::write(&path, r#"{"OPENAI_API_KEY":"sk-official"}"#).expect("write");
        assert_eq!(
            json_api_key(&path, &["/OPENAI_API_KEY"]).as_deref(),
            Some("sk-official")
        );
    }

    #[test]
    fn claude_oauth_requires_an_unexpired_official_access_token() {
        let valid = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "official-claude-oauth",
                "expiresAt": now_ms() + 120_000
            }
        });
        let credential = claude_oauth_from_value(&valid, "test".to_string()).expect("OAuth");
        assert_eq!(credential.kind, HostCredentialKind::ClaudeOAuth);
        assert_eq!(
            official_endpoint(ToolKind::Claude, &credential).expect("endpoint"),
            "https://api.anthropic.com"
        );
        let expired = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "expired",
                "expiresAt": now_ms() - 1
            }
        });
        assert!(claude_oauth_from_value(&expired, "test".to_string()).is_none());

        let directory = tempfile::tempdir().expect("tempdir");
        let settings = directory.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"third-party-token","ANTHROPIC_BASE_URL":"https://proxy.example"}}"#,
        )
        .expect("write");
        assert!(claude_settings_bearer(&settings).is_none());
        std::fs::write(
            &settings,
            r#"{"env":{"CLAUDE_CODE_OAUTH_TOKEN":"official-token","ANTHROPIC_BASE_URL":"https://api.anthropic.com"}}"#,
        )
        .expect("write");
        assert_eq!(
            claude_settings_bearer(&settings)
                .expect("official bearer")
                .kind,
            HostCredentialKind::ClaudeOAuth
        );
    }

    #[tokio::test]
    async fn real_owner_http_response_updates_the_bound_person_and_model() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let signed_request = request(&secret);
        let state = state_for(&signed_request, &secret);
        let history_directory = tempfile::tempdir().expect("history directory");
        let history_path = history_directory.path().join("usage-history.jsonl");
        state.inner.lock().expect("runtime").usage_history_path = Some(history_path.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut incoming = vec![0_u8; 16 * 1024];
            let size = stream.read(&mut incoming).await.expect("read");
            let request_text = String::from_utf8_lossy(&incoming[..size]);
            assert!(request_text.contains("authorization: Bearer sk-owner-test"));
            assert!(request_text.contains("POST /v1/responses HTTP/1.1"));
            let body = r#"{"id":"response-test","usage":{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":700}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.expect("write");
        });
        let response = execute_host_request_with(
            &state,
            signed_request,
            &HostCredential {
                secret: "sk-owner-test".to_string(),
                account_id: None,
                kind: HostCredentialKind::ApiKey,
                source: "test".to_string(),
            },
            &format!("http://{address}"),
        )
        .await
        .expect("relay response");
        server.await.expect("server");
        assert_eq!(response.status_code, 200);

        let runtime = state.inner.lock().expect("runtime");
        let usage = &runtime.active_car.as_ref().expect("car").seats[0].usage;
        assert_eq!(usage.models.len(), 1);
        assert_eq!(usage.models[0].model, "gpt-5.6-luna");
        assert_eq!(usage.models[0].input_tokens, 300);
        assert_eq!(usage.models[0].cache_read_tokens, 700);
        assert_eq!(usage.models[0].output_tokens, 200);
        assert!(usage.models[0].official_cost_microusd.is_some());
        drop(runtime);

        let history = crate::usage_history::read_all(&history_path).expect("usage history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].nickname, "friend");
        assert_eq!(history[0].passenger_peer_id, "passenger");
        assert_eq!(history[0].model, "gpt-5.6-luna");
        assert_eq!(history[0].input_tokens, 300);
        assert_eq!(history[0].cache_read_tokens, 700);
        assert_eq!(history[0].output_tokens, 200);
    }

    #[tokio::test]
    async fn owner_streams_official_chunks_before_end_and_records_usage() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let signed_request = request(&secret);
        let state = state_for(&signed_request, &secret);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let first = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        let second = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":200,\"input_tokens_details\":{\"cached_tokens\":700}}}}\n\n";
        let expected = format!("{first}{second}");
        let expected_for_server = expected.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut incoming = vec![0_u8; 16 * 1024];
            let _ = stream.read(&mut incoming).await.expect("read");
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                expected_for_server.len()
            );
            stream.write_all(headers.as_bytes()).await.expect("headers");
            stream
                .write_all(first.as_bytes())
                .await
                .expect("first chunk");
            stream.flush().await.expect("flush first chunk");
            tokio::time::sleep(Duration::from_millis(30)).await;
            stream
                .write_all(second.as_bytes())
                .await
                .expect("second chunk");
        });
        let events = Arc::new(std::sync::Mutex::new(Vec::<RelayStreamEvent>::new()));
        let captured = events.clone();
        execute_host_request_stream_with(
            &state,
            signed_request,
            &HostCredential {
                secret: "sk-owner-test".to_string(),
                account_id: None,
                kind: HostCredentialKind::ApiKey,
                source: "test".to_string(),
            },
            &format!("http://{address}"),
            move |event| {
                captured.lock().expect("events").push(event);
                Ok(())
            },
        )
        .await
        .expect("stream relay");
        server.await.expect("server");
        let events = events.lock().expect("events");
        assert_eq!(
            events.first().map(|event| event.kind),
            Some(RelayStreamKind::Start)
        );
        assert_eq!(
            events.last().map(|event| event.kind),
            Some(RelayStreamKind::End)
        );
        let response_body = events
            .iter()
            .filter(|event| event.kind == RelayStreamKind::Chunk)
            .flat_map(|event| {
                general_purpose::STANDARD
                    .decode(event.chunk_base64.as_deref().expect("chunk"))
                    .expect("base64")
            })
            .collect::<Vec<_>>();
        assert_eq!(response_body, expected.as_bytes());
        let expected_hash = sha256_label(expected.as_bytes());
        assert_eq!(
            events.last().and_then(|event| event.body_sha256.as_deref()),
            Some(expected_hash.as_str())
        );
        drop(events);
        let runtime = state.inner.lock().expect("runtime");
        let usage = &runtime.active_car.as_ref().expect("car").seats[0].usage;
        assert_eq!(usage.models[0].input_tokens, 300);
        assert_eq!(usage.models[0].cache_read_tokens, 700);
        assert_eq!(usage.models[0].output_tokens, 200);
    }

    #[tokio::test]
    async fn codex_chatgpt_oauth_uses_the_official_codex_path_and_account_header() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let signed_request = request(&secret);
        let state = state_for(&signed_request, &secret);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut incoming = vec![0_u8; 16 * 1024];
            let size = stream.read(&mut incoming).await.expect("read");
            let request_text = String::from_utf8_lossy(&incoming[..size]).to_ascii_lowercase();
            assert!(request_text.contains("post /responses http/1.1"));
            assert!(request_text.contains("authorization: bearer oauth-official"));
            assert!(request_text.contains("chatgpt-account-id: account-1"));
            let body = r#"{"usage":{"input_tokens":2,"output_tokens":1}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.expect("write");
        });
        execute_host_request_with(
            &state,
            signed_request,
            &HostCredential {
                secret: "oauth-official".to_string(),
                account_id: Some("account-1".to_string()),
                kind: HostCredentialKind::CodexChatGptOAuth,
                source: "test".to_string(),
            },
            &format!("http://{address}"),
        )
        .await
        .expect("relay response");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn four_people_are_accounted_independently_during_parallel_use() {
        let now = now_ms();
        let state = RuntimeState::default();
        let codes = [
            "7G2K5LQ8M4TZ",
            "M9Q3TP7W6KXR",
            "CR8W4N2HJ7KM",
            "K2J7HX9P4WQM",
        ];
        let mut requests = Vec::new();
        let mut seats = Vec::new();
        let mut bindings = Vec::new();
        for (index, code) in codes.iter().enumerate() {
            let secret = crate::protocol::new_session_secret().expect("secret");
            let signed = request(&secret);
            seats.push(Seat {
                seat_no: (index + 1) as u8,
                code: (*code).to_string(),
                nickname: Some(format!("friend-{}", index + 1)),
                state: SeatState::Connected,
                tool: None,
                usage: SeatUsageSummary::default(),
            });
            bindings.push((
                (*code).to_string(),
                HostSeatBinding {
                    code: (*code).to_string(),
                    claim_id: format!("claim-{index}"),
                    passenger_peer_id: format!("passenger-{index}"),
                    passenger_encryption_public_key: format!("key-{index}"),
                    access_id: signed.access_id.clone(),
                    session_secret: secret,
                    issued_at_ms: now,
                },
            ));
            requests.push(signed);
        }
        {
            let mut runtime = state.inner.lock().expect("runtime");
            runtime.active_car = Some(CarSession {
                car_id: "car-four".to_string(),
                car_name: "four friends".to_string(),
                owner_peer_id: "owner".to_string(),
                started_at: now - 1_000,
                expires_at: now + 60_000,
                enabled_tools: vec![ToolKind::Codex],
                seats,
            });
            runtime.host_bindings.extend(bindings);
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                tokio::spawn(async move {
                    let mut incoming = vec![0_u8; 16 * 1024];
                    let _ = stream.read(&mut incoming).await.expect("read");
                    let body = r#"{"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":40}}}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).await.expect("write");
                });
            }
        });
        let credential = HostCredential {
            secret: "sk-owner-test".to_string(),
            account_id: None,
            kind: HostCredentialKind::ApiKey,
            source: "test".to_string(),
        };
        let endpoint = format!("http://{address}");
        let (first, second, third, fourth) = tokio::join!(
            execute_host_request_with(&state, requests[0].clone(), &credential, &endpoint),
            execute_host_request_with(&state, requests[1].clone(), &credential, &endpoint),
            execute_host_request_with(&state, requests[2].clone(), &credential, &endpoint),
            execute_host_request_with(&state, requests[3].clone(), &credential, &endpoint),
        );
        for result in [first, second, third, fourth] {
            assert_eq!(result.expect("relay").status_code, 200);
        }
        server.await.expect("server");
        let runtime = state.inner.lock().expect("runtime");
        let car = runtime.active_car.as_ref().expect("car");
        assert_eq!(car.seats.len(), 4);
        for seat in &car.seats {
            assert_eq!(seat.usage.request_count, 1);
            assert_eq!(seat.usage.models.len(), 1);
            assert_eq!(seat.usage.models[0].input_tokens, 60);
            assert_eq!(seat.usage.models[0].cache_read_tokens, 40);
            assert_eq!(seat.usage.models[0].output_tokens, 20);
        }
    }
}
