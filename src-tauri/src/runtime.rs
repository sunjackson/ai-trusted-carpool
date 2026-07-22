use crate::account_router::AccountRouterState;
use crate::coordinator::CoordinatorMessage;
use crate::models::{CarSession, RideAccess};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct RuntimeState {
    pub inner: Arc<Mutex<AppRuntime>>,
}

impl RuntimeState {
    pub fn begin_ride_transition(&self) -> Result<RideTransitionGuard, String> {
        let mut runtime = self
            .inner
            .lock()
            .map_err(|_| "运行状态暂时不可用".to_string())?;
        if runtime.app_update_installing {
            return Err("应用正在安装更新，请完成重启后再发车或上车".to_string());
        }
        runtime.ride_transitions = runtime
            .ride_transitions
            .checked_add(1)
            .ok_or_else(|| "拼车启动状态暂时不可用".to_string())?;
        drop(runtime);
        Ok(RideTransitionGuard {
            state: self.clone(),
        })
    }
}

pub struct RideTransitionGuard {
    state: RuntimeState,
}

impl Drop for RideTransitionGuard {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.state.inner.lock() {
            runtime.ride_transitions = runtime.ride_transitions.saturating_sub(1);
        }
    }
}

#[derive(Default)]
pub struct AppRuntime {
    pub active_car: Option<CarSession>,
    pub accesses: HashMap<String, RideAccess>,
    pub ride_transitions: usize,
    pub app_update_installing: bool,
    pub access_secrets: HashMap<String, String>,
    pub passenger_contexts: HashMap<String, PassengerAccessContext>,
    pub host_bindings: HashMap<String, HostSeatBinding>,
    pub pending_signals: Vec<CoordinatorMessage>,
    pub relay_request_seen_at: HashMap<String, i64>,
    pub usage_history_path: Option<PathBuf>,
    pub account_pool_path: Option<PathBuf>,
    pub ride_history_path: Option<PathBuf>,
    pub account_router: AccountRouterState,
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
