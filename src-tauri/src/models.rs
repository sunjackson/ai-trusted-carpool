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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // Reserved for the proxy lifecycle and local risk controls.
pub enum SeatState {
    Waiting,
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
    pub enabled_tools: Vec<ToolKind>,
    pub seats: Vec<Seat>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCarInput {
    pub car_name: String,
    pub enabled_tools: Vec<ToolKind>,
    pub starts_at: i64,
    pub ends_at: i64,
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
    pub access_id: String,
    pub work_dir: Option<String>,
}
