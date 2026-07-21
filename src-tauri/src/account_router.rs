use crate::relay::HostCredential;
use std::collections::HashMap;

pub const MAX_ROUTE_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct RouteCandidate {
    pub id: String,
    pub priority: u32,
    pub credential: HostCredential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteFailure {
    Network,
    Authentication,
    RateLimited,
    Upstream,
}

#[derive(Debug, Clone, Default)]
struct CredentialHealth {
    consecutive_failures: u32,
    cooldown_until_ms: i64,
}

#[derive(Debug, Default)]
pub struct AccountRouterState {
    health: HashMap<String, CredentialHealth>,
    last_used_sequence: HashMap<String, u64>,
    next_sequence: u64,
}

impl AccountRouterState {
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
            let id = candidate.id.clone();
            self.mark_attempt(&id);
        }
        ordered
    }

    pub fn mark_attempt(&mut self, id: &str) {
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.last_used_sequence
            .insert(id.to_string(), self.next_sequence);
    }

    pub fn mark_success(&mut self, id: &str) {
        self.health.remove(id);
    }

    pub fn mark_failure(&mut self, id: &str, failure: RouteFailure, now_ms: i64) {
        let health = self.health.entry(id.to_string()).or_default();
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        let exponent = health.consecutive_failures.saturating_sub(1).min(5);
        let base_ms: i64 = match failure {
            RouteFailure::Network => 15_000,
            RouteFailure::Authentication => 5 * 60_000,
            RouteFailure::RateLimited => 60_000,
            RouteFailure::Upstream => 20_000,
        };
        let max_ms: i64 = match failure {
            RouteFailure::Authentication => 30 * 60_000,
            _ => 10 * 60_000,
        };
        let cooldown_ms = base_ms.saturating_mul(1_i64 << exponent).min(max_ms);
        health.cooldown_until_ms = now_ms.saturating_add(cooldown_ms);
    }

    pub fn remove(&mut self, id: &str) {
        self.health.remove(id);
        self.last_used_sequence.remove(id);
    }

    #[cfg(test)]
    fn cooldown_until(&self, id: &str) -> Option<i64> {
        self.health.get(id).map(|health| health.cooldown_until_ms)
    }
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
    fn failures_cool_down_and_success_restores_a_credential() {
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
        state.mark_success("a");
        assert_eq!(state.cooldown_until("a"), None);
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
