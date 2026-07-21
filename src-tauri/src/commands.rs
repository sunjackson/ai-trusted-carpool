use crate::account_pool::{
    AccountPool, AccountSource, AccountSummary, ImportOptions, ImportResult, UpdateAccountInput,
};
use crate::coordinator::{CoordinatorClient, CoordinatorMessage, IceServer, PublicInvitePayload};
use crate::crypto::{decrypt_access, encrypt_access, EncryptedEnvelope};
use crate::identity::{load_or_create, DeviceIdentity, PublicIdentity};
use crate::local_proxy::LOCAL_PROXY_PORT;
use crate::models::*;
use crate::protocol::{
    new_session_secret, AccessGrant, CarpoolClaim, LeaveNotice, CLAIM_TTL_MS, PROTOCOL_VERSION,
};
use crate::relay::{
    execute_host_request, start_host_request_stream, RelayBridge, RelayRequest, RelayResponse,
    RelayStreamEvent,
};
use crate::runtime::{HostSeatBinding, PassengerAccessContext, RuntimeState};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};
use tokio::time::{sleep, Instant};
use uuid::Uuid;

const JOIN_TIMEOUT_SECONDS: u64 = 20;
const SIGNAL_PAYLOAD_LIMIT: usize = 48 * 1024;
const PENDING_SIGNAL_LIMIT: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendWebRtcSignalInput {
    pub to_peer_id: String,
    pub kind: String,
    pub payload_json: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportAccountsInput {
    pub content: String,
    #[serde(default)]
    pub tool: Option<ToolKind>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub source: Option<ManualAccountSource>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManualAccountSource {
    Json,
    File,
}

impl From<ManualAccountSource> for AccountSource {
    fn from(source: ManualAccountSource) -> Self {
        match source {
            ManualAccountSource::Json => AccountSource::Json,
            ManualAccountSource::File => AccountSource::File,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateManagedAccountInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub priority: Option<i32>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn account_pool(state: &RuntimeState) -> Result<AccountPool, String> {
    let path = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?
        .account_pool_path
        .clone()
        .ok_or_else(|| "本地账号存储尚未初始化".to_string())?;
    Ok(AccountPool::new(path))
}

fn clear_managed_account_routes<'a>(
    state: &RuntimeState,
    account_ids: impl IntoIterator<Item = &'a str>,
) {
    if let Ok(mut runtime) = state.inner.lock() {
        for id in account_ids {
            runtime.account_router.remove(&format!("managed:{id}"));
        }
    }
}

fn account_update_resets_route_health(input: &UpdateManagedAccountInput) -> bool {
    input.enabled.is_some()
}

async fn query_account_quotas(
    tools: &[ToolKind],
    account_pool_path: Option<&Path>,
) -> Vec<AccountQuotaSnapshot> {
    let has_claude = tools.contains(&ToolKind::Claude);
    let has_codex = tools.contains(&ToolKind::Codex);
    match (has_claude, has_codex) {
        (true, true) => {
            let (claude, codex) = tokio::join!(
                crate::account_quota::query(ToolKind::Claude, account_pool_path),
                crate::account_quota::query(ToolKind::Codex, account_pool_path)
            );
            tools
                .iter()
                .map(|tool| match tool {
                    ToolKind::Claude => claude.clone(),
                    ToolKind::Codex => codex.clone(),
                })
                .collect()
        }
        (true, false) => {
            vec![crate::account_quota::query(ToolKind::Claude, account_pool_path).await]
        }
        (false, true) => {
            vec![crate::account_quota::query(ToolKind::Codex, account_pool_path).await]
        }
        (false, false) => Vec::new(),
    }
}

fn merge_account_quotas(
    previous: &[AccountQuotaSnapshot],
    next: Vec<AccountQuotaSnapshot>,
) -> Vec<AccountQuotaSnapshot> {
    next.into_iter()
        .map(|mut snapshot| {
            let Some(existing) = previous.iter().find(|item| item.tool == snapshot.tool) else {
                return snapshot;
            };
            if snapshot.state == AccountQuotaState::Error && !existing.windows.is_empty() {
                snapshot.plan_name = existing.plan_name.clone();
                snapshot.fetched_at = existing.fetched_at;
                snapshot.windows = existing.windows.clone();
                snapshot.message = Some(format!(
                    "刷新失败，当前显示上次官方结果：{}",
                    snapshot.message.unwrap_or_else(|| "未知错误".to_string())
                ));
            }
            snapshot
        })
        .collect()
}

fn spawn_account_quota_loop(state: RuntimeState, car_id: String) {
    tauri::async_runtime::spawn(async move {
        loop {
            let (tools, account_pool_path) = {
                let Ok(runtime) = state.inner.lock() else {
                    break;
                };
                let Some(car) = runtime.active_car.as_ref() else {
                    break;
                };
                if car.car_id != car_id {
                    break;
                }
                (car.enabled_tools.clone(), runtime.account_pool_path.clone())
            };
            let snapshots = query_account_quotas(&tools, account_pool_path.as_deref()).await;
            {
                let Ok(mut runtime) = state.inner.lock() else {
                    break;
                };
                let Some(car) = runtime.active_car.as_mut() else {
                    break;
                };
                if car.car_id != car_id {
                    break;
                }
                car.account_quotas = merge_account_quotas(&car.account_quotas, snapshots);
            }
            sleep(Duration::from_secs(60)).await;
        }
    });
}

fn path_candidates(command: &str) -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        vec![
            format!("{command}.exe"),
            format!("{command}.cmd"),
            format!("{command}.bat"),
            command.to_string(),
        ]
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![command.to_string()]
    }
}

#[cfg(not(target_os = "windows"))]
fn push_existing_subdirs(directories: &mut Vec<PathBuf>, parent: PathBuf, suffix: &str) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join(suffix);
        if candidate.is_dir() {
            directories.push(candidate);
        }
    }
}

fn executable_search_dirs() -> Vec<PathBuf> {
    let mut directories = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        #[cfg(not(target_os = "windows"))]
        {
            directories.extend([
                home.join(".local/bin"),
                home.join(".npm-global/bin"),
                home.join(".volta/bin"),
                home.join(".asdf/shims"),
                home.join(".bun/bin"),
                home.join(".local/share/pnpm"),
                home.join("Library/pnpm"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ]);
            push_existing_subdirs(&mut directories, home.join(".nvm/versions/node"), "bin");
            push_existing_subdirs(
                &mut directories,
                home.join(".fnm/node-versions"),
                "installation/bin",
            );
        }
        #[cfg(target_os = "windows")]
        {
            directories.extend([
                home.join(".local/bin"),
                home.join(".volta/bin"),
                home.join("AppData/Roaming/npm"),
                home.join("AppData/Local/Programs/nodejs"),
            ]);
        }
    }
    #[cfg(target_os = "windows")]
    {
        for variable in ["APPDATA", "LOCALAPPDATA", "ProgramFiles"] {
            if let Some(path) = env::var_os(variable) {
                let path = PathBuf::from(path);
                directories.push(match variable {
                    "APPDATA" => path.join("npm"),
                    "LOCALAPPDATA" => path.join("Programs/nodejs"),
                    _ => path.join("nodejs"),
                });
            }
        }
    }
    let mut seen = HashSet::new();
    directories
        .into_iter()
        .filter(|directory| seen.insert(directory.clone()))
        .collect()
}

fn find_executable_in(command: &str, directories: &[PathBuf]) -> Option<PathBuf> {
    for directory in directories {
        for candidate in path_candidates(command) {
            let full_path = directory.join(candidate);
            if full_path.is_file() {
                return Some(full_path);
            }
        }
    }
    None
}

fn find_executable(command: &str) -> Option<PathBuf> {
    find_executable_in(command, &executable_search_dirs())
}

