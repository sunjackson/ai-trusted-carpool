use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_ENTRIES: usize = 500;
static LOGS: OnceLock<Mutex<VecDeque<DebugLogEntry>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLogEntry {
    id: u64,
    timestamp: u64,
    level: &'static str,
    source: &'static str,
    message: String,
}

fn logs() -> &'static Mutex<VecDeque<DebugLogEntry>> {
    LOGS.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LOG_ENTRIES)))
}

pub fn record(level: &'static str, source: &'static str, message: impl Into<String>) {
    let message = message.into();
    #[cfg(debug_assertions)]
    eprintln!("[{level}] [{source}] {message}");

    let entry = DebugLogEntry {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        level,
        source,
        message,
    };
    if let Ok(mut entries) = logs().lock() {
        if entries.len() == MAX_LOG_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(entry);
    }
}

#[tauri::command]
pub fn get_debug_logs() -> Vec<DebugLogEntry> {
    logs()
        .lock()
        .map(|entries| entries.iter().cloned().collect())
        .unwrap_or_default()
}

#[tauri::command]
pub fn clear_debug_logs() {
    if let Ok(mut entries) = logs().lock() {
        entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
