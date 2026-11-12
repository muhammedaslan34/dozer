//! The dashboard window.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, RichText};

use crate::config::{Mode, ServerConfig};
use crate::gateway::{Hub, ServerView};
use crate::monitor::{Category, Group, Monitor, Safety, Snapshot};
use crate::sessions::AgentSession;
use crate::tray::TrayFlags;
use crate::winutil::end_tree;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Top,
    Sessions,
    Memory,
    OnDemand,
    Activity,
}

enum Confirm {
    End { what: String, pids: Vec<u32>, memory: u64, force: bool, safety: Safety },
    Route { names: Vec<String> },
    Restore { names: Vec<String> },
}

pub struct App {
    hub: Arc<Hub>,
    monitor: Arc<Monitor>,
    flags: Arc<TrayFlags>,
    tray: Option<tray_icon::TrayIcon>,
    tab: Tab,
    selected: HashSet<String>,
    confirm: Option<Confirm>,
    /// Messages from background jobs (Claude config changes).
    notice: Arc<Mutex<Option<(String, bool)>>>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    autostart: bool,
    quitting: bool,
    last_tooltip: Instant,
    main_visible: bool,
    /// eframe shows the window after its first frame even when asked not to; hide it then.
    hide_on_start: bool,
    widget_on: bool,
    widget_pos: Option<egui::Pos2>,
    widget_checked: bool,
    widget_shared: Arc<WidgetShared>,
    remote: Vec<(String, String)>,
    remote_read: Option<Instant>,
    /// Sessions already ended by auto-close, so each is ended once.
    auto_closed: HashSet<u32>,
    last_save: Instant,
}

const TEAL: Color32 = Color32::from_rgb(13, 148, 136);
const AMBER: Color32 = Color32::from_rgb(217, 119, 6);
const RED: Color32 = Color32::from_rgb(220, 38, 38);

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, hub: Arc<Hub>, monitor: Arc<Monitor>, start_hidden: bool) -> Self {
        let ctx = cc.egui_ctx.clone();
        *hub.ui_ctx.lock().unwrap() = Some(ctx.clone());
        ctx.set_visuals(egui::Visuals::dark());
        let flags = Arc::new(TrayFlags::default());
        let tray = crate::tray::create(flags.clone(), ctx);
        let (widget_on, widget_pos) = {
            let c = hub.config.read().unwrap();
            (c.widget, c.widget_pos.map(|p| egui::pos2(p[0], p[1])))
        };
        monitor.fast.store(!start_hidden || widget_on, Ordering::Relaxed);
        let selected = hub
            .config
            .read()
            .unwrap()
            .servers
            .iter()
            .filter(|s| !s.routed && suggested_for_routing(s))
            .map(|s| s.name.clone())
            .collect();
        let widget_shared = Arc::new(WidgetShared {
            monitor: monitor.clone(),
            actions: Mutex::default(),
            pos: Mutex::default(),
            limit_secs: std::sync::atomic::AtomicU64::new(3600),
            frames: std::sync::atomic::AtomicU32::new(0),
        });
        Self {
            hub,
            monitor,
            flags,
            tray,
            tab: match std::env::args().find_map(|a| a.strip_prefix("--tab=").map(String::from)).as_deref() {
                Some("type") => Tab::Memory,
                Some("sessions") => Tab::Sessions,
                Some("mcp") => Tab::OnDemand,
                Some("activity") => Tab::Activity,
                _ => Tab::Top,
            },
            selected,
            confirm: None,
            notice: Arc::default(),
            busy: Arc::default(),
            autostart: crate::autostart_enabled(),
            quitting: false,
            last_tooltip: Instant::now() - Duration::from_secs(60),
            main_visible: !start_hidden,
            hide_on_start: start_hidden,
            widget_on: widget_on || std::env::args().any(|a| a == "--widget"),
            widget_pos,
            widget_checked: false,
            widget_shared,
            remote: Vec::new(),
            remote_read: None,
            auto_closed: HashSet::new(),
            last_save: Instant::now(),
        }
    }

    fn save_widget_state(&self) {
        let mut cfg = self.hub.config.write().unwrap();
        cfg.widget = self.widget_on;
        cfg.widget_pos = self.widget_pos.map(|p| [p.x, p.y]);
        let _ = cfg.save();
    }

    fn session_limit(&self) -> Duration {
        Duration::from_secs(u64::from(self.hub.config.read().unwrap().session_idle_minutes.max(1)) * 60)
    }

    /// Ends sessions idle past the limit, if the user turned that on.
    fn auto_close_sessions(&mut self, snap: &Snapshot) {
        if !self.hub.config.read().unwrap().auto_close_sessions {
            return;
        }
        let limit = self.session_limit();
        for s in snap.sessions.iter().filter(|s| s.is_idle(limit)) {
            if self.auto_closed.insert(s.pid) {
                let (pid, hub) = (s.pid, self.hub.clone());
                let text = format!("closed {} in {} after {} idle", s.app, s.project, mins(s.idle));
                std::thread::spawn(move || {
                    end_tree(pid, true);
                    hub.log("sessions", text);
                });
            }
        }
    }

    fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.monitor.fast.store(true, Ordering::Relaxed);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.main_visible = false;
        self.monitor.fast.store(self.widget_on, Ordering::Relaxed);
    }

    fn set_notice(&self, text: impl Into<String>, ok: bool) {
        *self.notice.lock().unwrap() = Some((text.into(), ok));
    }
}