fn first_existing_path(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|path| path.exists()).cloned()
}

fn home_path(relative: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(relative)
}

fn managed_tools_root(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join("tools"))
}

/// Resolves a launchable CLI. A user-managed system install always wins; the
/// app-managed download is the zero-dependency fallback.
fn resolve_tool_executable(kind: ToolKind, managed_root: Option<&Path>) -> Option<(PathBuf, bool)> {
    if let Some(path) = find_executable(kind.command()) {
        return Some((path, false));
    }
    managed_root
        .and_then(|root| crate::tool_provisioner::managed_tool(root, kind))
        .map(|tool| (tool.path, true))
}

fn detect_tool(
    kind: ToolKind,
    managed_root: Option<&Path>,
    account_pool_path: Option<&Path>,
) -> ToolDetection {
    let system_executable = find_executable(kind.command());
    let managed = if system_executable.is_none() {
        managed_root.and_then(|root| crate::tool_provisioner::managed_tool(root, kind))
    } else {
        None
    };
    let managed_by_app = managed.is_some();
    let executable = system_executable.or_else(|| managed.as_ref().map(|tool| tool.path.clone()));
    let (name, config_candidates) = match kind {
        ToolKind::Claude => (
            "Claude Code",
            vec![
                home_path(".claude/.credentials.json"),
                home_path(".claude/settings.json"),
                home_path(".claude.json"),
                home_path(".claude"),
            ],
        ),
        ToolKind::Codex => (
            "Codex",
            vec![
                home_path(".codex/auth.json"),
                home_path(".codex/config.toml"),
            ],
        ),
    };
    let local_config = first_existing_path(&config_candidates);
    let credential = crate::relay::load_host_credential(kind);
    let pool_authenticated = account_pool_path
        .map(|path| crate::account_pool::AccountPool::new(path.to_path_buf()))
        .and_then(|pool| pool.has_enabled(kind).ok())
        .unwrap_or(false);
    let installed = executable.is_some();
    let authenticated = pool_authenticated || credential.is_some();
    let npm_available = find_executable("npm").is_some();
    let version = managed
        .as_ref()
        .map(|tool| format!("v{}", tool.version))
        .or_else(|| {
            executable
                .as_deref()
                .and_then(|path| crate::tool_installer::installed_version(kind, path))
        });
    let latest_version = managed_root
        .and_then(|root| crate::tool_provisioner::latest_known_version(root, kind))
        .map(|value| format!("v{value}"));
    let update_available = match (&managed, &latest_version) {
        (Some(current), Some(latest)) => crate::tool_provisioner::version_is_newer(
            latest.trim_start_matches('v'),
            &current.version,
        ),
        _ => false,
    };
    let detail = match (installed, authenticated, local_config.is_some()) {
        (true, true, _) => "已就绪",
        (false, _, _) => "未安装",
        (true, false, true) => "已登录，但缺少官方 API Key",
        (true, false, false) => "缺少官方 API Key",
    };
    let desktop = crate::client_launcher::detect(kind);
    ToolDetection {
        kind,
        name: name.to_string(),
        installed,
        authenticated,
        executable_path: executable.map(|path| path.to_string_lossy().into_owned()),
        config_path: if pool_authenticated {
            Some("可信拼车本地账号池".to_string())
        } else {
            credential
                .map(|value| value.source)
                .or_else(|| local_config.map(|path| path.to_string_lossy().into_owned()))
        },
        detail: detail.to_string(),
        version,
        npm_available,
        managed_by_app,
        latest_version,
        update_available,
        desktop_supported: desktop.supported,
        desktop_installed: desktop.installed,
        desktop_path: desktop.path,
        desktop_detail: desktop.detail,
    }
}

fn random_code() -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0_u8; 12];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "无法生成安全上车码".to_string())?;
    Ok(bytes
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect())
}

fn normalize_code(code: &str) -> Result<String, String> {
    let mut normalized = String::with_capacity(12);
    for character in code.chars() {
        if character == '-' || character.is_whitespace() {
            continue;
        }
        normalized.push(character.to_ascii_uppercase());
    }
    if normalized.len() != 12
        || !normalized
            .bytes()
            .all(|byte| matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'2'..=b'9'))
    {
        return Err("上车码格式不正确，应为 12 位安全码".to_string());
    }
    Ok(normalized)
}

fn preview_from_payload(payload: &PublicInvitePayload) -> Result<JoinPreview, String> {
    if payload.version != PROTOCOL_VERSION {
        return Err("上车码协议版本不受支持".to_string());
    }
    if payload.expires_at_ms <= now_ms() {
        return Err("上车码已过期".to_string());
    }
    if payload.seat_no == 0 || payload.seat_no > 4 || payload.enabled_tools.is_empty() {
        return Err("上车码公开信息无效".to_string());
    }
    Ok(JoinPreview {
        car_id: payload.car_id.clone(),
        car_name: payload.car_name.clone(),
        owner_label: payload.owner_label.clone(),
        seat_no: payload.seat_no,
        enabled_tools: payload.enabled_tools.clone(),
        starts_at: payload.starts_at_ms,
        expires_at: payload.expires_at_ms,
    })
}

fn validate_schedule(starts_at: i64, ends_at: i64, now: i64) -> Result<(), String> {
    if starts_at < now.saturating_sub(300_000)
        || starts_at > now.saturating_add(30 * 24 * 60 * 60 * 1_000)
    {
        return Err("开始时间应为现在起 30 天内".to_string());
    }
    let duration_ms = ends_at.saturating_sub(starts_at);
    if !(15 * 60_000..=24 * 60 * 60_000).contains(&duration_ms) {
        return Err("发车时间段必须在 15 分钟到 24 小时之间".to_string());
    }
    Ok(())
}

fn active_host_car(state: &RuntimeState, car_id: &str) -> bool {
    state
        .inner
        .lock()
        .ok()
        .and_then(|runtime| runtime.active_car.as_ref().map(|car| car.car_id == car_id))
        .unwrap_or(false)
}

fn access_grant_for_claim(
    state: &RuntimeState,
    identity: &DeviceIdentity,
    claim: &CarpoolClaim,
    now: i64,
) -> Result<Option<AccessGrant>, String> {
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let car = match runtime.active_car.as_ref() {
        Some(car)
            if car.car_id == claim.car_id && car.started_at <= now && car.expires_at > now =>
        {
            car.clone()
        }
        _ => return Ok(None),
    };
    if claim.owner_peer_id != identity.peer_id {
        return Ok(None);
    }
    let Some(seat) = car
        .seats
        .iter()
        .find(|seat| seat.code == claim.code && seat.seat_no == claim.seat_no)
    else {
        return Ok(None);
    };

    let binding = if let Some(existing) = runtime.host_bindings.get(&claim.code) {
        if existing.passenger_peer_id != claim.passenger_peer_id
            || existing.claim_id != claim.claim_id
            || existing.passenger_encryption_public_key != claim.passenger_encryption_public_key
        {
            return Ok(None);
        }
        existing.clone()
    } else {
        if !matches!(seat.state, SeatState::Waiting) {
            return Ok(None);
        }
        let binding = HostSeatBinding {
            code: claim.code.clone(),
            claim_id: claim.claim_id.clone(),
            passenger_peer_id: claim.passenger_peer_id.clone(),
            passenger_encryption_public_key: claim.passenger_encryption_public_key.clone(),
            access_id: Uuid::new_v4().to_string(),
            session_secret: new_session_secret()?,
            issued_at_ms: now,
        };
        runtime
            .host_bindings
            .insert(claim.code.clone(), binding.clone());
        binding
    };

    Ok(Some(AccessGrant {
        version: PROTOCOL_VERSION,
        claim_id: binding.claim_id,
        code: binding.code,
        car_id: car.car_id,
        seat_no: seat.seat_no,
        owner_peer_id: identity.peer_id.clone(),
        passenger_peer_id: binding.passenger_peer_id,
        access_id: binding.access_id,
        session_secret: binding.session_secret,
        local_proxy_port: LOCAL_PROXY_PORT,
        enabled_tools: car.enabled_tools,
        issued_at_ms: binding.issued_at_ms,
        expires_at_ms: car.expires_at,
    }))
}

