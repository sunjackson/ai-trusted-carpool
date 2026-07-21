use crate::account_pool::{
    codex_local_config_allows_official_api, codex_oauth_account_id, codex_oauth_expires_at_ms,
    is_official_provider_base_url, reject_non_official_provider_base_urls, AccountAuthKind,
    AccountPool,
};
use crate::account_router::{retryable_status, RouteCandidate, RouteFailure};
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
pub(crate) const RELAY_START_TIMEOUT_MS: u64 = 30_000;
const RELAY_ROUTE_START_BUDGET_MS: u64 = 25_000;

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

struct PreparedHostRequest {
    body: Vec<u8>,
    code: String,
    current_ms: i64,
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
            timeout_ms: RELAY_START_TIMEOUT_MS,
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

fn request_allows_ambiguous_retry(request: &RelayRequest) -> bool {
    request.method == "GET"
        || request.headers.iter().any(|header| {
            header.name.trim().eq_ignore_ascii_case("idempotency-key")
                && !header.value.trim().is_empty()
        })
}

fn route_failure_allows_retry(request: &RelayRequest, failure: RouteFailure) -> bool {
    matches!(
        failure,
        RouteFailure::Authentication | RouteFailure::RateLimited
    ) || request_allows_ambiguous_retry(request)
}

fn json_api_key(kind: ToolKind, path: &Path, pointers: &[&str]) -> Option<String> {
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    reject_non_official_provider_base_urls(&value, Some(kind)).ok()?;
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
    reject_non_official_provider_base_urls(&value, Some(ToolKind::Codex)).ok()?;
    let access_token = nonempty_string(&value, "/tokens/access_token")?;
    if codex_oauth_expires_at_ms(&value, &access_token)
        .is_some_and(|expires_at| expires_at <= now_ms().saturating_add(60_000))
    {
        return None;
    }
    Some(HostCredential {
        account_id: codex_oauth_account_id(&value, &access_token),
        secret: access_token,
        kind: HostCredentialKind::CodexChatGptOAuth,
        source: path.to_string_lossy().into_owned(),
    })
}

fn claude_oauth_from_value(value: &Value, source: String) -> Option<HostCredential> {
    reject_non_official_provider_base_urls(value, Some(ToolKind::Claude)).ok()?;
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
    reject_non_official_provider_base_urls(&value, Some(ToolKind::Claude)).ok()?;
    let secret = nonempty_string(&value, "/env/CLAUDE_CODE_OAUTH_TOKEN")
        .or_else(|| nonempty_string(&value, "/env/ANTHROPIC_AUTH_TOKEN"))?;
    Some(HostCredential {
        secret,
        account_id: None,
        kind: HostCredentialKind::ClaudeOAuth,
        source: path.to_string_lossy().into_owned(),
    })
}

fn claude_env_oauth() -> Option<HostCredential> {
    if !environment_allows_official_provider(ToolKind::Claude) {
        return None;
    }
    ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN"]
        .into_iter()
        .find_map(|env_name| {
            std::env::var(env_name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|secret| HostCredential {
                    secret,
                    account_id: None,
                    kind: HostCredentialKind::ClaudeOAuth,
                    source: format!("环境变量 {env_name}"),
                })
        })
}

fn environment_allows_official_provider(kind: ToolKind) -> bool {
    let env_name = match kind {
        ToolKind::Claude => "ANTHROPIC_BASE_URL",
        ToolKind::Codex => "OPENAI_BASE_URL",
    };
    let Ok(configured_base) = std::env::var(env_name) else {
        return true;
    };
    let configured_base = configured_base.trim();
    configured_base.is_empty() || is_official_provider_base_url(configured_base, Some(kind))
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

fn load_host_oauth_credential_from(
    kind: ToolKind,
    home: &Path,
    include_claude_keychain: bool,
) -> Option<HostCredential> {
    match kind {
        ToolKind::Claude => {
            let path = home.join(".claude/.credentials.json");
            read_json(&path)
                .and_then(|value| {
                    claude_oauth_from_value(&value, path.to_string_lossy().into_owned())
                })
                .or_else(|| claude_settings_bearer(&home.join(".claude/settings.json")))
                .or_else(|| {
                    include_claude_keychain
                        .then(claude_keychain_oauth)
                        .flatten()
                })
        }
        ToolKind::Codex => codex_oauth_credential(&home.join(".codex/auth.json")),
    }
}

/// Loads only subscription OAuth credentials for official quota queries.
///
/// Relay routing may intentionally prefer an API key. Quota monitoring must not:
/// API keys do not expose Claude/ChatGPT subscription windows and may be inherited
/// from an unrelated shell profile while the official CLI OAuth session is valid.
pub fn load_host_oauth_credential(kind: ToolKind) -> Option<HostCredential> {
    if !environment_allows_official_provider(kind) {
        return None;
    }
    if kind == ToolKind::Claude {
        if let Some(credential) = claude_env_oauth() {
            return Some(credential);
        }
    }
    load_host_oauth_credential_from(kind, &dirs::home_dir()?, true)
}

pub fn load_host_credential(kind: ToolKind) -> Option<HostCredential> {
    if !environment_allows_official_provider(kind) {
        return None;
    }
    let home = dirs::home_dir();
    let local_config_allows_api_key = kind != ToolKind::Codex
        || home
            .as_deref()
            .map(codex_local_config_allows_official_api)
            .unwrap_or(true);
    let env_name = match kind {
        ToolKind::Claude => "ANTHROPIC_API_KEY",
        ToolKind::Codex => "OPENAI_API_KEY",
    };
    if local_config_allows_api_key {
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
    }
    if kind == ToolKind::Claude {
        if let Some(credential) = claude_env_oauth() {
            return Some(credential);
        }
    }
    let home = home?;
    let candidates = if !local_config_allows_api_key {
        Vec::new()
    } else {
        credential_candidates(kind, &home)
    };
    if let Some(credential) = candidates.into_iter().find_map(|(path, pointers)| {
        json_api_key(kind, &path, pointers).map(|api_key| HostCredential {
            secret: api_key,
            account_id: None,
            kind: HostCredentialKind::ApiKey,
            source: path.to_string_lossy().into_owned(),
        })
    }) {
        return Some(credential);
    }
    load_host_oauth_credential_from(kind, &home, true)
}

fn missing_credential_message(tool: ToolKind) -> String {
    match tool {
        ToolKind::Claude => {
            "本机没有可用的 Claude 账号，请先导入官方 API Key 或 OAuth 授权".to_string()
        }
        ToolKind::Codex => {
            "本机没有可用的 Codex 账号，请先导入 OpenAI API Key 或 ChatGPT OAuth 授权".to_string()
        }
    }
}

fn managed_host_credential(
    candidate: crate::account_pool::AccountCandidate,
) -> Option<RouteCandidate> {
    let kind = match (candidate.tool, candidate.credential.auth_kind()) {
        (ToolKind::Claude, AccountAuthKind::ApiKey)
        | (ToolKind::Codex, AccountAuthKind::ApiKey) => HostCredentialKind::ApiKey,
        (ToolKind::Claude, AccountAuthKind::OAuth) => HostCredentialKind::ClaudeOAuth,
        (ToolKind::Codex, AccountAuthKind::OAuth) => HostCredentialKind::CodexChatGptOAuth,
    };
    if candidate
        .credential
        .is_expired_at(now_ms().saturating_add(60_000))
    {
        return None;
    }
    Some(RouteCandidate {
        id: format!("managed:{}", candidate.id),
        priority: candidate.priority,
        credential: HostCredential {
            secret: candidate.credential.secret().to_string(),
            account_id: candidate.credential.account_id().map(str::to_string),
            kind,
            source: format!("托管账号 {}", candidate.name),
        },
    })
}

fn route_candidates(state: &RuntimeState, tool: ToolKind) -> Result<Vec<RouteCandidate>, String> {
    let pool_path = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?
        .account_pool_path
        .clone();
    let mut candidates = Vec::new();
    let mut pool_failed = false;
    if let Some(path) = pool_path {
        match AccountPool::new(path).candidates(tool) {
            Ok(managed) => {
                candidates.extend(managed.into_iter().filter_map(managed_host_credential))
            }
            Err(error) => {
                pool_failed = true;
                crate::diagnostics::record(
                    "error",
                    "account-pool",
                    format!("failed to read the local account pool: {error}"),
                );
            }
        }
    }
    if let Some(credential) = load_host_credential(tool) {
        let duplicate = candidates.iter().any(|candidate| {
            candidate.credential.kind == credential.kind
                && candidate.credential.secret == credential.secret
        });
        if !duplicate {
            candidates.push(RouteCandidate {
                id: format!("local:{}", tool.command()),
                priority: u32::MAX,
                credential,
            });
        }
    }
    if candidates.is_empty() && pool_failed {
        return Err("本地账号池暂时无法读取，请打开调试模式查看详细日志".to_string());
    }
    if candidates.is_empty() {
        return Err(missing_credential_message(tool));
    }
    Ok(candidates)
}

fn reserve_route_candidates(
    state: &RuntimeState,
    candidates: Vec<RouteCandidate>,
    tool: ToolKind,
) -> Result<Vec<RouteCandidate>, String> {
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let ordered = runtime
        .account_router
        .order_and_reserve_candidates(candidates, now_ms());
    if ordered.is_empty() {
        let tool_label = match tool {
            ToolKind::Claude => "Claude",
            ToolKind::Codex => "Codex",
        };
        return Err(format!(
            "所有 {} 账号暂时处于冷却期，请稍后重试",
            tool_label
        ));
    }
    Ok(ordered)
}

fn mark_route_attempt(state: &RuntimeState, id: &str) {
    if let Ok(mut runtime) = state.inner.lock() {
        runtime.account_router.mark_attempt(id);
    }
}

fn mark_route_success(state: &RuntimeState, id: &str) {
    if let Ok(mut runtime) = state.inner.lock() {
        runtime.account_router.mark_success(id);
    }
}

fn mark_route_failure(state: &RuntimeState, id: &str, failure: RouteFailure, current_ms: i64) {
    if let Ok(mut runtime) = state.inner.lock() {
        runtime.account_router.mark_failure(id, failure, current_ms);
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
        .as_mut()
        .ok_or_else(|| "车主已经停止发车".to_string())?;
    if car.started_at > current_ms || car.expires_at <= current_ms {
        return Err("当前不在发车开放时间内".to_string());
    }
    if !car.enabled_tools.contains(&request.tool) {
        return Err("车主没有开放这个工具".to_string());
    }
    crate::quota::ensure_available(car, &binding.code, current_ms)?;
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

fn prepare_host_request(
    state: &RuntimeState,
    request: &RelayRequest,
) -> Result<PreparedHostRequest, String> {
    let current_ms = now_ms();
    let (_binding, code) = binding_for_request(state, request, current_ms)?;
    let body = decode_body(&request.body_base64, MAX_RELAY_BODY_BYTES, "请求")?;
    if sha256_label(&body) != request.body_sha256 {
        return Err("请求内容完整性校验失败".to_string());
    }
    Ok(PreparedHostRequest {
        body,
        code,
        current_ms,
    })
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
            crate::diagnostics::record(
                "error",
                "usage-history",
                format!("usage history append failed: {error}"),
            );
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
    emit: F,
) -> Result<(), String>
where
    F: FnMut(RelayStreamEvent) -> Result<(), String>,
{
    let tool = request.tool;
    let candidates = route_candidates(state, request.tool)?;
    execute_host_request_stream_routed_with(state, request, candidates, emit, |candidate| {
        official_endpoint(tool, &candidate.credential).map(str::to_string)
    })
    .await
}

async fn execute_host_request_stream_routed_with<F, E>(
    state: &RuntimeState,
    request: RelayRequest,
    candidates: Vec<RouteCandidate>,
    emit: F,
    endpoint_for: E,
) -> Result<(), String>
where
    F: FnMut(RelayStreamEvent) -> Result<(), String>,
    E: Fn(&RouteCandidate) -> Result<String, String>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(RELAY_ROUTE_START_BUDGET_MS);
    execute_host_request_stream_routed_before(
        state,
        request,
        candidates,
        emit,
        endpoint_for,
        deadline,
    )
    .await
}

async fn execute_host_request_stream_routed_before<F, E>(
    state: &RuntimeState,
    request: RelayRequest,
    candidates: Vec<RouteCandidate>,
    mut emit: F,
    endpoint_for: E,
    deadline: tokio::time::Instant,
) -> Result<(), String>
where
    F: FnMut(RelayStreamEvent) -> Result<(), String>,
    E: Fn(&RouteCandidate) -> Result<String, String>,
{
    let prepared = prepare_host_request(state, &request)?;
    let candidates = reserve_route_candidates(state, candidates, request.tool)?;
    let candidate_count = candidates.len();
    let mut last_error = None;
    for (index, candidate) in candidates.into_iter().enumerate() {
        if index > 0 {
            mark_route_attempt(state, &candidate.id);
        }
        let endpoint = endpoint_for(&candidate)?;
        let started = std::time::Instant::now();
        let builder = upstream_request(
            &request,
            prepared.body.clone(),
            &candidate.credential,
            &endpoint,
        )?;
        let mut response = match tokio::time::timeout_at(deadline, builder.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                mark_route_failure(state, &candidate.id, RouteFailure::Network, now_ms());
                crate::diagnostics::record(
                    "warn",
                    "account-router",
                    format!(
                        "{} upstream connection failed; local account entered cooldown: {error}",
                        request.tool.command()
                    ),
                );
                last_error = Some(format!("车主连接官方 API 失败: {error}"));
                if index + 1 < candidate_count
                    && route_failure_allows_retry(&request, RouteFailure::Network)
                {
                    continue;
                }
                break;
            }
            Err(_) => {
                mark_route_failure(state, &candidate.id, RouteFailure::Network, now_ms());
                crate::diagnostics::record(
                    "warn",
                    "account-router",
                    format!(
                        "{} route start budget expired; upstream request was cancelled",
                        request.tool.command()
                    ),
                );
                return Err("等待官方 API 响应超时，已停止账号切换".to_string());
            }
        };
        let status_code = response.status().as_u16();
        let retry_failure = retryable_status(status_code);
        if let Some(failure) = retry_failure {
            mark_route_failure(state, &candidate.id, failure, now_ms());
            crate::diagnostics::record(
                "warn",
                "account-router",
                format!(
                    "{} upstream returned HTTP {status_code}; local account entered cooldown",
                    request.tool.command()
                ),
            );
            if index + 1 < candidate_count && route_failure_allows_retry(&request, failure) {
                continue;
            }
        }
        let result = emit_stream_response_with_started(
            state,
            &request,
            &prepared,
            &mut response,
            started,
            &mut emit,
        )
        .await;
        match result {
            Ok(()) => {
                if retry_failure.is_none() {
                    mark_route_success(state, &candidate.id);
                }
                return Ok(());
            }
            Err(error) => {
                if retry_failure.is_none() {
                    if let Some(failure) = error.route_failure {
                        mark_route_failure(state, &candidate.id, failure, now_ms());
                        crate::diagnostics::record(
                            "warn",
                            "account-router",
                            format!(
                                "{} upstream stream failed after response start; local account entered cooldown",
                                request.tool.command()
                            ),
                        );
                    }
                }
                return Err(error.message);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| missing_credential_message(request.tool)))
}

#[cfg(test)]
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
    let prepared = prepare_host_request(state, &request)?;
    let started = std::time::Instant::now();
    let mut response = upstream_request(&request, prepared.body.clone(), credential, endpoint)?
        .send()
        .await
        .map_err(|error| format!("车主连接官方 API 失败: {error}"))?;
    emit_stream_response_with_started(
        state,
        &request,
        &prepared,
        &mut response,
        started,
        &mut emit,
    )
    .await
    .map_err(|error| error.message)
}

struct StreamResponseError {
    message: String,
    route_failure: Option<RouteFailure>,
}

impl StreamResponseError {
    fn local(message: String) -> Self {
        Self {
            message,
            route_failure: None,
        }
    }

    fn network(message: String) -> Self {
        Self {
            message,
            route_failure: Some(RouteFailure::Network),
        }
    }
}

async fn emit_stream_response_with_started<F>(
    state: &RuntimeState,
    request: &RelayRequest,
    prepared: &PreparedHostRequest,
    response: &mut reqwest::Response,
    started: std::time::Instant,
    emit: &mut F,
) -> Result<(), StreamResponseError>
where
    F: FnMut(RelayStreamEvent) -> Result<(), String>,
{
    let status_code = response.status().as_u16();
    emit(RelayStreamEvent {
        request_id: request.request_id.clone(),
        kind: RelayStreamKind::Start,
        status_code: Some(status_code),
        headers: response_headers(response),
        chunk_base64: None,
        body_sha256: None,
        latency_ms: None,
        error: None,
    })
    .map_err(StreamResponseError::local)?;
    let mut response_body = Vec::new();
    loop {
        let chunk = response.chunk().await.map_err(|error| {
            StreamResponseError::network(format!("读取官方 API 流式响应失败: {error}"))
        })?;
        let Some(chunk) = chunk else {
            break;
        };
        if response_body.len().saturating_add(chunk.len()) > MAX_RELAY_RESPONSE_BYTES {
            return Err(StreamResponseError::local(
                "官方 API 响应过大，已安全中止".to_string(),
            ));
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
        })
        .map_err(StreamResponseError::local)?;
    }
    let body_sha256 = sha256_label(&response_body);
    record_usage(
        state,
        request,
        &prepared.code,
        &prepared.body,
        &response_body,
        status_code,
        prepared.current_ms,
    )
    .map_err(StreamResponseError::local)?;
    emit(RelayStreamEvent {
        request_id: request.request_id.clone(),
        kind: RelayStreamKind::End,
        status_code: None,
        headers: Vec::new(),
        chunk_base64: None,
        body_sha256: Some(body_sha256),
        latency_ms: Some(started.elapsed().as_millis() as u64),
        error: None,
    })
    .map_err(StreamResponseError::local)?;
    Ok(())
}

pub async fn execute_host_request(
    state: &RuntimeState,
    request: RelayRequest,
) -> Result<RelayResponse, String> {
    let tool = request.tool;
    let candidates = route_candidates(state, request.tool)?;
    execute_host_request_routed_with(state, request, candidates, |candidate| {
        official_endpoint(tool, &candidate.credential).map(str::to_string)
    })
    .await
}

async fn execute_host_request_routed_with<E>(
    state: &RuntimeState,
    request: RelayRequest,
    candidates: Vec<RouteCandidate>,
    endpoint_for: E,
) -> Result<RelayResponse, String>
where
    E: Fn(&RouteCandidate) -> Result<String, String>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(RELAY_ROUTE_START_BUDGET_MS);
    execute_host_request_routed_before(state, request, candidates, endpoint_for, deadline).await
}

