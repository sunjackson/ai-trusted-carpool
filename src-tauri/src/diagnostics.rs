use crate::models::ToolKind;
use crate::runtime::RuntimeState;
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};
use zip::write::SimpleFileOptions;

const MAX_LOG_ENTRIES: usize = 500;
const LOG_RETENTION_DAYS: u64 = 7;
const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;
const MAX_FRONTEND_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_SOURCE_CHARS: usize = 80;
const LOG_DIR_NAME: &str = "diagnostic-logs";
const EXPORT_DIR_NAME: &str = "diagnostic-exports";
const REDACTED: &str = "[REDACTED]";
const REDACTED_EMAIL: &str = "[EMAIL]";
const REDACTED_JOIN_CODE: &str = "[JOIN_CODE]";

static STATE: OnceLock<Mutex<DiagnosticState>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLogEntry {
    id: u64,
    timestamp: u64,
    level: String,
    source: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrontendLogInput {
    level: String,
    source: String,
    message: String,
    #[serde(default)]
    timestamp: Option<u64>,
}

#[derive(Default)]
struct DiagnosticState {
    entries: VecDeque<DebugLogEntry>,
    log_dir: Option<PathBuf>,
}

fn state() -> &'static Mutex<DiagnosticState> {
    STATE.get_or_init(|| Mutex::new(DiagnosticState::default()))
}

pub fn configure(app_data_dir: &Path) -> Result<(), String> {
    let log_dir = app_data_dir.join(LOG_DIR_NAME);
    fs::create_dir_all(&log_dir).map_err(|error| format!("无法创建诊断日志目录: {error}"))?;
    secure_directory(&log_dir)?;
    cleanup_log_files(&log_dir, now_ms())?;
    let historical = load_historical_logs(&log_dir)?;

    let mut diagnostics = state()
        .lock()
        .map_err(|_| "诊断日志暂时不可用".to_string())?;
    if diagnostics.log_dir.is_some() {
        return Ok(());
    }
    let pending = diagnostics.entries.drain(..).collect::<Vec<_>>();
    diagnostics.entries = historical;
    let max_id = diagnostics
        .entries
        .iter()
        .map(|entry| entry.id)
        .max()
        .unwrap_or_default();
    advance_next_id(max_id.saturating_add(1));
    diagnostics.log_dir = Some(log_dir.clone());
    for mut entry in pending {
        entry.id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        push_memory(&mut diagnostics.entries, entry.clone());
        append_entry(&log_dir, &entry)?;
    }
    Ok(())
}

