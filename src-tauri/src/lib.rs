mod commands;
mod coordinator;
mod crypto;
mod identity;
mod local_proxy;
mod models;
mod pricing;
mod protocol;
mod relay;
mod runtime;
mod terminal_launcher;
mod usage;
mod usage_history;

use commands::{
    detect_tools, execute_relay_request, get_active_car, get_ice_servers, join_car, launch_tool,
    leave_car, poll_webrtc_signals, preview_invite, send_webrtc_signal, start_car,
    start_relay_request, stop_car, submit_relay_response, submit_relay_stream_event,
};
use relay::RelayBridge;
use runtime::RuntimeState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = RuntimeState::default();
    let setup_state = state.clone();
    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            let history_path = app
                .path()
                .app_data_dir()
                .map_err(std::io::Error::other)?
                .join("usage-history.jsonl");
            usage_history::prepare(&history_path).map_err(std::io::Error::other)?;
            setup_state
                .inner
                .lock()
                .map_err(|_| std::io::Error::other("运行状态暂时不可用"))?
                .usage_history_path = Some(history_path);
            tauri::async_runtime::block_on(
                RelayBridge::global().set_app_handle(app.handle().clone()),
            );
            local_proxy::start(setup_state.clone()).map_err(std::io::Error::other)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect_tools,
            start_car,
            stop_car,
            get_active_car,
            preview_invite,
            join_car,
            leave_car,
            launch_tool,
            get_ice_servers,
            send_webrtc_signal,
            poll_webrtc_signals,
            execute_relay_request,
            start_relay_request,
            submit_relay_response,
            submit_relay_stream_event
        ])
        .run(tauri::generate_context!())
        .expect("failed to run trusted carpool desktop");
}
