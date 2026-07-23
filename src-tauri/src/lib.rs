mod account_pool;
mod account_quota;
mod account_router;
mod account_transfer;
mod app_updater;
mod client_launcher;
mod client_process;
mod commands;
mod coordinator;
mod crypto;
mod diagnostics;
mod identity;
mod join_link;
mod local_proxy;
mod models;
mod pricing;
mod protocol;
mod quota;
mod relay;
mod ride_history;
mod runtime;
mod status_tray;
mod terminal_launcher;
mod tool_installer;
mod tool_provisioner;
mod usage;
mod usage_history;

use app_updater::{
    check_signed_app_update, download_app_update, install_app_update, restart_after_app_update,
    AppUpdaterState,
};
use commands::{
    cancel_account_import, cancel_account_restore, cancel_tool_install, check_app_update,
    close_client_instance, commit_account_import, commit_account_restore, confirm_passenger_link,
    delete_account, detect_tools, execute_relay_request, export_account_backup,
    focus_client_instance, get_active_car, get_ice_servers, get_shared_car_status, import_accounts,
    import_local_accounts, install_tool, join_car, launch_tool, leave_car, list_accounts,
    list_client_instances, list_ride_history, open_releases_page, poll_webrtc_signals,
    preview_account_import, preview_account_restore, preview_invite, refresh_account_quotas,
    resume_host_car, resume_passenger_ride, retry_account_route, send_webrtc_signal, start_car,
    start_relay_request, stop_car, submit_relay_response, submit_relay_stream_event, suspend_car,
    update_account, update_member_token_limits,
};
use diagnostics::{
    clear_debug_logs, export_diagnostic_bundle, get_debug_logs, open_debug_log_directory,
    record_frontend_log,
};
use relay::RelayBridge;
use runtime::RuntimeState;
use tauri::webview::PageLoadEvent;
use tauri::Manager;
use tauri_plugin_deep_link::DeepLinkExt;

