use crate::models::{AccountQuotaSnapshot, AccountQuotaState, AccountQuotaWindow, ToolKind};
use crate::relay::{load_host_credential, HostCredential, HostCredentialKind};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn pending(tool: ToolKind) -> AccountQuotaSnapshot {
    AccountQuotaSnapshot {
        tool,
        state: AccountQuotaState::Pending,
        plan_name: None,
        fetched_at: None,
        source: match tool {
            ToolKind::Claude => CLAUDE_USAGE_URL,
            ToolKind::Codex => CODEX_USAGE_URL,
        }
        .to_string(),
        message: Some("正在读取官方账号额度".to_string()),
        windows: Vec::new(),
    }
}

pub fn pending_for(tools: &[ToolKind]) -> Vec<AccountQuotaSnapshot> {
    tools.iter().copied().map(pending).collect()
}

fn unsupported(tool: ToolKind, message: &str) -> AccountQuotaSnapshot {
    let mut snapshot = pending(tool);
    snapshot.state = AccountQuotaState::Unsupported;
    snapshot.fetched_at = Some(now_ms());
    snapshot.message = Some(message.to_string());
    snapshot
}

fn failed(tool: ToolKind, message: impl Into<String>) -> AccountQuotaSnapshot {
    let mut snapshot = pending(tool);
    snapshot.state = AccountQuotaState::Error;
    snapshot.fetched_at = Some(now_ms());
    snapshot.message = Some(message.into());
    snapshot
}