fn mark_seat_joining(state: &RuntimeState, grant: &AccessGrant, nickname: &str) {
    let Ok(mut runtime) = state.inner.lock() else {
        return;
    };
    let Some(binding) = runtime.host_bindings.get(&grant.code) else {
        return;
    };
    if binding.claim_id != grant.claim_id || binding.passenger_peer_id != grant.passenger_peer_id {
        return;
    }
    let Some(car) = runtime.active_car.as_mut() else {
        return;
    };
    if car.car_id != grant.car_id {
        return;
    }
    if let Some(seat) = car.seats.iter_mut().find(|seat| seat.code == grant.code) {
        seat.nickname = Some(nickname.to_string());
        seat.state = SeatState::Joining;
    }
}

fn mark_seat_connected_by_peer(
    state: &RuntimeState,
    passenger_peer_id: &str,
) -> Result<(), String> {
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let code = runtime
        .host_bindings
        .values()
        .find(|binding| binding.passenger_peer_id == passenger_peer_id)
        .map(|binding| binding.code.clone())
        .ok_or_else(|| "成员不属于当前有效车队".to_string())?;
    let car = runtime
        .active_car
        .as_mut()
        .ok_or_else(|| "当前没有正在发车的车队".to_string())?;
    let seat = car
        .seats
        .iter_mut()
        .find(|seat| seat.code == code)
        .ok_or_else(|| "成员座位不存在".to_string())?;
    if matches!(seat.state, SeatState::Waiting | SeatState::Blocked) {
        return Err("座位尚未完成认领".to_string());
    }
    if !matches!(seat.state, SeatState::Using) {
        seat.state = SeatState::Connected;
    }
    Ok(())
}

/// Called by the host UI when the WebRTC data channel to a passenger opens.
#[tauri::command]
pub fn confirm_passenger_link(
    passenger_peer_id: String,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    mark_seat_connected_by_peer(&state, &passenger_peer_id)
}

fn handle_leave_message(
    state: &RuntimeState,
    identity: &DeviceIdentity,
    message: &CoordinatorMessage,
) -> Result<(), String> {
    CoordinatorClient::verify_message(message, None, &identity.peer_id, now_ms())?;
    // Leave notices arrive as sealed envelopes; the front-end's plain
    // `hangup` ping (used to tear down the data channel) has no notice body
    // and simply fails the parse below, which is fine.
    let payload_json =
        serde_json::from_str::<crate::crypto::EncryptedEnvelope>(&message.payload_json)
            .map_err(|error| format!("离开消息缺少加密信封: {error}"))
            .and_then(|envelope| {
                crate::crypto::decrypt_signal(identity, &message.from_peer_id, &envelope)
            })?;
    let notice: LeaveNotice = serde_json::from_str(&payload_json)
        .map_err(|error| format!("离开消息格式无效: {error}"))?;
    if notice.version != PROTOCOL_VERSION
        || notice.passenger_peer_id != message.from_peer_id
        || notice.timestamp_ms > now_ms().saturating_add(300_000)
        || now_ms() > notice.timestamp_ms.saturating_add(CLAIM_TTL_MS)
    {
        return Err("离开消息身份或有效期无效".to_string());
    }
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let Some(binding) = runtime.host_bindings.get(&notice.code) else {
        return Ok(());
    };
    if binding.passenger_peer_id != notice.passenger_peer_id
        || binding.access_id != notice.access_id
    {
        return Err("离开消息与已绑定座位不匹配".to_string());
    }
    let Some(car) = runtime.active_car.as_mut() else {
        return Ok(());
    };
    if car.car_id != notice.car_id {
        return Err("离开消息车队不匹配".to_string());
    }
    if let Some(seat) = car.seats.iter_mut().find(|seat| seat.code == notice.code) {
        seat.nickname = None;
        seat.tool = None;
        seat.state = SeatState::Waiting;
    }
    runtime.host_bindings.remove(&notice.code);
    runtime
        .pending_signals
        .retain(|signal| signal.from_peer_id != notice.passenger_peer_id);
    Ok(())
}

/// Unwraps an end-to-end encrypted signaling payload and enforces the same
/// size/shape rules on the decrypted plaintext.
fn decrypt_signal_payload(
    identity: &DeviceIdentity,
    sender_peer_id: &str,
    payload_json: &str,
) -> Result<String, String> {
    let envelope: crate::crypto::EncryptedEnvelope = serde_json::from_str(payload_json)
        .map_err(|_| "WebRTC 信令缺少端到端加密信封".to_string())?;
    let plaintext = crate::crypto::decrypt_signal(identity, sender_peer_id, &envelope)?;
    if plaintext.len() > SIGNAL_PAYLOAD_LIMIT
        || serde_json::from_str::<serde_json::Value>(&plaintext)
            .map(|value| !value.is_object())
            .unwrap_or(true)
    {
        return Err("WebRTC 信令格式无效或过大".to_string());
    }
    Ok(plaintext)
}

async fn handle_host_message(
    state: &RuntimeState,
    coordinator: &CoordinatorClient,
    identity: &DeviceIdentity,
    mut message: CoordinatorMessage,
) -> Result<(), String> {
    if message.kind == "hangup" {
        return handle_leave_message(state, identity, &message);
    }
    if matches!(
        message.kind.as_str(),
        "webrtc_offer" | "webrtc_answer" | "ice_candidate"
    ) {
        CoordinatorClient::verify_message(&message, None, &identity.peer_id, now_ms())?;
        message.payload_json =
            decrypt_signal_payload(identity, &message.from_peer_id, &message.payload_json)?;
        let mut runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        if !runtime
            .host_bindings
            .values()
            .any(|binding| binding.passenger_peer_id == message.from_peer_id)
        {
            return Err("WebRTC 信令发送者没有有效座位授权".to_string());
        }
        // Bound the queue so a stalled webview can never grow it unbounded.
        if runtime.pending_signals.len() >= PENDING_SIGNAL_LIMIT {
            runtime.pending_signals.remove(0);
        }
        runtime.pending_signals.push(message);
        return Ok(());
    }
    if message.kind != "carpool_claim" {
        return Ok(());
    }
    CoordinatorClient::verify_message(&message, None, &identity.peer_id, now_ms())?;
    let claim: CarpoolClaim = serde_json::from_str(&message.payload_json)
        .map_err(|error| format!("认领消息格式无效: {error}"))?;
    claim.validate(now_ms())?;
    if claim.passenger_peer_id != message.from_peer_id {
        return Err("认领设备身份与消息签名者不一致".to_string());
    }
    let Some(grant) = access_grant_for_claim(state, identity, &claim, now_ms())? else {
        return Ok(());
    };
    let envelope = encrypt_access(
        identity,
        &claim.passenger_peer_id,
        &claim.passenger_encryption_public_key,
        &grant,
    )?;
    coordinator
        .send_message(
            identity,
            &claim.passenger_peer_id,
            "carpool_access",
            serde_json::to_string(&envelope)
                .map_err(|error| format!("无法编码加密授权信封: {error}"))?,
            now_ms(),
        )
        .await?;
    mark_seat_joining(state, &grant, &claim.nickname);
    Ok(())
}