/// FleetAI's servers identify the calling agent by how they were started, so leave them direct.
fn suggested_for_routing(s: &ServerConfig) -> bool {
    !s.name.to_lowercase().starts_with("fleetai")
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_widget_actions(ctx);
        if self.hide_on_start {
            self.hide_on_start = false;
            self.hide_window(ctx);
        }
        if self.flags.open.swap(false, Ordering::Relaxed) || self.hub.show_requested.swap(false, Ordering::Relaxed) {
            self.show_window(ctx);
            self.main_visible = true;
        }
        if self.flags.widget.swap(false, Ordering::Relaxed) {
            self.widget_on = !self.widget_on;
            self.monitor.fast.store(self.widget_on || self.main_visible, Ordering::Relaxed);
            self.save_widget_state();
        }
        // Keep drawing the widget while the main window is hidden.
        if self.widget_on && !self.main_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
        }
        if self.last_save.elapsed() > Duration::from_secs(15) {
            self.last_save = Instant::now();
            let saved = self.hub.config.read().unwrap().widget_pos;
            if self.widget_pos.map(|p| [p.x, p.y]) != saved {
                self.save_widget_state();
            }
        }
        let snap_for_close = self.monitor.snapshot();
        self.auto_close_sessions(&snap_for_close);
        if self.flags.stop_all.swap(false, Ordering::Relaxed) {
            self.hub.stop_all();
        }
        if self.flags.quit.swap(false, Ordering::Relaxed) {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            // Closing the window keeps Dozer running in the tray; the gateway must stay up.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }
        if self.last_tooltip.elapsed() > Duration::from_secs(5)
            && let Some(tray) = &self.tray
        {
            self.last_tooltip = Instant::now();
            let snap = self.monitor.snapshot();
            let m = snap.memory;
            if m.total > 0 {
                let used = m.total - m.available;
                let running: usize = self.hub.views().iter().map(|v| v.running.len()).sum();
                let _ = tray.set_tooltip(Some(format!(
                    "Dozer — RAM {:.0}% ({} of {})\n{} on-demand server process(es) running",
                    used as f64 * 100.0 / m.total as f64,
                    gb(used),
                    gb(m.total),
                    running
                )));
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_secs(1));
        let snap = self.monitor.snapshot();
        if self.widget_on {
            self.widget(&ctx);
        }
        if !self.main_visible {
            return;
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            self.header(ui, &snap);
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Top, RichText::new("Top usage").size(15.0));
                let idle = snap.sessions.iter().filter(|s| s.is_idle(self.session_limit())).count();
                let label = if idle > 0 { format!("Agent sessions ({idle} idle)") } else { "Agent sessions".to_string() };
                ui.selectable_value(&mut self.tab, Tab::Sessions, RichText::new(label).size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Memory, RichText::new("By type").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::OnDemand, RichText::new("MCP on demand").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Activity, RichText::new("Activity").size(15.0));
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            if let Some((text, ok)) = self.notice.lock().unwrap().clone() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&text).color(if ok { TEAL } else { RED }));
                    if ui.small_button("Dismiss").clicked() {
                        *self.notice.lock().unwrap() = None;
                    }
                });
                ui.separator();
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.tab {
                Tab::Top => self.top_tab(ui, &snap),
                Tab::Sessions => self.sessions_tab(ui, &snap),
                Tab::Memory => self.memory_tab(ui, &snap),
                Tab::OnDemand => self.on_demand_tab(ui, &snap),
                Tab::Activity => self.activity_tab(ui),
            });
        });

        self.confirm_dialog(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_widget_state();
        self.hub.stop_all();
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let m = snap.memory;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Dozer").size(20.0).strong());
            ui.add_space(12.0);
            if m.total > 0 {
                let used = m.total - m.available;
                let frac = used as f32 / m.total as f32;
                let color = if frac > 0.9 { RED } else if frac > 0.75 { AMBER } else { TEAL };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(260.0)
                        .fill(color)
                        .text(format!("RAM {} of {} ({:.0}%)", gb(used), gb(m.total), frac * 100.0)),
                );
                let commit_used = m.commit_limit - m.commit_available;
                ui.label(format!("Committed {} of {}", gb(commit_used), gb(m.commit_limit)))
                    .on_hover_text("Memory promised to programs, including the page file. When this fills up, apps start failing.");
                ui.label(format!("· {} processes", snap.process_count));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.free_button(ui, snap));
            } else {
                ui.label("Reading memory…");
            }
        });
    }

    // ---------------------------------------------------------------- memory

    fn memory_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if snap.taken.is_none() {
            ui.spinner();
            return;
        }
        let categories = [
            Category::McpDirect,
            Category::StuckGit,
            Category::Agents,
            Category::DevServers,
            Category::Dozer,
            Category::Browsers,
            Category::Containers,
            Category::Other,
        ];
        for cat in categories {
            let groups: Vec<&Group> = snap.groups.iter().filter(|g| g.category == cat).collect();
            if groups.is_empty() {
                continue;
            }
            let (mem, count) = snap.category_total(cat);
            let title = format!("{}  —  {}  ·  {} processes", cat.title(), gb(mem), count);
            let warn = matches!(cat, Category::McpDirect | Category::StuckGit) && mem > 500 * MB;
            let text = if warn { RichText::new(title).strong().color(AMBER) } else { RichText::new(title).strong() };
            egui::CollapsingHeader::new(text)
                .id_salt(format!("{cat:?}"))
                .default_open(matches!(cat, Category::McpDirect | Category::StuckGit | Category::Agents))
                .show(ui, |ui| {
                    if !cat.hint().is_empty() {
                        ui.label(RichText::new(cat.hint()).weak());
                    }
                    let endable = groups.iter().all(|g| g.safety == Safety::Safe);
                    if endable {
                        let all: Vec<u32> = groups.iter().flat_map(|g| g.roots.iter().copied()).collect();
                        if ui.button(format!("End all ({})", gb(mem))).clicked() {
                            self.end_now(cat.title().to_string(), all, true, mem);
                        }
                        if cat == Category::McpDirect {
                            ui.label(
                                RichText::new("Ending these disconnects those tools in the sessions that started them.")
                                    .small()
                                    .weak(),
                            );
                        }
                    }
                    let limit = if cat == Category::Other { 25 } else { usize::MAX };
                    for g in groups.iter().take(limit) {
                        self.group_row(ui, g);
                    }
                });
        }
    }

    fn group_row(&mut self, ui: &mut egui::Ui, g: &Group) {
        let label = format!("{}   {}   ×{}", g.label, gb(g.memory), g.procs.len());
        egui::CollapsingHeader::new(label).id_salt(format!("{:?}/{}", g.category, g.label)).show(ui, |ui| {
            ui.horizontal(|ui| {
                safety_badge(ui, g);
                self.end_buttons(ui, g);
            });
            egui::Grid::new(format!("procs/{:?}/{}", g.category, g.label)).striped(true).num_columns(4).show(ui, |ui| {
                for p in g.procs.iter().take(60) {
                    ui.label(p.pid.to_string());
                    ui.label(&p.name);
                    ui.label(gb(p.memory));
                    let short: String = p.cmd.chars().take(110).collect();
                    ui.label(RichText::new(short).small().weak()).on_hover_text(&p.cmd);
                    ui.end_row();
                }
            });
        });
    }

    fn top_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if snap.taken.is_none() {
            ui.spinner();
            return;
        }
        ui.horizontal(|ui| {
            let m = snap.memory;
            let (ram, cpu) = snap.history.last().copied().unwrap_or((0.0, 0.0));
            ui.allocate_ui(egui::vec2(110.0, 130.0), |ui| gauge(ui, "RAM", ram, &format!("{} / {}", gb(m.total.saturating_sub(m.available)), gb(m.total))));
            ui.allocate_ui(egui::vec2(110.0, 130.0), |ui| gauge(ui, "CPU", cpu, &format!("{} processes", snap.process_count)));
            ui.vertical(|ui| {
                history_chart(ui, &snap.history, 70.0);
                ui.add_space(4.0);
                ram_breakdown(ui, snap, false);
            });
        });
        ui.separator();
        let mut groups: Vec<&Group> = snap.groups.iter().collect();
        groups.sort_by_key(|g| std::cmp::Reverse(g.memory));
        let safe: u64 = groups.iter().filter(|g| frees_for_good(g)).map(|g| g.memory).sum();
        ui.label(format!(
            "What uses your memory, biggest first. About {} can be freed now without losing anything (rows marked \u{201c}Safe to close\u{201d}).",
            gb(safe)
        ));
        ui.label(
            RichText::new(
                "Close asks the app to quit, like clicking X, so it can save. End task forces it, for programs that ignore Close.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        egui::Grid::new("top").striped(true).num_columns(6).spacing([14.0, 6.0]).show(ui, |ui| {
            for h in ["What", "Type", "Memory", "Processes", "Safe to close?", ""] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for g in groups.iter().take(40) {
                let detail = g
                    .procs
                    .iter()
                    .take(8)
                    .map(|p| format!("{} {} - {}", p.pid, gb(p.memory), p.cmd.chars().take(120).collect::<String>()))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.label(RichText::new(&g.label).strong()).on_hover_text(detail);
                ui.label(g.category.short());
                ui.label(gb(g.memory));
                ui.label(g.procs.len().to_string());
                safety_badge(ui, g);
                ui.horizontal(|ui| self.end_buttons(ui, g));
                ui.end_row();
            }
        });
    }

    /// One click: end everything marked "Safe to close".
    fn free_button(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let safe: Vec<&Group> = snap.groups.iter().filter(|g| frees_for_good(g)).collect();
        let memory: u64 = safe.iter().map(|g| g.memory).sum();
        let pids: Vec<u32> = safe.iter().flat_map(|g| g.roots.iter().copied()).collect();
        let list = safe.iter().map(|g| format!("{}  {}", gb(g.memory), g.label)).collect::<Vec<_>>().join("\n");
        let button = egui::Button::new(RichText::new(format!("Free {} now", gb(memory))).strong().color(Color32::WHITE))
            .fill(Color32::from_rgb(22, 163, 74))
            .min_size(egui::vec2(170.0, 30.0));
        let enabled = !pids.is_empty() && !self.busy.load(Ordering::Relaxed);
        let resp = ui.add_enabled(enabled, button).on_hover_text(format!(
            "Ends everything marked \u{201c}Safe to close\u{201d} in one click. Nothing is lost:\n{list}"
        ));
        if resp.clicked() {
            self.end_now(format!("{} safe item(s)", safe.len()), pids, true, memory);
        }
    }

    fn end_buttons(&mut self, ui: &mut egui::Ui, g: &Group) {
        let allowed = g.safety != Safety::Keep;
        let tip = if allowed { g.reason } else { "Part of Windows or Dozer; ending it is blocked here." };
        for (text, force) in [("Close", false), ("End task", true)] {
            let clicked = ui.add_enabled(allowed, egui::Button::new(text).small()).on_hover_text(tip).on_disabled_hover_text(tip).clicked();
            if !clicked {
                continue;
            }
            if g.safety == Safety::Safe {
                // Nothing to lose: act at once. Background programs have no window, so always force.
                self.end_now(g.label.clone(), g.roots.clone(), true, g.memory);
            } else {
                self.confirm = Some(Confirm::End {
                    what: g.label.clone(),
                    pids: g.roots.clone(),
                    memory: g.memory,
                    force,
                    safety: g.safety,
                });
            }
        }
    }

    /// Ends process trees on a worker thread so the window stays responsive.
    fn end_now(&self, what: String, pids: Vec<u32>, force: bool, memory: u64) {
        let notice = self.notice.clone();
        let busy = self.busy.clone();
        let hub = self.hub.clone();
        busy.store(true, Ordering::Relaxed);
        std::thread::spawn(move || {
            let me = std::process::id();
            let total = pids.len();
            let ended = pids.iter().filter(|&&p| p != me && end_tree(p, force)).count();
            let msg = if ended == total {
                format!("{} {what}, about {} freed.", if force { "Ended" } else { "Asked to close" }, gb(memory))
            } else if !force {
                format!("{ended} of {total} closed. Programs without a window ignore Close; use End task for those.")
            } else {
                format!("Ended {ended} of {total} in {what}. The rest had already exited or need administrator rights.")
            };
            *notice.lock().unwrap() = Some((msg, ended > 0));
            busy.store(false, Ordering::Relaxed);
            hub.repaint();
        });
    }

    // -------------------------------------------------------------- sessions

    fn sessions_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let limit = self.session_limit();
        let idle: Vec<&AgentSession> = snap.sessions.iter().filter(|s| s.is_idle(limit)).collect();
        let idle_mem: u64 = idle.iter().map(|s| s.memory).sum();
        ui.label(format!(
            "{} agent session(s) running. A session is idle when it has not written to its conversation or used CPU for {} min.",
            snap.sessions.len(),
            limit.as_secs() / 60
        ));
        ui.label(
            RichText::new("Closing a Claude desktop session only stops its background process: open the session in Claude again to resume it.")
                .small()
                .weak(),
        );
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let button = egui::Button::new(
                RichText::new(format!("Close {} idle session(s) \u{00b7} free {}", idle.len(), gb(idle_mem))).strong().color(Color32::WHITE),
            )
            .fill(Color32::from_rgb(22, 163, 74));
            if ui.add_enabled(!idle.is_empty(), button).clicked() {
                let pids = idle.iter().map(|s| s.pid).collect();
                self.end_now(format!("{} idle session(s)", idle.len()), pids, true, idle_mem);
            }
            ui.separator();
            let mut cfg = self.hub.config.write().unwrap();
            let mut changed = ui.checkbox(&mut cfg.auto_close_sessions, "Close idle sessions automatically after").changed();
            changed |= ui.add(egui::DragValue::new(&mut cfg.session_idle_minutes).range(5..=1440).suffix(" min")).changed();
            if changed {
                let _ = cfg.save();
            }
        });
        ui.separator();
        if snap.sessions.is_empty() {
            ui.label("No Claude Code, Codex or OpenCode sessions are running.");
            return;
        }
        let sessions = snap.sessions.clone();
        egui::Grid::new("sessions").striped(true).num_columns(7).spacing([14.0, 7.0]).show(ui, |ui| {
            for h in ["Project", "Working on", "App", "Last activity", "State", "Memory", ""] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for s in &sessions {
                ui.label(RichText::new(&s.project).strong()).on_hover_text(&s.project_dir);
                let task = s.task.clone().unwrap_or_else(|| "\u{2014}".into());
                let r = ui
                    .allocate_ui_with_layout(egui::vec2(340.0, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_width(340.0);
                        ui.add(egui::Label::new(&task).truncate())
                    })
                    .inner;
                if let Some(t) = &s.transcript {
                    r.on_hover_text(format!("{task}\n\n{}", t.display()));
                }
                ui.label(s.app);
                ui.label(if s.idle < Duration::from_secs(60) { "now".to_string() } else { format!("{} ago", mins(s.idle)) });
                let (text, color) = if s.is_idle(limit) {
                    (format!("Idle {}", mins(s.idle)), AMBER)
                } else if s.idle < Duration::from_secs(90) {
                    ("Working".to_string(), TEAL)
                } else {
                    ("Waiting".to_string(), Color32::GRAY)
                };
                ui.label(RichText::new(text).color(color));
                ui.label(format!("{}  ({} processes)", gb(s.memory), s.children + 1))
                    .on_hover_text("The session plus everything it started: MCP servers, dev servers, shells");
                if ui.small_button("Close").on_hover_text("Ends this session and everything it started").clicked() {
                    if s.is_idle(limit) {
                        self.end_now(format!("{} session in {}", s.app, s.project), vec![s.pid], true, s.memory);
                    } else {
                        self.confirm = Some(Confirm::End {
                            what: format!("{} session in {} (it is still active)", s.app, s.project),
                            pids: vec![s.pid],
                            memory: s.memory,
                            force: true,
                            safety: Safety::Caution,
                        });
                    }
                }
                ui.end_row();
            }
        });
    }

    // ---------------------------------------------------------------- widget

    /// The small always-on-top window. Drag it anywhere; its place is remembered.
    ///
    /// It is a *deferred* viewport: it repaints on its own mouse and keyboard input,
    /// without waiting for the (often hidden) main window, so it never feels sluggish.
    fn widget(&mut self, ctx: &egui::Context) {
        let size = egui::vec2(300.0, 470.0);
        if !self.widget_checked {
            // A saved place may be off screen (smaller monitor, unplugged screen): pull it back once.
            self.widget_checked = true;
            self.widget_pos = self.widget_pos.map(|p| keep_on_screen(ctx, p, size));
        }
        let pos = self.widget_pos.unwrap_or_else(|| default_widget_pos(ctx, size));
        let builder = egui::ViewportBuilder::default()
            .with_title("Dozer widget")
            .with_inner_size(size)
            .with_position(pos)
            .with_decorations(false)
            .with_resizable(false)
            .with_taskbar(false)
            .with_always_on_top();
        self.widget_shared.limit_secs.store(self.session_limit().as_secs(), Ordering::Relaxed);
        let shared = self.widget_shared.clone();
        ctx.show_viewport_deferred(egui::ViewportId::from_hash_of("dozer-widget"), builder, move |ui, _class| {
            widget_ui(ui, &shared);
        });
    }

    /// Acts on what the widget asked for (it cannot reach `self`).
    fn handle_widget_actions(&mut self, ctx: &egui::Context) {
        if let Some(p) = *self.widget_shared.pos.lock().unwrap() {
            self.widget_pos = Some(p);
        }
        let actions: Vec<WidgetAction> = std::mem::take(&mut *self.widget_shared.actions.lock().unwrap());
        for a in actions {
            match a {
                WidgetAction::OpenMain => {
                    self.show_window(ctx);
                    self.main_visible = true;
                }
                WidgetAction::Hide => {
                    self.widget_on = false;
                    self.monitor.fast.store(self.main_visible, Ordering::Relaxed);
                    self.save_widget_state();
                }
                WidgetAction::End { what, pids, memory } => self.end_now(what, pids, true, memory),
            }
        }
    }

    // -------------------------------------------------------------- on demand

    fn on_demand_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let (direct_mem, direct_n) = snap.category_total(Category::McpDirect);
        let (hub_mem, hub_n) = snap.category_total(Category::Dozer);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("How it works").strong());
            ui.label(
                "Agents connect to Dozer instead of starting their own copy of every server. \
                 Dozer answers the connection and the tool list from its cache, so an idle or busy \
                 agent costs nothing until it really calls a tool. Then the server starts, and it is \
                 stopped again after the idle minutes you set.",
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Started by sessions now: {} in {} processes", gb(direct_mem), direct_n)).color(AMBER));
                ui.label("·");
                ui.label(RichText::new(format!("Run by Dozer now: {} in {} processes", gb(hub_mem), hub_n)).color(TEAL));
            });
        });
        ui.add_space(6.0);

        let busy = self.busy.load(Ordering::Relaxed);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Import servers from Claude").on_hover_text("Reads the stdio MCP servers in ~/.claude.json").clicked() {
                let mut cfg = self.hub.config.write().unwrap();
                match cfg.import_from_claude() {
                    Ok(added) => {
                        let _ = cfg.save();
                        for n in &added {
                            if cfg.server(n).is_some_and(suggested_for_routing) {
                                self.selected.insert(n.clone());
                            }
                        }
                        self.set_notice(
                            if added.is_empty() { "No new stdio servers in the Claude config.".into() } else { format!("Imported: {}", added.join(", ")) },
                            true,
                        );
                    }
                    Err(e) => self.set_notice(format!("Cannot read the Claude config: {e}"), false),
                }
                drop(cfg);
                *self.monitor.servers.lock().unwrap() = self.hub.config.read().unwrap().servers.clone();
            }
            let to_route: Vec<String> = self.selected.iter().cloned().collect();
            if ui
                .add_enabled(!busy && !to_route.is_empty(), egui::Button::new(format!("Use Dozer in Claude ({})", to_route.len())))
                .on_hover_text("Switches the ticked servers in Claude Code to Dozer. A backup of ~/.claude.json is made first.")
                .clicked()
            {
                self.confirm = Some(Confirm::Route { names: to_route });
            }
            let routed: Vec<String> =
                self.hub.config.read().unwrap().servers.iter().filter(|s| s.routed).map(|s| s.name.clone()).collect();
            if ui
                .add_enabled(!busy && !routed.is_empty(), egui::Button::new("Restore Claude's original setup"))
                .clicked()
            {
                self.confirm = Some(Confirm::Restore { names: routed });
            }
            if ui.button("Stop all now").clicked() {
                self.hub.stop_all();
            }
            if busy {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            let mut on = self.autostart;
            if ui.checkbox(&mut on, "Start Dozer with Windows").on_hover_text("Needed once agents use Dozer: they cannot reach their servers while it is closed.").changed() {
                match crate::set_autostart(on) {
                    Ok(()) => self.autostart = on,
                    Err(e) => self.set_notice(format!("Cannot change autostart: {e}"), false),
                }
            }
            let port = self.hub.config.read().unwrap().port;
            ui.label(RichText::new(format!("Gateway: http://127.0.0.1:{port}/mcp/<name>")).weak());
        });
        ui.separator();

        // Remote servers: listed so nothing seems missing, but they need no on-demand handling.
        if self.remote_read.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            self.remote_read = Some(Instant::now());
            self.remote = crate::config::read_claude_remote_servers(self.hub.config.read().unwrap().port);
        }
        if !self.remote.is_empty() {
            egui::CollapsingHeader::new(RichText::new(format!("Remote servers ({}) \u{2014} run on the internet, use no RAM here", self.remote.len())).strong())
                .default_open(true)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("Claude talks to these over HTTP. No process runs on this PC for them, so Dozer leaves them as they are.")
                            .small()
                            .weak(),
                    );
                    egui::Grid::new("remote").striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                        for (name, url) in &self.remote {
                            ui.label(RichText::new(name).strong());
                            ui.label(RichText::new(url).weak());
                            ui.label(RichText::new("0 MB").color(TEAL));
                            ui.end_row();
                        }
                    });
                });
            ui.separator();
        }

        let views = self.hub.views();
        if views.is_empty() {
            ui.label("No local servers yet. Click “Import servers from Claude”.");
            return;
        }
        let mut changes: Vec<(String, Box<dyn FnOnce(&mut ServerConfig)>)> = Vec::new();
        let mut actions: Vec<(String, &'static str)> = Vec::new();
        egui::Grid::new("servers").striped(true).num_columns(8).spacing([12.0, 6.0]).show(ui, |ui| {
            for h in ["Switch", "Server", "Mode", "Stop after idle", "Status", "Tools", "Calls", ""] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for v in &views {
                let s = &v.config;
                if s.routed {
                    ui.label(RichText::new("In Claude").color(TEAL)).on_hover_text("Claude Code reaches this server through Dozer");
                } else {
                    let mut on = self.selected.contains(&s.name);
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            self.selected.insert(s.name.clone());
                        } else {
                            self.selected.remove(&s.name);
                        }
                    }
                }
                ui.label(RichText::new(&s.name).strong()).on_hover_text(format!("{} {}", s.command, s.args.join(" ")));

                let mut mode = s.mode;
                egui::ComboBox::from_id_salt(format!("mode/{}", s.name))
                    .selected_text(match mode {
                        Mode::Shared => "Shared",
                        Mode::PerSession => "Per session",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut mode, Mode::Shared, "Shared")
                            .on_hover_text("One process for all agents. Best for APIs (GitHub, Coolify…)");
                        ui.selectable_value(&mut mode, Mode::PerSession, "Per session")
                            .on_hover_text("Each agent session gets its own process. Needed for browsers.");
                    });
                if mode != s.mode {
                    changes.push((s.name.clone(), Box::new(move |c| c.mode = mode)));
                }

                let mut idle = s.idle_minutes;
                if ui.add(egui::DragValue::new(&mut idle).range(1..=240).suffix(" min")).changed() {
                    changes.push((s.name.clone(), Box::new(move |c| c.idle_minutes = idle)));
                }

                status_cell(ui, v);
                ui.label(v.cached_tools.map_or("not cached".to_string(), |n| n.to_string()))
                    .on_hover_text("Tool lists are cached so agents can connect without starting the server");
                ui.label(format!("{}", v.calls)).on_hover_text(format!(
                    "{} starts · {} sessions connected today{}",
                    v.starts,
                    v.sessions_seen,
                    v.last_call.map(|t| format!(" · last call {}", ago(t))).unwrap_or_default()
                ));
                ui.horizontal(|ui| {
                    if !v.running.is_empty() && ui.small_button("Stop").clicked() {
                        actions.push((s.name.clone(), "stop"));
                    }
                    let (label, tip) = if s.enabled {
                        ("Turn off", "Agents get an error instead of starting this server")
                    } else {
                        ("Turn on", "Allow agents to start this server again")
                    };
                    if ui.small_button(label).on_hover_text(tip).clicked() {
                        let on = !s.enabled;
                        changes.push((s.name.clone(), Box::new(move |c| c.enabled = on)));
                    }
                    if ui.small_button("Refresh tools").on_hover_text("Start it once, read its tools, stop it").clicked() {
                        actions.push((s.name.clone(), "warm"));
                    }
                    if !s.routed && ui.small_button("Remove").on_hover_text("Remove from Dozer").clicked() {
                        actions.push((s.name.clone(), "remove"));
                    }
                });
                ui.end_row();
            }
        });

        if !changes.is_empty() {
            let mut cfg = self.hub.config.write().unwrap();
            for (name, f) in changes {
                if let Some(s) = cfg.servers.iter_mut().find(|s| s.name == name) {
                    f(s);
                }
            }
            let _ = cfg.save();
        }
        for (name, action) in actions {
            match action {
                "stop" => self.hub.stop(&name, "stopped by you"),
                "warm" => {
                    self.hub.clear_cache(&name);
                    self.hub.warm_up(&name);
                }
                "remove" => {
                    self.hub.stop(&name, "removed");
                    let mut cfg = self.hub.config.write().unwrap();
                    cfg.servers.retain(|s| s.name != name);
                    let _ = cfg.save();
                    self.selected.remove(&name);
                }
                _ => {}
            }
        }
    }

    fn activity_tab(&self, ui: &mut egui::Ui) {
        let lines = self.hub.log_lines();
        if lines.is_empty() {
            ui.label("Nothing yet. Starts and stops of on-demand servers appear here.");
        }
        for l in lines.iter().rev() {
            ui.horizontal(|ui| {
                let t: chrono::DateTime<chrono::Local> = l.time.into();
                ui.label(RichText::new(t.format("%H:%M:%S").to_string()).monospace().weak());
                ui.label(RichText::new(&l.server).strong());
                ui.label(&l.text);
            });
        }
    }

    // --------------------------------------------------------------- dialogs

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let (title, body, ok) = match confirm {
            Confirm::End { what, pids, memory, force, safety } => (
                if *force { format!("End task: {what}?") } else { format!("Close {what}?") },
                format!(
                    "{} process tree(s), about {}.\n{}",
                    pids.len(),
                    gb(*memory),
                    match safety {
                        Safety::Safe => "Nothing is lost.",
                        _ if *force => "It is stopped at once. Unsaved work in it is lost.",
                        _ => "It is asked to quit and may show a save prompt.",
                    }
                ),
                if *force { "End task" } else { "Close" },
            ),
            Confirm::Route { names } => (
                "Use Dozer in Claude Code?".to_string(),
                format!(
                    "These servers will be reached through Dozer: {}.\n\n\
                     ~/.claude.json is backed up first. Sessions that are already open keep their old \
                     processes until you restart them. Keep Dozer running (tick “Start with Windows”).",
                    names.join(", ")
                ),
                "Switch",
            ),
            Confirm::Restore { names } => (
                "Restore Claude's original setup?".to_string(),
                format!("Claude Code will start these servers itself again: {}.", names.join(", ")),
                "Restore",
            ),
        };
        let mut decision = None;
        let resp = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading(title);
            ui.add_space(6.0);
            ui.label(body);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(ok).strong()).clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
        if resp.should_close() && decision.is_none() {
            decision = Some(false);
        }
        let Some(yes) = decision else { return };
        let confirm = self.confirm.take().unwrap();
        if !yes {
            return;
        }
        match confirm {
            Confirm::End { what, pids, force, memory, .. } => self.end_now(what, pids, force, memory),
            Confirm::Route { names } => self.run_claude_job(names, true),
            Confirm::Restore { names } => self.run_claude_job(names, false),
        }
    }

    fn run_claude_job(&self, names: Vec<String>, to_hub: bool) {
        let hub = self.hub.clone();
        let notice = self.notice.clone();
        let busy = self.busy.clone();
        busy.store(true, Ordering::Relaxed);
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                let backup = crate::claude::backup_claude_config()?;
                let mut done = Vec::new();
                let mut failed = Vec::new();
                for name in &names {
                    let (server, url) = {
                        let cfg = hub.config.read().unwrap();
                        (cfg.server(name).cloned(), cfg.url_for(name))
                    };
                    let Some(server) = server else { continue };
                    let r = if to_hub {
                        // Fill the cache first so the next agent connects without a start.
                        hub.warm_up(name);
                        crate::claude::route_to_hub(&server, &url)
                    } else {
                        crate::claude::restore_direct(&server)
                    };
                    match r {
                        Ok(()) => {
                            done.push(name.clone());
                            let mut cfg = hub.config.write().unwrap();
                            if let Some(s) = cfg.servers.iter_mut().find(|s| &s.name == name) {
                                s.routed = to_hub;
                            }
                            let _ = cfg.save();
                        }
                        Err(e) => failed.push(format!("{name}: {e}")),
                    }
                    hub.repaint();
                }
                let mut msg = format!(
                    "{} {}. Backup: {}",
                    if to_hub { "Now using Dozer:" } else { "Restored:" },
                    if done.is_empty() { "nothing".into() } else { done.join(", ") },
                    backup.display()
                );
                if to_hub && !done.is_empty() {
                    msg.push_str(". Restart open Claude sessions to free their old processes.");
                }
                if !failed.is_empty() {
                    msg.push_str(&format!("\nFailed: {}", failed.join("; ")));
                }
                Ok(msg)
            })();
            *notice.lock().unwrap() = Some(match result {
                Ok(m) => (m, true),
                Err(e) => (format!("Could not change the Claude config: {e}"), false),
            });
            busy.store(false, Ordering::Relaxed);
            hub.repaint();
        });
    }
}