async fn execute_host_request_routed_before<E>(
    state: &RuntimeState,
    request: RelayRequest,
    candidates: Vec<RouteCandidate>,
    endpoint_for: E,
    deadline: tokio::time::Instant,
) -> Result<RelayResponse, String>
where
    E: Fn(&RouteCandidate) -> Result<String, String>,
{
    let prepared = prepare_host_request(state, &request)?;
    let candidates = reserve_route_candidates(state, candidates, request.tool)?;
    let candidate_count = candidates.len();
    let mut last_error = None;
    for (index, candidate) in candidates.into_iter().enumerate() {
        if index > 0 {
            mark_route_attempt(state, &candidate.id);
        }
        let endpoint = endpoint_for(&candidate)?;
        match tokio::time::timeout_at(
            deadline,
            execute_upstream(
                &request,
                prepared.body.clone(),
                &candidate.credential,
                &endpoint,
            ),
        )
        .await
        {
            Ok(Ok(response)) => {
                let retry_failure = retryable_status(response.status_code);
                if let Some(failure) = retry_failure {
                    mark_route_failure(state, &candidate.id, failure, now_ms());
                    crate::diagnostics::record(
                        "warn",
                        "account-router",
                        format!(
                            "{} upstream returned HTTP {}; local account entered cooldown",
                            request.tool.command(),
                            response.status_code
                        ),
                    );
                    if index + 1 < candidate_count && route_failure_allows_retry(&request, failure)
                    {
                        continue;
                    }
                } else {
                    mark_route_success(state, &candidate.id);
                }
                record_prepared_response(state, &request, &prepared, &response)?;
                return Ok(response);
            }
            Ok(Err(error)) => {
                mark_route_failure(state, &candidate.id, RouteFailure::Network, now_ms());
                crate::diagnostics::record(
                    "warn",
                    "account-router",
                    format!(
                        "{} upstream request failed; local account entered cooldown: {error}",
                        request.tool.command()
                    ),
                );
                last_error = Some(error);
                if index + 1 < candidate_count
                    && route_failure_allows_retry(&request, RouteFailure::Network)
                {
                    continue;
                }
            }
            Err(_) => {
                mark_route_failure(state, &candidate.id, RouteFailure::Network, now_ms());
                crate::diagnostics::record(
                    "warn",
                    "account-router",
                    format!(
                        "{} route budget expired; upstream request was cancelled",
                        request.tool.command()
                    ),
                );
                return Err("等待官方 API 响应超时，已停止账号切换".to_string());
            }
        }
    }
    Err(last_error.unwrap_or_else(|| missing_credential_message(request.tool)))
}

