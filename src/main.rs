//! Dozer: shows what uses your RAM, lets you end it safely, and runs MCP
//! servers only when an agent actually needs them.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod child;
mod claude;
mod config;
mod gateway;
mod icon;
mod monitor;
mod sessions;
mod tray;
mod ui;
mod winutil;

use eframe::egui;

use config::{APP_NAME, Config};

fn main() -> anyhow::Result<()> {
    let start_hidden = std::env::args().any(|a| a == "--hidden");

    let mut config = Config::load();
    if config.servers.is_empty() && matches!(config.import_from_claude(), Ok(added) if !added.is_empty()) {
        let _ = config.save();
    }
    let port = config.port;

    // One Dozer per user: a second launch just shows the first one's window.
    let server = match gateway::bind(port) {
        Ok(s) => s,
        Err(err) => {
            if gateway::ask_running_instance_to_show(port) {
                return Ok(());
            }
            message_box(&format!("{APP_NAME} cannot start its gateway.\n\n{err}"));
            return Err(err);
        }
    };

    let hub = gateway::Hub::new(config.clone());
    hub.preload_caches();
    gateway::run(hub.clone(), server);

    let ui_hub = hub.clone();
    let monitor = monitor::Monitor::start(config.servers.clone(), move || ui_hub.repaint());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1060.0, 720.0])
            .with_min_inner_size([760.0, 480.0])
            .with_visible(!start_hidden)
            .with_icon(egui::IconData { rgba: icon::rgba(64), width: 64, height: 64 }),
        ..Default::default()
    };
    let exit_hub = hub.clone();
    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, hub, monitor, start_hidden)))),
    );
    exit_hub.stop_all();
    result.map_err(|e| anyhow::anyhow!("{e}"))
}

fn launcher() -> anyhow::Result<auto_launch::AutoLaunch> {
    let exe = std::env::current_exe()?;
    Ok(auto_launch::AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(&exe.to_string_lossy())
        .set_args(&["--hidden"])
        .set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser)
        .build()?)
}

pub fn autostart_enabled() -> bool {
    launcher().and_then(|l| Ok(l.is_enabled()?)).unwrap_or(false)
}

pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    let l = launcher()?;
    if on { l.enable()? } else { l.disable()? }
    Ok(())
}

fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (t, c) = (wide(text), wide(APP_NAME));
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONERROR);
    }
}
