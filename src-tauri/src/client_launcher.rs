use crate::models::{RideAccess, ToolKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const CLAUDE_PROFILE_ID: &str = "00000000-0000-4000-8000-000000157211";
const CLAUDE_PROFILE_NAME: &str = "可信拼车";
const ROUTE_STATE_DIR: &str = "client-routes";
const CODEX_PROFILE_DIR: &str = "client-profiles/codex";

#[derive(Debug, Clone)]
pub struct DesktopClientDetection {
    pub supported: bool,
    pub installed: bool,
    pub path: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone)]
enum DesktopLauncher {
    #[cfg(target_os = "macos")]
    MacBundle(PathBuf),
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    Executable(PathBuf),
    #[cfg(target_os = "windows")]
    WindowsAppUri(String),
}

#[derive(Debug, Clone)]
struct DetectedClient {
    supported: bool,
    launcher: Option<DesktopLauncher>,
    display_path: Option<String>,
    detail: String,
}

#[derive(Debug, Clone)]
struct ClaudePaths {
    normal_config: PathBuf,
    threep_config: PathBuf,
    profile: PathBuf,
    meta: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    access_id: String,
    files: Vec<BackupFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupFile {
    path: PathBuf,
    existed: bool,
    backup_name: Option<String>,
    unix_mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveRoute {
    access_id: String,
    strategy: RouteStrategy,
    profile_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RouteStrategy {
    ClaudeManagedProfile,
    CodexIsolatedHome,
    CodexGlobalConfig,
}

pub fn detect(kind: ToolKind) -> DesktopClientDetection {
    let detected = detect_client(kind);
    DesktopClientDetection {
        supported: detected.supported,
        installed: detected.launcher.is_some(),
        path: detected.display_path,
        detail: detected.detail,
    }
}

pub fn launch(
    app: &AppHandle,
    kind: ToolKind,
    access: &RideAccess,
    session_secret: &str,
) -> Result<(), String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    let detected = detect_client(kind);
    if !detected.supported {
        return Err(detected.detail);
    }
    let launcher = detected.launcher.ok_or_else(|| detected.detail.clone())?;

    restore_kind(&app_data, kind, None, false)?;
    close_client(kind);
    thread::sleep(Duration::from_millis(350));

    let route = match kind {
        ToolKind::Claude => {
            let paths = current_claude_paths()?;
            let backup_dir = backup_dir(&app_data, kind);
            apply_claude_route(
                &paths,
                &backup_dir,
                &access.access_id,
                &local_base(access, kind),
                session_secret,
            )?;
            ActiveRoute {
                access_id: access.access_id.clone(),
                strategy: RouteStrategy::ClaudeManagedProfile,
                profile_path: None,
            }
        }
        ToolKind::Codex => {
            let isolated_supported = match &launcher {
                #[cfg(target_os = "windows")]
                DesktopLauncher::WindowsAppUri(_) => false,
                _ => true,
            };
            if isolated_supported {
                let profile = codex_profile_path(&app_data, &access.access_id)?;
                prepare_codex_profile(&profile, &local_base(access, kind), session_secret)?;
                ActiveRoute {
                    access_id: access.access_id.clone(),
                    strategy: RouteStrategy::CodexIsolatedHome,
                    profile_path: Some(profile),
                }
            } else {
                let config = user_codex_config_path();
                snapshot_files(
                    &backup_dir(&app_data, kind),
                    &access.access_id,
                    std::slice::from_ref(&config),
                )?;
                atomic_write(
                    &config,
                    codex_config(&local_base(access, kind), session_secret).as_bytes(),
                )?;
                ActiveRoute {
                    access_id: access.access_id.clone(),
                    strategy: RouteStrategy::CodexGlobalConfig,
                    profile_path: None,
                }
            }
        }
    };

    if let Err(error) = write_json_secure(&active_route_path(&app_data, kind), &route) {
        let _ = rollback_prepared_route(&app_data, kind, &route);
        return Err(error);
    }
    if let Err(error) = launch_client(&launcher, route.profile_path.as_deref()) {
        let _ = restore_kind(&app_data, kind, Some(&access.access_id), false);
        return Err(error);
    }
    Ok(())
}

pub fn restore_access(app: &AppHandle, access_id: &str, reopen: bool) -> Result<(), String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    for kind in [ToolKind::Claude, ToolKind::Codex] {
        restore_kind(&app_data, kind, Some(access_id), reopen)?;
    }
    Ok(())
}

pub fn recover_stale(app: &AppHandle) -> Result<(), String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    for kind in [ToolKind::Claude, ToolKind::Codex] {
        restore_kind(&app_data, kind, None, false)?;
        let backup = backup_dir(&app_data, kind);
        if backup.join("manifest.json").exists() {
            restore_snapshot(&backup)?;
        }
    }
    let profiles = app_data.join(CODEX_PROFILE_DIR);
    if profiles.exists() {
        fs::remove_dir_all(&profiles)
            .map_err(|error| format!("无法清理 Codex 临时配置 {}: {error}", profiles.display()))?;
    }
    Ok(())
}

