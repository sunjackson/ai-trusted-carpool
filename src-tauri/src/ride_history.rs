use crate::models::{CarSession, JoinPreview, MemberTokenLimits, ToolKind};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const STORE_VERSION: u8 = 1;
const MAX_RECORDS: usize = 100;
const MAX_STORE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RideRole {
    Host,
    Passenger,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredSeat {
    pub seat_no: u8,
    pub code: String,
    pub token_limits: MemberTokenLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredRideRecord {
    pub record_id: String,
    pub role: RideRole,
    pub car_id: String,
    pub car_name: String,
    pub owner_peer_id: String,
    pub started_at: i64,
    pub expires_at: i64,
    pub always_on: bool,
    pub enabled_tools: Vec<ToolKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat_no: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seats: Vec<StoredSeat>,
    pub created_at: i64,
    pub last_active_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub stopped: bool,
}

impl StoredRideRecord {
    pub fn can_resume_at(&self, now: i64) -> bool {
        !self.stopped && self.ended_at.is_none() && (self.always_on || self.expires_at > now)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedRideHistory {
    version: u8,
    records: Vec<StoredRideRecord>,
}

#[derive(Debug)]
pub struct RideHistoryStore {
    path: PathBuf,
    records: Vec<StoredRideRecord>,
}

impl RideHistoryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            records: Vec::new(),
        }
    }

    /// Opens the history store, quarantining an invalid file and returning
    /// `recovered = true` when an empty store had to be substituted.
    pub fn prepare(&mut self) -> Result<bool, String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| "行程历史路径无效".to_string())?;
        fs::create_dir_all(parent).map_err(|error| format!("无法创建行程历史目录: {error}"))?;

        if !self.path.exists() {
            self.records.clear();
            self.persist()?;
            return Ok(false);
        }

        let parsed = fs::metadata(&self.path)
            .map_err(|error| format!("无法检查行程历史 {}: {error}", self.path.display()))
            .and_then(|metadata| {
                if metadata.len() > MAX_STORE_BYTES {
                    Err("行程历史文件过大".to_string())
                } else {
                    fs::read(&self.path).map_err(|error| {
                        format!("无法读取行程历史 {}: {error}", self.path.display())
                    })
                }
            })
            .and_then(|bytes| {
                serde_json::from_slice::<PersistedRideHistory>(&bytes)
                    .map_err(|error| format!("行程历史已损坏: {error}"))
            })
            .and_then(|state| {
                (state.version == STORE_VERSION)
                    .then_some(state)
                    .ok_or_else(|| "行程历史版本不受支持".to_string())
            });

        match parsed {
            Ok(mut state) => {
                let was_over_limit = state.records.len() > MAX_RECORDS;
                sort_and_limit(&mut state.records);
                self.records = state.records;
                if was_over_limit {
                    self.persist()?;
                } else {
                    secure_file(&self.path)?;
                }
                Ok(false)
            }
            Err(_) => {
                let quarantine = corrupt_path(&self.path);
                fs::rename(&self.path, &quarantine).map_err(|error| {
                    format!("无法隔离损坏的行程历史 {}: {error}", self.path.display())
                })?;
                self.records.clear();
                self.persist()?;
                Ok(true)
            }
        }
    }

    pub fn record_host_started(
        &mut self,
        car: &CarSession,
        now: i64,
    ) -> Result<StoredRideRecord, String> {
        for record in &mut self.records {
            if record.role == RideRole::Host && record.can_resume_at(now) {
                record.stopped = true;
                record.ended_at = Some(now);
                record.last_active_at = now;
            }
        }

        let record = StoredRideRecord {
            record_id: Uuid::new_v4().to_string(),
            role: RideRole::Host,
            car_id: car.car_id.clone(),
            car_name: car.car_name.clone(),
            owner_peer_id: car.owner_peer_id.clone(),
            started_at: car.started_at,
            expires_at: car.expires_at,
            always_on: car.always_on,
            enabled_tools: car.enabled_tools.clone(),
            seat_no: None,
            nickname: None,
            code: None,
            seats: car
                .seats
                .iter()
                .map(|seat| StoredSeat {
                    seat_no: seat.seat_no,
                    code: seat.code.clone(),
                    token_limits: seat.token_limits.clone(),
                })
                .collect(),
            created_at: now,
            last_active_at: now,
            ended_at: None,
            stopped: false,
        };
        self.records.push(record.clone());
        self.commit()?;
        Ok(record)
    }

    pub fn record_host_resumed(
        &mut self,
        record_id: &str,
        now: i64,
    ) -> Result<Option<StoredRideRecord>, String> {
        let Some(index) = self.records.iter().position(|record| {
            record.record_id == record_id
                && record.role == RideRole::Host
                && record.can_resume_at(now)
        }) else {
            return Ok(None);
        };
        self.records[index].last_active_at = now;
        let record = self.records[index].clone();
        self.commit()?;
        Ok(Some(record))
    }

    pub fn record_host_stopped(&mut self, car_id: &str, now: i64) -> Result<bool, String> {
        let mut changed = false;
        for record in &mut self.records {
            if record.role == RideRole::Host
                && record.car_id == car_id
                && !record.stopped
                && record.ended_at.is_none()
            {
                record.stopped = true;
                record.ended_at = Some(now);
                record.last_active_at = now;
                changed = true;
            }
        }
        if changed {
            self.commit()?;
        }
        Ok(changed)
    }

    pub fn record_passenger_joined(
        &mut self,
        preview: &JoinPreview,
        owner_peer_id: &str,
        code: &str,
        nickname: &str,
        now: i64,
    ) -> Result<StoredRideRecord, String> {
        if let Some(index) = self.records.iter().position(|record| {
            record.role == RideRole::Passenger
                && record.car_id == preview.car_id
                && record.seat_no == Some(preview.seat_no)
        }) {
            let record = &mut self.records[index];
            record.car_name = preview.car_name.clone();
            record.owner_peer_id = owner_peer_id.to_string();
            record.started_at = preview.starts_at;
            record.expires_at = preview.expires_at;
            record.always_on = preview.always_on;
            record.enabled_tools = preview.enabled_tools.clone();
            record.nickname = Some(nickname.to_string());
            record.code = Some(code.to_string());
            record.last_active_at = now;
            record.ended_at = None;
            record.stopped = false;
            let updated = record.clone();
            self.commit()?;
            return Ok(updated);
        }

        let record = StoredRideRecord {
            record_id: Uuid::new_v4().to_string(),
            role: RideRole::Passenger,
            car_id: preview.car_id.clone(),
            car_name: preview.car_name.clone(),
            owner_peer_id: owner_peer_id.to_string(),
            started_at: preview.starts_at,
            expires_at: preview.expires_at,
            always_on: preview.always_on,
            enabled_tools: preview.enabled_tools.clone(),
            seat_no: Some(preview.seat_no),
            nickname: Some(nickname.to_string()),
            code: Some(code.to_string()),
            seats: Vec::new(),
            created_at: now,
            last_active_at: now,
            ended_at: None,
            stopped: false,
        };
        self.records.push(record.clone());
        self.commit()?;
        Ok(record)
    }

    pub fn list(&self) -> Vec<StoredRideRecord> {
        let mut records = self.records.clone();
        records.sort_by(record_order);
        records
    }

    pub fn get(&self, record_id: &str) -> Option<StoredRideRecord> {
        self.records
            .iter()
            .find(|record| record.record_id == record_id)
            .cloned()
    }

    fn commit(&mut self) -> Result<(), String> {
        sort_and_limit(&mut self.records);
        self.persist()
    }

    fn persist(&self) -> Result<(), String> {
        let state = PersistedRideHistory {
            version: STORE_VERSION,
            records: self.records.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&state)
            .map_err(|error| format!("无法编码行程历史: {error}"))?;
        atomic_write_private(&self.path, &bytes)
    }
}