#[cfg(test)]
pub(crate) async fn execute_host_request_with(
    state: &RuntimeState,
    request: RelayRequest,
    credential: &HostCredential,
    endpoint: &str,
) -> Result<RelayResponse, String> {
    let prepared = prepare_host_request(state, &request)?;
    let response = execute_upstream(&request, prepared.body.clone(), credential, endpoint).await?;
    record_prepared_response(state, &request, &prepared, &response)?;
    Ok(response)
}

fn record_prepared_response(
    state: &RuntimeState,
    request: &RelayRequest,
    prepared: &PreparedHostRequest,
    response: &RelayResponse,
) -> Result<(), String> {
    if let Ok(response_body) = decode_body(&response.body_base64, MAX_RELAY_RESPONSE_BYTES, "响应")
    {
        record_usage(
            state,
            request,
            &prepared.code,
            &prepared.body,
            &response_body,
            response.status_code,
            prepared.current_ms,
        )?;
    }
    Ok(())
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
                token_limits: crate::models::MemberTokenLimits::default(),
                token_limit_status: crate::models::MemberTokenLimitStatus::default(),
                token_usage_events: Vec::new(),
            }],
            account_quotas: Vec::new(),
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

    fn routed_candidate(id: &str, secret: &str, priority: u32) -> RouteCandidate {
        RouteCandidate {
            id: id.to_string(),
            priority,
            credential: HostCredential {
                secret: secret.to_string(),
                account_id: None,
                kind: HostCredentialKind::ApiKey,
                source: "test".to_string(),
            },
        }
    }

    async fn sequence_server(statuses: Vec<u16>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut incoming = vec![0_u8; 16 * 1024];
                let size = stream.read(&mut incoming).await.expect("read");
                requests.push(String::from_utf8_lossy(&incoming[..size]).into_owned());
                let body = if status < 400 {
                    r#"{"id":"routed-success"}"#
                } else {
                    r#"{"error":{"message":"candidate unavailable"}}"#
                };
                let response = format!(
                    "HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
            requests
        });
        (format!("http://{address}"), server)
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
    fn concurrent_route_reservations_balance_equal_priority_accounts() {
        let state = RuntimeState::default();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve_route_candidates(
                        &state,
                        vec![
                            routed_candidate("a", "first-account", 10),
                            routed_candidate("b", "second-account", 10),
                        ],
                        ToolKind::Codex,
                    )
                    .expect("reserved candidates")[0]
                        .id
                        .clone()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let mut primaries = handles
            .into_iter()
            .map(|handle| handle.join().expect("reservation thread"))
            .collect::<Vec<_>>();
        primaries.sort();

        assert_eq!(primaries, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn buffered_routing_fails_over_retryable_statuses_in_priority_order() {
        for status in [401, 403, 429] {
            let secret = crate::protocol::new_session_secret().expect("secret");
            let request = request(&secret);
            let state = state_for(&request, &secret);
            let (endpoint, server) = sequence_server(vec![status, 200]).await;
            let candidates = vec![
                routed_candidate("primary", "first-account", 10),
                routed_candidate("backup", "second-account", 20),
            ];

            let response = execute_host_request_routed_with(&state, request, candidates, |_| {
                Ok(endpoint.clone())
            })
            .await
            .expect("backup response");

            assert_eq!(response.status_code, 200, "status {status}");
            let requests = server.await.expect("server");
            assert_eq!(requests.len(), 2, "status {status}");
            assert!(requests[0].contains("authorization: Bearer first-account"));
            assert!(requests[1].contains("authorization: Bearer second-account"));
        }
    }

    #[tokio::test]
    async fn buffered_post_does_not_replay_ambiguous_failures_without_idempotency() {
        for status in [408, 500, 503] {
            let secret = crate::protocol::new_session_secret().expect("secret");
            let request = request(&secret);
            let state = state_for(&request, &secret);
            let (endpoint, server) = sequence_server(vec![status]).await;
            let response = execute_host_request_routed_with(
                &state,
                request,
                vec![
                    routed_candidate("primary", "first-account", 10),
                    routed_candidate("backup", "second-account", 20),
                ],
                |_| Ok(endpoint.clone()),
            )
            .await
            .expect("ambiguous response should be returned without replay");

            assert_eq!(response.status_code, status);
            let requests = server.await.expect("server");
            assert_eq!(requests.len(), 1, "status {status}");
        }
    }

    #[tokio::test]
    async fn buffered_post_replays_ambiguous_failures_with_idempotency() {
        for status in [408, 500, 503] {
            let secret = crate::protocol::new_session_secret().expect("secret");
            let mut request = request(&secret);
            request.headers.push(RelayHeader {
                name: "idempotency-key".to_string(),
                value: "request-123".to_string(),
            });
            sign_request(&mut request, &secret).expect("resign");
            let state = state_for(&request, &secret);
            let (endpoint, server) = sequence_server(vec![status, 200]).await;
            let response = execute_host_request_routed_with(
                &state,
                request,
                vec![
                    routed_candidate("primary", "first-account", 10),
                    routed_candidate("backup", "second-account", 20),
                ],
                |_| Ok(endpoint.clone()),
            )
            .await
            .expect("idempotent response should fail over");

            assert_eq!(response.status_code, 200);
            let requests = server.await.expect("server");
            assert_eq!(requests.len(), 2, "status {status}");
        }
    }

    #[test]
    fn ambiguous_route_failures_require_an_idempotency_key_for_post() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let mut request = request(&secret);
        assert!(!route_failure_allows_retry(&request, RouteFailure::Network));
        assert!(!route_failure_allows_retry(
            &request,
            RouteFailure::Upstream
        ));
        assert!(route_failure_allows_retry(
            &request,
            RouteFailure::Authentication
        ));
        request.headers.push(RelayHeader {
            name: "idempotency-key".to_string(),
            value: "request-123".to_string(),
        });
        assert!(route_failure_allows_retry(&request, RouteFailure::Network));
        assert!(route_failure_allows_retry(&request, RouteFailure::Upstream));
    }

    #[tokio::test]
    async fn route_start_budget_cancels_before_trying_another_account() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let request = request(&secret);
        let state = state_for(&request, &secret);
        let mut events = Vec::new();
        let error = execute_host_request_stream_routed_before(
            &state,
            request,
            vec![
                routed_candidate("primary", "first-account", 10),
                routed_candidate("backup", "second-account", 20),
            ],
            |event| {
                events.push(event);
                Ok(())
            },
            |_| Ok("http://127.0.0.1:9".to_string()),
            tokio::time::Instant::now() - Duration::from_millis(1),
        )
        .await
        .expect_err("expired route budget");

        assert!(error.contains("等待官方 API 响应超时"));
        assert!(events.is_empty());
    }

    #[test]
    fn cooled_accounts_report_a_cooldown_instead_of_missing_credentials() {
        let state = RuntimeState::default();
        {
            let mut runtime = state.inner.lock().expect("runtime");
            runtime
                .account_router
                .mark_failure("primary", RouteFailure::Network, now_ms());
        }
        let error = reserve_route_candidates(
            &state,
            vec![routed_candidate("primary", "first-account", 10)],
            ToolKind::Codex,
        )
        .err()
        .expect("cooled account");
        assert!(error.contains("暂时处于冷却期"));
    }

    #[tokio::test]
    async fn streaming_routing_hides_failed_candidate_before_start() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let request = request(&secret);
        let state = state_for(&request, &secret);
        let (endpoint, server) = sequence_server(vec![429, 200]).await;
        let candidates = vec![
            routed_candidate("primary", "rate-limited", 10),
            routed_candidate("backup", "healthy", 20),
        ];
        let mut events = Vec::new();

        execute_host_request_stream_routed_with(
            &state,
            request,
            candidates,
            |event| {
                events.push(event);
                Ok(())
            },
            |_| Ok(endpoint.clone()),
        )
        .await
        .expect("streamed backup response");

        let starts = events
            .iter()
            .filter(|event| event.kind == RelayStreamKind::Start)
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].status_code, Some(200));
        assert_eq!(
            events.last().map(|event| event.kind),
            Some(RelayStreamKind::End)
        );
        let requests = server.await.expect("server");
        assert!(requests[0].contains("authorization: Bearer rate-limited"));
        assert!(requests[1].contains("authorization: Bearer healthy"));
    }

    #[tokio::test]
    async fn streaming_read_failure_after_start_cools_account_without_retrying() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let request = request(&secret);
        let state = state_for(&request, &secret);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut incoming = vec![0_u8; 16 * 1024];
            let _ = stream.read(&mut incoming).await.expect("read");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 64\r\nconnection: close\r\n\r\npartial",
                )
                .await
                .expect("truncated response");
            stream.shutdown().await.expect("shutdown");
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_ok()
        });
        let candidates = vec![
            routed_candidate("broken", "broken-account", 10),
            routed_candidate("backup", "unused-backup", 20),
        ];
        let mut events = Vec::new();

        let error = execute_host_request_stream_routed_with(
            &state,
            request,
            candidates,
            |event| {
                events.push(event);
                Ok(())
            },
            |_| Ok(format!("http://{address}")),
        )
        .await
        .expect_err("truncated stream must fail");

        assert!(error.contains("读取官方 API 流式响应失败"));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == RelayStreamKind::Start)
                .count(),
            1
        );
        assert!(!events
            .iter()
            .any(|event| event.kind == RelayStreamKind::End));
        assert!(!server.await.expect("server"), "must not retry after Start");
        let mut runtime = state.inner.lock().expect("runtime");
        assert!(runtime
            .account_router
            .order_candidates(
                vec![routed_candidate("broken", "broken-account", 10)],
                now_ms()
            )
            .is_empty());
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

    #[tokio::test]
    async fn exhausted_member_token_window_is_rejected_before_any_upstream_call() {
        let secret = crate::protocol::new_session_secret().expect("secret");
        let request = request(&secret);
        let state = state_for(&request, &secret);
        {
            let mut runtime = state.inner.lock().expect("runtime");
            let seat = &mut runtime.active_car.as_mut().expect("car").seats[0];
            seat.token_limits.five_hour_tokens = Some(1);
            seat.token_usage_events
                .push(crate::models::TokenUsageEvent {
                    occurred_at: now_ms() - 1_000,
                    tokens: 1,
                });
        }
        let credential = HostCredential {
            secret: "not-used".to_string(),
            account_id: None,
            kind: HostCredentialKind::ApiKey,
            source: "test".to_string(),
        };
        let error = execute_host_request_with(&state, request, &credential, "http://127.0.0.1:9")
            .await
            .expect_err("quota should block before network");
        assert!(error.contains("5 小时限额已用完"));
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
        assert!(json_api_key(ToolKind::Codex, &path, &["/OPENAI_API_KEY"]).is_none());
        let oauth = codex_oauth_credential(&path).expect("official OAuth");
        assert_eq!(oauth.kind, HostCredentialKind::CodexChatGptOAuth);
        assert_eq!(oauth.account_id.as_deref(), Some("account-1"));
        assert_eq!(
            official_endpoint(ToolKind::Codex, &oauth).expect("endpoint"),
            "https://chatgpt.com/backend-api/codex"
        );
        std::fs::write(&path, r#"{"OPENAI_API_KEY":"sk-official"}"#).expect("write");
        assert_eq!(
            json_api_key(ToolKind::Codex, &path, &["/OPENAI_API_KEY"]).as_deref(),
            Some("sk-official")
        );

        std::fs::write(
            &path,
            r#"{"OPENAI_API_KEY":"proxy-key","OPENAI_BASE_URL":"https://proxy.example"}"#,
        )
        .expect("write custom provider");
        assert!(json_api_key(ToolKind::Codex, &path, &["/OPENAI_API_KEY"]).is_none());
    }

    #[test]
    fn codex_oauth_requires_an_unexpired_official_access_token() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("auth.json");
        let token = |expires_at_seconds: i64| {
            let payload = serde_json::json!({
                "exp": expires_at_seconds,
                "https://api.openai.com/auth": {"chatgpt_account_id": "jwt-account"}
            });
            format!(
                "header.{}.signature",
                general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
            )
        };

        std::fs::write(
            &path,
            serde_json::json!({
                "tokens": {"access_token": token(now_ms() / 1_000 + 30)}
            })
            .to_string(),
        )
        .expect("write expiring OAuth");
        assert!(codex_oauth_credential(&path).is_none());

        std::fs::write(
            &path,
            serde_json::json!({
                "tokens": {"access_token": token(now_ms() / 1_000 + 120)}
            })
            .to_string(),
        )
        .expect("write valid OAuth");
        let credential = codex_oauth_credential(&path).expect("valid OAuth");
        assert_eq!(credential.account_id.as_deref(), Some("jwt-account"));

        std::fs::write(
            &path,
            serde_json::json!({
                "OPENAI_BASE_URL": "https://proxy.example",
                "tokens": {"access_token": token(now_ms() / 1_000 + 120)}
            })
            .to_string(),
        )
        .expect("write custom provider OAuth");
        assert!(codex_oauth_credential(&path).is_none());
    }

    #[test]
    fn quota_oauth_loader_ignores_codex_api_key_in_the_same_auth_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let codex = directory.path().join(".codex");
        std::fs::create_dir_all(&codex).expect("create codex directory");
        std::fs::write(
            codex.join("auth.json"),
            r#"{
              "OPENAI_API_KEY":"sk-unrelated-api-key",
              "tokens":{"access_token":"oauth-official","account_id":"account-1"}
            }"#,
        )
        .expect("write");

        let credential = load_host_oauth_credential_from(ToolKind::Codex, directory.path(), false)
            .expect("OAuth credential");
        assert_eq!(credential.kind, HostCredentialKind::CodexChatGptOAuth);
        assert_eq!(credential.secret, "oauth-official");
        assert_eq!(credential.account_id.as_deref(), Some("account-1"));
    }

    #[test]
    fn quota_oauth_loader_reads_claude_credentials_without_accepting_custom_base_tokens() {
        let directory = tempfile::tempdir().expect("tempdir");
        let claude = directory.path().join(".claude");
        std::fs::create_dir_all(&claude).expect("create claude directory");
        std::fs::write(
            claude.join("settings.json"),
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"proxy-token","ANTHROPIC_BASE_URL":"https://proxy.example"}}"#,
        )
        .expect("write settings");
        assert!(
            load_host_oauth_credential_from(ToolKind::Claude, directory.path(), false).is_none()
        );

        std::fs::write(
            claude.join(".credentials.json"),
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"oauth-official","expiresAt":{}}}}}"#,
                now_ms() + 120_000
            ),
        )
        .expect("write credentials");
        let credential = load_host_oauth_credential_from(ToolKind::Claude, directory.path(), false)
            .expect("OAuth credential");
        assert_eq!(credential.kind, HostCredentialKind::ClaudeOAuth);
        assert_eq!(credential.secret, "oauth-official");
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
                token_limits: crate::models::MemberTokenLimits::default(),
                token_limit_status: crate::models::MemberTokenLimitStatus::default(),
                token_usage_events: Vec::new(),
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
                account_quotas: Vec::new(),
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