fn local_base(access: &RideAccess, kind: ToolKind) -> String {
    let root = format!(
        "http://127.0.0.1:{}/access/{}",
        access.local_proxy_port, access.access_id
    );
    match kind {
        ToolKind::Claude => format!("{root}/claude"),
        ToolKind::Codex => format!("{root}/codex/v1"),
    }
}

fn restore_kind(
    app_data: &Path,
    kind: ToolKind,
    expected_access: Option<&str>,
    reopen: bool,
) -> Result<(), String> {
    let marker_path = active_route_path(app_data, kind);
    let route = match read_json::<ActiveRoute>(&marker_path) {
        Ok(route) => route,
        Err(_) => {
            close_client(kind);
            let backup = backup_dir(app_data, kind);
            if backup.join("manifest.json").exists() {
                restore_snapshot(&backup)?;
            }
            if matches!(kind, ToolKind::Codex) {
                let profiles = app_data.join(CODEX_PROFILE_DIR);
                if profiles.exists() {
                    fs::remove_dir_all(&profiles).map_err(|error| {
                        format!("无法清理 Codex 临时配置 {}: {error}", profiles.display())
                    })?;
                }
            }
            remove_file_if_exists(&marker_path)?;
            return Ok(());
        }
    };
    let Some(route) = route else {
        return Ok(());
    };
    if expected_access.is_some_and(|expected| route.access_id != expected) {
        return Ok(());
    }

    close_client(kind);
    thread::sleep(Duration::from_millis(250));
    match route.strategy {
        RouteStrategy::ClaudeManagedProfile | RouteStrategy::CodexGlobalConfig => {
            restore_snapshot(&backup_dir(app_data, kind))?;
        }
        RouteStrategy::CodexIsolatedHome => {
            if let Some(profile) = route.profile_path {
                if profile.exists() {
                    fs::remove_dir_all(&profile).map_err(|error| {
                        format!("无法清理 Codex 临时配置 {}: {error}", profile.display())
                    })?;
                }
            }
        }
    }
    remove_file_if_exists(&marker_path)?;

    if reopen {
        if let Some(launcher) = detect_client(kind).launcher {
            launch_client(&launcher, None)?;
        }
    }
    Ok(())
}

fn rollback_prepared_route(
    app_data: &Path,
    kind: ToolKind,
    route: &ActiveRoute,
) -> Result<(), String> {
    match route.strategy {
        RouteStrategy::ClaudeManagedProfile | RouteStrategy::CodexGlobalConfig => {
            restore_snapshot(&backup_dir(app_data, kind))
        }
        RouteStrategy::CodexIsolatedHome => {
            if let Some(profile) = route.profile_path.as_deref() {
                if profile.exists() {
                    fs::remove_dir_all(profile).map_err(|error| {
                        format!("无法清理 Codex 临时配置 {}: {error}", profile.display())
                    })?;
                }
            }
            Ok(())
        }
    }
}