fn spawn_host_claim_loop(
    state: RuntimeState,
    coordinator: CoordinatorClient,
    identity: DeviceIdentity,
    car_id: String,
) {
    tauri::async_runtime::spawn(async move {
        while active_host_car(&state, &car_id) {
            match coordinator.poll_messages(&identity, None, now_ms()).await {
                Ok(messages) => {
                    for message in messages {
                        if let Err(error) =
                            handle_host_message(&state, &coordinator, &identity, message).await
                        {
                            crate::diagnostics::record(
                                "warn",
                                "coordinator",
                                format!("ignored invalid carpool claim: {error}"),
                            );
                        }
                    }
                }
                Err(error) => {
                    crate::diagnostics::record(
                        "warn",
                        "coordinator",
                        format!("carpool claim poll failed: {error}"),
                    );
                    sleep(Duration::from_secs(2)).await;
                }
            }
            // Keep well under the coordinator per-peer poll budget so same-NAT
            // passengers can still drain answers and ICE candidates.
            sleep(Duration::from_millis(1_000)).await;
        }
    });
}

#[tauri::command]
pub fn list_accounts(state: State<'_, RuntimeState>) -> Result<Vec<AccountSummary>, String> {
    account_pool(&state)?.list()
}

#[tauri::command]
pub fn import_local_accounts(state: State<'_, RuntimeState>) -> Result<ImportResult, String> {
    let result = account_pool(&state)?.import_local()?;
    clear_managed_account_routes(
        &state,
        result.accounts.iter().map(|account| account.id.as_str()),
    );
    crate::diagnostics::record(
        "info",
        "account-pool",
        format!(
            "local account import completed: {} imported, {} updated",
            result.imported, result.updated
        ),
    );
    Ok(result)
}

#[tauri::command]
pub fn import_accounts(
    input: ImportAccountsInput,
    state: State<'_, RuntimeState>,
) -> Result<ImportResult, String> {
    let result = account_pool(&state)?.import_content(
        &input.content,
        ImportOptions {
            tool: input.tool,
            name: input.name,
            priority: input.priority,
            enabled: input.enabled,
            source: input.source.map(AccountSource::from),
        },
    )?;
    clear_managed_account_routes(
        &state,
        result.accounts.iter().map(|account| account.id.as_str()),
    );
    crate::diagnostics::record(
        "info",
        "account-pool",
        format!(
            "account import completed: {} imported, {} updated",
            result.imported, result.updated
        ),
    );
    Ok(result)
}

#[tauri::command]
pub fn update_account(
    input: UpdateManagedAccountInput,
    state: State<'_, RuntimeState>,
) -> Result<AccountSummary, String> {
    let reset_route_health = account_update_resets_route_health(&input);
    let id = input.id;
    let summary = account_pool(&state)?.update(
        &id,
        UpdateAccountInput {
            name: input.name,
            enabled: input.enabled,
            priority: input.priority,
        },
    )?;
    if reset_route_health {
        clear_managed_account_routes(&state, [id.as_str()]);
    }
    Ok(summary)
}

#[tauri::command]
pub fn delete_account(id: String, state: State<'_, RuntimeState>) -> Result<bool, String> {
    let deleted = account_pool(&state)?.delete(&id)?;
    if deleted {
        clear_managed_account_routes(&state, [id.as_str()]);
    }
    Ok(deleted)
}