fn status_cell(ui: &mut egui::Ui, v: &ServerView) {
    if !v.config.enabled {
        ui.label(RichText::new("Off").weak());
        return;
    }
    if v.running.is_empty() {
        let text = match &v.last_error {
            Some(_) => RichText::new("Error").color(RED),
            None => RichText::new("Sleeping (0 MB)").weak(),
        };
        let r = ui.label(text);
        if let Some(e) = &v.last_error {
            r.on_hover_text(e);
        }
        return;
    }
    let busy = v.running.iter().any(|r| r.busy);
    let idle = v.running.iter().map(|r| r.idle).min().unwrap_or_default();
    let left = Duration::from_secs(u64::from(v.config.idle_minutes) * 60).saturating_sub(idle);
    let text = if busy {
        format!("Working ×{}", v.running.len())
    } else {
        format!("Running ×{} · stops in {}", v.running.len(), mins(left))
    };
    ui.label(RichText::new(text).color(if busy { AMBER } else { TEAL })).on_hover_text(
        v.running
            .iter()
            .map(|r| {
                format!(
                    "pid {} · up {} · idle {}{}",
                    r.pid,
                    mins(r.uptime),
                    mins(r.idle),
                    r.session.as_ref().map(|s| format!(" · session {}", &s[..s.len().min(8)])).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

fn safety_badge(ui: &mut egui::Ui, g: &Group) {
    let color = match g.safety {
        Safety::Safe => Color32::from_rgb(34, 197, 94),
        Safety::Caution => AMBER,
        Safety::Keep => RED,
    };
    ui.horizontal(|ui| {
        // Drawn, not a glyph: the default font has no filled circle.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.5, color);
        ui.label(RichText::new(g.safety.label()).color(color));
    })
    .response
    .on_hover_text(g.reason);
}

const MB: u64 = 1024 * 1024;

pub fn gb(bytes: u64) -> String {
    if bytes >= 1024 * MB { format!("{:.1} GB", bytes as f64 / (1024.0 * MB as f64)) } else { format!("{} MB", bytes / MB) }
}

fn mins(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else if s < 3600 { format!("{}m", s / 60) } else { format!("{}h{}m", s / 3600, (s % 3600) / 60) }
}

fn ago(t: SystemTime) -> String {
    t.elapsed().map(|d| format!("{} ago", mins(d))).unwrap_or_else(|_| "just now".into())
}

/// Counted by the one-click button: safe, and stays closed. Windows shell parts
/// (Start menu, Search) restart at once, so ending them frees nothing for long.
fn frees_for_good(g: &Group) -> bool {
    g.safety == Safety::Safe && g.category != Category::Other
}

const CPU_COLOR: Color32 = Color32::from_rgb(129, 140, 248);

fn level_color(v: f32) -> Color32 {
    if v > 90.0 { RED } else if v > 75.0 { AMBER } else { TEAL }
}

pub fn category_color(c: Category) -> Color32 {
    match c {
        Category::McpDirect | Category::Dozer => Color32::from_rgb(245, 158, 11),
        Category::Agents => Color32::from_rgb(168, 85, 247),
        Category::Browsers => Color32::from_rgb(59, 130, 246),
        Category::DevServers => Color32::from_rgb(20, 184, 166),
        Category::Containers => Color32::from_rgb(236, 72, 153),
        Category::StuckGit => Color32::from_rgb(239, 68, 68),
        Category::Other => Color32::from_rgb(100, 116, 139),
    }
}

/// A ring gauge with the value in the middle.
fn gauge(ui: &mut egui::Ui, name: &str, value: f32, sub: &str) {
    ui.vertical_centered(|ui| {
        let d = 92.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::hover());
        let p = ui.painter_at(rect.expand(2.0));
        let c = rect.center();
        let r = d / 2.0 - 7.0;
        let color = level_color(value);
        p.circle_stroke(c, r, egui::Stroke::new(9.0, ui.visuals().extreme_bg_color));
        let frac = (value / 100.0).clamp(0.0, 1.0);
        if frac > 0.002 {
            let steps = (64.0 * frac).ceil().max(2.0) as usize;
            let start = -std::f32::consts::FRAC_PI_2;
            let pts: Vec<egui::Pos2> = (0..=steps)
                .map(|i| {
                    let a = start + std::f32::consts::TAU * frac * i as f32 / steps as f32;
                    c + r * egui::vec2(a.cos(), a.sin())
                })
                .collect();
            p.add(egui::Shape::line(pts, egui::Stroke::new(9.0, color)));
        }
        p.text(c - egui::vec2(0.0, 6.0), egui::Align2::CENTER_CENTER, format!("{value:.0}%"), egui::FontId::proportional(20.0), ui.visuals().strong_text_color());
        p.text(c + egui::vec2(0.0, 13.0), egui::Align2::CENTER_CENTER, name, egui::FontId::proportional(11.0), ui.visuals().weak_text_color());
        ui.label(RichText::new(sub).small().weak());
    });
}

/// RAM and CPU over the last minutes, with grid lines at 25/50/75 %.
fn history_chart(ui: &mut egui::Ui, history: &[(f32, f32)], height: f32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("History").small().weak());
        ui.label(RichText::new("\u{2014} RAM").small().color(TEAL));
        ui.label(RichText::new("\u{2014} CPU").small().color(CPU_COLOR));
    });
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    for f in [0.25, 0.5, 0.75] {
        let y = rect.bottom() - rect.height() * f;
        p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(1.0, ui.visuals().faint_bg_color));
    }
    if history.len() < 2 {
        p.text(rect.center(), egui::Align2::CENTER_CENTER, "collecting\u{2026}", egui::FontId::proportional(11.0), ui.visuals().weak_text_color());
        return;
    }
    let to_points = |pick: &dyn Fn(&(f32, f32)) -> f32| -> Vec<egui::Pos2> {
        let n = history.len() as f32 - 1.0;
        history
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let x = rect.left() + rect.width() * i as f32 / n;
                let y = rect.bottom() - (rect.height() - 2.0) * pick(h).clamp(0.0, 100.0) / 100.0;
                egui::pos2(x, y)
            })
            .collect()
    };
    let ram = to_points(&|h| h.0);
    let cpu = to_points(&|h| h.1);
    // Soft fill under the RAM line, drawn as thin vertical slices.
    for w in ram.windows(2) {
        let quad = vec![w[0], w[1], egui::pos2(w[1].x, rect.bottom()), egui::pos2(w[0].x, rect.bottom())];
        p.add(egui::Shape::convex_polygon(quad, TEAL.gamma_multiply(0.15), egui::Stroke::NONE));
    }
    p.add(egui::Shape::line(cpu, egui::Stroke::new(1.5, CPU_COLOR)));
    p.add(egui::Shape::line(ram, egui::Stroke::new(2.0, TEAL)));
}