fn apply_claude_route(
    paths: &ClaudePaths,
    backup_dir: &Path,
    access_id: &str,
    base_url: &str,
    api_key: &str,
) -> Result<(), String> {
    let targets = [
        paths.normal_config.clone(),
        paths.threep_config.clone(),
        paths.profile.clone(),
        paths.meta.clone(),
    ];
    snapshot_files(backup_dir, access_id, &targets)?;
    let result = (|| {
        write_deployment_mode(&paths.normal_config, "3p")?;
        write_deployment_mode(&paths.threep_config, "3p")?;
        write_json_secure(&paths.profile, &claude_gateway_profile(base_url, api_key))?;
        write_claude_meta(&paths.meta)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = restore_snapshot(backup_dir);
    }
    result
}

fn claude_gateway_profile(base_url: &str, api_key: &str) -> Value {
    json!({
        "coworkEgressAllowedHosts": ["*"],
        "disableDeploymentModeChooser": true,
        "inferenceGatewayApiKey": api_key,
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceGatewayBaseUrl": base_url,
        "inferenceProvider": "gateway",
        "inferenceModels": [
            { "name": "claude-sonnet-4-6", "supports1m": true },
            { "name": "claude-opus-4-8", "supports1m": true },
            { "name": "claude-haiku-4-5", "supports1m": true }
        ]
    })
}

fn write_deployment_mode(path: &Path, mode: &str) -> Result<(), String> {
    let mut value = read_json_value(path)?.unwrap_or_else(|| json!({}));
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("Claude 配置不是 JSON 对象: {}", path.display()))?;
    object.insert(
        "deploymentMode".to_string(),
        Value::String(mode.to_string()),
    );
    write_json_secure(path, &value)
}

fn write_claude_meta(path: &Path) -> Result<(), String> {
    let mut value = read_json_value(path)?.unwrap_or_else(|| json!({}));
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("Claude 配置索引不是 JSON 对象: {}", path.display()))?;
    let mut entries = object
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    entries.retain(|entry| entry.get("id").and_then(Value::as_str) != Some(CLAUDE_PROFILE_ID));
    entries.push(json!({ "id": CLAUDE_PROFILE_ID, "name": CLAUDE_PROFILE_NAME }));
    object.insert(
        "appliedId".to_string(),
        Value::String(CLAUDE_PROFILE_ID.to_string()),
    );
    object.insert("entries".to_string(), Value::Array(entries));
    write_json_secure(path, &value)
}

fn prepare_codex_profile(profile: &Path, base_url: &str, api_key: &str) -> Result<(), String> {
    if profile.exists() {
        fs::remove_dir_all(profile)
            .map_err(|error| format!("无法重建 Codex 临时配置 {}: {error}", profile.display()))?;
    }
    fs::create_dir_all(profile)
        .map_err(|error| format!("无法创建 Codex 临时配置 {}: {error}", profile.display()))?;
    secure_directory(profile)?;
    atomic_write(
        &profile.join("config.toml"),
        codex_config(base_url, api_key).as_bytes(),
    )
}

fn codex_config(base_url: &str, api_key: &str) -> String {
    format!(
        "model_provider = \"trusted_carpool\"\n\
         \n\
         [model_providers.trusted_carpool]\n\
         name = \"Trusted Carpool\"\n\
         base_url = {}\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = false\n\
         experimental_bearer_token = {}\n",
        toml_string(base_url),
        toml_string(api_key)
    )
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

fn snapshot_files(backup_dir: &Path, access_id: &str, paths: &[PathBuf]) -> Result<(), String> {
    if backup_dir.exists() {
        restore_snapshot(backup_dir)?;
    }
    fs::create_dir_all(backup_dir)
        .map_err(|error| format!("无法创建客户端配置备份 {}: {error}", backup_dir.display()))?;
    secure_directory(backup_dir)?;

    let mut files = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let existed = path.exists();
        let backup_name = existed.then(|| format!("file-{index}.bin"));
        let unix_mode = if existed { file_unix_mode(path)? } else { None };
        if let Some(name) = backup_name.as_deref() {
            let bytes = fs::read(path)
                .map_err(|error| format!("无法备份客户端配置 {}: {error}", path.display()))?;
            atomic_write(&backup_dir.join(name), &bytes)?;
        }
        files.push(BackupFile {
            path: path.clone(),
            existed,
            backup_name,
            unix_mode,
        });
    }
    write_json_secure(
        &backup_dir.join("manifest.json"),
        &BackupManifest {
            access_id: access_id.to_string(),
            files,
        },
    )
}