#[tauri::command]
pub async fn detect_tools(
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<Vec<ToolDetection>, String> {
    let root = managed_tools_root(&app);
    let account_pool_path = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?
        .account_pool_path
        .clone();
    if let Some(root) = root.clone() {
        // Refresh update metadata in the background; detection stays instant.
        tauri::async_runtime::spawn(crate::tool_provisioner::refresh_release_caches(root));
    }
    Ok(vec![
        detect_tool(
            ToolKind::Claude,
            root.as_deref(),
            account_pool_path.as_deref(),
        ),
        detect_tool(
            ToolKind::Codex,
            root.as_deref(),
            account_pool_path.as_deref(),
        ),
    ])
}

/// Installs (or updates) a CLI with zero prerequisites: the official native
/// binary is downloaded and verified first; npm is only a fallback when the
/// official channel is unreachable and Node.js already exists.
#[tauri::command]
pub async fn install_tool(
    kind: ToolKind,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<ToolDetection, String> {
    let guard = crate::tool_installer::InstallGuard::acquire(kind)?;
    let root = managed_tools_root(&app).ok_or_else(|| "无法定位应用数据目录".to_string())?;
    let system_install = find_executable(kind.command());
    let managed = crate::tool_provisioner::managed_tool(&root, kind);
    let should_provision = system_install.is_none()
        && match &managed {
            None => true,
            Some(current) => crate::tool_provisioner::latest_known_version(&root, kind)
                .is_some_and(|latest| {
                    crate::tool_provisioner::version_is_newer(&latest, &current.version)
                }),
        };
    if should_provision {
        match crate::tool_provisioner::provision(&app, kind, &root).await {
            Ok(_) => {}
            Err(error) if error.contains("已取消") => return Err(error),
            Err(native_error) if managed.is_some() => {
                // The previous managed version stays active when an update fails.
                return Err(format!("更新失败，已保留当前版本：{native_error}"));
            }
            Err(native_error) => match find_executable("npm") {
                Some(npm) => {
                    crate::tool_provisioner::emit_progress(&app, kind, "npm", 0, None, None);
                    let fallback = tauri::async_runtime::spawn_blocking(move || {
                        crate::tool_installer::install(kind, &npm)
                    })
                    .await
                    .map_err(|error| format!("安装任务意外中断: {error}"))?;
                    if let Err(npm_error) = fallback {
                        return Err(format!(
                            "官方直装失败：{native_error}\nnpm 备用安装也失败：{npm_error}"
                        ));
                    }
                }
                None => {
                    return Err(format!(
                        "{native_error}\n{}",
                        crate::tool_provisioner::manual_install_hint(kind)
                    ));
                }
            },
        }
    }
    drop(guard);
    let account_pool_path = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?
        .account_pool_path
        .clone();
    let detection = detect_tool(kind, Some(&root), account_pool_path.as_deref());
    if !detection.installed {
        return Err(format!(
            "安装已完成，但仍未检测到 {} 命令。请重启应用后重试。{}",
            kind.command(),
            crate::tool_provisioner::manual_install_hint(kind)
        ));
    }
    Ok(detection)
}

#[tauri::command]
pub fn cancel_tool_install(kind: ToolKind) -> bool {
    crate::tool_provisioner::request_cancel(kind)
}

#[tauri::command]
pub async fn check_app_update(
    app: AppHandle,
) -> Result<Option<crate::tool_provisioner::AppUpdateInfo>, String> {
    let root = managed_tools_root(&app).ok_or_else(|| "无法定位应用数据目录".to_string())?;
    let current_version = app.package_info().version.to_string();
    crate::tool_provisioner::check_app_update(&root, &current_version).await
}

#[tauri::command]
pub fn open_releases_page() -> Result<(), String> {
    crate::tool_provisioner::open_releases_page()
}

#[tauri::command]
pub async fn start_car(
    input: StartCarInput,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<CarSession, String> {
    if input.enabled_tools.is_empty() {
        return Err("至少选择一个工具".to_string());
    }
    let now = now_ms();
    validate_schedule(input.starts_at, input.ends_at, now)?;
    let managed_root = managed_tools_root(&app);
    let account_pool_path = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?
        .account_pool_path
        .clone();
    for kind in &input.enabled_tools {
        let detection = detect_tool(*kind, managed_root.as_deref(), account_pool_path.as_deref());
        if !detection.installed || !detection.authenticated {
            return Err(format!("{} 尚未就绪，请先完成安装和登录", detection.name));
        }
    }

    let identity = load_or_create(&app)?;
    let coordinator = CoordinatorClient::from_environment()?;
    let car_id = Uuid::new_v4().to_string();
    let started_at = input.starts_at;
    let expires_at = input.ends_at;
    let car_name = input.car_name.trim().chars().take(32).collect::<String>();
    let car_name = if car_name.is_empty() {
        "我的可信车队".to_string()
    } else {
        car_name
    };
    let mut seats = Vec::with_capacity(4);
    for seat_no in 1..=4 {
        seats.push(Seat {
            seat_no,
            code: random_code()?,
            nickname: None,
            state: SeatState::Waiting,
            tool: None,
            usage: SeatUsageSummary::default(),
            token_limits: MemberTokenLimits::default(),
            token_limit_status: MemberTokenLimitStatus::default(),
            token_usage_events: Vec::new(),
        });
    }
    let account_quotas = crate::account_quota::pending_for(&input.enabled_tools);
    let car = CarSession {
        owner_peer_id: identity.peer_id.clone(),
        car_id: car_id.clone(),
        car_name: car_name.clone(),
        started_at,
        expires_at,
        enabled_tools: input.enabled_tools,
        seats,
        account_quotas,
    };

    for seat in &car.seats {
        let payload = PublicInvitePayload {
            version: PROTOCOL_VERSION,
            code: seat.code.clone(),
            car_id: car.car_id.clone(),
            car_name: car.car_name.clone(),
            owner_label: "可信车主".to_string(),
            owner_peer_id: identity.peer_id.clone(),
            owner_encryption_public_key: identity.encryption_public_key.clone(),
            seat_no: seat.seat_no,
            enabled_tools: car.enabled_tools.clone(),
            starts_at_ms: car.started_at,
            expires_at_ms: car.expires_at,
        };
        let invite = coordinator.build_invite(&identity, &payload, now_ms())?;
        coordinator.register_invite(&invite).await?;
    }

    {
        let mut runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        runtime.active_car = Some(car.clone());
        runtime.host_bindings.clear();
        runtime.pending_signals.clear();
        runtime.relay_request_seen_at.clear();
    }
    spawn_host_claim_loop(state.inner().clone(), coordinator, identity, car_id);
    spawn_account_quota_loop(state.inner().clone(), car.car_id.clone());
    Ok(car)
}

#[tauri::command]
pub async fn stop_car(state: State<'_, RuntimeState>) -> Result<(), String> {
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    runtime.active_car = None;
    runtime.host_bindings.clear();
    runtime.pending_signals.clear();
    runtime.relay_request_seen_at.clear();
    Ok(())
}

#[tauri::command]
pub async fn get_active_car(state: State<'_, RuntimeState>) -> Result<Option<CarSession>, String> {
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    if let Some(car) = runtime.active_car.as_mut() {
        crate::quota::refresh_car(car, now_ms());
    }
    Ok(runtime.active_car.clone())
}

#[tauri::command]
pub async fn refresh_account_quotas(
    state: State<'_, RuntimeState>,
) -> Result<Vec<AccountQuotaSnapshot>, String> {
    let (car_id, tools, account_pool_path) = {
        let runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        let car = runtime
            .active_car
            .as_ref()
            .ok_or_else(|| "当前没有正在发车的车队".to_string())?;
        (
            car.car_id.clone(),
            car.enabled_tools.clone(),
            runtime.account_pool_path.clone(),
        )
    };
    let snapshots = query_account_quotas(&tools, account_pool_path.as_deref()).await;
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let car = runtime
        .active_car
        .as_mut()
        .filter(|car| car.car_id == car_id)
        .ok_or_else(|| "发车状态已经变化".to_string())?;
    car.account_quotas = merge_account_quotas(&car.account_quotas, snapshots);
    Ok(car.account_quotas.clone())
}

#[tauri::command]
pub fn update_member_token_limits(
    input: UpdateMemberTokenLimitsInput,
    state: State<'_, RuntimeState>,
) -> Result<Seat, String> {
    let limits = MemberTokenLimits {
        five_hour_tokens: input.five_hour_tokens,
        daily_tokens: input.daily_tokens,
        weekly_tokens: input.weekly_tokens,
    };
    crate::quota::validate_limits(&limits)?;
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let car = runtime
        .active_car
        .as_mut()
        .ok_or_else(|| "当前没有正在发车的车队".to_string())?;
    let seat = car
        .seats
        .iter_mut()
        .find(|seat| seat.seat_no == input.seat_no)
        .ok_or_else(|| "成员座位不存在".to_string())?;
    seat.token_limits = limits;
    crate::quota::refresh_seat(seat, now_ms());
    Ok(seat.clone())
}

#[tauri::command]
pub fn get_shared_car_status(
    passenger_peer_id: String,
    state: State<'_, RuntimeState>,
) -> Result<SharedCarStatus, String> {
    let mut runtime = state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let code = runtime
        .host_bindings
        .values()
        .find(|binding| binding.passenger_peer_id == passenger_peer_id)
        .map(|binding| binding.code.clone())
        .ok_or_else(|| "成员不属于当前有效车队".to_string())?;
    let car = runtime
        .active_car
        .as_mut()
        .ok_or_else(|| "车主已经停止发车".to_string())?;
    crate::quota::refresh_car(car, now_ms());
    let seat = car
        .seats
        .iter()
        .find(|seat| seat.code == code)
        .ok_or_else(|| "成员座位不存在".to_string())?;
    Ok(SharedCarStatus {
        car_id: car.car_id.clone(),
        car_name: car.car_name.clone(),
        started_at: car.started_at,
        expires_at: car.expires_at,
        enabled_tools: car.enabled_tools.clone(),
        account_quotas: car.account_quotas.clone(),
        member: SharedMemberStatus {
            seat_no: seat.seat_no,
            nickname: seat.nickname.clone().unwrap_or_else(|| "成员".to_string()),
            state: seat.state.clone(),
            tool: seat.tool,
            usage: seat.usage.clone(),
            token_limits: seat.token_limits.clone(),
            token_limit_status: seat.token_limit_status.clone(),
        },
    })
}

#[tauri::command]
pub async fn preview_invite(code: String) -> Result<JoinPreview, String> {
    let normalized = normalize_code(&code)?;
    let coordinator = CoordinatorClient::from_environment()?;
    let (payload, _) = coordinator.resolve_invite(&normalized).await?;
    if payload.code != normalized {
        return Err("上车码响应与请求不一致".to_string());
    }
    preview_from_payload(&payload)
}

fn claim_for_invite(
    code: String,
    nickname: String,
    payload: &PublicInvitePayload,
    identity: &DeviceIdentity,
) -> CarpoolClaim {
    let requested_at_ms = now_ms();
    CarpoolClaim {
        version: PROTOCOL_VERSION,
        claim_id: Uuid::new_v4().to_string(),
        code,
        car_id: payload.car_id.clone(),
        seat_no: payload.seat_no,
        owner_peer_id: payload.owner_peer_id.clone(),
        passenger_peer_id: identity.peer_id.clone(),
        passenger_encryption_public_key: identity.encryption_public_key.clone(),
        nickname,
        requested_at_ms,
        expires_at_ms: requested_at_ms + CLAIM_TTL_MS,
    }
}

fn access_from_message(
    message: &CoordinatorMessage,
    owner: &PublicIdentity,
    passenger: &DeviceIdentity,
    claim: &CarpoolClaim,
    preview: &JoinPreview,
) -> Result<Option<(RideAccess, String)>, String> {
    if message.kind != "carpool_access" {
        return Ok(None);
    }
    CoordinatorClient::verify_message(message, Some(owner), &passenger.peer_id, now_ms())?;
    let envelope: EncryptedEnvelope = serde_json::from_str(&message.payload_json)
        .map_err(|error| format!("授权信封格式无效: {error}"))?;
    let grant: AccessGrant = decrypt_access(passenger, &owner.peer_id, &envelope)?;
    grant.validate_for_claim(claim, now_ms())?;
    if grant.enabled_tools != preview.enabled_tools || grant.expires_at_ms != preview.expires_at {
        return Err("加密授权与上车码公开信息不一致".to_string());
    }
    Ok(Some((
        RideAccess {
            preview: preview.clone(),
            access_id: grant.access_id,
            owner_peer_id: owner.peer_id.clone(),
            local_proxy_port: grant.local_proxy_port,
            connection_state: ConnectionState::Connected,
        },
        grant.session_secret,
    )))
}

#[tauri::command]
pub async fn join_car(
    code: String,
    nickname: String,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<RideAccess, String> {
    let nickname = nickname.trim();
    if nickname.is_empty() || nickname.chars().count() > 20 {
        return Err("昵称应为 1 到 20 个字符".to_string());
    }
    let normalized = normalize_code(&code)?;
    let identity = load_or_create(&app)?;
    let coordinator = CoordinatorClient::from_environment()?;
    let (payload, owner) = coordinator.resolve_invite(&normalized).await?;
    let preview = preview_from_payload(&payload)?;
    if preview.starts_at > now_ms() {
        return Err("这辆车还没到开放时间，请稍后再上车".to_string());
    }
    if identity.peer_id == owner.peer_id {
        return Err("车主设备不能认领自己的座位".to_string());
    }
    let claim = claim_for_invite(normalized, nickname.to_string(), &payload, &identity);
    claim.validate(now_ms())?;
    let claim_json =
        serde_json::to_string(&claim).map_err(|error| format!("无法编码上车认领请求: {error}"))?;

    let deadline = Instant::now() + Duration::from_secs(JOIN_TIMEOUT_SECONDS);
    let mut next_send = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next_send {
            coordinator
                .send_message(
                    &identity,
                    &owner.peer_id,
                    "carpool_claim",
                    claim_json.clone(),
                    now_ms(),
                )
                .await?;
            next_send = Instant::now() + Duration::from_secs(4);
        }
        let messages = coordinator.poll_messages(&identity, None, now_ms()).await?;
        for message in messages {
            match access_from_message(&message, &owner, &identity, &claim, &preview) {
                Ok(Some((access, session_secret))) => {
                    let mut runtime = state
                        .inner
                        .lock()
                        .map_err(|_| "运行状态暂时不可用".to_string())?;
                    runtime
                        .access_secrets
                        .insert(access.access_id.clone(), session_secret);
                    runtime
                        .accesses
                        .insert(access.access_id.clone(), access.clone());
                    runtime.passenger_contexts.insert(
                        access.access_id.clone(),
                        PassengerAccessContext {
                            code: claim.code.clone(),
                            car_id: claim.car_id.clone(),
                            owner_peer_id: claim.owner_peer_id.clone(),
                            owner_public_key: owner.public_key.clone(),
                            owner_encryption_public_key: owner.encryption_public_key.clone(),
                        },
                    );
                    return Ok(access);
                }
                Ok(None) => {}
                Err(error) => crate::diagnostics::record(
                    "warn",
                    "coordinator",
                    format!("ignored invalid carpool access message: {error}"),
                ),
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err("车主暂未响应，请确认车主电脑在线后重试".to_string())
}

#[tauri::command]
pub async fn leave_car(
    access_id: String,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    let identity = load_or_create(&app)?;
    let context = {
        let runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        runtime
            .passenger_contexts
            .get(&access_id)
            .cloned()
            .ok_or_else(|| "当前上车会话已经结束".to_string())?
    };
    let notice = LeaveNotice {
        version: PROTOCOL_VERSION,
        code: context.code,
        car_id: context.car_id,
        access_id: access_id.clone(),
        passenger_peer_id: identity.peer_id.clone(),
        timestamp_ms: now_ms(),
    };
    crate::client_launcher::restore_access(&app, &access_id, true)?;
    let notice_json =
        serde_json::to_string(&notice).map_err(|error| format!("无法编码离开通知: {error}"))?;
    // The leave notice carries the seat code and access id; seal it so the
    // coordinator never sees that metadata in plaintext.
    let sealed_notice = crate::crypto::encrypt_signal(
        &identity,
        &context.owner_peer_id,
        &context.owner_encryption_public_key,
        &notice_json,
    )
    .and_then(|envelope| {
        serde_json::to_string(&envelope).map_err(|error| format!("无法编码离开信封: {error}"))
    });
    let send_result = match (CoordinatorClient::from_environment(), sealed_notice) {
        (Ok(coordinator), Ok(sealed)) => {
            coordinator
                .send_message(
                    &identity,
                    &context.owner_peer_id,
                    "hangup",
                    sealed,
                    now_ms(),
                )
                .await
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    {
        let mut runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        runtime.accesses.remove(&access_id);
        runtime.access_secrets.remove(&access_id);
        runtime.passenger_contexts.remove(&access_id);
    }
    if let Err(error) = send_result {
        crate::diagnostics::record(
            "warn",
            "coordinator",
            format!("failed to notify owner about passenger leave: {error}"),
        );
    }
    Ok(())
}

fn validate_work_dir(work_dir: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(value) = work_dir.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let expanded = if value == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(value))
    } else if let Some(rest) = value.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else {
        PathBuf::from(value)
    };
    if !expanded.is_dir() {
        return Err("项目目录不存在或不是文件夹".to_string());
    }
    Ok(Some(expanded))
}

fn spawn_tool(
    kind: ToolKind,
    access: &RideAccess,
    session_secret: &str,
    work_dir: Option<&Path>,
    executable: PathBuf,
    managed_by_app: bool,
) -> Result<(), String> {
    let base = format!(
        "http://127.0.0.1:{}/access/{}",
        access.local_proxy_port, access.access_id
    );
    let mut launch_env = BTreeMap::new();
    let mut args = Vec::new();
    match kind {
        ToolKind::Claude => {
            launch_env.insert("ANTHROPIC_BASE_URL".to_string(), format!("{base}/claude"));
            launch_env.insert("ANTHROPIC_API_KEY".to_string(), session_secret.to_string());
            launch_env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), String::new());
            launch_env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), String::new());
            if managed_by_app {
                // The app owns updates for its managed binaries; keep the
                // CLI's own auto-updater out of the way.
                launch_env.insert("DISABLE_AUTOUPDATER".to_string(), "1".to_string());
            }
            if Command::new(&executable)
                .arg("--help")
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).contains("--bare"))
                .unwrap_or(false)
            {
                args.push("--bare".to_string());
            }
        }
        ToolKind::Codex => {
            let codex_base = format!("{base}/codex/v1");
            launch_env.insert("OPENAI_BASE_URL".to_string(), codex_base.clone());
            launch_env.insert("OPENAI_API_KEY".to_string(), session_secret.to_string());
            args.extend([
                "-c".to_string(),
                "model_provider=\"trusted_carpool\"".to_string(),
                "-c".to_string(),
                "model_providers.trusted_carpool.name=\"Trusted Carpool\"".to_string(),
                "-c".to_string(),
                format!("model_providers.trusted_carpool.base_url=\"{codex_base}\""),
                "-c".to_string(),
                "model_providers.trusted_carpool.wire_api=\"responses\"".to_string(),
                "-c".to_string(),
                "model_providers.trusted_carpool.requires_openai_auth=true".to_string(),
            ]);
        }
    }
    crate::terminal_launcher::launch(crate::terminal_launcher::TerminalLaunchSpec {
        executable: &executable,
        args: &args,
        env: &launch_env,
        work_dir,
    })
}