/// One stacked bar: what the RAM is made of, plus free memory.
pub fn ram_breakdown(ui: &mut egui::Ui, snap: &Snapshot, compact: bool) {
    let m = snap.memory;
    if m.total == 0 {
        return;
    }
    let used = m.total.saturating_sub(m.available);
    let order = [
        (Category::Agents, "AI agents"),
        (Category::McpDirect, "MCP servers"),
        (Category::Browsers, "Browsers & web views"),
        (Category::DevServers, "Dev servers"),
        (Category::Containers, "Docker/WSL"),
        (Category::StuckGit, "Stuck git"),
    ];
    let mut parts: Vec<(String, u64, Color32)> = Vec::new();
    let mut known = 0;
    for (cat, name) in order {
        let mut mem = snap.category_total(cat).0;
        if cat == Category::McpDirect {
            mem += snap.category_total(Category::Dozer).0;
        }
        if mem > 0 {
            known += mem;
            parts.push((name.to_string(), mem, category_color(cat)));
        }
    }
    parts.push(("Windows & other apps".into(), used.saturating_sub(known), category_color(Category::Other)));
    parts.sort_by_key(|x| std::cmp::Reverse(x.1));
    // Free memory is the last column, drawn as an outline.
    parts.push(("Free".into(), m.available, Color32::TRANSPARENT));

    ui.label(RichText::new("What fills your RAM").small().weak());
    // Vertical columns: size on top, short name below, details on hover.
    let height = if compact { 96.0 } else { 120.0 };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let n = parts.len() as f32;
    let gap = 6.0;
    let col_w = ((rect.width() - gap * (n - 1.0)) / n).max(8.0);
    let top_pad = 14.0; // room for the size label
    let bottom_pad = 14.0; // room for the name
    let plot_h = rect.height() - top_pad - bottom_pad;
    let max = parts.iter().map(|x| x.1).max().unwrap_or(1).max(1) as f32;
    let baseline = rect.bottom() - bottom_pad;
    p.line_segment(
        [egui::pos2(rect.left(), baseline), egui::pos2(rect.right(), baseline)],
        egui::Stroke::new(1.0, ui.visuals().faint_bg_color),
    );
    let weak = ui.visuals().weak_text_color();
    let text = ui.visuals().text_color();
    for (i, (name, mem, color)) in parts.iter().enumerate() {
        let x = rect.left() + i as f32 * (col_w + gap);
        let h = (plot_h * (*mem as f32 / max)).max(2.0);
        let bar = egui::Rect::from_min_max(egui::pos2(x, baseline - h), egui::pos2(x + col_w, baseline));
        if *color == Color32::TRANSPARENT {
            p.rect_stroke(bar, 3.0, egui::Stroke::new(1.0, weak), egui::StrokeKind::Inside);
        } else {
            p.rect_filled(bar, 3.0, *color);
        }
        p.text(
            egui::pos2(bar.center().x, bar.top() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            short_gb(*mem),
            egui::FontId::proportional(10.0),
            text,
        );
        p.text(
            egui::pos2(bar.center().x, baseline + 2.0),
            egui::Align2::CENTER_TOP,
            short_name(name),
            egui::FontId::proportional(10.0),
            weak,
        );
        let hit = egui::Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(x + col_w, rect.bottom()));
        ui.interact(hit, egui::Id::unique(("ram-col", compact, i)), egui::Sense::hover()).on_hover_text(format!(
            "{name}\n{}  ({:.0}% of RAM)",
            gb(*mem),
            *mem as f64 * 100.0 / m.total as f64
        ));
    }
}