fn window(label: &str, used_percent: f64, resets_at: Option<i64>) -> AccountQuotaWindow {
    let used_percent = used_percent.clamp(0.0, 100.0);
    AccountQuotaWindow {
        label: label.to_string(),
        used_percent,
        remaining_percent: 100.0 - used_percent,
        resets_at,
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn parse_digits(value: &str, start: usize, end: usize) -> Option<i64> {
    value.get(start..end)?.parse().ok()
}

fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.len() < 20
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(10) != Some(&b'T')
    {
        return None;
    }
    let year = parse_digits(value, 0, 4)?;
    let month = parse_digits(value, 5, 7)?;
    let day = parse_digits(value, 8, 10)?;
    let hour = parse_digits(value, 11, 13)?;
    let minute = parse_digits(value, 14, 16)?;
    let second = parse_digits(value, 17, 19)?;
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let timezone_start = value[19..].find(['Z', '+', '-']).map(|index| index + 19)?;
    let timezone = &value[timezone_start..];
    let offset_seconds = if timezone == "Z" {
        0
    } else {
        if timezone.len() != 6 || timezone.as_bytes().get(3) != Some(&b':') {
            return None;
        }
        let offset_hours: i64 = timezone.get(1..3)?.parse().ok()?;
        let offset_minutes: i64 = timezone.get(4..6)?.parse().ok()?;
        if offset_hours > 23 || offset_minutes > 59 {
            return None;
        }
        let offset = offset_hours * 3_600 + offset_minutes * 60;
        if timezone.starts_with('+') {
            offset
        } else {
            -offset
        }
    };
    let seconds = days_from_civil(year, month, day)
        .saturating_mul(86_400)
        .saturating_add(hour * 3_600 + minute * 60 + second)
        .saturating_sub(offset_seconds);
    Some(seconds.saturating_mul(1_000))
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeWindow {
    utilization: f64,
    resets_at: String,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(default)]
    five_hour: ClaudeWindow,
    #[serde(default)]
    seven_day: ClaudeWindow,
    #[serde(default)]
    seven_day_sonnet: ClaudeWindow,
}

fn parse_claude_usage(body: &[u8], fetched_at: i64) -> Result<AccountQuotaSnapshot, String> {
    let response: ClaudeUsageResponse = serde_json::from_slice(body)
        .map_err(|error| format!("Claude 官方额度格式无效: {error}"))?;
    let mut windows = vec![window(
        "5 小时",
        response.five_hour.utilization,
        parse_rfc3339_ms(&response.five_hour.resets_at),
    )];
    if !response.seven_day.resets_at.is_empty() || response.seven_day.utilization > 0.0 {
        windows.push(window(
            "7 天",
            response.seven_day.utilization,
            parse_rfc3339_ms(&response.seven_day.resets_at),
        ));
    }
    if !response.seven_day_sonnet.resets_at.is_empty()
        || response.seven_day_sonnet.utilization > 0.0
    {
        windows.push(window(
            "7 天 Sonnet",
            response.seven_day_sonnet.utilization,
            parse_rfc3339_ms(&response.seven_day_sonnet.resets_at),
        ));
    }
    Ok(AccountQuotaSnapshot {
        tool: ToolKind::Claude,
        state: AccountQuotaState::Available,
        plan_name: None,
        fetched_at: Some(fetched_at),
        source: CLAUDE_USAGE_URL.to_string(),
        message: None,
        windows,
    })
}

#[derive(Debug, Deserialize)]
struct CodexWindow {
    used_percent: f64,
    limit_window_seconds: i64,
    reset_at: i64,
}

#[derive(Debug, Default, Deserialize)]
struct CodexRateLimit {
    primary_window: Option<CodexWindow>,
    secondary_window: Option<CodexWindow>,
}

#[derive(Debug, Deserialize)]
struct CodexAdditionalRateLimit {
    #[serde(default)]
    limit_name: String,
    rate_limit: Option<CodexRateLimit>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexUsageResponse {
    #[serde(default)]
    plan_type: String,
    rate_limit: Option<CodexRateLimit>,
    #[serde(default)]
    additional_rate_limits: Vec<CodexAdditionalRateLimit>,
}

fn codex_window_label(seconds: i64, fallback: &str) -> String {
    match seconds {
        17_900..=18_100 => "5 小时".to_string(),
        86_300..=86_500 => "24 小时".to_string(),
        604_700..=604_900 => "7 天".to_string(),
        value if value > 0 && value % 3_600 == 0 => format!("{} 小时", value / 3_600),
        _ => fallback.to_string(),
    }
}

fn push_codex_windows(
    output: &mut Vec<AccountQuotaWindow>,
    prefix: &str,
    rate_limit: &CodexRateLimit,
) {
    for (fallback, item) in [
        ("主要窗口", rate_limit.primary_window.as_ref()),
        ("次要窗口", rate_limit.secondary_window.as_ref()),
    ] {
        let Some(item) = item else { continue };
        let base = codex_window_label(item.limit_window_seconds, fallback);
        let label = if prefix.is_empty() {
            base
        } else {
            format!("{prefix} · {base}")
        };
        if output.iter().any(|window| window.label == label) {
            continue;
        }
        output.push(window(
            &label,
            item.used_percent,
            Some(item.reset_at.saturating_mul(1_000)),
        ));
    }
}

fn parse_codex_usage(body: &[u8], fetched_at: i64) -> Result<AccountQuotaSnapshot, String> {
    let response: CodexUsageResponse =
        serde_json::from_slice(body).map_err(|error| format!("Codex 官方额度格式无效: {error}"))?;
    let mut windows = Vec::new();
    if let Some(rate_limit) = &response.rate_limit {
        push_codex_windows(&mut windows, "", rate_limit);
    }
    for additional in &response.additional_rate_limits {
        if let Some(rate_limit) = &additional.rate_limit {
            push_codex_windows(&mut windows, additional.limit_name.trim(), rate_limit);
        }
    }
    if windows.is_empty() {
        return Err("Codex 官方额度响应没有可用窗口".to_string());
    }
    Ok(AccountQuotaSnapshot {
        tool: ToolKind::Codex,
        state: AccountQuotaState::Available,
        plan_name: (!response.plan_type.trim().is_empty())
            .then(|| response.plan_type.trim().to_string()),
        fetched_at: Some(fetched_at),
        source: CODEX_USAGE_URL.to_string(),
        message: None,
        windows,
    })
}

async fn query_claude(credential: &HostCredential) -> Result<AccountQuotaSnapshot, String> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("无法创建 Claude 额度客户端: {error}"))?
        .get(CLAUDE_USAGE_URL)
        .bearer_auth(&credential.secret)
        .header("accept", "application/json, text/plain, */*")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "claude-code/2.1.7")
        .send()
        .await
        .map_err(|error| format!("读取 Claude 官方额度失败: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Claude 官方额度接口返回 {}", response.status()));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Claude 官方额度响应失败: {error}"))?;
    parse_claude_usage(&body, now_ms())
}