#[tauri::command]
pub async fn launch_tool(
    input: LaunchToolInput,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    let work_dir = if input.mode == LaunchMode::Terminal {
        validate_work_dir(input.work_dir.as_deref())?
    } else {
        None
    };
    let (access, session_secret) = {
        let runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        let access = runtime
            .accesses
            .get(&input.access_id)
            .ok_or_else(|| "上车凭据已失效，请重新上车".to_string())?;
        if !access.preview.enabled_tools.contains(&input.kind) {
            return Err("这辆车没有开放所选工具".to_string());
        }
        let secret = runtime
            .access_secrets
            .get(&input.access_id)
            .ok_or_else(|| "加密会话密钥已失效，请重新上车".to_string())?;
        (access.clone(), secret.clone())
    };
    match input.mode {
        LaunchMode::Terminal => {
            let root = managed_tools_root(&app);
            let (executable, managed_by_app) = resolve_tool_executable(input.kind, root.as_deref())
                .ok_or_else(|| {
                    format!("未找到 {} 命令，可先在工具页一键安装", input.kind.command())
                })?;
            if managed_by_app {
                // Integrity gate: an app-managed binary must still match the
                // checksum recorded at install time before it may run.
                let verify_root = root
                    .clone()
                    .ok_or_else(|| "无法定位应用数据目录".to_string())?;
                let kind = input.kind;
                tauri::async_runtime::spawn_blocking(move || {
                    crate::tool_provisioner::verify_managed_binary(&verify_root, kind)
                })
                .await
                .map_err(|error| format!("完整性校验意外中断: {error}"))??;
            }
            spawn_tool(
                input.kind,
                &access,
                &session_secret,
                work_dir.as_deref(),
                executable,
                managed_by_app,
            )
        }
        LaunchMode::Desktop => {
            crate::client_launcher::launch(&app, input.kind, &access, &session_secret)
        }
    }
}

