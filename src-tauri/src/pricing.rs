use crate::models::ToolKind;

pub const OPENAI_PRICING_SOURCE: &str = "https://developers.openai.com/api/docs/pricing";
pub const ANTHROPIC_PRICING_SOURCE: &str =
    "https://platform.claude.com/docs/en/about-claude/pricing";

const OPENAI_LONG_CONTEXT_THRESHOLD: u64 = 272_000;
const SEPTEMBER_2026_UTC_MS: i64 = 1_788_220_800_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BillableTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
}

#[derive(Debug, Clone, Copy)]
struct RateCard {
    input_microusd_per_million: u64,
    output_microusd_per_million: u64,
    cache_read_microusd_per_million: Option<u64>,
    cache_write_5m_microusd_per_million: Option<u64>,
    cache_write_1h_microusd_per_million: Option<u64>,
    long_context: Option<TokenRates>,
    source: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct TokenRates {
    input_microusd_per_million: u64,
    output_microusd_per_million: u64,
    cache_read_microusd_per_million: Option<u64>,
    cache_write_5m_microusd_per_million: Option<u64>,
    cache_write_1h_microusd_per_million: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialPriceEstimate {
    pub cost_microusd: u64,
    pub source: &'static str,
}

fn usd(value: f64) -> u64 {
    (value * 1_000_000.0).round() as u64
}

fn openai(
    input: f64,
    output: f64,
    cached_input: Option<f64>,
    cache_write: Option<f64>,
    long_context: Option<(f64, Option<f64>, f64, Option<f64>)>,
) -> RateCard {
    RateCard {
        input_microusd_per_million: usd(input),
        output_microusd_per_million: usd(output),
        cache_read_microusd_per_million: cached_input.map(usd),
        cache_write_5m_microusd_per_million: cache_write.map(usd),
        cache_write_1h_microusd_per_million: cache_write.map(usd),
        long_context: long_context.map(
            |(long_input, long_cached_input, long_output, long_cache_write)| TokenRates {
                input_microusd_per_million: usd(long_input),
                output_microusd_per_million: usd(long_output),
                cache_read_microusd_per_million: long_cached_input.map(usd),
                cache_write_5m_microusd_per_million: long_cache_write.map(usd),
                cache_write_1h_microusd_per_million: long_cache_write.map(usd),
            },
        ),
        source: OPENAI_PRICING_SOURCE,
    }
}

fn anthropic(input: f64, output: f64) -> RateCard {
    RateCard {
        input_microusd_per_million: usd(input),
        output_microusd_per_million: usd(output),
        cache_read_microusd_per_million: Some(usd(input * 0.1)),
        cache_write_5m_microusd_per_million: Some(usd(input * 1.25)),
        cache_write_1h_microusd_per_million: Some(usd(input * 2.0)),
        long_context: None,
        source: ANTHROPIC_PRICING_SOURCE,
    }
}

fn is_model_or_snapshot(model: &str, model_id: &str) -> bool {
    model == model_id
        || model
            .strip_prefix(model_id)
            .is_some_and(|suffix| suffix.starts_with("-20"))
}

fn rate_for_model(tool: ToolKind, model: &str, used_at_ms: i64) -> Option<RateCard> {
    let model = model.trim().to_ascii_lowercase().replace('_', "-");
    match tool {
        ToolKind::Codex => {
            if model == "gpt-5.6" || is_model_or_snapshot(&model, "gpt-5.6-sol") {
                Some(openai(
                    5.0,
                    30.0,
                    Some(0.5),
                    Some(6.25),
                    Some((10.0, Some(1.0), 45.0, Some(12.5))),
                ))
            } else if is_model_or_snapshot(&model, "gpt-5.6-terra") {
                Some(openai(
                    2.5,
                    15.0,
                    Some(0.25),
                    Some(3.125),
                    Some((5.0, Some(0.5), 22.5, Some(6.25))),
                ))
            } else if is_model_or_snapshot(&model, "gpt-5.6-luna") {
                Some(openai(
                    1.0,
                    6.0,
                    Some(0.1),
                    Some(1.25),
                    Some((2.0, Some(0.2), 9.0, Some(2.5))),
                ))
            } else if is_model_or_snapshot(&model, "gpt-5.5-pro") {
                Some(openai(
                    30.0,
                    180.0,
                    None,
                    None,
                    Some((60.0, None, 270.0, None)),
                ))
            } else if is_model_or_snapshot(&model, "gpt-5.5") {
                Some(openai(
                    5.0,
                    30.0,
                    Some(0.5),
                    None,
                    Some((10.0, Some(1.0), 45.0, None)),
                ))
            } else if is_model_or_snapshot(&model, "gpt-5.4-pro") {
                Some(openai(
                    30.0,
                    180.0,
                    None,
                    None,
                    Some((60.0, None, 270.0, None)),
                ))
            } else if is_model_or_snapshot(&model, "gpt-5.4-mini") {
                Some(openai(0.75, 4.5, Some(0.075), None, None))
            } else if is_model_or_snapshot(&model, "gpt-5.4-nano") {
                Some(openai(0.2, 1.25, Some(0.02), None, None))
            } else if is_model_or_snapshot(&model, "gpt-5.4") {
                Some(openai(
                    2.5,
                    15.0,
                    Some(0.25),
                    None,
                    Some((5.0, Some(0.5), 22.5, None)),
                ))
            } else if is_model_or_snapshot(&model, "gpt-5.3-codex") {
                Some(openai(1.75, 14.0, Some(0.175), None, None))
            } else {
                None
            }
        }
        ToolKind::Claude => {
            if model.contains("fable-5") || model.contains("mythos-5") {
                Some(anthropic(10.0, 50.0))
            } else if ["opus-4-8", "opus-4-7", "opus-4-6", "opus-4-5"]
                .iter()
                .any(|name| model.contains(name))
            {
                Some(anthropic(5.0, 25.0))
            } else if model.contains("sonnet-5") {
                if used_at_ms < SEPTEMBER_2026_UTC_MS {
                    Some(anthropic(2.0, 10.0))
                } else {
                    Some(anthropic(3.0, 15.0))
                }
            } else if model.contains("sonnet-4-6") || model.contains("sonnet-4-5") {
                Some(anthropic(3.0, 15.0))
            } else if model.contains("haiku-4-5") {
                Some(anthropic(1.0, 5.0))
            } else {
                None
            }
        }
    }
}

fn component_cost(tokens: u64, microusd_per_million: u64) -> u64 {
    ((tokens as u128 * microusd_per_million as u128) / 1_000_000) as u64
}

pub fn estimate(
    tool: ToolKind,
    model: &str,
    tokens: BillableTokens,
    used_at_ms: i64,
) -> Option<OfficialPriceEstimate> {
    let rate = rate_for_model(tool, model, used_at_ms)?;
    let input_total = tokens
        .input
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write_5m)
        .saturating_add(tokens.cache_write_1h);
    let selected = if input_total > OPENAI_LONG_CONTEXT_THRESHOLD {
        rate.long_context.unwrap_or(TokenRates {
            input_microusd_per_million: rate.input_microusd_per_million,
            output_microusd_per_million: rate.output_microusd_per_million,
            cache_read_microusd_per_million: rate.cache_read_microusd_per_million,
            cache_write_5m_microusd_per_million: rate.cache_write_5m_microusd_per_million,
            cache_write_1h_microusd_per_million: rate.cache_write_1h_microusd_per_million,
        })
    } else {
        TokenRates {
            input_microusd_per_million: rate.input_microusd_per_million,
            output_microusd_per_million: rate.output_microusd_per_million,
            cache_read_microusd_per_million: rate.cache_read_microusd_per_million,
            cache_write_5m_microusd_per_million: rate.cache_write_5m_microusd_per_million,
            cache_write_1h_microusd_per_million: rate.cache_write_1h_microusd_per_million,
        }
    };
    let cache_read_rate = if tokens.cache_read == 0 {
        0
    } else {
        selected.cache_read_microusd_per_million?
    };
    let cache_write_5m_rate = if tokens.cache_write_5m == 0 {
        0
    } else {
        selected.cache_write_5m_microusd_per_million?
    };
    let cache_write_1h_rate = if tokens.cache_write_1h == 0 {
        0
    } else {
        selected.cache_write_1h_microusd_per_million?
    };
    let cost = component_cost(tokens.input, selected.input_microusd_per_million)
        .saturating_add(component_cost(
            tokens.output,
            selected.output_microusd_per_million,
        ))
        .saturating_add(component_cost(tokens.cache_read, cache_read_rate))
        .saturating_add(component_cost(tokens.cache_write_5m, cache_write_5m_rate))
        .saturating_add(component_cost(tokens.cache_write_1h, cache_write_1h_rate));
    Some(OfficialPriceEstimate {
        cost_microusd: cost,
        source: rate.source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_estimate_separates_input_output_and_cached_tokens() {
        let estimate = estimate(
            ToolKind::Codex,
            "gpt-5.6-sol",
            BillableTokens {
                input: 1_000_000,
                output: 1_000_000,
                cache_read: 1_000_000,
                cache_write_5m: 1_000_000,
                cache_write_1h: 0,
            },
            1_783_987_200_000,
        )
        .expect("known model");
        assert_eq!(estimate.cost_microusd, usd(68.5));
    }

    #[test]
    fn claude_cache_write_duration_changes_official_estimate() {
        let five_minutes = estimate(
            ToolKind::Claude,
            "claude-sonnet-4-6",
            BillableTokens {
                cache_write_5m: 1_000_000,
                ..BillableTokens::default()
            },
            1_783_987_200_000,
        )
        .expect("known model");
        let one_hour = estimate(
            ToolKind::Claude,
            "claude-sonnet-4-6",
            BillableTokens {
                cache_write_1h: 1_000_000,
                ..BillableTokens::default()
            },
            1_783_987_200_000,
        )
        .expect("known model");
        assert_eq!(five_minutes.cost_microusd, usd(3.75));
        assert_eq!(one_hour.cost_microusd, usd(6.0));
    }

    #[test]
    fn unknown_models_are_never_guessed() {
        assert!(estimate(
            ToolKind::Codex,
            "future-unknown-model",
            BillableTokens::default(),
            1_783_987_200_000,
        )
        .is_none());
    }

    #[test]
    fn openai_long_context_uses_the_published_threshold_rates() {
        let estimate = estimate(
            ToolKind::Codex,
            "gpt-5.6-sol",
            BillableTokens {
                input: 272_001,
                output: 1_000_000,
                ..BillableTokens::default()
            },
            1_783_987_200_000,
        )
        .expect("known long-context model");
        assert_eq!(estimate.cost_microusd, 47_720_010);
    }

    #[test]
    fn codex_specialized_model_uses_its_own_rate_card() {
        let estimate = estimate(
            ToolKind::Codex,
            "gpt-5.3-codex",
            BillableTokens {
                input: 1_000_000,
                output: 1_000_000,
                cache_read: 1_000_000,
                ..BillableTokens::default()
            },
            1_783_987_200_000,
        )
        .expect("official Codex model");
        assert_eq!(estimate.cost_microusd, usd(15.925));
    }
}