fn should_show_main_window(label: &str, event: PageLoadEvent) -> bool {
    label == "main" && event == PageLoadEvent::Finished
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    diagnostics::record("info", "runtime", "application starting");
    let state = RuntimeState::default();
    let updater_state = AppUpdaterState::default();
    let setup_state = state.clone();
    let single_instance_state = state.clone();
    let builder = tauri::Builder::default();
    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    let builder = builder
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(
            move |app, arguments, _working_directory| {
                join_link::accept_urls(app, &single_instance_state, arguments.iter());
                join_link::show_main_window(app);
            },
        ));
    builder
        .on_page_load(|webview, payload| {
            if should_show_main_window(webview.label(), payload.event()) {
                join_link::show_main_window(webview.app_handle());
                diagnostics::record("info", "runtime", "main window finished loading");
            }
        })
        .plugin(tauri_plugin_deep_link::init())
        .manage(state)
        .manage(updater_state)
        .setup(move |app| {
            client_launcher::recover_stale(app.handle()).map_err(std::io::Error::other)?;
            let app_data_dir = app.path().app_data_dir().map_err(std::io::Error::other)?;
            diagnostics::configure(&app_data_dir).map_err(std::io::Error::other)?;
            let history_path = app_data_dir.join("usage-history.jsonl");
            usage_history::prepare(&history_path).map_err(std::io::Error::other)?;
            let mut runtime = setup_state
                .inner
                .lock()
                .map_err(|_| std::io::Error::other("运行状态暂时不可用"))?;
            runtime.usage_history_path = Some(history_path);
            runtime.account_pool_path = Some(app_data_dir.join("accounts.json"));
            let ride_history_path = app_data_dir.join("ride-history.json");
            let ride_history_recovered =
                ride_history::RideHistoryStore::new(ride_history_path.clone())
                    .prepare()
                    .map_err(std::io::Error::other)?;
            runtime.ride_history_path = Some(ride_history_path);
            let route_state_recovered = runtime
                .account_router
                .configure(app_data_dir.join("account-route-state.json"))
                .map_err(std::io::Error::other)?;
            drop(runtime);
            if route_state_recovered {
                diagnostics::record(
                    "warn",
                    "account-router",
                    "damaged route health state was quarantined and reset",
                );
            }
            if ride_history_recovered {
                diagnostics::record(
                    "warn",
                    "ride-history",
                    "damaged ride history was quarantined and reset",
                );
            }
            let route_flush_state = setup_state.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if let Ok(mut runtime) = route_flush_state.inner.lock() {
                        if let Err(error) = runtime.account_router.flush() {
                            diagnostics::record(
                                "error",
                                "account-router",
                                format!("failed to persist route health state: {error}"),
                            );
                        }
                    }
                }
            });
            let credential_refresh_app = app.handle().clone();
            let credential_refresh_state = setup_state.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                loop {
                    let app = credential_refresh_app.clone();
                    let state = credential_refresh_state.clone();
                    match tauri::async_runtime::spawn_blocking(move || {
                        commands::refresh_known_local_accounts(&app, &state)
                    })
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) if error.starts_with("未找到本机") => {}
                        Ok(Err(error)) => diagnostics::record(
                            "warn",
                            "account-pool",
                            format!("periodic official client credential check failed: {error}"),
                        ),
                        Err(error) => diagnostics::record(
                            "warn",
                            "account-pool",
                            format!("periodic credential check stopped unexpectedly: {error}"),
                        ),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            });
            tauri::async_runtime::block_on(
                RelayBridge::global().set_app_handle(app.handle().clone()),
            );
            local_proxy::start(setup_state.clone()).map_err(std::io::Error::other)?;
            diagnostics::record("info", "local-proxy", "local relay proxy started");
            match status_tray::setup(app) {
                Ok(()) => {
                    status_tray::spawn_refresh_loop(app.handle().clone(), setup_state.clone())
                }
                Err(error) => diagnostics::record(
                    "warn",
                    "status-tray",
                    format!("system status tray is unavailable: {error}"),
                ),
            }

            #[cfg(any(windows, target_os = "linux"))]
            if let Err(error) = app.deep_link().register_all() {
                diagnostics::record(
                    "warn",
                    "deep-link",
                    format!("runtime deep-link registration is unavailable: {error}"),
                );
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
            list_accounts,
            import_local_accounts,
            import_accounts,
            preview_account_import,
            commit_account_import,
            cancel_account_import,
            export_account_backup,
            preview_account_restore,
            commit_account_restore,
            cancel_account_restore,
            update_account,
            retry_account_route,
            delete_account,
            install_tool,
            cancel_tool_install,
            check_app_update,
            check_signed_app_update,
            download_app_update,
            install_app_update,
            restart_after_app_update,
            open_releases_page,
            start_car,
            stop_car,
            suspend_car,
            get_active_car,
            list_ride_history,
            resume_host_car,
            resume_passenger_ride,
            refresh_account_quotas,
            update_member_token_limits,
            get_shared_car_status,
            preview_invite,
            join_car,
            leave_car,
            launch_tool,
            list_client_instances,
            focus_client_instance,
            close_client_instance,
            get_ice_servers,
            confirm_passenger_link,
            send_webrtc_signal,
            poll_webrtc_signals,
            execute_relay_request,
            start_relay_request,
            submit_relay_response,
            submit_relay_stream_event,
            get_debug_logs,
            clear_debug_logs,
            record_frontend_log,
            open_debug_log_directory,
            export_diagnostic_bundle,
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
                if let Some(state) = app.try_state::<RuntimeState>() {
                    if let Ok(mut runtime) = state.inner.lock() {
                        if let Err(error) = runtime.account_router.flush() {
                            diagnostics::record(
                                "error",
                                "account-router",
                                format!("failed to flush route health state on exit: {error}"),
                            );
                        }
                    }
                }
                if let Err(error) = client_launcher::recover_stale(app) {
                    diagnostics::record(
                        "error",
                        "client-launcher",
                        format!("failed to restore desktop client configuration on exit: {error}"),
                    );
                }
            }
        });
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn main_window_presents_a_dark_boot_surface_immediately() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri config");
        let window = &config["app"]["windows"][0];
        assert_eq!(window["visible"], true);
        assert_eq!(window["backgroundColor"], "#0f1115");

        let html = include_str!("../../index.html");
        assert!(html.contains("id=\"boot-splash\""));
        assert!(html.contains("正在安全启动"));

        assert!(!should_show_main_window("main", PageLoadEvent::Started));
        assert!(!should_show_main_window(
            "secondary",
            PageLoadEvent::Finished
        ));
        assert!(should_show_main_window("main", PageLoadEvent::Finished));
    }
}