fn allowed_signal_kind(kind: &str) -> bool {
    matches!(
        kind,
        "webrtc_offer" | "webrtc_answer" | "ice_candidate" | "hangup"
    )
}

#[tauri::command]
pub async fn get_ice_servers(app: AppHandle) -> Result<Vec<IceServer>, String> {
    let identity = load_or_create(&app)?;
    CoordinatorClient::from_environment()?
        .ice_servers(&identity)
        .await
}

#[tauri::command]
pub async fn send_webrtc_signal(
    input: SendWebRtcSignalInput,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    if !allowed_signal_kind(&input.kind)
        || input.payload_json.len() > SIGNAL_PAYLOAD_LIMIT
        || serde_json::from_str::<serde_json::Value>(&input.payload_json)
            .map(|value| !value.is_object())
            .unwrap_or(true)
    {
        return Err("WebRTC 信令类型、格式或大小无效".to_string());
    }
    let identity = load_or_create(&app)?;
    // The recipient's encryption key doubles as the authorization check: only
    // bound seats (host side) or the granting owner (passenger side) have one.
    let recipient_encryption_key = {
        let runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        runtime
            .host_bindings
            .values()
            .find(|binding| binding.passenger_peer_id == input.to_peer_id)
            .map(|binding| binding.passenger_encryption_public_key.clone())
            .or_else(|| {
                runtime
                    .passenger_contexts
                    .values()
                    .find(|context| context.owner_peer_id == input.to_peer_id)
                    .map(|context| context.owner_encryption_public_key.clone())
            })
    };
    let Some(recipient_encryption_key) = recipient_encryption_key else {
        return Err("信令目标不属于当前有效车队".to_string());
    };
    // SDP and ICE candidates reveal local/public IP addresses; seal them so
    // the coordinator only relays opaque envelopes.
    let envelope = crate::crypto::encrypt_signal(
        &identity,
        &input.to_peer_id,
        &recipient_encryption_key,
        &input.payload_json,
    )?;
    let sealed = serde_json::to_string(&envelope)
        .map_err(|error| format!("无法编码信令加密信封: {error}"))?;
    CoordinatorClient::from_environment()?
        .send_message(&identity, &input.to_peer_id, &input.kind, sealed, now_ms())
        .await
}

