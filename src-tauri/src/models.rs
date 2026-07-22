use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    Claude,
    Codex,
}

impl ToolKind {
    pub fn command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDetection {
    pub kind: ToolKind,
    pub name: String,
    pub installed: bool,
    pub authenticated: bool,
    pub executable_path: Option<String>,
    pub config_path: Option<String>,
    pub detail: String,
    pub version: Option<String>,
    pub npm_available: bool,
    pub managed_by_app: bool,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub desktop_supported: bool,
    pub desktop_installed: bool,
    pub desktop_path: Option<String>,
    pub desktop_detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    Terminal,
    Desktop,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // Reserved for the proxy lifecycle and local risk controls.
pub enum SeatState {
    Waiting,
    /// Claim accepted and access granted; WebRTC data channel not open yet.
    Joining,
    Connected,
    Using,
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Seat {
    pub seat_no: u8,
    pub code: String,
    pub nickname: Option<String>,
    pub state: SeatState,
    pub tool: Option<ToolKind>,
    pub usage: SeatUsageSummary,
    pub token_limits: MemberTokenLimits,
    pub token_limit_status: MemberTokenLimitStatus,
    #[serde(skip)]
    pub token_usage_events: Vec<TokenUsageEvent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemberTokenLimits {
    pub five_hour_tokens: Option<u64>,
    pub daily_tokens: Option<u64>,
    pub weekly_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemberTokenLimitStatus {
    pub five_hour: TokenWindowStatus,
    pub daily: TokenWindowStatus,
    pub weekly: TokenWindowStatus,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenWindowStatus {
    pub limit_tokens: Option<u64>,
    pub used_tokens: u64,
    pub remaining_tokens: Option<u64>,
    pub resets_at: Option<i64>,
    pub exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsageEvent {
    pub occurred_at: i64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatUsageSummary {
    pub request_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub total_tokens: u64,
    pub official_cost_microusd: u64,
    pub unpriced_request_count: u64,
    pub last_used_at: Option<i64>,
    pub models: Vec<ModelUsageSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageSummary {
    pub tool: ToolKind,
    pub model: String,
    pub request_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub official_cost_microusd: Option<u64>,
    pub unpriced_request_count: u64,
    pub pricing_source: Option<String>,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CarSession {
    pub car_id: String,
    pub car_name: String,
    pub owner_peer_id: String,
    pub started_at: i64,
    pub expires_at: i64,
    pub always_on: bool,
    pub enabled_tools: Vec<ToolKind>,
    pub seats: Vec<Seat>,
    pub account_quotas: Vec<AccountQuotaSnapshot>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuotaSnapshot {
    pub tool: ToolKind,
    pub state: AccountQuotaState,
    pub plan_name: Option<String>,
    pub fetched_at: Option<i64>,
    pub source: String,
    pub message: Option<String>,
    pub windows: Vec<AccountQuotaWindow>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountQuotaState {
    Pending,
    Available,
    Unsupported,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuotaWindow {
    pub label: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedCarStatus {
    pub car_id: String,
    pub car_name: String,
    pub started_at: i64,
    pub expires_at: i64,
    pub always_on: bool,
    pub enabled_tools: Vec<ToolKind>,
    pub account_quotas: Vec<AccountQuotaSnapshot>,
    pub member: SharedMemberStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedMemberStatus {
    pub seat_no: u8,
    pub nickname: String,
    pub state: SeatState,
    pub tool: Option<ToolKind>,
    pub usage: SeatUsageSummary,
    pub token_limits: MemberTokenLimits,
    pub token_limit_status: MemberTokenLimitStatus,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCarInput {
    pub car_name: String,
    pub enabled_tools: Vec<ToolKind>,
    pub starts_at: i64,
    pub ends_at: i64,
    #[serde(default)]
    pub always_on: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMemberTokenLimitsInput {
    pub seat_no: u8,
    pub five_hour_tokens: Option<u64>,
    pub daily_tokens: Option<u64>,
    pub weekly_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinPreview {
    pub car_id: String,
    pub car_name: String,
    pub owner_label: String,
    pub seat_no: u8,
    pub enabled_tools: Vec<ToolKind>,
    pub starts_at: i64,
    pub expires_at: i64,
    pub always_on: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RideHistoryRole {
    Host,
    Passenger,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RideAvailability {
    Online,
    Scheduled,
    Offline,
    Expired,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RideHistorySummary {
    pub record_id: String,
    pub role: RideHistoryRole,
    pub car_id: String,
    pub car_name: String,
    pub started_at: i64,
    pub expires_at: i64,
    pub always_on: bool,
    pub enabled_tools: Vec<ToolKind>,
    pub seat_no: Option<u8>,
    pub nickname: Option<String>,
    pub created_at: i64,
    pub last_active_at: i64,
    pub ended_at: Option<i64>,
    pub can_resume: bool,
    pub availability: RideAvailability,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RideAccess {
    #[serde(flatten)]
    pub preview: JoinPreview,
    pub access_id: String,
    pub owner_peer_id: String,
    pub local_proxy_port: u16,
    pub connection_state: ConnectionState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // The transport layer will surface reconnect and fallback states.
pub enum ConnectionState {
    Connecting,
    Connected,
    Degraded,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchToolInput {
    pub kind: ToolKind,
    pub mode: LaunchMode,
    pub access_id: String,
    pub work_dir: Option<String>,
}
