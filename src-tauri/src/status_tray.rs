use crate::join_link::show_main_window;
use crate::runtime::RuntimeState;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{App, AppHandle, Manager};

const TRAY_ID: &str = "trusted-carpool-status";
const STATUS_ITEM_ID: &str = "status";
const SHOW_ITEM_ID: &str = "show";
const QUIT_ITEM_ID: &str = "quit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Hosting { occupied: usize, total: usize },
    Riding,
    HostingAndRiding { occupied: usize, total: usize },
}

impl Status {
    fn title(&self) -> String {
        match self {
            Self::Idle => "空闲".to_string(),
            Self::Hosting { occupied, total } => format!("发车中 {occupied}/{total}"),
            Self::Riding => "已上车".to_string(),
            Self::HostingAndRiding { occupied, total } => {
                format!("发车 {occupied}/{total} · 已上车")
            }
        }
    }

    fn menu_text(&self) -> String {
        format!("当前状态：{}", self.title())
    }

    fn tooltip(&self) -> String {
        format!("可信拼车 · {}", self.title())
    }
}

pub struct TrayState {
    tray: TrayIcon,
    status_item: MenuItem<tauri::Wry>,
    quitting: AtomicBool,
}

pub fn status_from_runtime(state: &RuntimeState) -> Status {
    let Ok(runtime) = state.inner.lock() else {
        return Status::Idle;
    };
    let hosting = runtime.active_car.as_ref().map(|car| {
        let occupied = car
            .seats
            .iter()
            .filter(|seat| seat.nickname.is_some())
            .count();
        (occupied, car.seats.len())
    });
    let riding = !runtime.accesses.is_empty();
    match (hosting, riding) {
        (Some((occupied, total)), true) => Status::HostingAndRiding { occupied, total },
        (Some((occupied, total)), false) => Status::Hosting { occupied, total },
        (None, true) => Status::Riding,
        (None, false) => Status::Idle,
    }
}

pub fn setup(app: &App) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, STATUS_ITEM_ID, "当前状态：空闲", false, None::<&str>)?;
    let show = MenuItem::with_id(app, SHOW_ITEM_ID, "打开可信拼车", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, QUIT_ITEM_ID, "退出可信拼车", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&status, &show, &separator, &quit])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .title("空闲")
        .tooltip("可信拼车 · 空闲")
        .on_menu_event(|app, event| match event.id.as_ref() {
            SHOW_ITEM_ID => show_main_window(app),
            QUIT_ITEM_ID => {
                if let Some(tray_state) = app.try_state::<TrayState>() {
                    tray_state.quitting.store(true, Ordering::Relaxed);
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    app.manage(TrayState {
        tray,
        status_item: status,
        quitting: AtomicBool::new(false),
    });
    Ok(())
}

pub fn update(app: &AppHandle, runtime: &RuntimeState) {
    let status = status_from_runtime(runtime);
    let Some(tray_state) = app.try_state::<TrayState>() else {
        return;
    };
    let _ = tray_state.tray.set_title(Some(status.title()));
    let _ = tray_state.tray.set_tooltip(Some(status.tooltip()));
    let _ = tray_state.status_item.set_text(status.menu_text());
}

pub fn spawn_refresh_loop(app: AppHandle, runtime: RuntimeState) {
    tauri::async_runtime::spawn(async move {
        loop {
            update(&app, &runtime);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
}

pub fn should_hide_on_close(app: &AppHandle) -> bool {
    app.try_state::<TrayState>()
        .map(|state| !state.quitting.load(Ordering::Relaxed))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        CarSession, ConnectionState, JoinPreview, MemberTokenLimitStatus, MemberTokenLimits,
        RideAccess, Seat, SeatState, SeatUsageSummary, ToolKind,
    };

    fn car(occupied: usize) -> CarSession {
        CarSession {
            car_id: "car-id".to_string(),
            car_name: "熟人车队".to_string(),
            owner_peer_id: "owner".to_string(),
            started_at: 1,
            expires_at: 2,
            enabled_tools: vec![ToolKind::Claude],
            seats: (0_usize..4)
                .map(|index| Seat {
                    seat_no: index as u8 + 1,
                    code: format!("CODE{index}"),
                    nickname: (index < occupied).then(|| format!("成员{index}")),
                    state: if index < occupied {
                        SeatState::Connected
                    } else {
                        SeatState::Waiting
                    },
                    tool: None,
                    usage: SeatUsageSummary::default(),
                    token_limits: MemberTokenLimits::default(),
                    token_limit_status: MemberTokenLimitStatus::default(),
                    token_usage_events: Vec::new(),
                })
                .collect(),
            account_quotas: Vec::new(),
        }
    }

    #[test]
    fn derives_idle_and_hosting_status() {
        let runtime = RuntimeState::default();
        assert_eq!(status_from_runtime(&runtime), Status::Idle);
        runtime.inner.lock().unwrap().active_car = Some(car(3));
        assert_eq!(
            status_from_runtime(&runtime),
            Status::Hosting {
                occupied: 3,
                total: 4
            }
        );
    }

    #[test]
    fn derives_riding_and_combined_status() {
        let runtime = RuntimeState::default();
        runtime.inner.lock().unwrap().accesses.insert(
            "access-id".to_string(),
            RideAccess {
                preview: JoinPreview {
                    car_id: "other-car".to_string(),
                    car_name: "好友车队".to_string(),
                    owner_label: "好友".to_string(),
                    seat_no: 1,
                    enabled_tools: vec![ToolKind::Codex],
                    starts_at: 1,
                    expires_at: 2,
                },
                access_id: "access-id".to_string(),
                owner_peer_id: "other-owner".to_string(),
                local_proxy_port: 25_342,
                connection_state: ConnectionState::Connected,
            },
        );
        assert_eq!(status_from_runtime(&runtime), Status::Riding);
        runtime.inner.lock().unwrap().active_car = Some(car(2));
        assert_eq!(
            status_from_runtime(&runtime),
            Status::HostingAndRiding {
                occupied: 2,
                total: 4
            }
        );
    }
}
