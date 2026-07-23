use crate::client_process::{
    canonical_path_key, focus_process, list_processes, process_belongs_to_tool, profile_matches,
    terminate_process, ProcessInfo,
};
use crate::diagnostics;
use crate::models::{RideAccess, ToolKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const CLAUDE_PROFILE_ID: &str = "00000000-0000-4000-8000-000000157211";
const CLAUDE_PROFILE_NAME: &str = "可信拼车";
const ROUTE_STATE_DIR: &str = "client-routes";
const CLAUDE_PROFILE_DIR: &str = "client-profiles/claude";
const CODEX_PROFILE_DIR: &str = "client-profiles/codex";
const INSTANCE_REGISTRY_FILE: &str = "client-instances.json";
const INSTANCE_REGISTRY_VERSION: u8 = 1;
const CLIENT_READY_TIMEOUT: Duration = Duration::from_secs(8);
const CLIENT_READY_POLL: Duration = Duration::from_millis(200);
static CLIENT_REGISTRY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProfileLaunchSettings {
    env: BTreeMap<String, String>,
    args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClientInstanceStatus {
    Starting,
    Ready,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLaunchResult {
    pub instance_id: String,
    pub status: ClientInstanceStatus,
    pub reused: bool,
    pub ready_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInstanceSummary {
    pub instance_id: String,
    pub access_id: String,
    pub tool: ToolKind,
    pub status: ClientInstanceStatus,
    pub process_id: u32,
    pub launched_at_ms: i64,
    pub ready_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientInstanceRecord {
    instance_id: String,
    access_id: String,
    tool: ToolKind,
    status: ClientInstanceStatus,
    strategy: RouteStrategy,
    profile_path: Option<PathBuf>,
    launcher_fingerprint: String,
    process_id: Option<u32>,
    process_started_at: Option<String>,
    launched_at_ms: i64,
    ready_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientInstanceRegistry {
    version: u8,
    instances: Vec<ClientInstanceRecord>,
}

impl Default for ClientInstanceRegistry {
    fn default() -> Self {
        Self {
            version: INSTANCE_REGISTRY_VERSION,
            instances: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct LaunchReceipt {
    child: Child,
    tracks_application_process: bool,
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
    ClaudeIsolatedProfile,
    // Retained so upgrades can restore routes created by versions before
    // per-ride desktop profiles were introduced.
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

#[cfg(target_os = "windows")]
fn supports_isolated_profile(launcher: &DesktopLauncher) -> bool {
    !matches!(launcher, DesktopLauncher::WindowsAppUri(_))
}

#[cfg(not(target_os = "windows"))]
fn supports_isolated_profile(_launcher: &DesktopLauncher) -> bool {
    true
}

pub fn launch(
    app: &AppHandle,
    kind: ToolKind,
    access: &RideAccess,
    session_secret: &str,
) -> Result<ToolLaunchResult, String> {
    let _guard = lock_registry()?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    let detected = detect_client(kind);
    if !detected.supported {
        return Err(detected.detail);
    }
    let launcher = detected.launcher.ok_or_else(|| detected.detail.clone())?;
    let isolated_supported = supports_isolated_profile(&launcher);

    let processes = list_processes()?;
    let mut registry = load_registry(&app_data)?;
    prune_stale_records(&app_data, &mut registry, &processes)?;

    if let Some(existing) = registry
        .instances
        .iter()
        .find(|record| record.access_id == access.access_id && record.tool == kind)
        .cloned()
    {
        let process = matching_record_process(&existing, &processes)
            .ok_or_else(|| "拼车客户端状态已失效，请重试".to_string())?;
        focus_process(process.pid)?;
        return Ok(launch_result(&existing, true));
    }

    if !isolated_supported {
        if registry.instances.iter().any(|record| record.tool == kind) {
            return Err("Microsoft Store 客户端一次只能由一辆车管理，请先离开当前车辆".to_string());
        }
        if processes
            .iter()
            .any(|process| process_belongs_to_tool(process, kind))
        {
            return Err(
                "检测到普通客户端正在运行；Microsoft Store 版本无法隔离拼车配置，请先自行退出普通客户端，或改用独立安装版/终端"
                    .to_string(),
            );
        }
    }

    let route = match kind {
        ToolKind::Claude => {
            if isolated_supported {
                let profile = client_profile_path(&app_data, kind, &access.access_id)?;
                prepare_claude_profile(&profile, &local_base(access, kind), session_secret)?;
                ActiveRoute {
                    access_id: access.access_id.clone(),
                    strategy: RouteStrategy::ClaudeIsolatedProfile,
                    profile_path: Some(profile),
                }
            } else {
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
        }
        ToolKind::Codex => {
            if isolated_supported {
                let profile = client_profile_path(&app_data, kind, &access.access_id)?;
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

    if !isolated_supported {
        if let Err(error) = write_json_secure(&active_route_path(&app_data, kind), &route) {
            let _ = rollback_prepared_route(&app_data, kind, &route);
            return Err(error);
        }
    }

    let launched_at_ms = now_ms();
    let instance_id = Uuid::new_v4().to_string();
    let mut record = ClientInstanceRecord {
        instance_id: instance_id.clone(),
        access_id: access.access_id.clone(),
        tool: kind,
        status: ClientInstanceStatus::Starting,
        strategy: route.strategy,
        profile_path: route.profile_path.clone(),
        launcher_fingerprint: launcher_fingerprint(&launcher, kind),
        process_id: None,
        process_started_at: None,
        launched_at_ms,
        ready_at_ms: None,
    };
    registry.instances.push(record.clone());
    if let Err(error) = save_registry(&app_data, &registry) {
        let _ = rollback_prepared_route(&app_data, kind, &route);
        return Err(error);
    }

    let preexisting = processes
        .iter()
        .map(|process| process.pid)
        .collect::<BTreeSet<_>>();
    let receipt = match launch_client(&launcher, kind, route.profile_path.as_deref()) {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = remove_instance_record(&app_data, &instance_id, false);
            return Err(error);
        }
    };
    let direct_pid = receipt.child.id();
    let ready = wait_for_ready_process(
        &launcher,
        kind,
        route.profile_path.as_deref(),
        &preexisting,
        &receipt,
        CLIENT_READY_TIMEOUT,
    );
    let process = match ready {
        Ok(process) => process,
        Err(error) => {
            let rollback_is_safe = receipt.tracks_application_process
                && !preexisting.contains(&direct_pid)
                && terminate_process(direct_pid).is_ok();
            if rollback_is_safe {
                let _ = remove_instance_record(&app_data, &instance_id, false);
            } else {
                diagnostics::record(
                    "warn",
                    "client-launcher",
                    "kept conservative client launch guard after readiness failure because the launched process could not be safely terminated",
                );
            }
            return Err(error);
        }
    };

    record.process_id = Some(process.pid);
    record.process_started_at = Some(process.started_at);
    record.status = ClientInstanceStatus::Ready;
    record.ready_at_ms = Some(now_ms());
    if let Some(slot) = registry
        .instances
        .iter_mut()
        .find(|candidate| candidate.instance_id == instance_id)
    {
        *slot = record.clone();
    }
    if let Err(error) = save_registry(&app_data, &registry) {
        match terminate_record_process(&record) {
            Ok(()) => {
                let _ = remove_instance_record(&app_data, &instance_id, false);
            }
            Err(terminate_error) => diagnostics::record(
                "warn",
                "client-launcher",
                format!(
                    "kept conservative client launch guard because exact process state could not be saved and rollback failed: {terminate_error}"
                ),
            ),
        }
        return Err(error);
    }
    Ok(launch_result(&record, false))
}

pub fn restore_access(app: &AppHandle, access_id: &str, _reopen: bool) -> Result<(), String> {
    let _guard = lock_registry()?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    let registry = load_registry(&app_data)?;
    let instance_ids = registry
        .instances
        .iter()
        .filter(|record| record.access_id == access_id)
        .map(|record| record.instance_id.clone())
        .collect::<Vec<_>>();
    for instance_id in instance_ids {
        remove_instance_record(&app_data, &instance_id, true)?;
    }
    Ok(())
}

pub fn recover_stale(app: &AppHandle) -> Result<(), String> {
    let _guard = lock_registry()?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    let Some(mut registry) = load_registry_for_recovery(&app_data)? else {
        // A corrupt or newer registry cannot be trusted for process ownership.
        // Its records are quarantined for inspection, while legacy backups are
        // still safe to restore because that path never terminates processes.
        migrate_legacy_routes(&app_data)?;
        return Ok(());
    };
    if !registry.instances.is_empty() {
        // Windows process discovery may cold-start WMI. Query once for the
        // entire registry, and leave every unverified record untouched when
        // discovery fails instead of guessing which ordinary client to close.
        let processes = list_processes()?;
        let mut retained = Vec::new();
        let mut recovery_errors = Vec::new();
        for record in registry.instances.drain(..) {
            if let Some(process) = matching_record_process(&record, &processes) {
                if let Err(error) = terminate_process(process.pid) {
                    recovery_errors.push(error);
                    retained.push(record);
                    continue;
                }
            } else if possible_record_process(&record, &processes).is_some() {
                // The on-disk starting record may be the conservative guard
                // left when the final exact registry save failed. Never remove
                // a profile/config that a matching live process may still use.
                retained.push(record);
                continue;
            }
            if let Err(error) = cleanup_record_files(&app_data, &record) {
                recovery_errors.push(error);
                retained.push(record);
            }
        }
        registry.instances = retained;
        save_registry(&app_data, &registry)?;
        if !recovery_errors.is_empty() {
            return Err(format!(
                "{} 个客户端实例将在下次启动时继续安全恢复: {}",
                recovery_errors.len(),
                recovery_errors.join("；")
            ));
        }
        if !registry.instances.is_empty() {
            return Err(format!(
                "{} 个客户端实例仍可能使用拼车配置，已保留恢复状态并延后旧路由迁移",
                registry.instances.len()
            ));
        }
    }
    save_registry(&app_data, &registry)?;
    migrate_legacy_routes(&app_data)?;
    Ok(())
}

pub fn list_instances(app: &AppHandle) -> Result<Vec<ClientInstanceSummary>, String> {
    let _guard = lock_registry()?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    let processes = list_processes()?;
    let mut registry = load_registry(&app_data)?;
    prune_stale_records(&app_data, &mut registry, &processes)?;
    Ok(registry
        .instances
        .iter()
        .filter_map(instance_summary)
        .collect())
}

pub fn focus_instance(app: &AppHandle, instance_id: &str) -> Result<(), String> {
    let _guard = lock_registry()?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    let registry = load_registry(&app_data)?;
    let record = registry
        .instances
        .iter()
        .find(|record| record.instance_id == instance_id)
        .ok_or_else(|| "客户端实例不存在或已经退出".to_string())?;
    let processes = list_processes()?;
    let process = matching_record_process(record, &processes)
        .ok_or_else(|| "客户端实例已经退出".to_string())?;
    focus_process(process.pid)
}

pub fn close_instance(app: &AppHandle, instance_id: &str) -> Result<bool, String> {
    let _guard = lock_registry()?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位客户端临时配置目录: {error}"))?;
    remove_instance_record(&app_data, instance_id, true)
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

fn migrate_legacy_routes(app_data: &Path) -> Result<(), String> {
    for kind in [ToolKind::Claude, ToolKind::Codex] {
        let marker_path = active_route_path(app_data, kind);
        let route = read_json::<ActiveRoute>(&marker_path).ok().flatten();
        let backup = backup_dir(app_data, kind);

        // Older versions managed one route per tool and sometimes terminated
        // every process with the same application name. Migration only repairs
        // files and profiles; it never guesses which ordinary client to close.
        if backup.join("manifest.json").exists() {
            restore_snapshot(&backup)?;
        }
        if let Some(route) = route {
            if matches!(
                route.strategy,
                RouteStrategy::ClaudeIsolatedProfile | RouteStrategy::CodexIsolatedHome
            ) {
                if let Some(profile) = route.profile_path {
                    remove_profile_directory(&profile)?;
                }
            }
        }
        remove_file_if_exists(&marker_path)?;
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
            let result = restore_snapshot(&backup_dir(app_data, kind));
            let marker_result = remove_file_if_exists(&active_route_path(app_data, kind));
            result.and(marker_result)
        }
        RouteStrategy::ClaudeIsolatedProfile | RouteStrategy::CodexIsolatedHome => {
            if let Some(profile) = route.profile_path.as_deref() {
                if profile.exists() {
                    fs::remove_dir_all(profile).map_err(|error| {
                        format!("无法清理客户端临时配置 {}: {error}", profile.display())
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

fn prepare_claude_profile(profile: &Path, base_url: &str, api_key: &str) -> Result<(), String> {
    if profile.exists() {
        fs::remove_dir_all(profile)
            .map_err(|error| format!("无法重建 Claude 拼车配置 {}: {error}", profile.display()))?;
    }
    fs::create_dir_all(profile)
        .map_err(|error| format!("无法创建 Claude 拼车配置 {}: {error}", profile.display()))?;
    secure_directory(profile)?;

    let library = profile.join("configLibrary");
    let result = (|| {
        write_deployment_mode(&profile.join("claude_desktop_config.json"), "3p")?;
        write_json_secure(
            &library.join(format!("{CLAUDE_PROFILE_ID}.json")),
            &claude_gateway_profile(base_url, api_key),
        )?;
        write_claude_meta(&library.join("_meta.json"))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(profile);
    }
    result
}

fn prepare_codex_profile(profile: &Path, base_url: &str, api_key: &str) -> Result<(), String> {
    if profile.exists() {
        fs::remove_dir_all(profile)
            .map_err(|error| format!("无法重建 Codex 临时配置 {}: {error}", profile.display()))?;
    }
    fs::create_dir_all(profile)
        .map_err(|error| format!("无法创建 Codex 临时配置 {}: {error}", profile.display()))?;
    secure_directory(profile)?;
    let result = (|| {
        write_json_secure(
            &profile.join("auth.json"),
            &json!({
                "auth_mode": "apikey",
                "OPENAI_API_KEY": api_key,
            }),
        )?;
        atomic_write(
            &profile.join("config.toml"),
            codex_config(base_url, api_key).as_bytes(),
        )?;
        let app_data = profile.join("app-data");
        fs::create_dir_all(&app_data).map_err(|error| {
            format!(
                "无法创建 Codex 客户端运行目录 {}: {error}",
                app_data.display()
            )
        })?;
        secure_directory(&app_data)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(profile);
    }
    result
}

fn codex_config(base_url: &str, api_key: &str) -> String {
    format!(
        "model_provider = \"trusted_carpool\"\n\
         \n\
         [model_providers.trusted_carpool]\n\
         name = \"Trusted Carpool\"\n\
         base_url = {}\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = true\n\
         experimental_bearer_token = {}\n\
         supports_websockets = false\n",
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

fn registry_path(app_data: &Path) -> PathBuf {
    app_data.join(ROUTE_STATE_DIR).join(INSTANCE_REGISTRY_FILE)
}

fn lock_registry() -> Result<MutexGuard<'static, ()>, String> {
    CLIENT_REGISTRY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "客户端实例注册表暂时不可用".to_string())
}

fn load_registry(app_data: &Path) -> Result<ClientInstanceRegistry, String> {
    let registry =
        read_json::<ClientInstanceRegistry>(&registry_path(app_data))?.unwrap_or_default();
    if registry.version != INSTANCE_REGISTRY_VERSION {
        return Err("客户端实例状态版本不受支持，未改动任何普通客户端".to_string());
    }
    Ok(registry)
}

fn load_registry_for_recovery(app_data: &Path) -> Result<Option<ClientInstanceRegistry>, String> {
    let path = registry_path(app_data);
    if !path.exists() {
        return Ok(Some(ClientInstanceRegistry::default()));
    }
    let bytes = fs::read(&path)
        .map_err(|error| format!("无法读取客户端配置状态 {}: {error}", path.display()))?;
    let quarantine_invalid = |load_error: String| {
        let quarantine = path.with_file_name(format!(
            "{INSTANCE_REGISTRY_FILE}.invalid-{}-{}.json",
            now_ms(),
            Uuid::new_v4()
        ));
        fs::rename(&path, &quarantine).map_err(|error| {
            format!(
                "客户端配置状态已损坏 {}: {load_error}；且无法隔离该状态: {error}",
                path.display()
            )
        })?;
        diagnostics::record(
            "warn",
            "client-launcher",
            format!(
                "quarantined unreadable client registry at {}; legacy routes will be restored without terminating processes",
                quarantine.display()
            ),
        );
        Ok(None)
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => return quarantine_invalid(error.to_string()),
    };
    let Some(version) = value.get("version").and_then(Value::as_u64) else {
        return quarantine_invalid("缺少有效版本号".to_string());
    };
    if version != u64::from(INSTANCE_REGISTRY_VERSION) {
        return Err("客户端实例状态版本不受支持，未改动任何普通客户端".to_string());
    }
    match serde_json::from_value::<ClientInstanceRegistry>(value) {
        Ok(registry) => Ok(Some(registry)),
        Err(error) => quarantine_invalid(error.to_string()),
    }
}

fn save_registry(app_data: &Path, registry: &ClientInstanceRegistry) -> Result<(), String> {
    write_json_secure(&registry_path(app_data), registry)
}

fn launch_result(record: &ClientInstanceRecord, reused: bool) -> ToolLaunchResult {
    ToolLaunchResult {
        instance_id: record.instance_id.clone(),
        status: record.status,
        reused,
        ready_at_ms: record.ready_at_ms.unwrap_or(record.launched_at_ms),
    }
}

fn instance_summary(record: &ClientInstanceRecord) -> Option<ClientInstanceSummary> {
    Some(ClientInstanceSummary {
        instance_id: record.instance_id.clone(),
        access_id: record.access_id.clone(),
        tool: record.tool,
        status: record.status,
        process_id: record.process_id?,
        launched_at_ms: record.launched_at_ms,
        ready_at_ms: record.ready_at_ms?,
    })
}

fn prune_stale_records(
    app_data: &Path,
    registry: &mut ClientInstanceRegistry,
    processes: &[ProcessInfo],
) -> Result<(), String> {
    let mut stale = Vec::new();
    registry.instances.retain(|record| {
        let keep = (record.status == ClientInstanceStatus::Starting
            && now_ms().saturating_sub(record.launched_at_ms) <= 10_000)
            || possible_record_process(record, processes).is_some();
        if !keep {
            stale.push(record.clone());
        }
        keep
    });
    for record in stale {
        cleanup_record_files(app_data, &record)?;
    }
    save_registry(app_data, registry)
}

fn remove_instance_record(
    app_data: &Path,
    instance_id: &str,
    terminate: bool,
) -> Result<bool, String> {
    let mut registry = load_registry(app_data)?;
    let Some(index) = registry
        .instances
        .iter()
        .position(|record| record.instance_id == instance_id)
    else {
        return Ok(false);
    };
    let record = registry.instances.remove(index);
    if terminate {
        terminate_record_process(&record)?;
    }
    cleanup_record_files(app_data, &record)?;
    save_registry(app_data, &registry)?;
    Ok(true)
}

fn cleanup_record_files(app_data: &Path, record: &ClientInstanceRecord) -> Result<(), String> {
    match record.strategy {
        RouteStrategy::ClaudeManagedProfile | RouteStrategy::CodexGlobalConfig => {
            let backup = backup_dir(app_data, record.tool);
            if backup.join("manifest.json").exists() {
                restore_snapshot(&backup)?;
            }
            remove_file_if_exists(&active_route_path(app_data, record.tool))?;
        }
        RouteStrategy::ClaudeIsolatedProfile | RouteStrategy::CodexIsolatedHome => {
            if let Some(profile) = record.profile_path.as_deref() {
                remove_profile_directory(profile)?;
            }
        }
    }
    Ok(())
}

fn remove_profile_directory(profile: &Path) -> Result<(), String> {
    match fs::remove_dir_all(profile) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "无法清理客户端临时配置 {}: {error}",
            profile.display()
        )),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Claude => "claude",
        ToolKind::Codex => "codex",
    }
}

fn client_profile_path(
    app_data: &Path,
    kind: ToolKind,
    access_id: &str,
) -> Result<PathBuf, String> {
    let id = Uuid::parse_str(access_id).map_err(|_| "上车凭据格式无效".to_string())?;
    let root = match kind {
        ToolKind::Claude => CLAUDE_PROFILE_DIR,
        ToolKind::Codex => CODEX_PROFILE_DIR,
    };
    Ok(app_data.join(root).join(id.to_string()))
}

fn profile_launch_settings(kind: ToolKind, profile: Option<&Path>) -> ProfileLaunchSettings {
    let Some(profile) = profile else {
        return ProfileLaunchSettings::default();
    };
    let profile_value = profile.to_string_lossy().into_owned();
    let mut env = BTreeMap::new();
    let args = match kind {
        ToolKind::Claude => {
            env.insert("CLAUDE_USER_DATA_DIR".to_string(), profile_value.clone());
            vec!["--user-data-dir".to_string(), profile_value]
        }
        ToolKind::Codex => {
            let app_data = profile.join("app-data").to_string_lossy().into_owned();
            env.insert("CODEX_HOME".to_string(), profile_value);
            env.insert(
                "CODEX_ELECTRON_USER_DATA_PATH".to_string(),
                app_data.clone(),
            );
            vec![format!("--user-data-dir={app_data}")]
        }
    };
    ProfileLaunchSettings { env, args }
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
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("无法提交客户端配置 {}: {error}", path.display()));
    }
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
                "已找到 ChatGPT.app（Codex 客户端），可使用拼车配置独立启动".to_string()
            } else {
                "已安装，可使用拼车配置独立启动".to_string()
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
    specs
        .iter()
        .flat_map(|(bundle, executable)| {
            [system_applications, user_applications]
                .into_iter()
                .map(move |root| (root, bundle, executable))
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
        ],
    };
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return DetectedClient {
            supported: true,
            launcher: Some(DesktopLauncher::Executable(path.clone())),
            display_path: Some(path.to_string_lossy().into_owned()),
            detail: "已安装，可使用拼车配置独立启动".to_string(),
        };
    }

    let package_query = match kind {
        ToolKind::Claude => {
            "Get-AppxPackage | Where-Object { $_.Name -match '^Anthropic.*Claude' } | Select-Object -First 1 -ExpandProperty PackageFamilyName"
        }
        ToolKind::Codex => {
            "Get-AppxPackage | Where-Object { $_.Name -match '^OpenAI\\.(ChatGPT|Codex)' } | Sort-Object @{ Expression = { if ($_.Name -match '^OpenAI\\.ChatGPT') { 0 } else { 1 } } }, @{ Expression = { $_.Version }; Descending = $true } | Select-Object -First 1 -ExpandProperty PackageFamilyName"
        }
    };
    if let Some(uri) = windows_app_uri(package_query) {
        return DetectedClient {
            supported: true,
            launcher: Some(DesktopLauncher::WindowsAppUri(uri.clone())),
            display_path: Some(uri),
            detail: "已安装，将临时切换为拼车配置后启动".to_string(),
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
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", package_query]);
    hide_console_window(&mut command);
    let output = command.output().ok()?;
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
            detail: "已安装，可使用拼车配置独立启动".to_string(),
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

#[cfg(any(target_os = "macos", target_os = "windows", test))]
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
fn launch_client(
    launcher: &DesktopLauncher,
    kind: ToolKind,
    profile: Option<&Path>,
) -> Result<LaunchReceipt, String> {
    let DesktopLauncher::MacBundle(bundle) = launcher;
    let settings = profile_launch_settings(kind, profile);
    let mut command = Command::new("open");
    command
        .env_remove("__CFBundleIdentifier")
        .env_remove("XPC_SERVICE_NAME");
    for (key, value) in settings.env {
        command.arg("--env").arg(format!("{key}={value}"));
    }
    command.args(["-n", "-a"]).arg(bundle);
    if !settings.args.is_empty() {
        command.arg("--args").args(settings.args);
    }
    command
        .spawn()
        .map(|child| LaunchReceipt {
            child,
            tracks_application_process: false,
        })
        .map_err(|error| format!("无法启动客户端 {}: {error}", bundle.display()))
}

#[cfg(target_os = "windows")]
fn launch_client(
    launcher: &DesktopLauncher,
    kind: ToolKind,
    profile: Option<&Path>,
) -> Result<LaunchReceipt, String> {
    let settings = profile_launch_settings(kind, profile);
    match launcher {
        DesktopLauncher::Executable(path) => {
            let mut command = Command::new(path);
            command.envs(settings.env).args(settings.args);
            hide_console_window(&mut command);
            command
                .spawn()
                .map(|child| LaunchReceipt {
                    child,
                    tracks_application_process: true,
                })
                .map_err(|error| format!("无法启动客户端 {}: {error}", path.display()))
        }
        DesktopLauncher::WindowsAppUri(uri) => {
            if profile.is_some() {
                return Err(
                    "Microsoft Store 客户端无法接收独立拼车配置，请使用客户端独立安装版或终端"
                        .to_string(),
                );
            }
            let mut command = Command::new("explorer.exe");
            command.arg(uri);
            hide_console_window(&mut command);
            command
                .spawn()
                .map(|child| LaunchReceipt {
                    child,
                    tracks_application_process: false,
                })
                .map_err(|error| format!("无法启动客户端: {error}"))
        }
    }
}

#[cfg(target_os = "linux")]
fn launch_client(
    launcher: &DesktopLauncher,
    kind: ToolKind,
    profile: Option<&Path>,
) -> Result<LaunchReceipt, String> {
    let DesktopLauncher::Executable(path) = launcher;
    let settings = profile_launch_settings(kind, profile);
    let mut command = Command::new(path);
    command.envs(settings.env).args(settings.args);
    command
        .spawn()
        .map(|child| LaunchReceipt {
            child,
            tracks_application_process: true,
        })
        .map_err(|error| format!("无法启动客户端 {}: {error}", path.display()))
}

fn wait_for_ready_process(
    launcher: &DesktopLauncher,
    kind: ToolKind,
    profile: Option<&Path>,
    preexisting: &BTreeSet<u32>,
    receipt: &LaunchReceipt,
    timeout: Duration,
) -> Result<ProcessInfo, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut candidates = list_processes()?
            .into_iter()
            .filter(|process| !preexisting.contains(&process.pid))
            .filter(|process| process_matches_launch(process, launcher, kind, profile))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|process| (process.pid != receipt.child.id(), process.pid));
        if let Some(process) = candidates.into_iter().next() {
            return Ok(process);
        }
        if Instant::now() >= deadline {
            return Err(
                "客户端已启动但 8 秒内未检测到正确的可执行文件和拼车配置，已回滚；请确认客户端版本支持独立 profile"
                    .to_string(),
            );
        }
        thread::sleep(CLIENT_READY_POLL);
    }
}

fn matching_record_process<'a>(
    record: &ClientInstanceRecord,
    processes: &'a [ProcessInfo],
) -> Option<&'a ProcessInfo> {
    let pid = record.process_id?;
    let started_at = record.process_started_at.as_deref()?;
    processes.iter().find(|process| {
        process.pid == pid
            && process.started_at == started_at
            && process_matches_fingerprint(process, record.tool, &record.launcher_fingerprint)
            && profile_matches(process, record.profile_path.as_deref())
    })
}

fn possible_record_process<'a>(
    record: &ClientInstanceRecord,
    processes: &'a [ProcessInfo],
) -> Option<&'a ProcessInfo> {
    matching_record_process(record, processes).or_else(|| {
        processes.iter().find(|process| {
            process_matches_fingerprint(process, record.tool, &record.launcher_fingerprint)
                && profile_matches(process, record.profile_path.as_deref())
        })
    })
}

fn process_matches_launch(
    process: &ProcessInfo,
    launcher: &DesktopLauncher,
    kind: ToolKind,
    profile: Option<&Path>,
) -> bool {
    process_matches_fingerprint(process, kind, &launcher_fingerprint(launcher, kind))
        && profile_matches(process, profile)
}

fn process_matches_fingerprint(process: &ProcessInfo, kind: ToolKind, fingerprint: &str) -> bool {
    if fingerprint.starts_with("appx:") {
        return process_belongs_to_tool(process, kind);
    }
    canonical_path_key(&process.executable) == fingerprint
}

#[cfg(target_os = "macos")]
fn launcher_fingerprint(launcher: &DesktopLauncher, kind: ToolKind) -> String {
    let DesktopLauncher::MacBundle(bundle) = launcher;
    canonical_path_key(&macos_bundle_executable(bundle, kind))
}

#[cfg(target_os = "macos")]
fn macos_bundle_executable(bundle: &Path, kind: ToolKind) -> PathBuf {
    let executable = match kind {
        ToolKind::Claude => "Claude",
        ToolKind::Codex
            if bundle.file_name().and_then(|name| name.to_str()) == Some("ChatGPT.app") =>
        {
            "ChatGPT"
        }
        ToolKind::Codex => "Codex",
    };
    bundle.join("Contents/MacOS").join(executable)
}

#[cfg(target_os = "windows")]
fn launcher_fingerprint(launcher: &DesktopLauncher, _kind: ToolKind) -> String {
    match launcher {
        DesktopLauncher::Executable(path) => canonical_path_key(path),
        DesktopLauncher::WindowsAppUri(uri) => format!("appx:{uri}"),
    }
}

#[cfg(target_os = "linux")]
fn launcher_fingerprint(launcher: &DesktopLauncher, _kind: ToolKind) -> String {
    let DesktopLauncher::Executable(path) = launcher;
    canonical_path_key(path)
}

fn terminate_record_process(record: &ClientInstanceRecord) -> Result<(), String> {
    let processes = list_processes()?;
    if let Some(process) = matching_record_process(record, &processes) {
        terminate_process(process.pid)?;
    } else if possible_record_process(record, &processes).is_some() {
        return Err(
            "检测到可能仍在使用拼车配置的客户端，但无法验证其精确进程身份；已保留配置以避免影响普通客户端"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_process::{linux_process_start_marker, parse_ps_process_line};
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
    fn codex_profile_is_provider_scoped_and_uses_carpool_auth() {
        let config = codex_config("http://127.0.0.1:25342/access/id/codex/v1", "secret\"token");
        assert!(config.contains("model_provider = \"trusted_carpool\""));
        assert!(config.contains("[model_providers.trusted_carpool]"));
        assert!(config.contains("wire_api = \"responses\""));
        assert!(config.contains("requires_openai_auth = true"));
        assert!(config.contains("experimental_bearer_token = \"secret\\\"token\""));
        assert!(config.contains("supports_websockets = false"));
    }

    #[test]
    fn codex_isolated_profile_separates_config_and_electron_data() {
        let temp = TempDir::new().expect("temp dir");
        let profile = temp.path().join("codex-profile");
        prepare_codex_profile(
            &profile,
            "http://127.0.0.1:25342/access/id/codex/v1",
            "session-secret",
        )
        .expect("prepare profile");

        let config = fs::read_to_string(profile.join("config.toml")).expect("read config");
        let auth: Value = read_json(&profile.join("auth.json")).unwrap().unwrap();
        assert!(config.contains("model_provider = \"trusted_carpool\""));
        assert!(config.contains("experimental_bearer_token = \"session-secret\""));
        assert_eq!(auth["auth_mode"], "apikey");
        assert_eq!(auth["OPENAI_API_KEY"], "session-secret");
        assert!(profile.join("app-data").is_dir());
    }

    #[test]
    fn claude_isolated_profile_contains_only_the_carpool_gateway() {
        let temp = TempDir::new().expect("temp dir");
        let profile = temp.path().join("claude-profile");
        prepare_claude_profile(
            &profile,
            "http://127.0.0.1:25342/access/id/claude",
            "session-secret",
        )
        .expect("prepare profile");

        let desktop: Value = read_json(&profile.join("claude_desktop_config.json"))
            .unwrap()
            .unwrap();
        let gateway: Value = read_json(
            &profile
                .join("configLibrary")
                .join(format!("{CLAUDE_PROFILE_ID}.json")),
        )
        .unwrap()
        .unwrap();
        let meta: Value = read_json(&profile.join("configLibrary/_meta.json"))
            .unwrap()
            .unwrap();

        assert_eq!(desktop["deploymentMode"], "3p");
        assert_eq!(gateway["inferenceProvider"], "gateway");
        assert_eq!(
            gateway["inferenceGatewayBaseUrl"],
            "http://127.0.0.1:25342/access/id/claude"
        );
        assert_eq!(gateway["inferenceGatewayApiKey"], "session-secret");
        assert_eq!(meta["appliedId"], CLAUDE_PROFILE_ID);
    }

    #[test]
    fn isolated_desktop_launches_bind_their_profile_environment_and_arguments() {
        let profile = Path::new("/tmp/trusted carpool/profile");

        let claude = profile_launch_settings(ToolKind::Claude, Some(profile));
        assert_eq!(
            claude.env.get("CLAUDE_USER_DATA_DIR"),
            Some(&profile.to_string_lossy().into_owned())
        );
        assert_eq!(
            claude.args,
            vec![
                "--user-data-dir".to_string(),
                profile.to_string_lossy().into_owned()
            ]
        );

        let codex = profile_launch_settings(ToolKind::Codex, Some(profile));
        let app_data = profile.join("app-data").to_string_lossy().into_owned();
        assert_eq!(
            codex.env.get("CODEX_HOME"),
            Some(&profile.to_string_lossy().into_owned())
        );
        assert_eq!(
            codex.env.get("CODEX_ELECTRON_USER_DATA_PATH"),
            Some(&app_data)
        );
        assert_eq!(codex.args, vec![format!("--user-data-dir={app_data}")]);
    }

    #[test]
    fn instance_registry_keeps_multiple_cars_without_session_secrets() {
        let temp = TempDir::new().expect("temp dir");
        let records = ["access-a", "access-b"]
            .into_iter()
            .enumerate()
            .map(|(index, access_id)| ClientInstanceRecord {
                instance_id: format!("instance-{index}"),
                access_id: access_id.to_string(),
                tool: ToolKind::Codex,
                status: ClientInstanceStatus::Ready,
                strategy: RouteStrategy::CodexIsolatedHome,
                profile_path: Some(temp.path().join(access_id)),
                launcher_fingerprint: "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"
                    .to_string(),
                process_id: Some(100 + index as u32),
                process_started_at: Some(format!("start-{index}")),
                launched_at_ms: 1000 + index as i64,
                ready_at_ms: Some(2000 + index as i64),
            })
            .collect::<Vec<_>>();
        let registry = ClientInstanceRegistry {
            version: INSTANCE_REGISTRY_VERSION,
            instances: records,
        };
        save_registry(temp.path(), &registry).expect("save registry");
        let loaded = load_registry(temp.path()).expect("load registry");
        assert_eq!(loaded.instances.len(), 2);
        assert_ne!(loaded.instances[0].access_id, loaded.instances[1].access_id);
        let stored = fs::read_to_string(registry_path(temp.path())).expect("read registry");
        assert!(!stored.contains("session-secret"));
        assert!(!stored.contains("apiKey"));
    }

    #[test]
    fn incomplete_launch_guard_keeps_profile_while_matching_process_may_use_it() {
        let temp = TempDir::new().expect("temp dir");
        let profile = temp.path().join("client-profiles/codex/access-a");
        fs::create_dir_all(&profile).expect("create guarded profile");
        let executable = PathBuf::from("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT");
        let record = ClientInstanceRecord {
            instance_id: "guarded-instance".to_string(),
            access_id: "access-a".to_string(),
            tool: ToolKind::Codex,
            status: ClientInstanceStatus::Starting,
            strategy: RouteStrategy::CodexIsolatedHome,
            profile_path: Some(profile.clone()),
            launcher_fingerprint: canonical_path_key(&executable),
            process_id: None,
            process_started_at: None,
            launched_at_ms: 0,
            ready_at_ms: None,
        };
        let mut registry = ClientInstanceRegistry {
            version: INSTANCE_REGISTRY_VERSION,
            instances: vec![record],
        };
        let processes = vec![ProcessInfo {
            pid: 4242,
            executable,
            command_line: format!(
                "ChatGPT --user-data-dir={}",
                profile.join("app-data").display()
            ),
            started_at: "start-a".to_string(),
        }];

        prune_stale_records(temp.path(), &mut registry, &processes).expect("prune registry");

        assert_eq!(registry.instances.len(), 1);
        assert!(profile.exists());
    }

    #[test]
    fn corrupt_registry_is_quarantined_before_legacy_backup_recovery() {
        let temp = TempDir::new().expect("temp dir");
        let config = temp.path().join("user/config.toml");
        atomic_write(&config, b"personal=true\n").expect("write personal config");
        snapshot_files(
            &backup_dir(temp.path(), ToolKind::Codex),
            "old-access",
            std::slice::from_ref(&config),
        )
        .expect("snapshot");
        atomic_write(&config, b"carpool=true\n").expect("write managed config");
        atomic_write(&registry_path(temp.path()), b"{not-json").expect("write corrupt registry");

        assert!(load_registry_for_recovery(temp.path())
            .expect("quarantine corrupt registry")
            .is_none());
        migrate_legacy_routes(temp.path()).expect("restore legacy route");

        assert_eq!(fs::read(&config).unwrap(), b"personal=true\n");
        assert!(!registry_path(temp.path()).exists());
        let route_dir = temp.path().join(ROUTE_STATE_DIR);
        assert!(fs::read_dir(route_dir)
            .expect("read route state")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{INSTANCE_REGISTRY_FILE}.invalid-"))));
    }

    #[test]
    fn unknown_registry_version_is_preserved_and_blocks_unsafe_recovery() {
        let temp = TempDir::new().expect("temp dir");
        atomic_write(
            &registry_path(temp.path()),
            br#"{"version":255,"instances":[{"status":"suspended","futureField":true}]}"#,
        )
        .expect("write future registry");

        let error = load_registry_for_recovery(temp.path()).expect_err("reject future registry");
        assert!(error.contains("版本不受支持"));
        assert!(registry_path(temp.path()).exists());
    }

    #[test]
    fn malformed_current_registry_is_quarantined_for_safe_legacy_recovery() {
        let temp = TempDir::new().expect("temp dir");
        atomic_write(
            &registry_path(temp.path()),
            br#"{"version":1,"instances":[{"status":"suspended"}]}"#,
        )
        .expect("write malformed current registry");

        assert!(load_registry_for_recovery(temp.path())
            .expect("quarantine malformed current registry")
            .is_none());
        assert!(!registry_path(temp.path()).exists());
    }

    #[test]
    fn process_matching_rejects_pid_reuse_and_wrong_profiles() {
        let profile = PathBuf::from("/tmp/trusted-carpool/access-a");
        let process = ProcessInfo {
            pid: 4242,
            executable: PathBuf::from("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"),
            command_line: format!(
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT --user-data-dir={}",
                profile.join("app-data").display()
            ),
            started_at: "Wed Jul 22 10:00:00 2026".to_string(),
        };
        let mut record = ClientInstanceRecord {
            instance_id: "instance-a".to_string(),
            access_id: "access-a".to_string(),
            tool: ToolKind::Codex,
            status: ClientInstanceStatus::Ready,
            strategy: RouteStrategy::CodexIsolatedHome,
            profile_path: Some(profile),
            launcher_fingerprint: canonical_path_key(&process.executable),
            process_id: Some(process.pid),
            process_started_at: Some(process.started_at.clone()),
            launched_at_ms: 1,
            ready_at_ms: Some(2),
        };
        let processes = vec![process];
        assert!(matching_record_process(&record, &processes).is_some());

        record.process_started_at = Some("Thu Jul 23 10:00:00 2026".to_string());
        assert!(matching_record_process(&record, &processes).is_none());
        record.process_started_at = Some(processes[0].started_at.clone());
        record.profile_path = Some(PathBuf::from("/tmp/trusted-carpool/access-b"));
        assert!(matching_record_process(&record, &processes).is_none());
    }

    #[test]
    fn legacy_route_migration_restores_files_without_process_termination() {
        let temp = TempDir::new().expect("temp dir");
        let config = temp.path().join("user/config.toml");
        atomic_write(&config, b"personal=true\n").expect("write personal config");
        snapshot_files(
            &backup_dir(temp.path(), ToolKind::Codex),
            "old-access",
            std::slice::from_ref(&config),
        )
        .expect("snapshot");
        atomic_write(&config, b"carpool=true\n").expect("write managed config");
        write_json_secure(
            &active_route_path(temp.path(), ToolKind::Codex),
            &ActiveRoute {
                access_id: "old-access".to_string(),
                strategy: RouteStrategy::CodexGlobalConfig,
                profile_path: None,
            },
        )
        .expect("write legacy marker");

        migrate_legacy_routes(temp.path()).expect("migrate");
        assert_eq!(fs::read(&config).unwrap(), b"personal=true\n");
        assert!(!active_route_path(temp.path(), ToolKind::Codex).exists());
    }

    #[test]
    fn process_parsers_keep_stable_start_markers() {
        let ps = parse_ps_process_line(
            " 4242 Wed Jul 22 10:11:12 2026 /Applications/Claude.app/Contents/MacOS/Claude --user-data-dir /tmp/profile",
        )
        .expect("parse ps");
        assert_eq!(ps.pid, 4242);
        assert_eq!(ps.started_at, "Wed Jul 22 10:11:12 2026");
        assert!(ps.command_line.ends_with("--user-data-dir /tmp/profile"));

        let mut fields = vec!["S".to_string(); 19];
        fields.push("123456".to_string());
        assert_eq!(
            linux_process_start_marker(&format!("4242 (client app) {}", fields.join(" "))),
            Some("123456".to_string())
        );
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

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_desktop_detection_prefers_the_current_chatgpt_app() {
        let temp = TempDir::new().expect("temp dir");
        let system = temp.path().join("Applications");
        let user = temp.path().join("UserApplications");
        for executable in [
            user.join("ChatGPT.app/Contents/MacOS/ChatGPT"),
            system.join("Codex.app/Contents/MacOS/Codex"),
        ] {
            fs::create_dir_all(executable.parent().unwrap()).expect("create fake app");
            fs::write(&executable, b"fake executable").expect("write fake app");
        }

        assert_eq!(
            find_macos_client(ToolKind::Codex, &system, &user),
            Some(user.join("ChatGPT.app"))
        );
    }
}