/// Column labels must fit a narrow column.
fn short_name(name: &str) -> &str {
    match name {
        "AI agents" => "AI",
        "MCP servers" => "MCP",
        "Browsers & web views" => "Web",
        "Dev servers" => "Dev",
        "Docker/WSL" => "Docker",
        "Stuck git" => "Git",
        "Windows & other apps" => "Win",
        other => other,
    }
}

/// `10.4 GB` -> `10.4G`, `520 MB` -> `520M`.
fn short_gb(bytes: u64) -> String {
    gb(bytes).replace(" GB", "G").replace(" MB", "M")
}

/// A labelled horizontal bar, scaled against `max`.
fn bar_row(ui: &mut egui::Ui, label: &str, value: u64, max: u64, color: Color32) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let w = rect.width() * (value as f32 / max as f32).clamp(0.02, 1.0);
    p.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height())), 3.0, color.gamma_multiply(0.35));
    p.text(rect.left_center() + egui::vec2(6.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(12.0), ui.visuals().text_color());
    p.text(rect.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, gb(value), egui::FontId::monospace(11.0), ui.visuals().strong_text_color());
    resp.on_hover_text(format!("{label}: {}", gb(value)));
}

/// Bottom-right corner of the work area (above the taskbar).
fn default_widget_pos(ctx: &egui::Context, size: egui::Vec2) -> egui::Pos2 {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};
    let mut r = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
    unsafe {
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut _, 0);
    }
    let ppp = ctx.pixels_per_point().max(0.5);
    egui::pos2(r.right as f32 / ppp - size.x - 16.0, r.bottom as f32 / ppp - size.y - 16.0)
}

