use crate::models::{CarSession, ModelUsageSummary, SeatState, ToolKind};
use crate::pricing::{estimate, BillableTokens};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageDelta {
    pub tool: ToolKind,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub occurred_at: i64,
}

fn number(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or_default()
}

fn contains_one_hour_cache(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            (key == "ttl" && value.as_str() == Some("1h")) || contains_one_hour_cache(value)
        }),
        Value::Array(items) => items.iter().any(contains_one_hour_cache),
        _ => false,
    }
}

fn usage_objects(response_body: &[u8]) -> Vec<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(response_body) {
        return vec![value];
    }
    String::from_utf8_lossy(response_body)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .map(str::trim)
        .filter(|payload| !payload.is_empty() && *payload != "[DONE]")
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .collect()
}

fn usage_candidates(value: &Value) -> Vec<&Value> {
    [
        value.get("usage"),
        value.pointer("/message/usage"),
        value.pointer("/response/usage"),
        value.pointer("/data/usage"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn extract_usage(
    tool: ToolKind,
    request_body: &[u8],
    response_body: &[u8],
    occurred_at: i64,
) -> Result<UsageDelta, String> {
    let request: Value = serde_json::from_slice(request_body)
        .map_err(|error| format!("无法解析模型请求以统计用量: {error}"))?;
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "模型请求缺少 model，无法按模型统计".to_string())?
        .to_string();
    let one_hour_cache = contains_one_hour_cache(&request);

    let mut raw_input = 0_u64;
    let mut output = 0_u64;
    let mut cache_read = 0_u64;
    let mut cache_write_5m = 0_u64;
    let mut cache_write_1h = 0_u64;
    for event in usage_objects(response_body) {
        for usage in usage_candidates(&event) {
            raw_input = raw_input.max(number(
                usage
                    .get("input_tokens")
                    .or_else(|| usage.get("prompt_tokens")),
            ));
            output = output.max(number(
                usage
                    .get("output_tokens")
                    .or_else(|| usage.get("completion_tokens")),
            ));
            let details = usage
                .get("input_tokens_details")
                .or_else(|| usage.get("prompt_tokens_details"));
            cache_read = cache_read.max(
                number(usage.get("cache_read_input_tokens"))
                    .max(number(details.and_then(|value| value.get("cached_tokens")))),
            );

            if let Some(cache_creation) = usage.get("cache_creation") {
                cache_write_5m =
                    cache_write_5m.max(number(cache_creation.get("ephemeral_5m_input_tokens")));
                cache_write_1h =
                    cache_write_1h.max(number(cache_creation.get("ephemeral_1h_input_tokens")));
            }
            let aggregate_cache_write = number(
                usage
                    .get("cache_creation_input_tokens")
                    .or_else(|| details.and_then(|value| value.get("cache_write_tokens"))),
            );
            if cache_write_5m == 0 && cache_write_1h == 0 {
                if one_hour_cache {
                    cache_write_1h = cache_write_1h.max(aggregate_cache_write);
                } else {
                    cache_write_5m = cache_write_5m.max(aggregate_cache_write);
                }
            }
        }
    }

    if raw_input == 0
        && output == 0
        && cache_read == 0
        && cache_write_5m == 0
        && cache_write_1h == 0
    {
        return Err("模型响应没有可识别的 usage 字段".to_string());
    }
    let input = if matches!(tool, ToolKind::Codex) {
        raw_input
            .saturating_sub(cache_read)
            .saturating_sub(cache_write_5m)
            .saturating_sub(cache_write_1h)
    } else {
        raw_input
    };
    Ok(UsageDelta {
        tool,
        model,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_5m_tokens: cache_write_5m,
        cache_write_1h_tokens: cache_write_1h,
        occurred_at,
    })
}

pub fn apply_usage(car: &mut CarSession, code: &str, delta: UsageDelta) -> Result<(), String> {
    let delta_price = estimate(
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
    let seat = car
        .seats
        .iter_mut()
        .find(|seat| seat.code == code)
        .ok_or_else(|| "用量记录对应的座位不存在".to_string())?;
    seat.tool = Some(delta.tool);
    seat.state = SeatState::Using;

    let model = if let Some(existing) = seat
        .usage
        .models
        .iter_mut()
        .find(|item| item.tool == delta.tool && item.model == delta.model)
    {
        existing
    } else {
        seat.usage.models.push(ModelUsageSummary {
            tool: delta.tool,
            model: delta.model.clone(),
            request_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_5m_tokens: 0,
            cache_write_1h_tokens: 0,
            official_cost_microusd: None,
            unpriced_request_count: 0,
            pricing_source: None,
            last_used_at: delta.occurred_at,
        });
        seat.usage.models.last_mut().expect("inserted model usage")
    };
    model.request_count = model.request_count.saturating_add(1);
    model.input_tokens = model.input_tokens.saturating_add(delta.input_tokens);
    model.output_tokens = model.output_tokens.saturating_add(delta.output_tokens);
    model.cache_read_tokens = model
        .cache_read_tokens
        .saturating_add(delta.cache_read_tokens);
    model.cache_write_5m_tokens = model
        .cache_write_5m_tokens
        .saturating_add(delta.cache_write_5m_tokens);
    model.cache_write_1h_tokens = model
        .cache_write_1h_tokens
        .saturating_add(delta.cache_write_1h_tokens);
    model.cache_write_tokens = model
        .cache_write_5m_tokens
        .saturating_add(model.cache_write_1h_tokens);
    model.last_used_at = delta.occurred_at;
    if let Some(price) = delta_price {
        model.official_cost_microusd = Some(
            model
                .official_cost_microusd
                .unwrap_or_default()
                .saturating_add(price.cost_microusd),
        );
        model.pricing_source = Some(price.source.to_string());
    } else {
        model.unpriced_request_count = model.unpriced_request_count.saturating_add(1);
    }

    seat.usage.request_count = seat
        .usage
        .models
        .iter()
        .map(|item| item.request_count)
        .sum();
    seat.usage.input_tokens = seat.usage.models.iter().map(|item| item.input_tokens).sum();
    seat.usage.output_tokens = seat
        .usage
        .models
        .iter()
        .map(|item| item.output_tokens)
        .sum();
    seat.usage.cache_read_tokens = seat
        .usage
        .models
        .iter()
        .map(|item| item.cache_read_tokens)
        .sum();
    seat.usage.cache_write_5m_tokens = seat
        .usage
        .models
        .iter()
        .map(|item| item.cache_write_5m_tokens)
        .sum();
    seat.usage.cache_write_1h_tokens = seat
        .usage
        .models
        .iter()
        .map(|item| item.cache_write_1h_tokens)
        .sum();
    seat.usage.cache_write_tokens = seat
        .usage
        .cache_write_5m_tokens
        .saturating_add(seat.usage.cache_write_1h_tokens);
    seat.usage.total_tokens = seat
        .usage
        .input_tokens
        .saturating_add(seat.usage.output_tokens)
        .saturating_add(seat.usage.cache_read_tokens)
        .saturating_add(seat.usage.cache_write_tokens);
    seat.usage.official_cost_microusd = seat
        .usage
        .models
        .iter()
        .filter_map(|item| item.official_cost_microusd)
        .sum();
    seat.usage.unpriced_request_count = seat
        .usage
        .models
        .iter()
        .map(|item| item.unpriced_request_count)
        .sum();
    seat.usage.last_used_at = Some(delta.occurred_at);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Seat, SeatUsageSummary};

    #[test]
    fn parses_claude_input_output_and_both_cache_categories() {
        let request = br#"{"model":"claude-sonnet-4-6","cache_control":{"ttl":"1h"}}"#;
        let response = br#"{"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":300,"cache_creation_input_tokens":400}}"#;
        let usage =
            extract_usage(ToolKind::Claude, request, response, 1_700_000_000_000).expect("usage");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 300);
        assert_eq!(usage.cache_write_1h_tokens, 400);
        assert_eq!(usage.cache_write_5m_tokens, 0);
    }

    #[test]
    fn parses_claude_stream_usage_with_mixed_cache_write_ttls() {
        let request = br#"{"model":"claude-sonnet-4-6","stream":true}"#;
        let response = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":250,"ephemeral_1h_input_tokens":150}}}}

event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":20}}
"#;
        let usage =
            extract_usage(ToolKind::Claude, request, response, 1_700_000_000_000).expect("usage");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 300);
        assert_eq!(usage.cache_write_5m_tokens, 250);
        assert_eq!(usage.cache_write_1h_tokens, 150);
    }

    #[test]
    fn parses_openai_sse_and_subtracts_cached_input_from_uncached_input() {
        let request = br#"{"model":"gpt-5.6-terra"}"#;
        let response = br#"event: response.completed
data: {"type":"response.completed","response":{"usage":{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":700}}}}

data: [DONE]
"#;
        let usage =
            extract_usage(ToolKind::Codex, request, response, 1_700_000_000_000).expect("usage");
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.cache_read_tokens, 700);
        assert_eq!(usage.output_tokens, 200);
    }

    #[test]
    fn parses_openai_cache_write_as_a_separate_model_component() {
        let request = br#"{"model":"gpt-5.6-sol"}"#;
        let response = br#"{"usage":{"input_tokens":1200,"output_tokens":200,"input_tokens_details":{"cached_tokens":700,"cache_write_tokens":300}}}"#;
        let usage =
            extract_usage(ToolKind::Codex, request, response, 1_700_000_000_000).expect("usage");
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.output_tokens, 200);
        assert_eq!(usage.cache_read_tokens, 700);
        assert_eq!(usage.cache_write_5m_tokens, 300);
        assert_eq!(usage.cache_write_1h_tokens, 0);
    }

    #[test]
    fn keeps_models_separate_and_recalculates_official_list_price() {
        let mut car = CarSession {
            car_id: "car".to_string(),
            car_name: "friends".to_string(),
            owner_peer_id: "owner".to_string(),
            started_at: 1,
            expires_at: i64::MAX,
            enabled_tools: vec![ToolKind::Claude, ToolKind::Codex],
            seats: vec![Seat {
                seat_no: 1,
                code: "7G2K5LQ8M4TZ".to_string(),
                nickname: Some("朋友".to_string()),
                state: SeatState::Connected,
                tool: None,
                usage: SeatUsageSummary::default(),
            }],
        };
        apply_usage(
            &mut car,
            "7G2K5LQ8M4TZ",
            UsageDelta {
                tool: ToolKind::Codex,
                model: "gpt-5.6-luna".to_string(),
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_read_tokens: 0,
                cache_write_5m_tokens: 0,
                cache_write_1h_tokens: 0,
                occurred_at: 1_783_987_200_000,
            },
        )
        .expect("codex usage");
        apply_usage(
            &mut car,
            "7G2K5LQ8M4TZ",
            UsageDelta {
                tool: ToolKind::Claude,
                model: "claude-sonnet-4-6".to_string(),
                input_tokens: 1_000_000,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_5m_tokens: 0,
                cache_write_1h_tokens: 0,
                occurred_at: 1_783_987_200_000,
            },
        )
        .expect("claude usage");
        let seat = &car.seats[0];
        assert_eq!(seat.usage.models.len(), 2);
        assert_eq!(seat.usage.request_count, 2);
        assert_eq!(seat.usage.official_cost_microusd, 14_000_000);
    }

    #[test]
    fn preserves_request_time_pricing_across_catalog_cutovers() {
        let mut car = CarSession {
            car_id: "car".to_string(),
            car_name: "friends".to_string(),
            owner_peer_id: "owner".to_string(),
            started_at: 1,
            expires_at: i64::MAX,
            enabled_tools: vec![ToolKind::Claude],
            seats: vec![Seat {
                seat_no: 1,
                code: "7G2K5LQ8M4TZ".to_string(),
                nickname: Some("朋友".to_string()),
                state: SeatState::Connected,
                tool: None,
                usage: SeatUsageSummary::default(),
            }],
        };
        for occurred_at in [1_788_220_799_999, 1_788_220_800_000] {
            apply_usage(
                &mut car,
                "7G2K5LQ8M4TZ",
                UsageDelta {
                    tool: ToolKind::Claude,
                    model: "claude-sonnet-5".to_string(),
                    input_tokens: 1_000_000,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_5m_tokens: 0,
                    cache_write_1h_tokens: 0,
                    occurred_at,
                },
            )
            .expect("usage");
        }
        assert_eq!(car.seats[0].usage.official_cost_microusd, 5_000_000);
    }
}