fn restore_snapshot(backup_dir: &Path) -> Result<(), String> {
    let manifest_path = backup_dir.join("manifest.json");
    let Some(manifest) = read_json::<BackupManifest>(&manifest_path)? else {
        return Ok(());
    };
    for file in manifest.files {
        if file.existed {
            let name = file
                .backup_name
                .ok_or_else(|| "客户端配置备份索引损坏".to_string())?;
            let bytes = fs::read(backup_dir.join(name))
                .map_err(|error| format!("无法读取客户端配置备份: {error}"))?;
            atomic_write(&file.path, &bytes)?;
            restore_unix_mode(&file.path, file.unix_mode)?;
        } else {
            remove_file_if_exists(&file.path)?;
        }
    }
    fs::remove_dir_all(backup_dir).map_err(|error| {
        format!(
            "无法删除已恢复的客户端配置备份 {}: {error}",
            backup_dir.display()
        )
    })?;
    Ok(())
}

fn backup_dir(app_data: &Path, kind: ToolKind) -> PathBuf {
    app_data
        .join(ROUTE_STATE_DIR)
        .join(format!("{}-backup", kind_name(kind)))
}

fn active_route_path(app_data: &Path, kind: ToolKind) -> PathBuf {
    app_data
        .join(ROUTE_STATE_DIR)
        .join(format!("active-{}.json", kind_name(kind)))
}

fn kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Claude => "claude",
        ToolKind::Codex => "codex",
    }
}

fn codex_profile_path(app_data: &Path, access_id: &str) -> Result<PathBuf, String> {
    let id = Uuid::parse_str(access_id).map_err(|_| "上车凭据格式无效".to_string())?;
    Ok(app_data.join(CODEX_PROFILE_DIR).join(id.to_string()))
}

fn user_codex_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex/config.toml")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("无法读取客户端配置状态 {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("客户端配置状态已损坏 {}: {error}", path.display()))
}

fn read_json_value(path: &Path) -> Result<Option<Value>, String> {
    read_json(path)
}

fn write_json_secure(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("无法编码客户端配置: {error}"))?;
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("客户端配置路径无效: {}", path.display()))?;
    let parent_existed = parent.exists();
    fs::create_dir_all(parent)
        .map_err(|error| format!("无法创建客户端配置目录 {}: {error}", parent.display()))?;
    if !parent_existed {
        secure_directory(parent)?;
    }
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config"),
        Uuid::new_v4()
    ));
    fs::write(&temp, bytes)
        .map_err(|error| format!("无法写入客户端临时配置 {}: {error}", temp.display()))?;
    secure_file(&temp)?;
    #[cfg(target_os = "windows")]
    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("无法替换客户端配置 {}: {error}", path.display()))?;
    }
    fs::rename(&temp, path)
        .map_err(|error| format!("无法提交客户端配置 {}: {error}", path.display()))?;
    secure_file(path)
}

