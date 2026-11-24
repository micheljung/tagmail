//! OS integration of the paired app: tray with the unread count, badge,
//! notifications, mailto and pairing links, single instance, autostart and
//! close-to-tray.

use std::sync::Arc;

use tagmail_api_core::desktop_dto::DesktopSettingsDto;
use tagmail_desktop::desktop::Desktop;
use tagmail_desktop::mailto::{LinkQueue, parse_link};
use tagmail_desktop::{notify, tray};
use tagmail_replica::ReplicaEvent;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_notification::NotificationExt;
use tokio::sync::broadcast::error::RecvError;

pub const WINDOW: &str = "main";
const TRAY: &str = "main";
const PAIR_SCHEME: &str = "tagmail";

const TRAY_ICONS: [&[u8]; 11] = [
    include_bytes!("../icons/tray/0.png"),
    include_bytes!("../icons/tray/1.png"),
    include_bytes!("../icons/tray/2.png"),
    include_bytes!("../icons/tray/3.png"),
    include_bytes!("../icons/tray/4.png"),
    include_bytes!("../icons/tray/5.png"),
    include_bytes!("../icons/tray/6.png"),
    include_bytes!("../icons/tray/7.png"),
    include_bytes!("../icons/tray/8.png"),
    include_bytes!("../icons/tray/9.png"),
    include_bytes!("../icons/tray/10.png"),
];

fn tray_icon(unread: i64) -> Image<'static> {
    Image::from_bytes(TRAY_ICONS[tray::icon_index(unread)]).expect("tray icons are valid PNGs")
}

/// Plugins that must exist before setup; single-instance comes first so a
/// second launch hands over its arguments and exits.
pub fn plugins(builder: tauri::Builder<Wry>) -> tauri::Builder<Wry> {
    builder
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show(app);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
}

pub fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn open_links(app: &AppHandle, links: &LinkQueue, urls: impl IntoIterator<Item = String>) {
    let mut any = false;
    for url in urls {
        match parse_link(&url) {
            Some(link) => {
                any = true;
                links.push(link);
            }
            None => tracing::debug!(%url, "ignoring an unknown link"),
        }
    }
    if any {
        show(app);
    }
}

pub fn install(
    app: &tauri::App,
    desktop: Arc<Desktop>,
    links: Arc<LinkQueue>,
) -> tauri::Result<()> {
    let handle = app.handle().clone();
    let deep_links = app.deep_link();
    if let Err(e) = deep_links.register(PAIR_SCHEME) {
        tracing::debug!(error = %e, "registering tagmail:// failed");
    }
    let started_with = deep_links.get_current().ok().flatten().unwrap_or_default();
    open_links(&handle, &links, started_with.into_iter().map(String::from));
    {
        let handle = handle.clone();
        let links = Arc::clone(&links);
        deep_links.on_open_url(move |event| {
            open_links(&handle, &links, event.urls().into_iter().map(String::from));
        });
    }

    let open = MenuItem::with_id(app, "open", "Open Tagmail", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let tray = TrayIconBuilder::with_id(TRAY)
        .icon(tray_icon(0))
        .tooltip(tray::tooltip(0))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show(tray.app_handle());
            }
        })
        .build(app)?;

    tauri::async_runtime::spawn(count_unread(handle.clone(), Arc::clone(&desktop), tray));
    tauri::async_runtime::spawn(notify_new_mail(handle.clone(), Arc::clone(&desktop)));
    tauri::async_runtime::spawn(apply_settings(handle, desktop));
    Ok(())
}

/// Hides instead of closing when the setting says so.
pub fn on_window_event(window: &tauri::Window, event: &WindowEvent, desktop: &Desktop) {
    if let WindowEvent::CloseRequested { api, .. } = event
        && desktop.settings_changes().borrow().close_to_tray
    {
        api.prevent_close();
        let _ = window.hide();
    }
}

async fn count_unread(app: AppHandle, desktop: Arc<Desktop>, tray: TrayIcon) {
    let mut changes = desktop.changes();
    let mut shown = None;
    loop {
        let unread = desktop.unread_inbox().await.unwrap_or(0);
        if shown != Some(unread) {
            shown = Some(unread);
            let _ = tray.set_icon(Some(tray_icon(unread)));
            let _ = tray.set_tooltip(Some(tray::tooltip(unread)));
            if let Some(window) = app.get_webview_window(WINDOW) {
                #[cfg(not(windows))]
                let _ = window.set_badge_count((unread > 0).then_some(unread));
                #[cfg(windows)]
                let _ = window.set_overlay_icon((unread > 0).then(|| tray_icon(unread)));
            }
            #[cfg(feature = "test-hooks")]
            println!("DESKTOP_TRAY {unread}");
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}

async fn notify_new_mail(app: AppHandle, desktop: Arc<Desktop>) {
    let mut events = desktop.subscribe();
    loop {
        let arrivals = match events.recv().await {
            Ok(ReplicaEvent::NewMail(arrivals)) => arrivals,
            Ok(_) => continue,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let settings: DesktopSettingsDto = desktop.settings_changes().borrow().clone();
        if !settings.notify {
            continue;
        }
        for n in notify::plan(&arrivals, settings.notify_content) {
            if let Err(e) = app
                .notification()
                .builder()
                .title(&n.title)
                .body(&n.body)
                .show()
            {
                tracing::warn!(error = %e, "showing a notification failed");
            }
        }
    }
}

async fn apply_settings(app: AppHandle, desktop: Arc<Desktop>) {
    let mut settings = desktop.settings_changes();
    let mut autostart = settings.borrow_and_update().autostart;
    while settings.changed().await.is_ok() {
        let wanted = settings.borrow_and_update().autostart;
        if wanted == autostart {
            continue;
        }
        autostart = wanted;
        let manager = app.autolaunch();
        let result = if wanted {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "changing autostart failed");
        }
    }
}
