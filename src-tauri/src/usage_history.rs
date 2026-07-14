use crate::models::ToolKind;
use crate::pricing::{estimate, BillableTokens};
use crate::usage::UsageDelta;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

const HISTORY_VERSION: u8 = 1;
static HISTORY_APPEND_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
pub struct UsageHistoryContext<'a> {
    pub car_id: &'a str,
    pub car_name: &'a str,
    pub seat_no: u8,
    pub nickname: &'a str,
    pub passenger_peer_id: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistoryRecord {
    pub version: u8,
    pub event_id: String,
    pub occurred_at: i64,
    pub car_id: String,
    pub car_name: String,
    pub seat_no: u8,
    pub nickname: String,
    pub passenger_peer_id: String,
    pub tool: ToolKind,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub official_cost_microusd: Option<u64>,
    pub pricing_source: Option<String>,
}

impl UsageHistoryRecord {
    pub fn from_usage(context: UsageHistoryContext<'_>, delta: &UsageDelta) -> Self {
        let official_price = estimate(
            delta.tool,
            &delta.model,
            BillableTokens {
                input: delta.input_tokens,
                output: delta.output_tokens,
                cache_read: delta.cache_read_tokens,
                cache_write_5m: delta.cache_write_5m_tokens,
                cache_write_1h: delta.cache_write_1h_tokens,
            },
            delta.occurred_at,
        );
        Self {
            version: HISTORY_VERSION,
            event_id: Uuid::new_v4().to_string(),
            occurred_at: delta.occurred_at,
            car_id: context.car_id.to_string(),
            car_name: context.car_name.to_string(),
            seat_no: context.seat_no,
            nickname: context.nickname.to_string(),
            passenger_peer_id: context.passenger_peer_id.to_string(),
            tool: delta.tool,
            model: delta.model.clone(),
            input_tokens: delta.input_tokens,
            output_tokens: delta.output_tokens,
            cache_read_tokens: delta.cache_read_tokens,
            cache_write_5m_tokens: delta.cache_write_5m_tokens,
            cache_write_1h_tokens: delta.cache_write_1h_tokens,
            official_cost_microusd: official_price.as_ref().map(|price| price.cost_microusd),
            pricing_source: official_price.map(|price| price.source.to_string()),
        }
    }
}

pub fn prepare(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "用量记录目录无效".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建用量记录目录: {error}"))?;
    Ok(())
}

pub fn append(path: &Path, record: &UsageHistoryRecord) -> Result<(), String> {
    let _guard = HISTORY_APPEND_LOCK
        .lock()
        .map_err(|_| "用量记录写入锁暂时不可用".to_string())?;
    prepare(path)?;
    #[cfg(unix)]
    let existed = path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("无法打开用量记录: {error}"))?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("无法保护用量记录权限: {error}"))?;
    }
    serde_json::to_writer(&mut file, record)
        .map_err(|error| format!("无法编码用量记录: {error}"))?;
    file.write_all(b"\n")
        .map_err(|error| format!("无法写入用量记录: {error}"))?;
    file.sync_data()
        .map_err(|error| format!("无法持久化用量记录: {error}"))?;
    Ok(())
}

#[allow(dead_code)]
pub fn read_all(path: &Path) -> Result<Vec<UsageHistoryRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path).map_err(|error| format!("无法读取用量记录: {error}"))?;
    BufReader::new(file)
        .lines()
        .filter_map(|line| match line {
            Ok(line) if line.trim().is_empty() => None,
            other => Some(other),
        })
        .map(|line| {
            let line = line.map_err(|error| format!("无法读取用量记录行: {error}"))?;
            serde_json::from_str(&line).map_err(|error| format!("用量记录格式无效: {error}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_delta() -> UsageDelta {
        UsageDelta {
            tool: ToolKind::Claude,
            model: "claude-sonnet-4-6".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 300,
            cache_write_5m_tokens: 400,
            cache_write_1h_tokens: 0,
            occurred_at: 1_783_987_200_000,
        }
    }

    #[test]
    fn append_only_history_retains_per_person_per_model_usage_without_payloads() {
        let path = std::env::temp_dir().join(format!(
            "trusted-carpool-usage-history-{}.jsonl",
            Uuid::new_v4()
        ));
        let context = UsageHistoryContext {
            car_id: "car-1",
            car_name: "熟人车队",
            seat_no: 2,
            nickname: "小雨",
            passenger_peer_id: "peer-2",
        };
        let first = UsageHistoryRecord::from_usage(context.clone(), &sample_delta());
        let second = UsageHistoryRecord::from_usage(context, &sample_delta());
        append(&path, &first).expect("first append");
        append(&path, &second).expect("second append");

        let records = read_all(&path).expect("read history");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].nickname, "小雨");
        assert_eq!(records[0].model, "claude-sonnet-4-6");
        assert!(records[0].official_cost_microusd.is_some());
        let raw = fs::read_to_string(&path).expect("raw history");
        for forbidden in [
            "prompt",
            "body",
            "response",
            "secret",
            "apiKey",
            "inviteCode",
        ] {
            assert!(!raw.contains(forbidden), "history leaked {forbidden}");
        }
        let _ = fs::remove_file(path);
    }
}
