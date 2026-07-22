use crate::models::{
    CarSession, MemberTokenLimitStatus, MemberTokenLimits, Seat, TokenUsageEvent, TokenWindowStatus,
};

const FIVE_HOURS_MS: i64 = 5 * 60 * 60 * 1_000;
const ONE_DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const ONE_WEEK_MS: i64 = 7 * ONE_DAY_MS;
const MAX_MEMBER_TOKEN_LIMIT: u64 = 1_000_000_000_000;

fn window_status(
    events: &[TokenUsageEvent],
    now_ms: i64,
    duration_ms: i64,
    limit_tokens: Option<u64>,
) -> TokenWindowStatus {
    let start = now_ms.saturating_sub(duration_ms);
    let mut used_tokens = 0_u64;
    let mut oldest: Option<i64> = None;
    for event in events
        .iter()
        .filter(|event| event.occurred_at > start && event.occurred_at <= now_ms)
    {
        used_tokens = used_tokens.saturating_add(event.tokens);
        oldest = Some(oldest.map_or(event.occurred_at, |value| value.min(event.occurred_at)));
    }
    let remaining_tokens = limit_tokens.map(|limit| limit.saturating_sub(used_tokens));
    TokenWindowStatus {
        limit_tokens,
        used_tokens,
        remaining_tokens,
        resets_at: oldest.map(|timestamp| timestamp.saturating_add(duration_ms)),
        exhausted: limit_tokens.is_some_and(|limit| used_tokens >= limit),
    }
}

pub fn validate_limits(limits: &MemberTokenLimits) -> Result<(), String> {
    for (label, value) in [
        ("5 小时", limits.five_hour_tokens),
        ("日", limits.daily_tokens),
        ("7 天", limits.weekly_tokens),
    ] {
        if value.is_some_and(|limit| limit == 0 || limit > MAX_MEMBER_TOKEN_LIMIT) {
            return Err(format!(
                "{label}限额应在 1 到 {MAX_MEMBER_TOKEN_LIMIT} Token 之间，留空表示不限额"
            ));
        }
    }
    Ok(())
}

pub fn refresh_seat(seat: &mut Seat, now_ms: i64) {
    seat.token_usage_events
        .retain(|event| event.occurred_at > now_ms.saturating_sub(ONE_WEEK_MS));
    seat.token_limit_status = MemberTokenLimitStatus {
        five_hour: window_status(
            &seat.token_usage_events,
            now_ms,
            FIVE_HOURS_MS,
            seat.token_limits.five_hour_tokens,
        ),
        daily: window_status(
            &seat.token_usage_events,
            now_ms,
            ONE_DAY_MS,
            seat.token_limits.daily_tokens,
        ),
        weekly: window_status(
            &seat.token_usage_events,
            now_ms,
            ONE_WEEK_MS,
            seat.token_limits.weekly_tokens,
        ),
    };
}

pub fn refresh_car(car: &mut CarSession, now_ms: i64) {
    for seat in &mut car.seats {
        refresh_seat(seat, now_ms);
    }
}

pub fn record_tokens(seat: &mut Seat, occurred_at: i64, tokens: u64) {
    seat.token_usage_events.push(TokenUsageEvent {
        occurred_at,
        tokens,
    });
    refresh_seat(seat, occurred_at);
}

pub fn ensure_available(car: &mut CarSession, code: &str, now_ms: i64) -> Result<(), String> {
    let seat = car
        .seats
        .iter_mut()
        .find(|seat| seat.code == code)
        .ok_or_else(|| "用量限额对应的座位不存在".to_string())?;
    refresh_seat(seat, now_ms);
    for (label, status) in [
        ("5 小时", &seat.token_limit_status.five_hour),
        ("日", &seat.token_limit_status.daily),
        ("7 天", &seat.token_limit_status.weekly),
    ] {
        if status.exhausted {
            return Err(format!(
                "成员 Token {label}限额已用完，车主可调整限额或等待窗口重置"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountQuotaSnapshot, CarSession, SeatState, SeatUsageSummary, ToolKind};

    fn seat() -> Seat {
        Seat {
            seat_no: 1,
            code: "ABCDEFGHJKLM".to_string(),
            nickname: Some("阿杰".to_string()),
            state: SeatState::Connected,
            tool: Some(ToolKind::Claude),
            usage: SeatUsageSummary::default(),
            token_limits: MemberTokenLimits {
                five_hour_tokens: Some(1_000),
                daily_tokens: Some(2_000),
                weekly_tokens: None,
            },
            token_limit_status: MemberTokenLimitStatus::default(),
            token_usage_events: Vec::new(),
        }
    }

    fn car(seat: Seat) -> CarSession {
        CarSession {
            car_id: "car-1".to_string(),
            car_name: "熟人车".to_string(),
            owner_peer_id: "owner".to_string(),
            started_at: 0,
            expires_at: i64::MAX,
            always_on: true,
            enabled_tools: vec![ToolKind::Claude],
            seats: vec![seat],
            account_quotas: Vec::<AccountQuotaSnapshot>::new(),
        }
    }

    #[test]
    fn rolling_windows_expire_independently() {
        let now = 10 * ONE_DAY_MS;
        let mut seat = seat();
        record_tokens(&mut seat, now - FIVE_HOURS_MS + 1_000, 700);
        record_tokens(&mut seat, now - FIVE_HOURS_MS - 1_000, 500);
        refresh_seat(&mut seat, now);

        assert_eq!(seat.token_limit_status.five_hour.used_tokens, 700);
        assert_eq!(
            seat.token_limit_status.five_hour.remaining_tokens,
            Some(300)
        );
        assert_eq!(seat.token_limit_status.daily.used_tokens, 1_200);
        assert_eq!(seat.token_limit_status.daily.remaining_tokens, Some(800));
        assert!(!seat.token_limit_status.weekly.exhausted);
    }

    #[test]
    fn exhausted_limit_blocks_the_next_request_until_the_window_resets() {
        let now = 10 * ONE_DAY_MS;
        let mut car = car(seat());
        record_tokens(&mut car.seats[0], now - 1_000, 1_000);
        let error = ensure_available(&mut car, "ABCDEFGHJKLM", now).expect_err("blocked");
        assert!(error.contains("5 小时"));

        ensure_available(&mut car, "ABCDEFGHJKLM", now + FIVE_HOURS_MS + 1).expect("window reset");
    }

    #[test]
    fn zero_is_rejected_and_none_means_unlimited() {
        let invalid = MemberTokenLimits {
            five_hour_tokens: Some(0),
            daily_tokens: None,
            weekly_tokens: None,
        };
        assert!(validate_limits(&invalid).is_err());
        assert!(validate_limits(&MemberTokenLimits::default()).is_ok());
    }
}