fn remove_file_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "无法删除客户端临时配置 {}: {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("无法保护客户端配置 {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("无法保护客户端配置目录 {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn file_unix_mode(path: &Path) -> Result<Option<u32>, String> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| Some(metadata.permissions().mode()))
        .map_err(|error| format!("无法读取客户端配置权限 {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn file_unix_mode(_path: &Path) -> Result<Option<u32>, String> {
    Ok(None)
}

#[cfg(unix)]
fn restore_unix_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("无法恢复客户端配置权限 {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restore_unix_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn detect_client(kind: ToolKind) -> DetectedClient {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let bundle = find_macos_client(kind, Path::new("/Applications"), &home.join("Applications"));
    match bundle {
        Some(path) => DetectedClient {
            supported: true,
            launcher: Some(DesktopLauncher::MacBundle(path.clone())),
            display_path: Some(path.to_string_lossy().into_owned()),
            detail: if matches!(kind, ToolKind::Codex)
                && path.file_name().and_then(|value| value.to_str()) == Some("ChatGPT.app")
            {
                "已找到 ChatGPT.app（Codex 客户端），可一键配置并启动".to_string()
            } else {
                "已安装，可一键配置并启动".to_string()
            },
        },
        None => DetectedClient {
            supported: true,
            launcher: None,
            display_path: None,
            detail: match kind {
                ToolKind::Claude => "未找到官方 Claude 客户端".to_string(),
                ToolKind::Codex => "未找到官方 ChatGPT/Codex 客户端".to_string(),
            },
        },
    }
}

#[cfg(target_os = "macos")]
fn find_macos_client(
    kind: ToolKind,
    system_applications: &Path,
    user_applications: &Path,
) -> Option<PathBuf> {
    let specs: &[(&str, &str)] = match kind {
        ToolKind::Claude => &[("Claude.app", "Contents/MacOS/Claude")],
        ToolKind::Codex => &[
            ("ChatGPT.app", "Contents/MacOS/ChatGPT"),
            ("Codex.app", "Contents/MacOS/Codex"),
        ],
    };
    [system_applications, user_applications]
        .into_iter()
        .flat_map(|root| {
            specs
                .iter()
                .map(move |(bundle, executable)| (root, bundle, executable))
        })
        .find_map(|(root, bundle, executable)| {
            let path = root.join(bundle);
            (path.is_dir() && path.join(executable).is_file()).then_some(path)
        })
}

#[cfg(target_os = "windows")]
fn detect_client(kind: ToolKind) -> DetectedClient {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("AppData/Local"));
    let candidates = match kind {
        ToolKind::Claude => {
            let mut paths = vec![
                local.join("Programs/Claude/Claude.exe"),
                local.join("AnthropicClaude/Claude.exe"),
                local.join("Microsoft/WindowsApps/Claude.exe"),
            ];
            paths.extend(windows_versioned_executables(
                &local.join("AnthropicClaude"),
                "Claude.exe",
            ));
            paths.extend(windows_versioned_executables(
                &local.join("Claude"),
                "Claude.exe",
            ));
            paths
        }
        ToolKind::Codex => vec![
            local.join("Programs/ChatGPT/ChatGPT.exe"),
            local.join("Programs/Codex/Codex.exe"),
            local.join("Microsoft/WindowsApps/ChatGPT.exe"),
            local.join("Microsoft/WindowsApps/Codex.exe"),
        ],
    };
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return DetectedClient {
            supported: true,
            launcher: Some(DesktopLauncher::Executable(path.clone())),
            display_path: Some(path.to_string_lossy().into_owned()),
            detail: "已安装，可一键配置并启动".to_string(),
        };
    }

    let package_query = match kind {
        ToolKind::Claude => {
            "Get-AppxPackage | Where-Object { $_.Name -match '^Anthropic.*Claude' } | Select-Object -First 1 -ExpandProperty PackageFamilyName"
        }
        ToolKind::Codex => {
            "Get-AppxPackage OpenAI.Codex | Select-Object -First 1 -ExpandProperty PackageFamilyName"
        }
    };
    if let Some(uri) = windows_app_uri(package_query) {
        return DetectedClient {
            supported: true,
            launcher: Some(DesktopLauncher::WindowsAppUri(uri.clone())),
            display_path: Some(uri),
            detail: "已安装，可一键配置并启动".to_string(),
        };
    }

    let name = if matches!(kind, ToolKind::Claude) {
        "Claude"
    } else {
        "Codex"
    };
    DetectedClient {
        supported: true,
        launcher: None,
        display_path: None,
        detail: format!("未找到官方 {name} 客户端"),
    }
}

#[cfg(target_os = "windows")]
fn windows_versioned_executables(parent: &Path, executable: &str) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join(executable))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths.reverse();
    paths
}

#[cfg(target_os = "windows")]
fn windows_app_uri(package_query: &str) -> Option<String> {
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", package_query])
        .output()
        .ok()?;
    let family = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !family.is_empty())
        .then(|| format!("shell:AppsFolder\\{family}!App"))
}

#[cfg(target_os = "linux")]
fn detect_client(kind: ToolKind) -> DetectedClient {
    if matches!(kind, ToolKind::Claude) {
        return DetectedClient {
            supported: false,
            launcher: None,
            display_path: None,
            detail: "Claude 官方尚未提供 Linux 桌面客户端，请使用 Claude Code 终端".to_string(),
        };
    }
    let path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .flat_map(|directory| {
            ["codex-desktop", "codex-app"]
                .into_iter()
                .map(move |name| directory.join(name))
        })
        .find(|path| path.is_file());
    match path {
        Some(path) => DetectedClient {
            supported: true,
            launcher: Some(DesktopLauncher::Executable(path.clone())),
            display_path: Some(path.to_string_lossy().into_owned()),
            detail: "已安装，可一键配置并启动".to_string(),
        },
        None => DetectedClient {
            supported: true,
            launcher: None,
            display_path: None,
            detail: "未找到官方 Codex 客户端，请使用 Codex 终端".to_string(),
        },
    }
}

