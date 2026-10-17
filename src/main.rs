#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod doh;
mod icon;
mod icon_data;
mod proxy;
mod state;
mod system_proxy;

use std::sync::Mutex;

use tao::event_loop::{ControlFlow, EventLoop};

use tray_icon::menu::{
    self, CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::state::{EvasionMode, PROVIDERS, Settings};

#[cfg(target_os = "macos")]
mod update;

pub(crate) static PROXY: Mutex<Option<proxy::Handle>> = Mutex::new(None);

/// Drain the proxy and clear the system proxy setting before the process
/// exits (update restart path).
pub(crate) fn prepare_shutdown() {
    let _ = system_proxy::disable();
    if let Some(h) = PROXY.lock().unwrap().take() {
        h.stop();
    }
}

fn main() {
    let settings = state::load();
    state::set_live(&settings.provider, settings.evasion);

    #[cfg(target_os = "macos")]
    let event_loop = {
        use tao::platform::macos::EventLoopExtMacOS;
        let mut el = EventLoop::new();
        // Run as an "accessory" app: menu-bar only, never in the Dock (the
        // packaged .app also declares LSUIElement for belt and braces).
        el.set_activation_policy(tao::platform::macos::ActivationPolicy::Accessory);
        el
    };
    #[cfg(not(target_os = "macos"))]
    let event_loop = EventLoop::new();

    let mut tray = build_tray(&settings);

    // Resume proxying automatically if it was enabled last session.
    if settings.enabled {
        apply_enabled(&mut tray.ui, true);
    }

    let menu_rx = MenuEvent::receiver();
    event_loop.run(move |_event, _, control_flow| {
        if *control_flow == ControlFlow::Exit {
            return;
        }
        *control_flow = ControlFlow::Wait;
        while let Ok(event) = menu_rx.try_recv() {
            if handle_menu(&mut tray.ui, &event.id) {
                *control_flow = ControlFlow::Exit;
            }
        }
    });
}

struct TrayUi {
    /// Keeps the tray icon alive; the handle itself is retained by the OS.
    #[allow(dead_code)]
    tray: TrayIcon,
    toggle: CheckMenuItem,
    autostart: CheckMenuItem,
    #[cfg(target_os = "macos")]
    update: MenuItem,
    quit: MenuItem,
    provider_items: Vec<CheckMenuItem>,
    evasion_items: Vec<CheckMenuItem>,
    enabled: bool,
}

struct Tray {
    ui: TrayUi,
}

fn build_tray(settings: &Settings) -> Tray {
    let menu = Menu::new();

    let toggle = CheckMenuItem::new(
        "Enabled",
        true,
        settings.enabled,
        None::<menu::accelerator::Accelerator>,
    );
    menu.append(&toggle).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();

    let dns_sub = Submenu::new("DNS Provider", true);
    let provider_items: Vec<CheckMenuItem> = PROVIDERS
        .iter()
        .map(|p| {
            CheckMenuItem::new(
                p.name,
                true,
                p.id == settings.provider,
                None::<menu::accelerator::Accelerator>,
            )
        })
        .collect();
    for item in &provider_items {
        dns_sub.append(item).unwrap();
    }
    menu.append(&dns_sub).unwrap();

    let ev_sub = Submenu::new("Evasion", true);
    let evasion_items: Vec<CheckMenuItem> = EvasionMode::all()
        .iter()
        .map(|m| {
            CheckMenuItem::new(
                m.label(),
                true,
                *m == settings.evasion,
                None::<menu::accelerator::Accelerator>,
            )
        })
        .collect();
    for item in &evasion_items {
        ev_sub.append(item).unwrap();
    }
    menu.append(&ev_sub).unwrap();

    menu.append(&PredefinedMenuItem::separator()).unwrap();
    let autostart_item = CheckMenuItem::new(
        "Start on startup",
        true,
        settings.autostart,
        None::<menu::accelerator::Accelerator>,
    );
    menu.append(&autostart_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();

    #[cfg(target_os = "macos")]
    let update_item = MenuItem::new(
        "Check for updates…",
        true,
        None::<menu::accelerator::Accelerator>,
    );
    #[cfg(target_os = "macos")]
    menu.append(&update_item).unwrap();

    let quit = MenuItem::new("Quit", true, None::<menu::accelerator::Accelerator>);
    menu.append(&quit).unwrap();

    let (rgba, size) = icon::rgba();
    let icon = Icon::from_rgba(rgba, size, size).expect("icon");

    let tray = TrayIconBuilder::new()
        .with_id("not-a-vpn")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(true)
        .with_icon(icon)
        .with_tooltip("not-a-vpn")
        .build()
        .expect("tray");

    Tray {
        ui: TrayUi {
            tray,
            toggle,
            autostart: autostart_item,
            #[cfg(target_os = "macos")]
            update: update_item,
            quit,
            provider_items,
            evasion_items,
            enabled: settings.enabled,
        },
    }
}

/// Returns true when the app should exit.
fn handle_menu(ui: &mut TrayUi, id: &MenuId) -> bool {
    let mut settings = state::load();

    if id == ui.toggle.id() {
        apply_enabled(ui, !ui.enabled);
        settings.enabled = ui.enabled;
        state::save(&settings);
        return false;
    }

    if id == ui.autostart.id() {
        let want = !settings.autostart;
        match autostart::set(want) {
            Ok(()) => {
                settings.autostart = want;
                state::save(&settings);
                ui.autostart.set_checked(want);
            }
            Err(e) => {
                eprintln!("autostart: {e}");
                ui.autostart.set_checked(settings.autostart);
            }
        }
        return false;
    }

    if id == ui.quit.id() {
        if ui.enabled {
            let _ = system_proxy::disable();
        }
        let _ = PROXY.lock().unwrap().take().map(|h| h.stop());
        return true;
    }

    #[cfg(target_os = "macos")]
    if id == ui.update.id() {
        update::spawn_check();
        return false;
    }

    for (i, item) in ui.provider_items.iter().enumerate() {
        if id == item.id() {
            settings.provider = PROVIDERS[i].id.to_string();
            state::set_live(PROVIDERS[i].id, settings.evasion);
            state::save(&settings);
            for (j, other) in ui.provider_items.iter().enumerate() {
                other.set_checked(j == i);
            }
            return false;
        }
    }

    for (i, item) in ui.evasion_items.iter().enumerate() {
        if id == item.id() {
            let mode = EvasionMode::all()[i];
            settings.evasion = mode;
            state::set_live(&settings.provider, mode);
            state::save(&settings);
            for (j, other) in ui.evasion_items.iter().enumerate() {
                other.set_checked(j == i);
            }
            return false;
        }
    }

    false
}

/// Start/stop the local proxy and the OS-level redirection, keeping the menu
/// state truthful about what actually happened.
fn apply_enabled(ui: &mut TrayUi, want: bool) {
    let mut guard = PROXY.lock().unwrap();
    let outcome = if want {
        if guard.is_some() {
            Ok(true)
        } else {
            match proxy::start() {
                Ok(handle) => match system_proxy::enable(handle.port()) {
                    Ok(()) => {
                        *guard = Some(handle);
                        Ok(true)
                    }
                    Err(e) => {
                        handle.stop();
                        eprintln!("system proxy: {e}");
                        Err(())
                    }
                },
                Err(e) => {
                    eprintln!("proxy: {e}");
                    Err(())
                }
            }
        }
    } else {
        let _ = system_proxy::disable();
        if let Some(h) = guard.take() {
            h.stop();
        }
        Ok(false)
    };
    drop(guard);

    match outcome {
        Ok(on) => {
            ui.enabled = on;
            ui.toggle.set_checked(on);
            let mut settings = state::load();
            settings.enabled = on;
            state::save(&settings);
        }
        Err(()) => {
            ui.enabled = false;
            ui.toggle.set_checked(false);
        }
    }
}
