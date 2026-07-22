use crate::relay::HostCredential;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const MAX_ROUTE_ATTEMPTS: usize = 3;
const ROUTE_STATE_VERSION: u8 = 1;
const MAX_ROUTE_STATE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct RouteCandidate {
    pub id: String,
    pub priority: u32,
    pub credential: HostCredential,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RouteFailure {
    Network,
    Authentication,
    RateLimited,
    Upstream,
    Expired,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RouteHealthStatus {
    Normal,
    Cooling,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RouteHealthSummary {
    pub status: RouteHealthStatus,
    pub reason: Option<RouteFailure>,
    pub cooldown_until_ms: Option<i64>,
    pub consecutive_failures: u32,
    pub last_attempt_at_ms: Option<i64>,
    pub last_success_at_ms: Option<i64>,
    pub last_failure_at_ms: Option<i64>,
}

impl Default for RouteHealthSummary {
    fn default() -> Self {
        Self {
            status: RouteHealthStatus::Normal,
            reason: None,
            cooldown_until_ms: None,
            consecutive_failures: 0,
            last_attempt_at_ms: None,
            last_success_at_ms: None,
            last_failure_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialHealth {
    consecutive_failures: u32,
    cooldown_until_ms: i64,
    last_failure: Option<RouteFailure>,
    last_attempt_at_ms: Option<i64>,
    last_success_at_ms: Option<i64>,
    last_failure_at_ms: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedRouterState {
    version: u8,
    #[serde(default)]
    health: HashMap<String, CredentialHealth>,
    #[serde(default)]
    last_used_sequence: HashMap<String, u64>,
    #[serde(default)]
    next_sequence: u64,
}

#[derive(Debug, Default)]
pub struct AccountRouterState {
    health: HashMap<String, CredentialHealth>,
    last_used_sequence: HashMap<String, u64>,
    next_sequence: u64,
    path: Option<PathBuf>,
    dirty: bool,
}

impl AccountRouterState {
    /// Configures the independent, credential-free route state file. A damaged
    /// file is quarantined and reset rather than blocking access to the account
    /// pool. Returns true when recovery was required.
    pub fn configure(&mut self, path: PathBuf) -> Result<bool, String> {
        self.path = Some(path.clone());
        if !path.exists() {
            return Ok(false);
        }
        let bytes = fs::read(&path)
            .map_err(|error| format!("无法读取账号路由状态 {}: {error}", path.display()))?;
        let parsed = if bytes.len() as u64 > MAX_ROUTE_STATE_BYTES {
            Err("账号路由状态文件过大".to_string())
        } else {
            serde_json::from_slice::<PersistedRouterState>(&bytes)
                .map_err(|error| format!("账号路由状态已损坏: {error}"))
                .and_then(|state| {
                    (state.version == ROUTE_STATE_VERSION)
                        .then_some(state)
                        .ok_or_else(|| "账号路由状态版本不受支持".to_string())
                })
        };
        match parsed {
            Ok(state) => {
                self.health = state.health;
                self.last_used_sequence = state.last_used_sequence;
                self.next_sequence = state.next_sequence;
                self.dirty = false;
                Ok(false)
            }
            Err(_) => {
                let quarantine = path.with_extension(format!("corrupt-{}", now_ms()));
                fs::rename(&path, &quarantine).map_err(|error| {
                    format!("无法隔离损坏的账号路由状态 {}: {error}", path.display())
                })?;
                self.health.clear();
                self.last_used_sequence.clear();
                self.next_sequence = 0;
                self.dirty = true;
                Ok(true)
            }
        }
    }

    pub fn order_candidates(
        &mut self,
        mut candidates: Vec<RouteCandidate>,
        now_ms: i64,
    ) -> Vec<RouteCandidate> {
        candidates.retain(|candidate| {
            self.health
                .get(&candidate.id)
                .map(|health| health.cooldown_until_ms <= now_ms)
                .unwrap_or(true)
        });
        candidates.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| {
                    let left_used = self.last_used_sequence.get(&left.id).copied().unwrap_or(0);
                    let right_used = self.last_used_sequence.get(&right.id).copied().unwrap_or(0);
                    left_used.cmp(&right_used)
                })
                .then_with(|| left.id.cmp(&right.id))
        });
        candidates.truncate(MAX_ROUTE_ATTEMPTS);
        candidates
    }

    pub fn order_and_reserve_candidates(
        &mut self,
        candidates: Vec<RouteCandidate>,
        now_ms: i64,
    ) -> Vec<RouteCandidate> {
        let ordered = self.order_candidates(candidates, now_ms);
        if let Some(candidate) = ordered.first() {
            self.mark_attempt_at(&candidate.id, now_ms);
        }
        ordered
    }

    pub fn mark_attempt(&mut self, id: &str) {
        self.mark_attempt_at(id, now_ms());
    }

    fn mark_attempt_at(&mut self, id: &str, attempted_at_ms: i64) {
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.last_used_sequence
            .insert(id.to_string(), self.next_sequence);
        self.health
            .entry(id.to_string())
            .or_default()
            .last_attempt_at_ms = Some(attempted_at_ms);
        self.dirty = true;
    }

    pub fn mark_success(&mut self, id: &str) {
        let health = self.health.entry(id.to_string()).or_default();
        health.consecutive_failures = 0;
        health.cooldown_until_ms = 0;
        health.last_failure = None;
        health.last_success_at_ms = Some(now_ms());
        self.dirty = true;
    }

    pub fn mark_failure(&mut self, id: &str, failure: RouteFailure, now_ms: i64) {
        let health = self.health.entry(id.to_string()).or_default();
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        let exponent = health.consecutive_failures.saturating_sub(1).min(5);
        let base_ms: i64 = match failure {
            RouteFailure::Network => 15_000,
            RouteFailure::Authentication | RouteFailure::Expired => 5 * 60_000,
            RouteFailure::RateLimited => 60_000,
            RouteFailure::Upstream => 20_000,
        };
        let max_ms: i64 = match failure {
            RouteFailure::Authentication | RouteFailure::Expired => 30 * 60_000,
            _ => 10 * 60_000,
        };
        let cooldown_ms = base_ms.saturating_mul(1_i64 << exponent).min(max_ms);
        health.cooldown_until_ms = now_ms.saturating_add(cooldown_ms);
        health.last_failure = Some(failure);
        health.last_failure_at_ms = Some(now_ms);
        self.dirty = true;
    }

    pub fn retry_now(&mut self, id: &str) {
        let health = self.health.entry(id.to_string()).or_default();
        health.consecutive_failures = 0;
        health.cooldown_until_ms = 0;
        health.last_failure = None;
        self.dirty = true;
    }

    pub fn remove(&mut self, id: &str) {
        self.health.remove(id);
        self.last_used_sequence.remove(id);
        self.dirty = true;
    }

    pub fn summary(&self, id: &str, now_ms: i64) -> RouteHealthSummary {
        let Some(health) = self.health.get(id) else {
            return RouteHealthSummary::default();
        };
        let cooling = health.cooldown_until_ms > now_ms;
        RouteHealthSummary {
            status: if cooling {
                RouteHealthStatus::Cooling
            } else {
                RouteHealthStatus::Normal
            },
            reason: health.last_failure,
            cooldown_until_ms: (health.cooldown_until_ms > 0).then_some(health.cooldown_until_ms),
            consecutive_failures: health.consecutive_failures,
            last_attempt_at_ms: health.last_attempt_at_ms,
            last_success_at_ms: health.last_success_at_ms,
            last_failure_at_ms: health.last_failure_at_ms,
        }
    }

    pub fn summaries(&self, now_ms: i64) -> HashMap<String, RouteHealthSummary> {
        self.health
            .keys()
            .map(|id| (id.clone(), self.summary(id, now_ms)))
            .collect()
    }

    pub fn flush(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let state = PersistedRouterState {
            version: ROUTE_STATE_VERSION,
            health: self.health.clone(),
            last_used_sequence: self.last_used_sequence.clone(),
            next_sequence: self.next_sequence,
        };
        let bytes = serde_json::to_vec_pretty(&state)
            .map_err(|error| format!("无法编码账号路由状态: {error}"))?;
        atomic_write_private(path, &bytes)?;
        self.dirty = false;
        Ok(())
    }

    #[cfg(test)]
    fn cooldown_until(&self, id: &str) -> Option<i64> {
        self.health.get(id).map(|health| health.cooldown_until_ms)
    }
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "账号路由状态路径无效".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建账号路由状态目录: {error}"))?;
    let temp = parent.join(format!(".route-state-{}.tmp", Uuid::new_v4()));
    fs::write(&temp, bytes).map_err(|error| format!("无法写入账号路由状态: {error}"))?;
    secure_file(&temp)?;
    #[cfg(target_os = "windows")]
    if path.exists() {
        fs::remove_file(path).map_err(|error| format!("无法替换账号路由状态: {error}"))?;
    }
    fs::rename(&temp, path).map_err(|error| format!("无法提交账号路由状态: {error}"))?;
    secure_file(path)
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("无法保护账号路由状态: {error}"))
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub fn retryable_status(status: u16) -> Option<RouteFailure> {
    match status {
        401 | 403 => Some(RouteFailure::Authentication),
        408 => Some(RouteFailure::Network),
        429 => Some(RouteFailure::RateLimited),
        500..=599 => Some(RouteFailure::Upstream),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{HostCredential, HostCredentialKind};
    use tempfile::TempDir;

    fn candidate(id: &str, priority: u32) -> RouteCandidate {
        RouteCandidate {
            id: id.to_string(),
            priority,
            credential: HostCredential {
                secret: format!("secret-{id}"),
                account_id: None,
                kind: HostCredentialKind::ApiKey,
                source: "test".to_string(),
            },
        }
    }

    #[test]
    fn lower_priority_number_wins_and_equal_priority_rotates_lru() {
        let mut state = AccountRouterState::default();
        let ordered = state.order_and_reserve_candidates(
            vec![
                candidate("later", 20),
                candidate("a", 10),
                candidate("b", 10),
            ],
            1_000,
        );
        assert_eq!(
            ordered
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "later"]
        );
        let ordered =
            state.order_and_reserve_candidates(vec![candidate("a", 10), candidate("b", 10)], 1_001);
        assert_eq!(ordered[0].id, "b");
    }

    #[test]
    fn reserving_a_primary_keeps_back_to_back_selections_balanced() {
        let mut state = AccountRouterState::default();
        let first =
            state.order_and_reserve_candidates(vec![candidate("a", 10), candidate("b", 10)], 1_000);
        let second =
            state.order_and_reserve_candidates(vec![candidate("a", 10), candidate("b", 10)], 1_000);

        assert_eq!(first[0].id, "a");
        assert_eq!(second[0].id, "b");
    }

    #[test]
    fn failures_cool_down_and_manual_retry_restores_a_credential() {
        let mut state = AccountRouterState::default();
        state.mark_failure("a", RouteFailure::RateLimited, 5_000);
        assert_eq!(state.cooldown_until("a"), Some(65_000));
        assert!(state
            .order_candidates(vec![candidate("a", 1)], 64_999)
            .is_empty());
        assert_eq!(
            state.order_candidates(vec![candidate("a", 1)], 65_000)[0].id,
            "a"
        );
        state.retry_now("a");
        assert_eq!(state.summary("a", 5_001).status, RouteHealthStatus::Normal);
    }

    #[test]
    fn health_lru_and_failure_details_survive_restart_without_secrets() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("route-health.json");
        let mut state = AccountRouterState::default();
        assert!(!state.configure(path.clone()).expect("configure"));
        state.order_and_reserve_candidates(vec![candidate("managed:a", 1)], 10_000);
        state.mark_failure("managed:a", RouteFailure::Authentication, 10_100);
        state.flush().expect("flush");

        let raw = fs::read_to_string(&path).expect("read state");
        assert!(!raw.contains("secret-a"));
        let mut restored = AccountRouterState::default();
        assert!(!restored.configure(path).expect("restore"));
        let summary = restored.summary("managed:a", 10_200);
        assert_eq!(summary.status, RouteHealthStatus::Cooling);
        assert_eq!(summary.reason, Some(RouteFailure::Authentication));
        assert_eq!(summary.last_attempt_at_ms, Some(10_000));
    }

    #[test]
    fn exponential_cooldown_is_capped_and_persisted_reason_is_enumerated() {
        let mut state = AccountRouterState::default();
        for index in 0..10 {
            state.mark_failure("a", RouteFailure::Network, index * 1_000);
        }
        let summary = state.summary("a", 10_000);
        assert_eq!(summary.reason, Some(RouteFailure::Network));
        assert!(summary.cooldown_until_ms.unwrap() <= 9_000 + 10 * 60_000);
    }

    #[test]
    fn only_transient_or_credential_statuses_are_retried() {
        for status in [401, 403, 408, 429, 500, 502, 503, 599] {
            assert!(retryable_status(status).is_some(), "status {status}");
        }
        for status in [200, 400, 404, 409, 422] {
            assert!(retryable_status(status).is_none(), "status {status}");
        }
    }
}
