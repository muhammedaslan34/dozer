//! Tray icon. Its events only set flags and wake the UI, which acts on them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::APP_NAME;

#[derive(Default)]
pub struct TrayFlags {
    pub open: AtomicBool,
    pub widget: AtomicBool,
    pub stop_all: AtomicBool,
    pub quit: AtomicBool,
}

pub fn create(flags: Arc<TrayFlags>, ctx: eframe::egui::Context) -> Option<TrayIcon> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Dozer", true, None);
    let widget = MenuItem::with_id("widget", "Show / hide floating widget", true, None);
    let stop = MenuItem::with_id("stop_all", "Stop all on-demand servers", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    menu.append_items(&[&open, &widget, &stop, &PredefinedMenuItem::separator(), &quit]).ok()?;

    {
        let flags = flags.clone();
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            match event.id.0.as_str() {
                "open" => flags.open.store(true, Ordering::Relaxed),
                "widget" => flags.widget.store(true, Ordering::Relaxed),
                "stop_all" => flags.stop_all.store(true, Ordering::Relaxed),
                "quit" => flags.quit.store(true, Ordering::Relaxed),
                _ => {}
            }
            ctx.request_repaint();
        }));
    }
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let open = matches!(
            event,
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
        );
        if open {
            flags.open.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        }
    }));

    let icon = tray_icon::Icon::from_rgba(crate::icon::rgba(32), 32, 32).ok();
    let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_menu_on_left_click(false).with_tooltip(APP_NAME);
    if let Some(icon) = icon {
        builder = builder.with_icon(icon);
    }
    builder.build().inspect_err(|e| log::error!("cannot create tray icon: {e}")).ok()
}
