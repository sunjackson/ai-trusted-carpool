use crate::runtime::{AppRuntime, RuntimeState};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::Serialize;
#[cfg(any(target_os = "macos", test))]
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{ipc::Channel, AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};

#[cfg(target_os = "macos")]
const MACOS_AUTO_UPDATE_OPT_IN: &str = "TRUSTED_CARPOOL_ENABLE_MACOS_AUTO_UPDATE";

#[cfg(any(windows, target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopArchitecture {
    X86_64,
    Aarch64,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopPlatform {
    #[cfg(any(windows, test))]
    Windows { architecture: DesktopArchitecture },
    #[cfg(any(target_os = "linux", test))]
    Linux {
        app_image: bool,
        architecture: DesktopArchitecture,
    },
    #[cfg(any(target_os = "macos", test))]
    Macos { explicitly_enabled: bool },
    #[cfg(any(not(any(windows, target_os = "linux", target_os = "macos")), test))]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallBlockReason {
    UnsupportedPlatform,
    ActiveRide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstallPolicy {
    supported: bool,
    block_reason: Option<InstallBlockReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MinisignPublicKey {
    key_id: [u8; 8],
    key: [u8; 32],
}

impl InstallPolicy {
    fn evaluate(platform: DesktopPlatform, active_ride: bool) -> Self {
        let supported = match platform {
            #[cfg(any(windows, test))]
            DesktopPlatform::Windows { architecture } => architecture != DesktopArchitecture::Other,
            #[cfg(any(target_os = "linux", test))]
            DesktopPlatform::Linux {
                app_image,
                architecture,
            } => app_image && architecture != DesktopArchitecture::Other,
            #[cfg(any(target_os = "macos", test))]
            DesktopPlatform::Macos { explicitly_enabled } => explicitly_enabled,
            #[cfg(any(not(any(windows, target_os = "linux", target_os = "macos")), test))]
            DesktopPlatform::Other => false,
        };
        let block_reason = if !supported {
            Some(InstallBlockReason::UnsupportedPlatform)
        } else if active_ride {
            Some(InstallBlockReason::ActiveRide)
        } else {
            None
        };
        Self {
            supported,
            block_reason,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedAppUpdateInfo {
    current_version: String,
    version: String,
    notes: Option<String>,
    date: Option<String>,
    install_supported: bool,
    install_block_reason: Option<InstallBlockReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppUpdateProgressEvent {
    Started,
    Progress,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateProgress {
    event: AppUpdateProgressEvent,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateDownloadResult {
    update: SignedAppUpdateInfo,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
}

struct PendingUpdate<T> {
    update: T,
    verified_bytes: Option<Vec<u8>>,
    total_bytes: Option<u64>,
}

impl<T> PendingUpdate<T> {
    fn discovered(update: T) -> Self {
        Self {
            update,
            verified_bytes: None,
            total_bytes: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PendingInstallError<E> {
    MissingUpdate,
    NotDownloaded,
    Install(E),
}

fn install_pending<T, E>(
    pending: &mut Option<PendingUpdate<T>>,
    install: impl FnOnce(&T, &[u8]) -> Result<(), E>,
) -> Result<(), PendingInstallError<E>> {
    let package = pending.as_ref().ok_or(PendingInstallError::MissingUpdate)?;
    let bytes = package
        .verified_bytes
        .as_deref()
        .ok_or(PendingInstallError::NotDownloaded)?;
    install(&package.update, bytes).map_err(PendingInstallError::Install)?;
    *pending = None;
    Ok(())
}

fn verified_pending<T>(pending: &Option<PendingUpdate<T>>) -> Option<&PendingUpdate<T>> {
    pending
        .as_ref()
        .filter(|package| package.verified_bytes.is_some())
}

fn require_restart_latch(restart_required: &AtomicBool) -> Result<(), String> {
    if restart_required.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("没有已安装且等待重启的应用更新".to_string())
    }
}

#[derive(Default)]
pub struct AppUpdaterState {
    pending: tokio::sync::Mutex<Option<PendingUpdate<Update>>>,
    restart_required: AtomicBool,
}

fn has_active_ride(runtime: &AppRuntime) -> bool {
    has_active_ride_parts(
        runtime.active_car.is_some(),
        runtime.accesses.len(),
        runtime.ride_transitions,
    )
}

fn has_active_ride_parts(hosting: bool, joined_accesses: usize, ride_transitions: usize) -> bool {
    hosting || joined_accesses > 0 || ride_transitions > 0
}

struct UpdateInstallGuard {
    runtime_state: RuntimeState,
}

impl UpdateInstallGuard {
    fn begin(runtime_state: &RuntimeState, platform: DesktopPlatform) -> Result<Self, String> {
        let mut runtime = runtime_state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        let policy = InstallPolicy::evaluate(platform, has_active_ride(&runtime));
        if let Some(reason) = policy.block_reason {
            return Err(policy_error(reason));
        }
        if runtime.app_update_installing {
            return Err("应用更新已经在安装中".to_string());
        }
        runtime.app_update_installing = true;
        drop(runtime);
        Ok(Self {
            runtime_state: runtime_state.clone(),
        })
    }
}

impl Drop for UpdateInstallGuard {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.runtime_state.inner.lock() {
            runtime.app_update_installing = false;
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn explicit_opt_in(value: Option<&OsStr>) -> bool {
    value
        .and_then(OsStr::to_str)
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1")
}

#[cfg(windows)]
fn current_platform(_app: &AppHandle) -> DesktopPlatform {
    DesktopPlatform::Windows {
        architecture: current_architecture(),
    }
}

#[cfg(target_os = "linux")]
fn current_platform(app: &AppHandle) -> DesktopPlatform {
    use tauri::utils::{config::BundleType, platform::bundle_type};

    DesktopPlatform::Linux {
        app_image: app.env().appimage.is_some()
            && matches!(bundle_type(), Some(BundleType::AppImage)),
        architecture: current_architecture(),
    }
}

#[cfg(target_os = "macos")]
fn current_platform(_app: &AppHandle) -> DesktopPlatform {
    DesktopPlatform::Macos {
        explicitly_enabled: explicit_opt_in(std::env::var_os(MACOS_AUTO_UPDATE_OPT_IN).as_deref()),
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn current_platform(_app: &AppHandle) -> DesktopPlatform {
    DesktopPlatform::Other
}

#[cfg(any(windows, target_os = "linux"))]
fn current_architecture() -> DesktopArchitecture {
    match std::env::consts::ARCH {
        "x86_64" => DesktopArchitecture::X86_64,
        "aarch64" => DesktopArchitecture::Aarch64,
        _ => DesktopArchitecture::Other,
    }
}

fn update_date(update: &Update) -> Option<String> {
    update
        .raw_json
        .get("pub_date")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn parse_embedded_updater_public_key() -> Result<MinisignPublicKey, String> {
    let config: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
        .map_err(|_| "内置更新公钥配置无效".to_string())?;
    let encoded = config
        .pointer("/plugins/updater/pubkey")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "内置更新公钥缺失".to_string())?;
    let decoded = BASE64
        .decode(encoded)
        .map_err(|_| "内置更新公钥编码无效".to_string())?;
    let decoded =
        std::str::from_utf8(&decoded).map_err(|_| "内置更新公钥不是 UTF-8".to_string())?;
    let lines = decoded.lines().collect::<Vec<_>>();
    if lines.len() != 2 || !lines[0].starts_with("untrusted comment: ") {
        return Err("内置更新公钥格式无效".to_string());
    }
    let payload = BASE64
        .decode(lines[1])
        .map_err(|_| "内置更新公钥载荷无效".to_string())?;
    if payload.len() != 42 || !matches!(&payload[..2], [0x45, 0x64] | [0x45, 0x44]) {
        return Err("内置更新公钥算法无效".to_string());
    }

    let mut key_id = [0_u8; 8];
    key_id.copy_from_slice(&payload[2..10]);
    let mut key = [0_u8; 32];
    key.copy_from_slice(&payload[10..42]);
    Ok(MinisignPublicKey { key_id, key })
}

fn authenticated_signature_file(
    encoded_signature: &str,
    public_key: &MinisignPublicKey,
) -> Result<String, String> {
    let decoded = BASE64
        .decode(encoded_signature.trim())
        .map_err(|_| "更新签名编码无效".to_string())?;
    let decoded = std::str::from_utf8(&decoded).map_err(|_| "更新签名不是 UTF-8".to_string())?;
    let lines = decoded.lines().collect::<Vec<_>>();
    if lines.len() != 4
        || !lines[0].starts_with("untrusted comment: ")
        || !lines[2].starts_with("trusted comment: ")
    {
        return Err("更新签名格式无效".to_string());
    }

    let payload = BASE64
        .decode(lines[1])
        .map_err(|_| "更新签名载荷无效".to_string())?;
    let global_signature = BASE64
        .decode(lines[3])
        .map_err(|_| "更新全局签名无效".to_string())?;
    if payload.len() != 74
        || payload[..2] != [0x45, 0x44]
        || global_signature.len() != 64
        || payload[2..10] != public_key.key_id
    {
        return Err("更新签名算法或密钥标识不匹配".to_string());
    }

    let trusted_comment = lines[2]
        .strip_prefix("trusted comment: ")
        .ok_or_else(|| "更新签名缺少可信元数据".to_string())?;
    let mut global_message = Vec::with_capacity(64 + trusted_comment.len());
    global_message.extend_from_slice(&payload[10..74]);
    global_message.extend_from_slice(trusted_comment.as_bytes());
    UnparsedPublicKey::new(&ED25519, public_key.key)
        .verify(&global_message, &global_signature)
        .map_err(|_| "更新签名元数据未通过内置公钥验证".to_string())?;

    trusted_comment
        .split('\t')
        .find_map(|field| field.strip_prefix("file:"))
        .filter(|file_name| !file_name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "更新签名缺少已认证文件名".to_string())
}

fn percent_decode_path_segment(segment: &str) -> Result<String, String> {
    fn hex(value: u8) -> Option<u8> {
        match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            b'A'..=b'F' => Some(value - b'A' + 10),
            _ => None,
        }
    }

    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|value| hex(*value));
            let low = bytes.get(index + 2).and_then(|value| hex(*value));
            let (Some(high), Some(low)) = (high, low) else {
                return Err("更新下载地址包含无效转义".to_string());
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "更新下载地址文件名不是 UTF-8".to_string())
}

fn update_url_file_name(download_url: &url::Url) -> Result<String, String> {
    let segment = download_url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| "更新下载地址缺少文件名".to_string())?;
    percent_decode_path_segment(segment)
}

fn file_name_contains_version(file_name: &str, version: &str) -> bool {
    file_name.match_indices(version).any(|(start, _)| {
        let before = file_name[..start].chars().next_back();
        let end = start + version.len();
        let mut after = file_name[end..].chars();
        let first_after = after.next();
        let invalid_after = first_after.is_some_and(|value| {
            value.is_ascii_alphanumeric()
                || matches!(value, '-' | '+')
                || (value == '.' && after.next().is_some_and(|next| next.is_ascii_digit()))
        });
        before.is_none_or(|value| !value.is_ascii_alphanumeric()) && !invalid_after
    })
}

fn platform_requires_versioned_artifact(platform: DesktopPlatform) -> bool {
    match platform {
        #[cfg(any(windows, test))]
        DesktopPlatform::Windows { .. } => true,
        #[cfg(any(target_os = "linux", test))]
        DesktopPlatform::Linux { app_image, .. } => app_image,
        #[cfg(any(target_os = "macos", test))]
        DesktopPlatform::Macos { .. } => false,
        #[cfg(any(not(any(windows, target_os = "linux", target_os = "macos")), test))]
        DesktopPlatform::Other => false,
    }
}

fn expected_artifact_suffix(platform: DesktopPlatform, _version: &str) -> Result<String, String> {
    match platform {
        #[cfg(any(windows, test))]
        DesktopPlatform::Windows { architecture } => match architecture {
            DesktopArchitecture::X86_64 => Ok(format!("_{_version}_x64-setup.exe")),
            DesktopArchitecture::Aarch64 => Ok(format!("_{_version}_arm64-setup.exe")),
            DesktopArchitecture::Other => Err("当前 Windows 架构不支持自动更新产物".to_string()),
        },
        #[cfg(any(target_os = "linux", test))]
        DesktopPlatform::Linux {
            app_image: true,
            architecture,
        } => match architecture {
            DesktopArchitecture::X86_64 => Ok(format!("_{_version}_amd64.AppImage")),
            DesktopArchitecture::Aarch64 => Ok(format!("_{_version}_aarch64.AppImage")),
            DesktopArchitecture::Other => Err("当前 Linux 架构不支持自动更新产物".to_string()),
        },
        #[cfg(any(target_os = "linux", test))]
        DesktopPlatform::Linux {
            app_image: false, ..
        } => Err("当前 Linux 安装格式不支持自动更新产物".to_string()),
        #[cfg(any(target_os = "macos", test))]
        DesktopPlatform::Macos { .. } => Ok(".app.tar.gz".to_string()),
        #[cfg(any(not(any(windows, target_os = "linux", target_os = "macos")), test))]
        DesktopPlatform::Other => Err("当前平台不支持自动更新产物".to_string()),
    }
}

fn validate_authenticated_update_fields(
    signature: &str,
    download_url: &url::Url,
    version: &str,
    platform: DesktopPlatform,
    public_key: &MinisignPublicKey,
) -> Result<(), String> {
    let signed_file_name = authenticated_signature_file(signature, public_key)?;
    let url_file_name = update_url_file_name(download_url)?;
    if signed_file_name != url_file_name {
        return Err("更新签名文件名与下载地址不匹配".to_string());
    }
    if platform_requires_versioned_artifact(platform)
        && !file_name_contains_version(&signed_file_name, version)
    {
        return Err("更新签名文件名与声明版本不匹配".to_string());
    }
    let expected_suffix = expected_artifact_suffix(platform, version)?;
    if !signed_file_name.ends_with(&expected_suffix) {
        return Err("更新签名文件名与当前平台或架构不匹配".to_string());
    }
    Ok(())
}

fn validate_authenticated_update_metadata(
    update: &Update,
    platform: DesktopPlatform,
) -> Result<(), String> {
    let public_key = parse_embedded_updater_public_key()?;
    validate_authenticated_update_fields(
        &update.signature,
        &update.download_url,
        &update.version,
        platform,
        &public_key,
    )
}

fn update_info(update: &Update, policy: InstallPolicy) -> SignedAppUpdateInfo {
    SignedAppUpdateInfo {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        notes: update.body.clone(),
        date: update_date(update),
        install_supported: policy.supported,
        install_block_reason: policy.block_reason,
    }
}

fn progress_event(
    event: AppUpdateProgressEvent,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
) -> AppUpdateProgress {
    AppUpdateProgress {
        event,
        downloaded_bytes,
        total_bytes,
    }
}

fn send_progress(
    progress: &Channel<AppUpdateProgress>,
    event: AppUpdateProgressEvent,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
) {
    let _ = progress.send(progress_event(event, downloaded_bytes, total_bytes));
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn policy_error(reason: InstallBlockReason) -> String {
    match reason {
        InstallBlockReason::UnsupportedPlatform => {
            "当前安装格式不支持应用内自动安装，请从官方发布页手动更新".to_string()
        }
        InstallBlockReason::ActiveRide => {
            "活跃拼车期间禁止安装更新，请结束发车或上车后重试".to_string()
        }
    }
}

#[tauri::command]
pub async fn check_signed_app_update(
    app: AppHandle,
    runtime_state: State<'_, RuntimeState>,
    updater_state: State<'_, AppUpdaterState>,
) -> Result<Option<SignedAppUpdateInfo>, String> {
    let mut pending = updater_state.pending.lock().await;
    if let Some(package) = verified_pending(&pending) {
        let runtime = runtime_state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        let policy = InstallPolicy::evaluate(current_platform(&app), has_active_ride(&runtime));
        return Ok(Some(update_info(&package.update, policy)));
    }
    let updater = app
        .updater()
        .map_err(|error| format!("无法初始化签名更新器: {error}"))?;
    let Some(update) = updater
        .check()
        .await
        .map_err(|error| format!("检查签名更新失败: {error}"))?
    else {
        *pending = None;
        return Ok(None);
    };

    let platform = current_platform(&app);
    validate_authenticated_update_metadata(&update, platform)
        .map_err(|error| format!("签名更新元数据验证失败: {error}"))?;

    let runtime = runtime_state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let policy = InstallPolicy::evaluate(platform, has_active_ride(&runtime));
    let info = update_info(&update, policy);
    *pending = Some(PendingUpdate::discovered(update));
    Ok(Some(info))
}

#[tauri::command]
pub async fn download_app_update(
    progress: Channel<AppUpdateProgress>,
    app: AppHandle,
    runtime_state: State<'_, RuntimeState>,
    updater_state: State<'_, AppUpdaterState>,
) -> Result<AppUpdateDownloadResult, String> {
    let mut pending = updater_state.pending.lock().await;
    let package = pending
        .as_mut()
        .ok_or_else(|| "没有可下载的签名应用更新，请先重新检查".to_string())?;

    if let Some(bytes) = package.verified_bytes.as_ref() {
        let downloaded_bytes = usize_to_u64(bytes.len());
        send_progress(
            &progress,
            AppUpdateProgressEvent::Started,
            0,
            package.total_bytes,
        );
        send_progress(
            &progress,
            AppUpdateProgressEvent::Progress,
            downloaded_bytes,
            package.total_bytes,
        );
        send_progress(
            &progress,
            AppUpdateProgressEvent::Finished,
            downloaded_bytes,
            package.total_bytes,
        );
        let runtime = runtime_state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        let policy = InstallPolicy::evaluate(current_platform(&app), has_active_ride(&runtime));
        return Ok(AppUpdateDownloadResult {
            update: update_info(&package.update, policy),
            downloaded_bytes,
            total_bytes: package.total_bytes,
        });
    }

    let mut downloaded_bytes = 0_u64;
    let mut total_bytes = None;
    let mut started = false;
    let bytes = package
        .update
        .download(
            |chunk_length, content_length| {
                if !started {
                    total_bytes = content_length;
                    send_progress(&progress, AppUpdateProgressEvent::Started, 0, total_bytes);
                    started = true;
                }
                downloaded_bytes = downloaded_bytes.saturating_add(usize_to_u64(chunk_length));
                send_progress(
                    &progress,
                    AppUpdateProgressEvent::Progress,
                    downloaded_bytes,
                    total_bytes,
                );
            },
            || {},
        )
        .await
        .map_err(|error| format!("下载或验证签名更新失败: {error}"))?;

    if !started {
        send_progress(&progress, AppUpdateProgressEvent::Started, 0, total_bytes);
    }
    downloaded_bytes = usize_to_u64(bytes.len());
    send_progress(
        &progress,
        AppUpdateProgressEvent::Finished,
        downloaded_bytes,
        total_bytes,
    );
    package.total_bytes = total_bytes;
    package.verified_bytes = Some(bytes);

    let runtime = runtime_state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())?;
    let policy = InstallPolicy::evaluate(current_platform(&app), has_active_ride(&runtime));
    Ok(AppUpdateDownloadResult {
        update: update_info(&package.update, policy),
        downloaded_bytes,
        total_bytes,
    })
}

#[tauri::command]
pub async fn install_app_update(
    app: AppHandle,
    runtime_state: State<'_, RuntimeState>,
    updater_state: State<'_, AppUpdaterState>,
) -> Result<(), String> {
    let mut pending = updater_state.pending.lock().await;
    let install_guard = UpdateInstallGuard::begin(runtime_state.inner(), current_platform(&app))?;
    let result =
        install_pending(&mut pending, |update, bytes| update.install(bytes)).map_err(|error| {
            match error {
                PendingInstallError::MissingUpdate => "没有待安装的签名应用更新".to_string(),
                PendingInstallError::NotDownloaded => "应用更新尚未完成下载和签名验证".to_string(),
                PendingInstallError::Install(error) => format!("安装签名应用更新失败: {error}"),
            }
        });
    if result.is_ok() {
        updater_state
            .restart_required
            .store(true, Ordering::Release);
    }
    drop(pending);
    drop(install_guard);
    result
}

#[tauri::command]
pub async fn restart_after_app_update(
    app: AppHandle,
    runtime_state: State<'_, RuntimeState>,
    updater_state: State<'_, AppUpdaterState>,
) -> Result<(), String> {
    let operation = updater_state.pending.lock().await;
    require_restart_latch(&updater_state.restart_required)?;
    let restart_guard = UpdateInstallGuard::begin(runtime_state.inner(), current_platform(&app))?;
    drop(operation);

    app.request_restart();
    std::mem::forget(restart_guard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    const WINDOWS_X64: DesktopPlatform = DesktopPlatform::Windows {
        architecture: DesktopArchitecture::X86_64,
    };
    const WINDOWS_ARM64: DesktopPlatform = DesktopPlatform::Windows {
        architecture: DesktopArchitecture::Aarch64,
    };
    const LINUX_X64_APPIMAGE: DesktopPlatform = DesktopPlatform::Linux {
        app_image: true,
        architecture: DesktopArchitecture::X86_64,
    };
    const LINUX_ARM64_APPIMAGE: DesktopPlatform = DesktopPlatform::Linux {
        app_image: true,
        architecture: DesktopArchitecture::Aarch64,
    };
    const LINUX_X64_DEB: DesktopPlatform = DesktopPlatform::Linux {
        app_image: false,
        architecture: DesktopArchitecture::X86_64,
    };

    fn signed_metadata(file_name: &str) -> (MinisignPublicKey, String) {
        let key_pair =
            Ed25519KeyPair::from_seed_unchecked(&[7_u8; 32]).expect("fixture signing key is valid");
        let key_id = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        let mut key = [0_u8; 32];
        key.copy_from_slice(key_pair.public_key().as_ref());
        let public_key = MinisignPublicKey { key_id, key };

        let primary_signature = [9_u8; 64];
        let trusted_comment = format!("timestamp:1784695535\tfile:{file_name}");
        let mut global_message = Vec::from(primary_signature);
        global_message.extend_from_slice(trusted_comment.as_bytes());
        let global_signature = key_pair.sign(&global_message);

        let mut payload = Vec::with_capacity(74);
        payload.extend_from_slice(&[0x45, 0x44]);
        payload.extend_from_slice(&key_id);
        payload.extend_from_slice(&primary_signature);
        let signature = format!(
            "untrusted comment: signature from test key\n{}\ntrusted comment: {}\n{}\n",
            BASE64.encode(payload),
            trusted_comment,
            BASE64.encode(global_signature.as_ref()),
        );
        (public_key, BASE64.encode(signature))
    }

    #[test]
    fn platform_policy_only_allows_supported_install_formats() {
        assert!(InstallPolicy::evaluate(WINDOWS_X64, false).supported);
        assert!(InstallPolicy::evaluate(LINUX_X64_APPIMAGE, false).supported);
        assert_eq!(
            InstallPolicy::evaluate(LINUX_X64_DEB, false),
            InstallPolicy {
                supported: false,
                block_reason: Some(InstallBlockReason::UnsupportedPlatform),
            }
        );
        assert_eq!(
            InstallPolicy::evaluate(
                DesktopPlatform::Macos {
                    explicitly_enabled: false,
                },
                false,
            ),
            InstallPolicy {
                supported: false,
                block_reason: Some(InstallBlockReason::UnsupportedPlatform),
            }
        );
        assert!(
            InstallPolicy::evaluate(
                DesktopPlatform::Macos {
                    explicitly_enabled: true,
                },
                false,
            )
            .supported
        );
        assert!(
            !InstallPolicy::evaluate(
                DesktopPlatform::Windows {
                    architecture: DesktopArchitecture::Other,
                },
                false,
            )
            .supported
        );
        assert!(!InstallPolicy::evaluate(DesktopPlatform::Other, false).supported);
    }

    #[test]
    fn either_hosting_or_joined_access_blocks_install_but_not_download_capability() {
        for active in [
            has_active_ride_parts(true, 0, 0),
            has_active_ride_parts(false, 1, 0),
            has_active_ride_parts(true, 1, 0),
            has_active_ride_parts(false, 0, 1),
        ] {
            let policy = InstallPolicy::evaluate(WINDOWS_X64, active);
            assert!(policy.supported);
            assert_eq!(policy.block_reason, Some(InstallBlockReason::ActiveRide));
        }

        assert!(!has_active_ride_parts(false, 0, 0));
        assert_eq!(
            InstallPolicy::evaluate(WINDOWS_X64, false).block_reason,
            None
        );
    }

    #[test]
    fn unsupported_platform_takes_priority_over_active_ride() {
        let policy = InstallPolicy::evaluate(LINUX_X64_DEB, true);
        assert!(!policy.supported);
        assert_eq!(
            policy.block_reason,
            Some(InstallBlockReason::UnsupportedPlatform)
        );
    }

    #[test]
    fn macos_gate_requires_an_explicit_true_value() {
        assert!(explicit_opt_in(Some(OsStr::new("1"))));
        assert!(explicit_opt_in(Some(OsStr::new(" true "))));
        assert!(!explicit_opt_in(Some(OsStr::new("0"))));
        assert!(!explicit_opt_in(Some(OsStr::new("yes"))));
        assert!(!explicit_opt_in(None));
    }

    #[test]
    fn signed_metadata_binds_download_file_and_version_for_auto_install_targets() {
        let file_name = "可信拼车_0.0.5_x64-setup.exe";
        let (public_key, signature) = signed_metadata(file_name);
        let download_url = url::Url::parse(
            "https://github.com/example/repo/releases/download/v0.0.5/%E5%8F%AF%E4%BF%A1%E6%8B%BC%E8%BD%A6_0.0.5_x64-setup.exe",
        )
        .expect("fixture URL is valid");

        validate_authenticated_update_fields(
            &signature,
            &download_url,
            "0.0.5",
            WINDOWS_X64,
            &public_key,
        )
        .expect("signed filename, URL, and version match");
        assert_eq!(
            validate_authenticated_update_fields(
                &signature,
                &download_url,
                "0.0.6",
                WINDOWS_X64,
                &public_key,
            ),
            Err("更新签名文件名与声明版本不匹配".to_string())
        );

        let wrong_url = url::Url::parse(
            "https://github.com/example/repo/releases/download/v0.0.5/other_0.0.5_x64-setup.exe",
        )
        .expect("fixture URL is valid");
        assert_eq!(
            validate_authenticated_update_fields(
                &signature,
                &wrong_url,
                "0.0.5",
                WINDOWS_X64,
                &public_key,
            ),
            Err("更新签名文件名与下载地址不匹配".to_string())
        );
    }

    #[test]
    fn signed_metadata_rejects_platform_and_architecture_swaps() {
        let fixtures = [
            ("可信拼车_0.0.5_x64-setup.exe", LINUX_X64_APPIMAGE),
            ("可信拼车_0.0.5_amd64.AppImage", WINDOWS_X64),
            ("可信拼车_0.0.5.app.tar.gz", WINDOWS_X64),
            ("可信拼车_0.0.5_arm64-setup.exe", WINDOWS_X64),
            ("可信拼车_0.0.5_aarch64.AppImage", LINUX_X64_APPIMAGE),
        ];

        for (file_name, platform) in fixtures {
            let (public_key, signature) = signed_metadata(file_name);
            let download_url = url::Url::parse(&format!(
                "https://github.com/example/repo/releases/download/v0.0.5/{file_name}"
            ))
            .expect("fixture URL is valid");
            assert_eq!(
                validate_authenticated_update_fields(
                    &signature,
                    &download_url,
                    "0.0.5",
                    platform,
                    &public_key,
                ),
                Err("更新签名文件名与当前平台或架构不匹配".to_string())
            );
        }

        for (file_name, platform) in [
            ("可信拼车_0.0.5_arm64-setup.exe", WINDOWS_ARM64),
            ("可信拼车_0.0.5_aarch64.AppImage", LINUX_ARM64_APPIMAGE),
        ] {
            let (public_key, signature) = signed_metadata(file_name);
            let download_url = url::Url::parse(&format!(
                "https://github.com/example/repo/releases/download/v0.0.5/{file_name}"
            ))
            .expect("fixture URL is valid");
            validate_authenticated_update_fields(
                &signature,
                &download_url,
                "0.0.5",
                platform,
                &public_key,
            )
            .expect("matching native architecture is accepted");
        }
    }

    #[test]
    fn stable_versions_reject_prerelease_and_build_artifact_names() {
        for file_name in [
            "可信拼车_0.0.5-rc.1_x64-setup.exe",
            "可信拼车_0.0.5+build.7_x64-setup.exe",
            "可信拼车_0.0.5.1_x64-setup.exe",
        ] {
            let (public_key, signature) = signed_metadata(file_name);
            let download_url = url::Url::parse(&format!(
                "https://github.com/example/repo/releases/download/v0.0.5/{file_name}"
            ))
            .expect("fixture URL is valid");
            assert_eq!(
                validate_authenticated_update_fields(
                    &signature,
                    &download_url,
                    "0.0.5",
                    WINDOWS_X64,
                    &public_key,
                ),
                Err("更新签名文件名与声明版本不匹配".to_string())
            );
        }

        assert!(file_name_contains_version(
            "可信拼车_0.0.5_x64-setup.exe",
            "0.0.5"
        ));
        assert!(!file_name_contains_version(
            "可信拼车_0.0.5-rc.1_x64-setup.exe",
            "0.0.5"
        ));
        assert!(!file_name_contains_version(
            "可信拼车_0.0.5+build.7_x64-setup.exe",
            "0.0.5"
        ));
        assert!(!file_name_contains_version(
            "可信拼车_0.0.5.1_x64-setup.exe",
            "0.0.5"
        ));
    }

    #[test]
    fn signed_metadata_rejects_tampering_and_keeps_macos_version_gate_disabled() {
        let (public_key, signature) = signed_metadata("可信拼车.app.tar.gz");
        let download_url = url::Url::parse(
            "https://github.com/example/repo/releases/download/v0.0.5/%E5%8F%AF%E4%BF%A1%E6%8B%BC%E8%BD%A6.app.tar.gz",
        )
        .expect("fixture URL is valid");
        validate_authenticated_update_fields(
            &signature,
            &download_url,
            "0.0.5",
            DesktopPlatform::Macos {
                explicitly_enabled: true,
            },
            &public_key,
        )
        .expect("macOS remains filename-bound while its artifacts omit the version");

        let decoded = String::from_utf8(BASE64.decode(&signature).expect("fixture base64"))
            .expect("fixture UTF-8");
        let tampered = BASE64.encode(decoded.replace("可信拼车.app.tar.gz", "other.app.tar.gz"));
        assert_eq!(
            authenticated_signature_file(&tampered, &public_key),
            Err("更新签名元数据未通过内置公钥验证".to_string())
        );
    }

    #[test]
    fn embedded_updater_public_key_has_the_expected_minisign_shape() {
        let public_key = parse_embedded_updater_public_key().expect("embedded key is valid");
        assert_ne!(public_key.key_id, [0_u8; 8]);
        assert_ne!(public_key.key, [0_u8; 32]);
    }

    #[test]
    fn failed_install_retains_the_pending_update_and_verified_bytes() {
        let mut pending = Some(PendingUpdate {
            update: "0.0.5",
            verified_bytes: Some(vec![1, 2, 3, 4]),
            total_bytes: Some(4),
        });

        let result = install_pending(&mut pending, |_update, _bytes| Err("disk full"));

        assert_eq!(result, Err(PendingInstallError::Install("disk full")));
        let restored = pending.as_ref().expect("pending update must remain");
        assert_eq!(restored.update, "0.0.5");
        assert_eq!(
            restored.verified_bytes.as_deref(),
            Some([1, 2, 3, 4].as_slice())
        );
        assert_eq!(restored.total_bytes, Some(4));
    }

    #[test]
    fn successful_install_is_the_only_transition_that_clears_pending_state() {
        let mut pending = Some(PendingUpdate {
            update: "0.0.5",
            verified_bytes: Some(vec![1, 2, 3, 4]),
            total_bytes: Some(4),
        });

        install_pending(&mut pending, |_update, bytes| {
            assert_eq!(bytes, [1, 2, 3, 4]);
            Ok::<_, ()>(())
        })
        .expect("install succeeds");

        assert!(pending.is_none());
    }

    #[test]
    fn a_verified_download_survives_rechecks() {
        let pending = Some(PendingUpdate {
            update: "0.0.5",
            verified_bytes: Some(vec![1, 2, 3]),
            total_bytes: Some(3),
        });
        assert_eq!(
            verified_pending(&pending).map(|package| package.update),
            Some("0.0.5")
        );

        let discovered = Some(PendingUpdate::discovered("0.0.6"));
        assert!(verified_pending(&discovered).is_none());
    }

    #[test]
    fn update_install_and_ride_transitions_exclude_each_other_without_holding_runtime_lock() {
        let state = RuntimeState::default();
        let ride = state
            .begin_ride_transition()
            .expect("ride transition starts");
        assert!(UpdateInstallGuard::begin(&state, WINDOWS_X64).is_err());
        drop(ride);

        let install =
            UpdateInstallGuard::begin(&state, WINDOWS_X64).expect("update installation starts");
        assert!(state.begin_ride_transition().is_err());
        assert!(state.inner.try_lock().is_ok());
        drop(install);

        assert!(state.begin_ride_transition().is_ok());
    }

    #[test]
    fn restart_requires_a_successful_install_latch_and_supports_retry() {
        let restart_required = AtomicBool::new(false);
        assert_eq!(
            require_restart_latch(&restart_required),
            Err("没有已安装且等待重启的应用更新".to_string())
        );

        restart_required.store(true, Ordering::Release);
        assert!(require_restart_latch(&restart_required).is_ok());
        assert!(require_restart_latch(&restart_required).is_ok());
    }

    #[test]
    fn progress_and_update_dtos_use_the_frontend_contract() {
        let progress = serde_json::to_value(progress_event(
            AppUpdateProgressEvent::Progress,
            128,
            Some(256),
        ))
        .expect("serialize progress");
        assert_eq!(
            progress,
            serde_json::json!({
                "event": "progress",
                "downloadedBytes": 128,
                "totalBytes": 256,
            })
        );

        let info = SignedAppUpdateInfo {
            current_version: "0.0.4".to_string(),
            version: "0.0.5".to_string(),
            notes: Some("Security update".to_string()),
            date: Some("2026-07-22T00:00:00Z".to_string()),
            install_supported: true,
            install_block_reason: Some(InstallBlockReason::ActiveRide),
        };
        let info = serde_json::to_value(info).expect("serialize update info");
        assert_eq!(info["currentVersion"], "0.0.4");
        assert_eq!(info["version"], "0.0.5");
        assert_eq!(info["notes"], "Security update");
        assert_eq!(info["date"], "2026-07-22T00:00:00Z");
        assert_eq!(info["installSupported"], true);
        assert_eq!(info["installBlockReason"], "activeRide");
    }
}