fn record_order(left: &StoredRideRecord, right: &StoredRideRecord) -> std::cmp::Ordering {
    right
        .last_active_at
        .cmp(&left.last_active_at)
        .then_with(|| right.created_at.cmp(&left.created_at))
        .then_with(|| right.record_id.cmp(&left.record_id))
}

fn sort_and_limit(records: &mut Vec<StoredRideRecord>) {
    records.sort_by(record_order);
    records.truncate(MAX_RECORDS);
}

fn corrupt_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ride-history.json");
    path.with_file_name(format!(
        "{file_name}.corrupt-{}-{}",
        now_ms(),
        Uuid::new_v4()
    ))
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "行程历史路径无效".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建行程历史目录: {error}"))?;
    let temp = parent.join(format!(".ride-history-{}.tmp", Uuid::new_v4()));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|error| format!("无法创建行程历史临时文件: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("无法写入行程历史: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("无法同步行程历史: {error}"))?;
        drop(file);
        secure_file(&temp)?;

        #[cfg(target_os = "windows")]
        if path.exists() {
            fs::remove_file(path).map_err(|error| format!("无法替换行程历史: {error}"))?;
        }
        fs::rename(&temp, path).map_err(|error| format!("无法提交行程历史: {error}"))?;
        secure_file(path)?;
        sync_directory(parent)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("无法保护行程历史: {error}"))
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("无法同步行程历史目录: {error}"))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AccountQuotaSnapshot, MemberTokenLimitStatus, Seat, SeatState, SeatUsageSummary,
    };
    use tempfile::TempDir;

    fn car(id: &str, code: &str, expires_at: i64) -> CarSession {
        CarSession {
            car_id: id.to_string(),
            car_name: format!("car-{id}"),
            owner_peer_id: "owner-peer".to_string(),
            started_at: 100,
            expires_at,
            always_on: false,
            enabled_tools: vec![ToolKind::Claude, ToolKind::Codex],
            seats: vec![Seat {
                seat_no: 1,
                code: code.to_string(),
                nickname: None,
                state: SeatState::Waiting,
                tool: None,
                usage: SeatUsageSummary::default(),
                token_limits: MemberTokenLimits {
                    five_hour_tokens: Some(1_000),
                    daily_tokens: Some(2_000),
                    weekly_tokens: None,
                },
                token_limit_status: MemberTokenLimitStatus::default(),
                token_usage_events: Vec::new(),
            }],
            account_quotas: Vec::<AccountQuotaSnapshot>::new(),
        }
    }

    fn preview(car_id: &str, seat_no: u8) -> JoinPreview {
        JoinPreview {
            car_id: car_id.to_string(),
            car_name: "shared car".to_string(),
            owner_label: "owner".to_string(),
            seat_no,
            enabled_tools: vec![ToolKind::Codex],
            starts_at: 100,
            expires_at: 10_000,
            always_on: false,
        }
    }

    fn prepared_store(path: impl Into<PathBuf>) -> RideHistoryStore {
        let mut store = RideHistoryStore::new(path);
        store.prepare().expect("prepare store");
        store
    }

    #[test]
    fn round_trip_and_resume_keep_car_id_and_codes() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("rides.json");
        let mut store = RideHistoryStore::new(&path);
        let recovered = store.prepare().expect("prepare");
        assert!(!recovered);
        let record = store
            .record_host_started(&car("car-1", "HOST-CODE", 10_000), 200)
            .expect("record host");
        drop(store);

        let mut reopened = RideHistoryStore::new(&path);
        let recovered = reopened.prepare().expect("reopen");
        assert!(!recovered);
        let resumed = reopened
            .record_host_resumed(&record.record_id, 300)
            .expect("resume")
            .expect("resumable record");
        assert_eq!(resumed.car_id, "car-1");
        assert_eq!(resumed.seats[0].code, "HOST-CODE");
        assert_eq!(resumed.last_active_at, 300);
    }

    #[test]
    fn raw_file_contains_no_sensitive_field_names() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("rides.json");
        let mut store = prepared_store(&path);
        store
            .record_host_started(&car("car-1", "SAFE-CODE", 10_000), 200)
            .expect("record host");
        let raw = fs::read_to_string(path).expect("read store");
        for forbidden in [
            "access_id",
            "accessId",
            "session_secret",
            "sessionSecret",
            "host_binding",
            "hostBinding",
        ] {
            assert!(!raw.contains(forbidden), "found {forbidden}");
        }
    }

    #[test]
    fn corrupt_file_is_quarantined_and_reported() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("rides.json");
        fs::write(&path, b"not json").expect("write damage");
        let mut store = RideHistoryStore::new(&path);
        let recovered = store.prepare().expect("recover");
        assert!(recovered);
        assert!(store.list().is_empty());
        assert!(path.exists());
        assert!(fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")));
    }

    #[test]
    fn stop_and_new_host_archive_prior_records() {
        let dir = TempDir::new().expect("temp dir");
        let mut store = prepared_store(dir.path().join("rides.json"));
        let first = store
            .record_host_started(&car("car-1", "ONE", 10_000), 200)
            .expect("first");
        assert!(store.record_host_stopped("car-1", 300).expect("stop"));
        let stopped = store.get(&first.record_id).expect("stopped record");
        assert!(stopped.stopped);
        assert_eq!(stopped.ended_at, Some(300));

        let second = store
            .record_host_started(&car("car-2", "TWO", 10_000), 400)
            .expect("second");
        store
            .record_host_started(&car("car-3", "THREE", 10_000), 500)
            .expect("third");
        let archived = store.get(&second.record_id).expect("archived second");
        assert!(archived.stopped);
        assert_eq!(archived.ended_at, Some(500));
    }

    #[test]
    fn passenger_join_upserts_by_car_and_seat() {
        let dir = TempDir::new().expect("temp dir");
        let mut store = prepared_store(dir.path().join("rides.json"));
        let first = store
            .record_passenger_joined(&preview("car-1", 2), "owner-a", "CODE-A", "Alice", 200)
            .expect("first join");
        let second = store
            .record_passenger_joined(&preview("car-1", 2), "owner-b", "CODE-B", "Bob", 300)
            .expect("second join");
        assert_eq!(first.record_id, second.record_id);
        assert_eq!(store.list().len(), 1);
        assert_eq!(second.code.as_deref(), Some("CODE-B"));
        assert_eq!(second.nickname.as_deref(), Some("Bob"));
        assert_eq!(second.owner_peer_id, "owner-b");
    }

    #[test]
    fn retains_only_the_latest_hundred_records() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("rides.json");
        let mut store = prepared_store(&path);
        for index in 0..=MAX_RECORDS {
            store
                .record_host_started(
                    &car(&format!("car-{index}"), &format!("CODE-{index}"), 10_000),
                    index as i64,
                )
                .expect("record host");
        }
        assert_eq!(store.list().len(), MAX_RECORDS);
        assert!(store.list().iter().all(|record| record.car_id != "car-0"));

        let mut reopened = RideHistoryStore::new(path);
        reopened.prepare().expect("reopen");
        assert_eq!(reopened.list().len(), MAX_RECORDS);
    }

    #[cfg(unix)]
    #[test]
    fn store_has_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("rides.json");
        prepared_store(&path);
        let mode = fs::metadata(path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
