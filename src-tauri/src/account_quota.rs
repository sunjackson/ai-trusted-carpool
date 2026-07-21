use crate::account_pool::{AccountAuthKind, AccountPool};
use crate::models::{AccountQuotaSnapshot, AccountQuotaState, AccountQuotaWindow, ToolKind};
use crate::relay::{
    load_host_credential, load_host_oauth_credential, HostCredential, HostCredentialKind,
};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_QUOTA_CREDENTIAL_ATTEMPTS: usize = 3;
const QUOTA_TASK_FAILED_MESSAGE: &str = "读取官方额度任务异常结束";
const ACCOUNT_POOL_UNAVAILABLE_MESSAGE: &str =
    "本地账号池暂时无法读取；未找到可用于额度查询的本机 OAuth 备用登录";

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

#[derive(Debug, Deserialize)]
struct ClaudeWindow {
    #[serde(default)]
    utilization: f64,
    #[serde(default)]
    resets_at: String,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(default)]
    five_hour: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_opus: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_overage_included: Option<ClaudeWindow>,
}

fn parse_claude_usage(body: &[u8], fetched_at: i64) -> Result<AccountQuotaSnapshot, String> {
    let response: ClaudeUsageResponse = serde_json::from_slice(body)
        .map_err(|error| format!("Claude 官方额度格式无效: {error}"))?;
    let mut windows = Vec::new();
    for (label, item) in [
        ("5 小时", response.five_hour.as_ref()),
        ("7 天", response.seven_day.as_ref()),
        ("7 天 Sonnet", response.seven_day_sonnet.as_ref()),
        ("7 天 Opus", response.seven_day_opus.as_ref()),
        ("7 天扩展用量", response.seven_day_overage_included.as_ref()),
    ] {
        let Some(item) = item else { continue };
        windows.push(window(
            label,
            item.utilization,
            parse_rfc3339_ms(&item.resets_at),
        ));
    }
    if windows.is_empty() {
        return Err("Claude 官方额度响应没有可用窗口".to_string());
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

fn quota_credentials(
    tool: ToolKind,
    pool_path: Option<&Path>,
) -> (Vec<HostCredential>, bool, bool) {
    let mut oauth = Vec::new();
    let mut has_api_key = false;
    let mut pool_load_failed = false;
    if let Some(path) = pool_path {
        match AccountPool::new(path.to_path_buf()).candidates(tool) {
            Ok(candidates) => {
                for candidate in candidates {
                    match candidate.credential.auth_kind() {
                        AccountAuthKind::ApiKey => has_api_key = true,
                        AccountAuthKind::OAuth => {
                            if candidate
                                .credential
                                .is_expired_at(now_ms().saturating_add(60_000))
                            {
                                continue;
                            }
                            oauth.push(HostCredential {
                                secret: candidate.credential.secret().to_string(),
                                account_id: candidate.credential.account_id().map(str::to_string),
                                kind: match tool {
                                    ToolKind::Claude => HostCredentialKind::ClaudeOAuth,
                                    ToolKind::Codex => HostCredentialKind::CodexChatGptOAuth,
                                },
                                source: "可信拼车本地账号池".to_string(),
                            });
                        }
                    }
                }
            }
            Err(_) => {
                pool_load_failed = true;
                crate::diagnostics::record(
                    "error",
                    "account-quota",
                    format!(
                        "failed to read the encrypted local {} account pool for quota lookup; trying local fallback credentials",
                        tool.command()
                    ),
                );
            }
        }
    }
    if let Some(local) = load_host_oauth_credential(tool) {
        let duplicate = oauth
            .iter()
            .any(|credential| credential.kind == local.kind && credential.secret == local.secret);
        if !duplicate {
            oauth.push(local);
        }
    }
    if load_host_credential(tool)
        .is_some_and(|credential| credential.kind == HostCredentialKind::ApiKey)
    {
        has_api_key = true;
    }
    (oauth, has_api_key, pool_load_failed)
}

fn take_decidable_result<T>(
    outcomes: &mut [Option<Result<T, String>>],
) -> Option<Result<T, String>> {
    let mut decision_index = None;
    let mut last_error_index = None;
    for (index, outcome) in outcomes.iter().enumerate() {
        match outcome {
            Some(Ok(_)) => {
                decision_index = Some(index);
                break;
            }
            Some(Err(_)) => last_error_index = Some(index),
            None => return None,
        }
    }
    let index = decision_index.or(last_error_index)?;
    outcomes[index].take()
}

async fn resolve_prioritized_queries<T: Send + 'static>(
    mut queries: JoinSet<(usize, Result<T, String>)>,
    query_count: usize,
) -> Option<Result<T, String>> {
    let mut outcomes = (0..query_count).map(|_| None).collect::<Vec<_>>();
    while let Some(joined) = queries.join_next().await {
        if let Ok((index, result)) = joined {
            if let Some(outcome) = outcomes.get_mut(index) {
                *outcome = Some(result);
            }
            if let Some(decision) = take_decidable_result(&mut outcomes) {
                queries.abort_all();
                return Some(decision);
            }
        }
    }
    for outcome in &mut outcomes {
        if outcome.is_none() {
            *outcome = Some(Err(QUOTA_TASK_FAILED_MESSAGE.to_string()));
        }
    }
    take_decidable_result(&mut outcomes)
}

pub async fn query(tool: ToolKind, pool_path: Option<&Path>) -> AccountQuotaSnapshot {
    let (mut credentials, has_api_key, pool_load_failed) = quota_credentials(tool, pool_path);
    credentials.truncate(MAX_QUOTA_CREDENTIAL_ATTEMPTS);
    let query_count = credentials.len();
    let mut queries = JoinSet::new();
    for (index, credential) in credentials.into_iter().enumerate() {
        queries.spawn(async move {
            let result = match (tool, credential.kind) {
                (ToolKind::Claude, HostCredentialKind::ClaudeOAuth) => {
                    query_claude(&credential).await
                }
                (ToolKind::Codex, HostCredentialKind::CodexChatGptOAuth) => {
                    query_codex(&credential).await
                }
                _ => Err("本机 OAuth 账号类型与工具不匹配".to_string()),
            };
            (index, result)
        });
    }
    if let Some(result) = resolve_prioritized_queries(queries, query_count).await {
        return result.unwrap_or_else(|error| failed(tool, error));
    }
    if pool_load_failed {
        return failed(tool, ACCOUNT_POOL_UNAVAILABLE_MESSAGE);
    }
    if has_api_key {
        return match tool {
            ToolKind::Claude => unsupported(
                tool,
                "Claude API Key 官方未提供订阅额度查询；成员限额仍按本车实际 Token 统计",
            ),
            ToolKind::Codex => unsupported(
                tool,
                "OpenAI API Key 官方未提供 ChatGPT 套餐额度查询；成员限额仍按本车实际 Token 统计",
            ),
        };
    }
    match tool {
        ToolKind::Claude => failed(
            tool,
            "未检测到 Claude 官方 OAuth 登录；API Key 无法查询官方套餐额度",
        ),
        ToolKind::Codex => failed(tool, "未检测到 Codex 的 ChatGPT OAuth 登录，请先导入账号"),
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
    fn parses_claude_nullable_model_windows() {
        let snapshot = parse_claude_usage(
            br#"{
              "five_hour":{"utilization":12,"resets_at":"2026-07-15T08:00:00Z"},
              "seven_day":{"utilization":34,"resets_at":"2026-07-20T08:00:00Z"},
              "seven_day_sonnet":null,
              "seven_day_opus":{"utilization":56,"resets_at":"2026-07-20T08:00:00Z"}
            }"#,
            124,
        )
        .expect("snapshot");
        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(snapshot.windows[2].label, "7 天 Opus");
        assert_eq!(snapshot.windows[2].remaining_percent, 44.0);
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
    fn parses_codex_pro_weekly_window_when_secondary_is_null() {
        let snapshot = parse_codex_usage(
            br#"{
              "plan_type":"pro",
              "rate_limit":{"primary_window":{"used_percent":17,"limit_window_seconds":604800,"reset_at":1784707200},"secondary_window":null},
              "additional_rate_limits":[{"limit_name":"Spark","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_at":1784707200},"secondary_window":null}}]
            }"#,
            457,
        )
        .expect("snapshot");
        assert_eq!(snapshot.plan_name.as_deref(), Some("pro"));
        assert_eq!(snapshot.windows[0].label, "7 天");
        assert_eq!(snapshot.windows[0].remaining_percent, 83.0);
        assert_eq!(snapshot.windows[1].label, "Spark · 7 天");
    }

    #[test]
    fn rfc3339_parser_handles_utc_and_offsets() {
        assert_eq!(
            parse_rfc3339_ms("2026-07-15T08:00:00Z"),
            parse_rfc3339_ms("2026-07-15T16:00:00+08:00")
        );
        assert!(parse_rfc3339_ms("not-a-time").is_none());
    }

    #[test]
    fn prioritized_results_wait_for_unresolved_higher_priority_accounts() {
        let mut outcomes: Vec<Option<Result<&'static str, String>>> =
            vec![None, Some(Ok("backup")), Some(Ok("later"))];
        assert!(take_decidable_result(&mut outcomes).is_none());

        outcomes[0] = Some(Err("primary failed".to_string()));
        assert_eq!(
            take_decidable_result(&mut outcomes).expect("decidable backup"),
            Ok("backup")
        );
    }

    #[test]
    fn prioritized_results_keep_the_last_error_when_every_account_fails() {
        let mut outcomes = vec![
            Some(Err::<&'static str, _>("primary failed".to_string())),
            Some(Err("backup failed".to_string())),
        ];
        assert_eq!(
            take_decidable_result(&mut outcomes).expect("decidable failure"),
            Err("backup failed".to_string())
        );
    }

    #[tokio::test]
    async fn prioritized_queries_do_not_wait_for_pending_lower_priority_accounts() {
        let mut queries = JoinSet::new();
        queries.spawn(async { (0, Ok::<_, String>("primary")) });
        queries.spawn(async {
            std::future::pending::<()>().await;
            (1, Ok::<_, String>("backup"))
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            resolve_prioritized_queries(queries, 2),
        )
        .await
        .expect("lower-priority pending query must be aborted")
        .expect("query result")
        .expect("successful primary");
        assert_eq!(result, "primary");
    }
}
