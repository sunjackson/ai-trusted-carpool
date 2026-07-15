use crate::coordinator::CoordinatorMessage;
use crate::models::{CarSession, RideAccess};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct RuntimeState {
    pub inner: Arc<Mutex<AppRuntime>>,
}

#[derive(Default)]
pub struct AppRuntime {
    pub active_car: Option<CarSession>,
    pub accesses: HashMap<String, RideAccess>,
    pub access_secrets: HashMap<String, String>,
    pub passenger_contexts: HashMap<String, PassengerAccessContext>,
    pub host_bindings: HashMap<String, HostSeatBinding>,
    pub pending_signals: Vec<CoordinatorMessage>,
    pub relay_request_seen_at: HashMap<String, i64>,
    pub usage_history_path: Option<PathBuf>,
    pub pending_join_code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PassengerAccessContext {
    pub code: String,
    pub car_id: String,
    pub owner_peer_id: String,
    pub owner_public_key: String,
    pub owner_encryption_public_key: String,
}

#[derive(Debug, Clone)]
pub struct HostSeatBinding {
    pub code: String,
    pub claim_id: String,
    pub passenger_peer_id: String,
    pub passenger_encryption_public_key: String,
    pub access_id: String,
    pub session_secret: String,
    pub issued_at_ms: i64,
}