async fn query_codex(credential: &HostCredential) -> Result<AccountQuotaSnapshot, String> {
    let account_id = credential
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Codex OAuth 缺少 ChatGPT Account ID，请重新登录 Codex".to_string())?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("无法创建 Codex 额度客户端: {error}"))?
        .get(CODEX_USAGE_URL)
        .bearer_auth(&credential.secret)
        .header("chatgpt-account-id", account_id)
        .header("openai-beta", "codex-1")
        .header("oai-language", "zh-CN")
        .header("originator", "Codex Desktop")
        .header("accept", "application/json")
        .header("sec-fetch-site", "none")
        .header("sec-fetch-mode", "no-cors")
        .header("sec-fetch-dest", "empty")
        .header("priority", "u=4, i")
        .send()
        .await
        .map_err(|error| format!("读取 Codex 官方额度失败: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Codex 官方额度接口返回 {}", response.status()));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Codex 官方额度响应失败: {error}"))?;
    parse_codex_usage(&body, now_ms())
}

pub async fn query(tool: ToolKind) -> AccountQuotaSnapshot {
    let Some(credential) = load_host_credential(tool) else {
        return failed(tool, "未检测到可用的本机官方账号");
    };
    match (tool, credential.kind) {
        (ToolKind::Claude, HostCredentialKind::ClaudeOAuth) => query_claude(&credential)
            .await
            .unwrap_or_else(|error| failed(tool, error)),
        (ToolKind::Codex, HostCredentialKind::CodexChatGptOAuth) => query_codex(&credential)
            .await
            .unwrap_or_else(|error| failed(tool, error)),
        (ToolKind::Claude, HostCredentialKind::ApiKey) => unsupported(
            tool,
            "Claude API Key 官方未提供订阅额度查询；成员限额仍按本车实际 Token 统计",
        ),
        (ToolKind::Codex, HostCredentialKind::ApiKey) => unsupported(
            tool,
            "OpenAI API Key 官方未提供 ChatGPT 套餐额度查询；成员限额仍按本车实际 Token 统计",
        ),
        _ => failed(tool, "本机账号类型与工具不匹配"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_official_windows_and_remaining_percent() {
        let snapshot = parse_claude_usage(
            br#"{
              "five_hour":{"utilization":25.5,"resets_at":"2026-07-15T08:00:00Z"},
              "seven_day":{"utilization":70,"resets_at":"2026-07-20T08:00:00+00:00"},
              "seven_day_sonnet":{"utilization":10,"resets_at":"2026-07-20T08:00:00Z"}
            }"#,
            123,
        )
        .expect("snapshot");
        assert_eq!(snapshot.state, AccountQuotaState::Available);
        assert_eq!(snapshot.windows[0].label, "5 小时");
        assert_eq!(snapshot.windows[0].remaining_percent, 74.5);
        assert!(snapshot.windows[0].resets_at.is_some());
    }

    #[test]
    fn parses_codex_primary_secondary_and_feature_windows() {
        let snapshot = parse_codex_usage(
            br#"{
              "plan_type":"plus",
              "rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000,"reset_after_seconds":90,"reset_at":1784102400},"secondary_window":{"used_percent":15,"limit_window_seconds":604800,"reset_after_seconds":90,"reset_at":1784707200}},
              "additional_rate_limits":[{"limit_name":"Spark","metered_feature":"spark","rate_limit":{"primary_window":{"used_percent":20,"limit_window_seconds":18000,"reset_after_seconds":60,"reset_at":1784102400}}}]
            }"#,
            456,
        )
        .expect("snapshot");
        assert_eq!(snapshot.plan_name.as_deref(), Some("plus"));
        assert_eq!(snapshot.windows[0].label, "5 小时");
        assert_eq!(snapshot.windows[1].label, "7 天");
        assert_eq!(snapshot.windows[2].label, "Spark · 5 小时");
        assert_eq!(snapshot.windows[0].remaining_percent, 58.0);
    }

    #[test]
    fn rfc3339_parser_handles_utc_and_offsets() {
        assert_eq!(
            parse_rfc3339_ms("2026-07-15T08:00:00Z"),
            parse_rfc3339_ms("2026-07-15T16:00:00+08:00")
        );
        assert!(parse_rfc3339_ms("not-a-time").is_none());
    }
}