/// Six dots that show the widget can be dragged.
fn grip(ui: &mut egui::Ui) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
    let color = ui.visuals().weak_text_color();
    for row in 0..3 {
        for col in 0..2 {
            let c = rect.min + egui::vec2(2.5 + col as f32 * 5.0, 2.5 + row as f32 * 4.5);
            ui.painter().circle_filled(c, 1.3, color);
        }
    }
    resp.on_hover_text("Drag anywhere to move");
}

/// Moves a window position so the whole window is inside the desktop (all monitors).
fn keep_on_screen(ctx: &egui::Context, pos: egui::Pos2, size: egui::Vec2) -> egui::Pos2 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    let ppp = ctx.pixels_per_point().max(0.5);
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN) as f32,
            GetSystemMetrics(SM_YVIRTUALSCREEN) as f32,
            GetSystemMetrics(SM_CXVIRTUALSCREEN) as f32,
            GetSystemMetrics(SM_CYVIRTUALSCREEN) as f32,
        )
    };
    // Leave room for the taskbar at the bottom.
    let (min_x, min_y, max_x, max_y) = (x / ppp, y / ppp, (x + w) / ppp - size.x, (y + h) / ppp - size.y - 48.0);
    egui::pos2(pos.x.clamp(min_x, max_x.max(min_x)), pos.y.clamp(min_y, max_y.max(min_y)))
}