#[cfg(target_os = "macos")]
fn current_claude_paths() -> Result<ClaudePaths, String> {
    let home = dirs::home_dir().ok_or_else(|| "无法定位用户目录".to_string())?;
    Ok(claude_paths_from_dirs(
        home.join("Library/Application Support/Claude"),
        home.join("Library/Application Support/Claude-3p"),
    ))
}

#[cfg(target_os = "windows")]
fn current_claude_paths() -> Result<ClaudePaths, String> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("AppData/Local"));
    Ok(claude_paths_from_dirs(
        pick_windows_claude_dir(&local, false).unwrap_or_else(|| local.join("Claude")),
        pick_windows_claude_dir(&local, true).unwrap_or_else(|| local.join("Claude-3p")),
    ))
}

#[cfg(target_os = "windows")]
fn pick_windows_claude_dir(local: &Path, threep: bool) -> Option<PathBuf> {
    let exact = local.join(if threep { "Claude-3p" } else { "Claude" });
    if exact.exists() {
        return Some(exact);
    }
    let mut candidates = fs::read_dir(local)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            path.is_dir() && name.starts_with("Claude") && name.contains("-3p") == threep
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().next()
}

#[cfg(target_os = "linux")]
fn current_claude_paths() -> Result<ClaudePaths, String> {
    Err("Claude 官方尚未提供 Linux 桌面客户端".to_string())
}

fn claude_paths_from_dirs(normal: PathBuf, threep: PathBuf) -> ClaudePaths {
    let library = threep.join("configLibrary");
    ClaudePaths {
        normal_config: normal.join("claude_desktop_config.json"),
        threep_config: threep.join("claude_desktop_config.json"),
        profile: library.join(format!("{CLAUDE_PROFILE_ID}.json")),
        meta: library.join("_meta.json"),
    }
}

#[cfg(target_os = "macos")]
fn launch_client(launcher: &DesktopLauncher, codex_home: Option<&Path>) -> Result<(), String> {
    let DesktopLauncher::MacBundle(bundle) = launcher;
    let mut command = Command::new("open");
    if let Some(home) = codex_home {
        command
            .arg("--env")
            .arg(format!("CODEX_HOME={}", home.display()));
    }
    command.args(["-n", "-a"]).arg(bundle);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法启动客户端 {}: {error}", bundle.display()))
}

#[cfg(target_os = "windows")]
fn launch_client(launcher: &DesktopLauncher, codex_home: Option<&Path>) -> Result<(), String> {
    match launcher {
        DesktopLauncher::Executable(path) => {
            let mut command = Command::new(path);
            if let Some(home) = codex_home {
                command.env("CODEX_HOME", home);
            }
            command
                .spawn()
                .map(|_| ())
                .map_err(|error| format!("无法启动客户端 {}: {error}", path.display()))
        }
        DesktopLauncher::WindowsAppUri(uri) => Command::new("explorer.exe")
            .arg(uri)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("无法启动 Codex 客户端: {error}")),
    }
}

