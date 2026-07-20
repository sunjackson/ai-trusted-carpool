mod account_quota;
mod client_launcher;
mod commands;
mod coordinator;
mod crypto;
mod identity;
mod join_link;
mod local_proxy;
mod models;
mod pricing;
mod protocol;
mod quota;
mod relay;
mod runtime;
mod status_tray;
mod terminal_launcher;
mod tool_installer;
mod usage;
mod usage_history;

use commands::{
    detect_tools, execute_relay_request, get_active_car, get_ice_servers, get_shared_car_status,
    install_tool, join_car, launch_tool, leave_car, poll_webrtc_signals, preview_invite,
    refresh_account_quotas, send_webrtc_signal, start_car, start_relay_request, stop_car,
    submit_relay_response, submit_relay_stream_event, update_member_token_limits,
};
use relay::RelayBridge;
use runtime::RuntimeState;
use tauri::Manager;
use tauri_plugin_deep_link::DeepLinkExt;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = RuntimeState::default();
    let setup_state = state.clone();
    let single_instance_state = state.clone();
    let builder = tauri::Builder::default();
    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(
        move |app, arguments, _working_directory| {
            join_link::accept_urls(app, &single_instance_state, arguments.iter());
            join_link::show_main_window(app);
        },
    ));
    builder
        .plugin(tauri_plugin_deep_link::init())
        .manage(state)
        .setup(move |app| {
            client_launcher::recover_stale(app.handle()).map_err(std::io::Error::other)?;
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
            match status_tray::setup(app) {
                Ok(()) => {
                    status_tray::spawn_refresh_loop(app.handle().clone(), setup_state.clone())
                }
                Err(error) => eprintln!("system status tray is unavailable: {error}"),
            }

            #[cfg(any(windows, target_os = "linux"))]
            if let Err(error) = app.deep_link().register_all() {
                eprintln!("runtime deep-link registration is unavailable: {error}");
            }

            if let Some(urls) = app
                .deep_link()
                .get_current()
                .map_err(std::io::Error::other)?
            {
                join_link::accept_urls(
                    app.handle(),
                    &setup_state,
                    urls.iter().map(ToString::to_string),
                );
            }
            let open_state = setup_state.clone();
            let open_app = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                join_link::accept_urls(
                    &open_app,
                    &open_state,
                    event.urls().iter().map(ToString::to_string),
                );
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect_tools,
            install_tool,
            start_car,
            stop_car,
            get_active_car,
            refresh_account_quotas,
            update_member_token_limits,
            get_shared_car_status,
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
            submit_relay_stream_event,
            join_link::take_pending_join_code
        ])
        .build(tauri::generate_context!())
        .expect("failed to build trusted carpool desktop")
        .run(|app, event| {
            if let tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } = &event
            {
                if label == "main" && status_tray::should_hide_on_close(app) {
                    api.prevent_close();
                    if let Some(window) = app.get_webview_window(label) {
                        let _ = window.hide();
                    }
                }
            }
            if matches!(event, tauri::RunEvent::Exit) {
                if let Err(error) = client_launcher::recover_stale(app) {
                    eprintln!("failed to restore desktop client configuration on exit: {error}");
                }
            }
        });
}