pub enum WidgetAction {
    OpenMain,
    Hide,
    End { what: String, pids: Vec<u32>, memory: u64 },
}

/// What the widget window shares with the main app.
pub struct WidgetShared {
    pub monitor: Arc<Monitor>,
    pub actions: Mutex<Vec<WidgetAction>>,
    pub pos: Mutex<Option<egui::Pos2>>,
    pub limit_secs: std::sync::atomic::AtomicU64,
    pub frames: std::sync::atomic::AtomicU32,
}

fn widget_ui(ui: &mut egui::Ui, shared: &WidgetShared) {
    let ctx = ui.ctx().clone();
    // New numbers arrive every 2 s; input repaints at once on its own.
    ctx.request_repaint_after(Duration::from_millis(1000));
    let snap = shared.monitor.snapshot();
    let limit = Duration::from_secs(shared.limit_secs.load(Ordering::Relaxed).max(60));
    let mut actions = Vec::new();
    // Windows reports a placeholder position for the first frames; only trust it once settled.
    let frames = shared.frames.fetch_add(1, Ordering::Relaxed);
    if frames > 20
        && let Some(r) = ui.input(|i| i.viewport().outer_rect)
    {
        *shared.pos.lock().unwrap() = Some(r.min);
    }
    if ui.input(|i| i.viewport().close_requested()) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
        actions.push(WidgetAction::Hide);
    }
    // Text must not grab the mouse, or dragging selects text instead of moving the widget.
    ui.style_mut().interaction.selectable_labels = false;
    egui::CentralPanel::default_margins().show(ui, |ui| {
        let bg = ui.interact(ui.max_rect(), egui::Id::unique("dozer-widget-drag"), egui::Sense::click_and_drag());
        if bg.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        ui.horizontal(|ui| {
            grip(ui);
            ui.label(RichText::new("Dozer").strong().size(15.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button(" \u{2013} ").on_hover_text("Hide widget (tray menu brings it back)").clicked() {
                    actions.push(WidgetAction::Hide);
                }
                if ui.small_button("Open").clicked() {
                    actions.push(WidgetAction::OpenMain);
                }
            });
        });
        ui.add_space(4.0);

        let m = snap.memory;
        let (ram, cpu) = snap.history.last().copied().unwrap_or((0.0, 0.0));
        ui.columns(2, |cols| {
            gauge(&mut cols[0], "RAM", ram, &format!("{} / {}", gb(m.total.saturating_sub(m.available)), gb(m.total)));
            gauge(&mut cols[1], "CPU", cpu, &format!("{} processes", snap.process_count));
        });
        ui.add_space(6.0);
        ram_breakdown(ui, &snap, true);
        ui.add_space(6.0);

        let safe: Vec<&Group> = snap.groups.iter().filter(|g| frees_for_good(g)).collect();
        let safe_mem: u64 = safe.iter().map(|g| g.memory).sum();
        let idle: Vec<_> = snap.sessions.iter().filter(|s| s.is_idle(limit)).collect();
        let idle_mem: u64 = idle.iter().map(|s| s.memory).sum();
        ui.horizontal(|ui| {
            ui.label(format!("Safe to free: {}", gb(safe_mem)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let b = egui::Button::new(RichText::new("Free").color(Color32::WHITE)).fill(Color32::from_rgb(22, 163, 74));
                if ui.add_enabled(safe_mem > 0, b).clicked() {
                    actions.push(WidgetAction::End {
                        what: "safe items".into(),
                        pids: safe.iter().flat_map(|g| g.roots.iter().copied()).collect(),
                        memory: safe_mem,
                    });
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(format!("Agents: {} \u{00b7} {} idle", snap.sessions.len(), idle.len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!idle.is_empty(), egui::Button::new(format!("Close idle ({})", gb(idle_mem)))).clicked() {
                    actions.push(WidgetAction::End {
                        what: "idle sessions".into(),
                        pids: idle.iter().map(|s| s.pid).collect(),
                        memory: idle_mem,
                    });
                }
            });
        });
        ui.separator();
        ui.label(RichText::new("Biggest now").small().weak());
        let mut top: Vec<&Group> = snap.groups.iter().collect();
        top.sort_by_key(|g| std::cmp::Reverse(g.memory));
        let max = top.first().map_or(1, |g| g.memory.max(1));
        for g in top.iter().take(4) {
            bar_row(ui, &g.label, g.memory, max, category_color(g.category));
        }
    });
    if !actions.is_empty() {
        shared.actions.lock().unwrap().extend(actions);
        // Wake the main app so it acts now, even while its window is hidden.
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }
}