#[cfg(target_os = "linux")]
fn launch_client(launcher: &DesktopLauncher, codex_home: Option<&Path>) -> Result<(), String> {
    let DesktopLauncher::Executable(path) = launcher;
    let mut command = Command::new(path);
    if let Some(home) = codex_home {
        command.env("CODEX_HOME", home);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法启动客户端 {}: {error}", path.display()))
}

#[cfg(target_os = "macos")]
fn close_client(kind: ToolKind) {
    let bundle_id = match kind {
        ToolKind::Claude => "com.anthropic.claudefordesktop",
        ToolKind::Codex => "com.openai.codex",
    };
    let script = format!("tell application id \"{bundle_id}\" to quit");
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

#[cfg(target_os = "windows")]
fn close_client(kind: ToolKind) {
    let images: &[&str] = if matches!(kind, ToolKind::Claude) {
        &["Claude.exe"]
    } else {
        &["ChatGPT.exe", "Codex.exe"]
    };
    for image in images {
        let _ = Command::new("taskkill")
            .args(["/IM", image, "/F", "/T"])
            .status();
    }
}

#[cfg(target_os = "linux")]
fn close_client(kind: ToolKind) {
    let names: &[&str] = if matches!(kind, ToolKind::Claude) {
        &["claude-desktop"]
    } else {
        &["codex-desktop", "codex-app"]
    };
    for name in names {
        let _ = Command::new("pkill").args(["-x", name]).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn claude_route_uses_managed_gateway_and_restores_exact_files() {
        let temp = TempDir::new().expect("temp dir");
        let paths =
            claude_paths_from_dirs(temp.path().join("Claude"), temp.path().join("Claude-3p"));
        let originals = [
            (
                &paths.normal_config,
                br#"{"deploymentMode":"1p","keep":1}"#.as_slice(),
            ),
            (
                &paths.threep_config,
                br#"{"deploymentMode":"1p","keep":2}"#.as_slice(),
            ),
            (&paths.profile, br#"{"old":true}"#.as_slice()),
            (
                &paths.meta,
                br#"{"appliedId":"old","entries":[{"id":"old","name":"Old"}]}"#.as_slice(),
            ),
        ];
        for (path, bytes) in originals {
            atomic_write(path, bytes).expect("write original");
        }

        let backup = temp.path().join("backup");
        apply_claude_route(
            &paths,
            &backup,
            "access-id",
            "http://127.0.0.1:25342/access/id/claude",
            "secret-token",
        )
        .expect("apply route");

        let normal: Value = read_json(&paths.normal_config).unwrap().unwrap();
        let profile: Value = read_json(&paths.profile).unwrap().unwrap();
        let meta: Value = read_json(&paths.meta).unwrap().unwrap();
        assert_eq!(normal["deploymentMode"], "3p");
        assert_eq!(normal["keep"], 1);
        assert_eq!(profile["inferenceProvider"], "gateway");
        assert_eq!(profile["inferenceGatewayAuthScheme"], "bearer");
        assert_eq!(profile["inferenceGatewayApiKey"], "secret-token");
        assert_eq!(profile["inferenceModels"].as_array().unwrap().len(), 3);
        assert_eq!(meta["appliedId"], CLAUDE_PROFILE_ID);

        restore_snapshot(&backup).expect("restore");
        for (path, bytes) in originals {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn restore_removes_files_that_did_not_exist_before_route() {
        let temp = TempDir::new().expect("temp dir");
        let paths =
            claude_paths_from_dirs(temp.path().join("Claude"), temp.path().join("Claude-3p"));
        let backup = temp.path().join("backup");
        apply_claude_route(
            &paths,
            &backup,
            "access-id",
            "http://127.0.0.1:25342/access/id/claude",
            "secret-token",
        )
        .expect("apply route");
        assert!(paths.profile.exists());
        restore_snapshot(&backup).expect("restore");
        assert!(!paths.normal_config.exists());
        assert!(!paths.threep_config.exists());
        assert!(!paths.profile.exists());
        assert!(!paths.meta.exists());
    }

    #[test]
    fn codex_profile_is_provider_scoped_and_does_not_require_openai_auth() {
        let config = codex_config("http://127.0.0.1:25342/access/id/codex/v1", "secret\"token");
        assert!(config.contains("model_provider = \"trusted_carpool\""));
        assert!(config.contains("[model_providers.trusted_carpool]"));
        assert!(config.contains("wire_api = \"responses\""));
        assert!(config.contains("requires_openai_auth = false"));
        assert!(config.contains("experimental_bearer_token = \"secret\\\"token\""));
        assert!(!config.contains("auth.json"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_desktop_detection_accepts_the_chatgpt_app_name() {
        let temp = TempDir::new().expect("temp dir");
        let system = temp.path().join("Applications");
        let user = temp.path().join("UserApplications");
        let executable = system.join("ChatGPT.app/Contents/MacOS/ChatGPT");
        fs::create_dir_all(executable.parent().unwrap()).expect("create fake app");
        fs::write(&executable, b"fake executable").expect("write fake app");

        assert_eq!(
            find_macos_client(ToolKind::Codex, &system, &user),
            Some(system.join("ChatGPT.app"))
        );
    }
}