fn advance_next_id(candidate: u64) {
    let mut current = NEXT_ID.load(Ordering::Relaxed);
    while current < candidate {
        match NEXT_ID.compare_exchange_weak(
            current,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

pub fn record(level: &'static str, source: &'static str, message: impl Into<String>) {
    record_at(level, source, message.into(), now_ms());
}

fn record_at(level: &str, source: &str, message: String, timestamp: u64) {
    let level = normalize_level(level).to_string();
    let source = sanitize_source(source);
    let message = redact_message(&message);
    #[cfg(debug_assertions)]
    eprintln!("[{level}] [{source}] {message}");

    let entry = DebugLogEntry {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        timestamp,
        level,
        source,
        message,
    };
    if let Ok(mut diagnostics) = state().lock() {
        push_memory(&mut diagnostics.entries, entry.clone());
        if let Some(log_dir) = diagnostics.log_dir.as_deref() {
            if let Err(_error) = append_entry(log_dir, &entry) {
                #[cfg(debug_assertions)]
                eprintln!("[error] [diagnostics] {}", redact_message(&_error));
            }
        }
    }
}

#[tauri::command]
pub fn record_frontend_log(input: FrontendLogInput) -> Result<(), String> {
    if input.message.len() > MAX_FRONTEND_MESSAGE_BYTES {
        return Err("前端日志消息过大".to_string());
    }
    if input.source.chars().count() > MAX_SOURCE_CHARS {
        return Err("前端日志来源过长".to_string());
    }
    record_at(
        normalize_level(&input.level),
        &format!("frontend · {}", input.source),
        input.message,
        input.timestamp.unwrap_or_else(now_ms),
    );
    Ok(())
}

fn normalize_level(level: &str) -> &'static str {
    match level.to_ascii_lowercase().as_str() {
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    }
}

fn sanitize_source(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_SOURCE_CHARS)
        .collect::<String>()
}

fn push_memory(entries: &mut VecDeque<DebugLogEntry>, entry: DebugLogEntry) {
    if entries.len() == MAX_LOG_ENTRIES {
        entries.pop_front();
    }
    entries.push_back(entry);
}

#[tauri::command]
pub fn get_debug_logs() -> Vec<DebugLogEntry> {
    state()
        .lock()
        .map(|diagnostics| diagnostics.entries.iter().cloned().collect())
        .unwrap_or_default()
}

#[tauri::command]
pub fn clear_debug_logs() {
    if let Ok(mut diagnostics) = state().lock() {
        diagnostics.entries.clear();
        if let Some(log_dir) = diagnostics.log_dir.as_deref() {
            if let Ok(files) = log_files(log_dir) {
                for path in files {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}

#[tauri::command]
pub fn open_debug_log_directory() -> Result<String, String> {
    let directory = state()
        .lock()
        .map_err(|_| "诊断日志暂时不可用".to_string())?
        .log_dir
        .clone()
        .ok_or_else(|| "诊断日志目录尚未初始化".to_string())?;
    open_directory(&directory)?;
    Ok(directory.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn export_diagnostic_bundle(
    app: AppHandle,
    runtime_state: State<'_, RuntimeState>,
) -> Result<String, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位诊断目录: {error}"))?;
    let export_dir = app_data.join(EXPORT_DIR_NAME);
    fs::create_dir_all(&export_dir).map_err(|error| format!("无法创建诊断导出目录: {error}"))?;
    secure_directory(&export_dir)?;
    let output = export_dir.join(format!("trusted-carpool-diagnostics-{}.zip", now_ms()));

    let log_dir = state()
        .lock()
        .map_err(|_| "诊断日志暂时不可用".to_string())?
        .log_dir
        .clone();
    let route_health = {
        let runtime = runtime_state
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        runtime.account_router.summaries(now_ms() as i64)
    };
    let health = route_health
        .into_iter()
        .map(|(id, summary)| {
            json!({
                "accountRef": stable_redacted_id(&id),
                "status": summary.status,
                "reason": summary.reason,
                "cooldownUntilMs": summary.cooldown_until_ms,
                "consecutiveFailures": summary.consecutive_failures,
                "lastAttemptAtMs": summary.last_attempt_at_ms,
                "lastSuccessAtMs": summary.last_success_at_ms,
                "lastFailureAtMs": summary.last_failure_at_ms,
            })
        })
        .collect::<Vec<_>>();
    let clients = [ToolKind::Claude, ToolKind::Codex]
        .into_iter()
        .map(|tool| {
            let detected = crate::client_launcher::detect(tool);
            json!({
                "tool": tool,
                "supported": detected.supported,
                "installed": detected.installed,
                "path": detected.path.map(|path| redact_message(&path)),
                "detail": redact_message(&detected.detail),
            })
        })
        .collect::<Vec<_>>();
    let system = json!({
        "generatedAtMs": now_ms(),
        "appVersion": app.package_info().version.to_string(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
    });
    write_support_bundle(
        &output,
        log_dir.as_deref(),
        &system,
        &Value::Array(clients),
        &Value::Array(health),
    )?;
    secure_file(&output)?;
    Ok(output.to_string_lossy().into_owned())
}

fn write_support_bundle(
    output: &Path,
    log_dir: Option<&Path>,
    system: &Value,
    clients: &Value,
    health: &Value,
) -> Result<(), String> {
    let file = File::create(output).map_err(|error| format!("无法创建诊断包: {error}"))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    write_zip_json(&mut writer, "system.json", system, options)?;
    write_zip_json(&mut writer, "client-detection.json", clients, options)?;
    write_zip_json(&mut writer, "account-route-health.json", health, options)?;
    if let Some(log_dir) = log_dir {
        for path in log_files(log_dir)? {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let mut content = String::new();
            File::open(&path)
                .and_then(|mut file| file.read_to_string(&mut content))
                .map_err(|error| format!("无法读取脱敏日志: {error}"))?;
            writer
                .start_file(format!("logs/{name}"), options)
                .map_err(|error| format!("无法写入诊断包日志: {error}"))?;
            writer
                .write_all(redact_message(&content).as_bytes())
                .map_err(|error| format!("无法写入诊断包日志: {error}"))?;
        }
    }
    writer
        .finish()
        .map_err(|error| format!("无法完成诊断包: {error}"))?;
    Ok(())
}

fn write_zip_json(
    writer: &mut zip::ZipWriter<File>,
    name: &str,
    value: &Value,
    options: SimpleFileOptions,
) -> Result<(), String> {
    writer
        .start_file(name, options)
        .map_err(|error| format!("无法写入诊断包: {error}"))?;
    let encoded = serde_json::to_string_pretty(value)
        .map_err(|error| format!("无法编码诊断信息: {error}"))?;
    writer
        .write_all(redact_message(&encoded).as_bytes())
        .map_err(|error| format!("无法写入诊断包: {error}"))
}

fn stable_redacted_id(value: &str) -> String {
    let hash = digest(&SHA256, value.as_bytes());
    hash.as_ref()[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn append_entry(log_dir: &Path, entry: &DebugLogEntry) -> Result<(), String> {
    let path = log_path(log_dir, entry.timestamp);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("无法写入诊断日志: {error}"))?;
    secure_file(&path)?;
    serde_json::to_writer(&mut file, entry)
        .map_err(|error| format!("无法编码诊断日志: {error}"))?;
    file.write_all(b"\n")
        .map_err(|error| format!("无法写入诊断日志: {error}"))?;
    cleanup_log_files(log_dir, entry.timestamp)
}

fn log_path(log_dir: &Path, timestamp_ms: u64) -> PathBuf {
    log_dir.join(format!("diagnostics-{}.jsonl", timestamp_ms / 86_400_000))
}

fn log_files(log_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = fs::read_dir(log_dir)
        .map_err(|error| format!("无法读取诊断日志目录: {error}"))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("diagnostics-") && name.ends_with(".jsonl"))
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn cleanup_log_files(log_dir: &Path, current_ms: u64) -> Result<(), String> {
    let current_day = current_ms / 86_400_000;
    let mut files = log_files(log_dir)?;
    for path in files.clone() {
        let day = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.strip_prefix("diagnostics-"))
            .and_then(|day| day.parse::<u64>().ok());
        if day.is_some_and(|day| current_day.saturating_sub(day) >= LOG_RETENTION_DAYS) {
            let _ = fs::remove_file(path);
        }
    }
    files = log_files(log_dir)?;
    let mut total = files
        .iter()
        .filter_map(|path| fs::metadata(path).ok().map(|metadata| metadata.len()))
        .sum::<u64>();
    for path in files {
        if total <= MAX_LOG_BYTES {
            break;
        }
        let size = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        fs::remove_file(&path).map_err(|error| format!("无法轮转诊断日志: {error}"))?;
        total = total.saturating_sub(size);
    }
    Ok(())
}

fn load_historical_logs(log_dir: &Path) -> Result<VecDeque<DebugLogEntry>, String> {
    let mut entries = VecDeque::with_capacity(MAX_LOG_ENTRIES);
    for path in log_files(log_dir)? {
        let content =
            fs::read_to_string(&path).map_err(|error| format!("无法读取历史诊断日志: {error}"))?;
        for line in content.lines() {
            let Ok(mut entry) = serde_json::from_str::<DebugLogEntry>(line) else {
                continue;
            };
            entry.level = normalize_level(&entry.level).to_string();
            entry.source = sanitize_source(&entry.source);
            entry.message = redact_message(&entry.message);
            push_memory(&mut entries, entry);
        }
    }
    Ok(entries)
}

fn redact_message(message: &str) -> String {
    let mut redacted = message.to_string();
    if let Some(home) = dirs::home_dir().and_then(|path| path.to_str().map(ToString::to_string)) {
        if !home.is_empty() {
            redacted = redacted.replace(&home, "~");
            redacted = redacted.replace(&home.replace('/', "\\"), "~");
        }
    }
    redacted = redact_after_prefix(&redacted, "bearer ", REDACTED);
    redacted = redact_after_prefix(&redacted, "basic ", REDACTED);
    let sensitive_keys = [
        "access_token",
        "accesstoken",
        "refresh_token",
        "refreshtoken",
        "id_token",
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "password",
        "client_secret",
        "session_secret",
        "sessionsecret",
        "credential",
        "secret",
        "prompt",
        "response_body",
        "request_body",
        "requestbody",
        "environment",
    ];
    for key in sensitive_keys {
        redacted = redact_assignments(&redacted, key);
    }
    redacted = redact_prefixed_tokens(&redacted, "sk-");
    redacted = redact_prefixed_tokens(&redacted, "eyJ");
    redact_words(&redacted)
}

fn redact_assignments(message: &str, key: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find(key) {
        let start = cursor + relative;
        let before_ok = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        let key_end = start + key.len();
        let after_ok = key_end >= lower.len() || !lower.as_bytes()[key_end].is_ascii_alphanumeric();
        if !before_ok || !after_ok {
            output.push_str(&message[cursor..key_end]);
            cursor = key_end;
            continue;
        }
        let bytes = message.as_bytes();
        let mut separator = key_end;
        if separator < bytes.len() && matches!(bytes[separator], b'"' | b'\'' | b'`') {
            separator += 1;
        }
        while separator < bytes.len() && bytes[separator].is_ascii_whitespace() {
            separator += 1;
        }
        if separator >= bytes.len() || !matches!(bytes[separator], b':' | b'=') {
            output.push_str(&message[cursor..key_end]);
            cursor = key_end;
            continue;
        }
        separator += 1;
        while separator < bytes.len() && bytes[separator].is_ascii_whitespace() {
            separator += 1;
        }
        let quote = bytes
            .get(separator)
            .copied()
            .filter(|byte| matches!(byte, b'"' | b'\'' | b'`'));
        let value_start = separator + usize::from(quote.is_some());
        let mut end = value_start;
        while end < bytes.len() {
            if let Some(quote) = quote {
                if bytes[end] == quote {
                    break;
                }
            } else if bytes[end].is_ascii_whitespace()
                || matches!(bytes[end], b',' | b';' | b'}' | b']' | b'&')
            {
                break;
            }
            end += 1;
        }
        output.push_str(&message[cursor..separator]);
        if let Some(quote) = quote {
            output.push(quote as char);
            output.push_str(REDACTED);
            if end < bytes.len() && bytes[end] == quote {
                output.push(quote as char);
                end += 1;
            }
        } else {
            output.push_str(REDACTED);
        }
        cursor = end;
    }
    output.push_str(&message[cursor..]);
    output
}

fn redact_after_prefix(message: &str, prefix: &str, replacement: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let prefix_lower = prefix.to_ascii_lowercase();
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find(&prefix_lower) {
        let start = cursor + relative;
        let value_start = start + prefix.len();
        let mut end = value_start;
        let bytes = message.as_bytes();
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && !matches!(bytes[end], b',' | b';' | b'}' | b']')
        {
            end += 1;
        }
        output.push_str(&message[cursor..value_start]);
        output.push_str(replacement);
        cursor = end;
    }
    output.push_str(&message[cursor..]);
    output
}

fn redact_prefixed_tokens(message: &str, prefix: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0usize;
    while let Some(relative) = message[cursor..].find(prefix) {
        let start = cursor + relative;
        let mut end = start + prefix.len();
        let bytes = message.as_bytes();
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'-' | b'_' | b'.'))
        {
            end += 1;
        }
        if end.saturating_sub(start) < 10 {
            output.push_str(&message[cursor..end]);
        } else {
            output.push_str(&message[cursor..start]);
            output.push_str(REDACTED);
        }
        cursor = end;
    }
    output.push_str(&message[cursor..]);
    output
}

fn redact_words(message: &str) -> String {
    message
        .split_inclusive(|character: char| character.is_whitespace())
        .map(|part| {
            let trimmed = part.trim_matches(|character: char| {
                character.is_whitespace() || ",;()[]{}<>\"'".contains(character)
            });
            let candidate = trimmed.rsplit(['=', ':']).next().unwrap_or(trimmed);
            if looks_like_email(candidate) {
                part.replacen(candidate, REDACTED_EMAIL, 1)
            } else if looks_like_join_code(candidate) {
                part.replacen(candidate, REDACTED_JOIN_CODE, 1)
            } else {
                part.to_string()
            }
        })
        .collect()
}

fn looks_like_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._%+-@".contains(&byte))
}

fn looks_like_join_code(value: &str) -> bool {
    value.len() == 12
        && value
            .bytes()
            .all(|byte| matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'2'..=b'9'))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(target_os = "macos")]
fn open_directory(path: &Path) -> Result<(), String> {
    command_success(Command::new("open").arg(path), "无法打开诊断日志目录")
}

#[cfg(target_os = "windows")]
fn open_directory(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new("explorer.exe");
    command.arg(path).creation_flags(CREATE_NO_WINDOW);
    command_success(&mut command, "无法打开诊断日志目录")
}

#[cfg(target_os = "linux")]
fn open_directory(path: &Path) -> Result<(), String> {
    command_success(Command::new("xdg-open").arg(path), "无法打开诊断日志目录")
}

fn command_success(command: &mut Command, context: &str) -> Result<(), String> {
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("{context}: {error}"))
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("无法保护诊断文件: {error}"))
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("无法保护诊断目录: {error}"))
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn keeps_recent_runtime_logs_available_to_the_debug_panel() {
        clear_debug_logs();
        record("info", "test", "started");
        record("error", "test", "failed");
        let entries = get_debug_logs();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "started");
        assert_eq!(entries[1].level, "error");
    }

    #[test]
    fn redacts_secrets_identity_and_payloads_before_storage() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/Users/test"));
        let message = format!(
            "Authorization: Bearer backend-secret apiKey=sk-ant-secret-value-123456 password='guess-me' email=user@example.com code=ABCD2345EFGH prompt=private-text path={}",
            home.display()
        );
        let redacted = redact_message(&message);
        for forbidden in [
            "backend-secret",
            "sk-ant-secret-value-123456",
            "guess-me",
            "user@example.com",
            "ABCD2345EFGH",
            "private-text",
            home.to_string_lossy().as_ref(),
        ] {
            assert!(!redacted.contains(forbidden), "leaked {forbidden}");
        }
        assert!(redacted.contains(REDACTED));
        assert!(redacted.contains(REDACTED_EMAIL));
        assert!(redacted.contains(REDACTED_JOIN_CODE));
    }

    #[test]
    fn jsonl_history_rotates_by_age_and_reloads_only_recent_entries() {
        let temp = TempDir::new().expect("temp dir");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        let current = 20 * 86_400_000;
        let recent = DebugLogEntry {
            id: 7,
            timestamp: current,
            level: "info".to_string(),
            source: "test".to_string(),
            message: "safe".to_string(),
        };
        append_entry(&log_dir, &recent).expect("append recent");
        let old = log_dir.join("diagnostics-1.jsonl");
        fs::write(&old, "{}\n").unwrap();
        cleanup_log_files(&log_dir, current).expect("cleanup");
        assert!(!old.exists());
        let loaded = load_historical_logs(&log_dir).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].message, "safe");
    }

    #[test]
    fn support_bundle_contains_only_redacted_allowed_categories() {
        let temp = TempDir::new().expect("temp dir");
        let logs = temp.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("diagnostics-1.jsonl"),
            "Authorization: Bearer fake-secret user@example.com ABCD2345EFGH\n",
        )
        .unwrap();
        let output = temp.path().join("support.zip");
        write_support_bundle(
            &output,
            Some(&logs),
            &json!({"appVersion":"0.0.3"}),
            &json!([]),
            &json!([{"accountRef":"abcdef123456","reason":"network"}]),
        )
        .expect("bundle");

        let file = File::open(output).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .collect::<Vec<_>>();
        assert!(names.contains(&"system.json".to_string()));
        assert!(names.contains(&"client-detection.json".to_string()));
        assert!(names.contains(&"account-route-health.json".to_string()));
        assert!(names.iter().any(|name| name.starts_with("logs/")));
        assert!(!names.iter().any(|name| {
            (name.contains("account") && !name.contains("route-health"))
                || name.contains("identity")
                || name.contains("invite")
                || name.contains("request")
        }));
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            for forbidden in ["fake-secret", "user@example.com", "ABCD2345EFGH"] {
                assert!(!content.contains(forbidden));
            }
        }
    }
}