#[tauri::command]
pub async fn poll_webrtc_signals(
    access_id: Option<String>,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<Vec<CoordinatorMessage>, String> {
    if access_id.is_none() {
        let mut runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        return Ok(std::mem::take(&mut runtime.pending_signals));
    }
    let access_id = access_id.expect("checked");
    let context = {
        let runtime = state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        runtime
            .passenger_contexts
            .get(&access_id)
            .cloned()
            .ok_or_else(|| "上车会话已经结束".to_string())?
    };
    let identity = load_or_create(&app)?;
    let owner = PublicIdentity {
        peer_id: context.owner_peer_id,
        public_key: context.owner_public_key,
        encryption_public_key: context.owner_encryption_public_key,
    };
    let messages = CoordinatorClient::from_environment()?
        .poll_messages(&identity, None, now_ms())
        .await?;
    Ok(sanitize_passenger_signals(
        messages,
        &identity,
        &owner,
        now_ms(),
    ))
}

/// Keeps only verifiable, decryptable signals from the expected owner. The
/// coordinator mailbox is drain-on-read, so one bad message (a stranger's
/// spam, a stale car, a malformed envelope) must never discard the valid
/// signals fetched in the same batch.
fn sanitize_passenger_signals(
    messages: Vec<CoordinatorMessage>,
    identity: &DeviceIdentity,
    owner: &PublicIdentity,
    now: i64,
) -> Vec<CoordinatorMessage> {
    let mut verified = Vec::new();
    for mut message in messages {
        if !allowed_signal_kind(&message.kind) {
            continue;
        }
        if CoordinatorClient::verify_message(&message, Some(owner), &identity.peer_id, now).is_err()
        {
            continue;
        }
        let Ok(plaintext) =
            decrypt_signal_payload(identity, &message.from_peer_id, &message.payload_json)
        else {
            continue;
        };
        message.payload_json = plaintext;
        verified.push(message);
    }
    verified
}

#[tauri::command]
pub async fn execute_relay_request(
    request: RelayRequest,
    state: State<'_, RuntimeState>,
) -> Result<RelayResponse, String> {
    execute_host_request(state.inner(), request).await
}

#[tauri::command]
pub fn start_relay_request(
    request: RelayRequest,
    app: AppHandle,
    state: State<'_, RuntimeState>,
) -> Result<bool, String> {
    Ok(start_host_request_stream(
        app,
        state.inner().clone(),
        request,
    ))
}

#[tauri::command]
pub async fn submit_relay_response(
    request_id: String,
    payload_json: String,
) -> Result<bool, String> {
    RelayBridge::global().submit(request_id, payload_json).await
}

#[tauri::command]
pub async fn submit_relay_stream_event(event: RelayStreamEvent) -> Result<bool, String> {
    RelayBridge::global().submit_stream_event(event).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_only_account_updates_preserve_route_health() {
        let metadata_only = UpdateManagedAccountInput {
            id: "account".to_string(),
            name: Some("新名称".to_string()),
            enabled: None,
            priority: Some(10),
        };
        let toggle = UpdateManagedAccountInput {
            id: "account".to_string(),
            name: None,
            enabled: Some(true),
            priority: None,
        };

        assert!(!account_update_resets_route_health(&metadata_only));
        assert!(account_update_resets_route_health(&toggle));
    }

    fn identity(path: &Path) -> DeviceIdentity {
        crate::identity::load_or_create_at(path).expect("identity")
    }

    fn sealed_signal(
        sender: &DeviceIdentity,
        recipient: &PublicIdentity,
        kind: &str,
        payload: &str,
        now: i64,
    ) -> CoordinatorMessage {
        let envelope = crate::crypto::encrypt_signal(
            sender,
            &recipient.peer_id,
            &recipient.encryption_public_key,
            payload,
        )
        .expect("seal signal");
        crate::coordinator::signed_test_message(
            sender,
            &recipient.peer_id,
            kind,
            serde_json::to_string(&envelope).expect("envelope json"),
            now,
        )
    }

    #[test]
    fn manual_account_import_cannot_claim_local_provenance() {
        let local = serde_json::from_value::<ImportAccountsInput>(serde_json::json!({
            "content": "sk-test",
            "source": "local"
        }));
        assert!(local.is_err());

        let file = serde_json::from_value::<ImportAccountsInput>(serde_json::json!({
            "content": "sk-test",
            "source": "file"
        }))
        .expect("file source");
        assert!(matches!(file.source, Some(ManualAccountSource::File)));
    }

    #[test]
    fn one_bad_signal_never_discards_the_valid_batch() {
        let directory = tempfile::tempdir().expect("tempdir");
        let owner = identity(&directory.path().join("owner.json"));
        let passenger = identity(&directory.path().join("passenger.json"));
        let stranger = identity(&directory.path().join("stranger.json"));
        let passenger_public = passenger.public();
        let now = now_ms();

        let valid_answer = sealed_signal(
            &owner,
            &passenger_public,
            "webrtc_answer",
            r#"{"sdp":{"type":"answer","sdp":"v=0"}}"#,
            now,
        );
        // A stranger with a perfectly valid signature is still not the owner.
        let stranger_spam = sealed_signal(
            &stranger,
            &passenger_public,
            "ice_candidate",
            r#"{"candidate":{"candidate":"spam"}}"#,
            now,
        );
        // Legacy/plaintext payloads without an envelope must be dropped too.
        let plaintext_signal = crate::coordinator::signed_test_message(
            &owner,
            &passenger_public.peer_id,
            "ice_candidate",
            r#"{"candidate":{"candidate":"plain"}}"#.to_string(),
            now,
        );
        let wrong_kind = crate::coordinator::signed_test_message(
            &owner,
            &passenger_public.peer_id,
            "carpool_access",
            "{}".to_string(),
            now,
        );

        let survivors = sanitize_passenger_signals(
            vec![stranger_spam, plaintext_signal, wrong_kind, valid_answer],
            &passenger,
            &owner.public(),
            now,
        );
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].kind, "webrtc_answer");
        // Delivered payloads are already decrypted for the webview.
        assert!(survivors[0].payload_json.contains("\"type\":\"answer\""));
    }

    #[test]
    fn signal_payloads_are_sealed_before_reaching_the_coordinator() {
        let directory = tempfile::tempdir().expect("tempdir");
        let owner = identity(&directory.path().join("owner.json"));
        let passenger = identity(&directory.path().join("passenger.json"));
        let payload =
            r#"{"candidate":{"candidate":"candidate:1 1 udp 2113937151 10.0.0.8 51000 typ host"}}"#;
        let message = sealed_signal(
            &passenger,
            &owner.public(),
            "ice_candidate",
            payload,
            now_ms(),
        );
        assert!(
            !message.payload_json.contains("10.0.0.8"),
            "coordinator-visible payload must not contain candidate IPs"
        );
        let plaintext = decrypt_signal_payload(&owner, &passenger.peer_id, &message.payload_json)
            .expect("decrypt");
        assert_eq!(plaintext, payload);
        // The wrong recipient cannot open it.
        let stranger = identity(&directory.path().join("stranger.json"));
        assert!(
            decrypt_signal_payload(&stranger, &passenger.peer_id, &message.payload_json).is_err()
        );
    }

    #[test]
    fn codes_have_about_sixty_bits_and_avoid_ambiguous_characters() {
        for _ in 0..100 {
            let code = random_code().expect("code");
            assert_eq!(code.len(), 12);
            assert!(!code.contains('0'));
            assert!(!code.contains('O'));
            assert!(!code.contains('1'));
            assert!(!code.contains('I'));
            assert_eq!(
                normalize_code(&format!("{}-{}-{}", &code[..4], &code[4..8], &code[8..]))
                    .expect("normalized"),
                code
            );
        }
    }

    #[test]
    fn work_dir_rejects_missing_path() {
        assert!(validate_work_dir(Some("/definitely/not/a/real/trusted-carpool-path")).is_err());
    }

    #[test]
    fn executable_detection_does_not_depend_on_the_gui_process_path() {
        let directory = tempfile::tempdir().expect("temp directory");
        let executable_name = path_candidates("codex")
            .into_iter()
            .next()
            .expect("candidate name");
        let executable = directory.path().join(executable_name);
        std::fs::write(&executable, b"test executable").expect("write executable");
        assert_eq!(
            find_executable_in("codex", &[directory.path().to_path_buf()]),
            Some(executable)
        );
    }

    #[test]
    fn schedule_accepts_immediate_or_future_ranges_and_rejects_invalid_windows() {
        let now = 1_700_000_000_000;
        assert!(validate_schedule(now, now + 2 * 60 * 60_000, now).is_ok());
        assert!(validate_schedule(
            now + 7 * 24 * 60 * 60_000,
            now + 7 * 24 * 60 * 60_000 + 60 * 60_000,
            now
        )
        .is_ok());
        assert!(validate_schedule(now, now + 10 * 60_000, now).is_err());
        assert!(validate_schedule(now, now + 25 * 60 * 60_000, now).is_err());
        assert!(validate_schedule(
            now + 31 * 24 * 60 * 60_000,
            now + 31 * 24 * 60 * 60_000 + 60 * 60_000,
            now
        )
        .is_err());
    }

    #[test]
    fn first_device_binding_prevents_a_second_device_from_reusing_the_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let owner = identity(&directory.path().join("owner.json"));
        let first = identity(&directory.path().join("first.json"));
        let second = identity(&directory.path().join("second.json"));
        let state = RuntimeState::default();
        let now = now_ms();
        let code = "7G2K5LQ8M4TZ".to_string();
        let car_id = Uuid::new_v4().to_string();
        state.inner.lock().expect("runtime").active_car = Some(CarSession {
            car_id: car_id.clone(),
            car_name: "测试车队".to_string(),
            owner_peer_id: owner.peer_id.clone(),
            started_at: now,
            expires_at: now + 60_000,
            enabled_tools: vec![ToolKind::Claude, ToolKind::Codex],
            seats: vec![Seat {
                seat_no: 1,
                code: code.clone(),
                nickname: None,
                state: SeatState::Waiting,
                tool: None,
                usage: SeatUsageSummary::default(),
                token_limits: MemberTokenLimits::default(),
                token_limit_status: MemberTokenLimitStatus::default(),
                token_usage_events: Vec::new(),
            }],
            account_quotas: Vec::new(),
        });
        let claim = |identity: &DeviceIdentity| CarpoolClaim {
            version: PROTOCOL_VERSION,
            claim_id: Uuid::new_v4().to_string(),
            code: code.clone(),
            car_id: car_id.clone(),
            seat_no: 1,
            owner_peer_id: owner.peer_id.clone(),
            passenger_peer_id: identity.peer_id.clone(),
            passenger_encryption_public_key: identity.encryption_public_key.clone(),
            nickname: "乘客".to_string(),
            requested_at_ms: now,
            expires_at_ms: now + CLAIM_TTL_MS,
        };
        let first_claim = claim(&first);
        assert!(access_grant_for_claim(&state, &owner, &first_claim, now)
            .expect("first")
            .is_some());
        let second_claim = claim(&second);
        assert!(access_grant_for_claim(&state, &owner, &second_claim, now)
            .expect("second")
            .is_none());
        assert!(access_grant_for_claim(&state, &owner, &first_claim, now)
            .expect("retry")
            .is_some());
    }
}
