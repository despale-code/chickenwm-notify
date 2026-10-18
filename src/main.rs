//! `chickenwm-notify` — ChickenWM's notification service.
//!
//! - Toasts stack in a screen corner (`notifications { position = … }` in chickenwm.conf),
//!   styled by `~/.config/chickenwm/notifications.css` (colors from the system theme).
//! - Sources: Windows app notifications (`toasts`), legacy tray balloons (forwarded by
//!   chickenwm-rooster), and `chickenctl notify` / IPC on `\\.\pipe\chickenwm-notify-ipc`.
//! - `Super + N` toggles the notification center: history, do-not-disturb, blocked apps.
//! - One transparent, never-focused window that resizes to its content and lets clicks through
//!   when nothing is shown.

// Release builds are windowed apps (no console window).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod toasts;

use chickenwm_core::config::{config_dir, NotificationsConfig, RgbaColor};
use chickenwm_core::css::{load_css_sheet, spawn_config_watcher, ConfigWatchEvent, CssStyleSheet};
use chickenwm_core::ipc::{IpcCommand, IpcResponse, Notification, PIPE_NAME_NOTIFY};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Messages from the IPC server, the toast reader and the config watcher.
pub enum Incoming {
    /// Show (and record) a notification.
    Note { note: Notification, toast_id: Option<u32>, created: Option<u64> },
    /// Record without popping up (old notifications found at startup).
    History { note: Notification, created: u64 },
    DismissAll,
    ToggleCenter,
    Reload,
}

static CTX: OnceLock<egui::Context> = OnceLock::new();

fn wake() {
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ───────────────────────────── persisted state ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    app: String,
    title: String,
    body: String,
    time: u64,
    #[serde(default)]
    toast_id: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    do_not_disturb: bool,
    history: Vec<HistoryEntry>,
}

const HISTORY_MAX: usize = 200;

fn store_path() -> std::path::PathBuf {
    config_dir().join("notifications.json")
}

fn load_store() -> Stored {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

// ───────────────────────────── style ─────────────────────────────

#[derive(Clone, Copy, PartialEq)]
struct Style {
    card_bg: Color32,
    fg: Color32,
    sub: Color32,
    accent: Color32,
    accent_fg: Color32,
    border: Color32,
    shadow: Color32,
    critical: Color32,
    card_w: f32,
    radius: u8,
    padding: f32,
    gap: f32,
    title_size: f32,
    body_size: f32,
    app_size: f32,
    icon: f32,
    anim_secs: f32,
    progress_h: f32,
}

fn c(rgba: RgbaColor) -> Color32 {
    Color32::from_rgba_unmultiplied(rgba.r, rgba.g, rgba.b, rgba.a)
}

impl Style {
    fn from_css(s: &CssStyleSheet) -> Self {
        let col = |n: &str, r: u8, g: u8, b: u8, a: u8| c(s.var_color(n, RgbaColor { r, g, b, a }));
        Self {
            card_bg: col("--card-bg", 251, 251, 251, 248),
            fg: col("--fg-color", 36, 35, 33, 255),
            sub: col("--subtext-color", 98, 95, 89, 255),
            accent: col("--accent-color", 36, 35, 33, 255),
            accent_fg: col("--accent-fg", 247, 246, 246, 255),
            border: col("--border-color", 36, 35, 33, 40),
            shadow: col("--shadow-color", 20, 19, 18, 46),
            critical: col("--critical-color", 192, 57, 43, 255),
            card_w: s.var_px("--card-width", 360.0).clamp(220.0, 640.0),
            radius: s.var_px("--border-radius", 12.0).clamp(0.0, 24.0) as u8,
            padding: s.var_px("--padding", 14.0).clamp(6.0, 32.0),
            gap: s.var_px("--gap", 10.0).clamp(0.0, 40.0),
            title_size: s.var_px("--title-size", 15.0).clamp(10.0, 28.0),
            body_size: s.var_px("--body-size", 13.5).clamp(9.0, 24.0),
            app_size: s.var_px("--app-size", 11.5).clamp(8.0, 20.0),
            icon: s.var_px("--icon-size", 28.0).clamp(0.0, 64.0),
            anim_secs: (s.var_px("--anim-ms", 320.0) / 1000.0).clamp(0.01, 2.0),
            progress_h: s.var_px("--progress-height", 2.0).clamp(0.0, 8.0),
        }
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn time_ago(t: u64) -> String {
    let d = now_secs().saturating_sub(t);
    match d {
        0..=44 => "now".into(),
        45..=3599 => format!("{} min", (d + 30) / 60),
        3600..=86_399 => format!("{} h", d / 3600),
        _ => format!("{} d", d / 86_400),
    }
}

// ───────────────────────────── app ─────────────────────────────

struct Toast {
    key: u64,
    note: Notification,
    toast_id: Option<u32>,
    critical: bool,
    /// Seconds on screen; `None` = until dismissed.
    timeout: Option<f32>,
    elapsed: f32,
    born: Instant,
    leaving: Option<Instant>,
}

struct App {
    rx: Receiver<Incoming>,
    cfg: NotificationsConfig,
    css: Arc<Mutex<CssStyleSheet>>,
    /// Keeps the UI font in step with `--font-family` in notifications.css.
    font: chickenwm_core::fonts::EguiFont,
    style: Style,
    stored: Stored,
    toasts: Vec<Toast>,
    queue: VecDeque<Toast>,
    next_key: u64,
    center_open: bool,
    center_since: Instant,
    last_frame: Instant,
    window_ready: bool,
    last_geometry: Option<(Pos2, Vec2, bool)>,
    work_area: [f32; 4],
    work_at: Instant,
    visuals_set: bool,
    /// Light/dark choice in the master theme provider (`app_modes = notifications:…`).
    app_mode: chickenwm_core::theme::AppMode,
    _watcher: Option<notify_watcher::Watcher>,
}

/// Keeps the config watcher alive (type alias to avoid a direct `notify` dependency).
mod notify_watcher {
    pub type Watcher = Box<dyn std::any::Any>;
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, rx: Receiver<Incoming>, tx: Sender<Incoming>) -> Self {
        let _ = CTX.set(cc.egui_ctx.clone());
        let css = Arc::new(Mutex::new(load_css_sheet("notifications.css").unwrap_or_default()));
        let watcher = {
            let css = css.clone();
            spawn_config_watcher(move |ev| match ev {
                ConfigWatchEvent::CssChanged { file_name, .. } if file_name.eq_ignore_ascii_case("notifications.css") => {
                    if let Ok(sheet) = load_css_sheet("notifications.css") {
                        *css.lock() = sheet;
                    }
                    wake();
                }
                ConfigWatchEvent::HyprConfChanged(_) => {
                    let _ = tx.send(Incoming::Reload);
                    wake();
                }
                _ => {}
            })
            .ok()
            .map(|w| Box::new(w) as Box<dyn std::any::Any>)
        };
        let style = Style::from_css(&css.lock());
        Self {
            rx,
            cfg: load_config(),
            css,
            font: Default::default(),
            style,
            stored: load_store(),
            toasts: Vec::new(),
            queue: VecDeque::new(),
            next_key: 1,
            center_open: false,
            center_since: Instant::now(),
            last_frame: Instant::now(),
            window_ready: false,
            last_geometry: None,
            work_area: primary_work_area(),
            work_at: Instant::now(),
            visuals_set: false,
            app_mode: load_app_mode(),
            _watcher: watcher,
        }
    }

    fn save_store(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.stored) {
            let _ = std::fs::write(store_path(), text);
        }
    }

    fn is_blocked(&self, app: &str) -> bool {
        self.cfg.blocked.iter().any(|b| b.eq_ignore_ascii_case(app.trim()))
    }

    fn is_dnd_exception(&self, app: &str) -> bool {
        self.cfg.dnd_exceptions.iter().any(|e| e.eq_ignore_ascii_case(app.trim()))
    }

    fn is_quiet_hours_now(&self) -> bool {
        let (_, _, _, minute, _) = chickenwm_core::theme_tools::local_clock();
        is_in_quiet_hours(&self.cfg.quiet_hours, minute)
    }

    fn effective_dnd(&self) -> bool {
        self.stored.do_not_disturb || self.is_quiet_hours_now()
    }

    fn record(&mut self, note: &Notification, time: u64, toast_id: Option<u32>) {
        if !self.center_open {
            UNREAD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.stored.history.insert(
            0,
            HistoryEntry { app: note.app.clone(), title: note.title.clone(), body: note.body.clone(), time, toast_id },
        );
        self.stored.history.truncate(HISTORY_MAX);
        self.save_store();
    }

    fn push(&mut self, note: Notification, toast_id: Option<u32>, created: Option<u64>) {
        if !self.cfg.enabled || self.is_blocked(&note.app) {
            return;
        }
        self.record(&note, created.unwrap_or_else(now_secs), toast_id);
        let critical = note.urgency.as_deref() == Some("critical");
        if self.effective_dnd() && !critical && !self.is_dnd_exception(&note.app) {
            return;
        }
        let secs = note.timeout_secs.unwrap_or(self.cfg.timeout);
        let toast = Toast {
            key: self.next_key,
            timeout: (secs > 0 && !critical).then_some(secs as f32),
            note,
            toast_id,
            critical,
            elapsed: 0.0,
            born: Instant::now(),
            leaving: None,
        };
        self.next_key += 1;
        self.queue.push_back(toast);
    }

    fn dismiss(&mut self, key: u64) {
        if let Some(t) = self.toasts.iter_mut().find(|t| t.key == key && t.leaving.is_none()) {
            t.leaving = Some(Instant::now());
            if let Some(id) = t.toast_id {
                std::thread::spawn(move || toasts::remove(id));
            }
        }
    }

    fn dismiss_all(&mut self) {
        let keys: Vec<u64> = self.toasts.iter().map(|t| t.key).collect();
        for k in keys {
            self.dismiss(k);
        }
        self.queue.clear();
    }

    /// Blocks an app: stored in chickenwm.conf (`notifications { blocked = … }`).
    fn set_blocked(&mut self, app: &str, blocked: bool) {
        let app = app.trim().to_string();
        if blocked {
            if !self.is_blocked(&app) {
                self.cfg.blocked.push(app.clone());
            }
            let keys: Vec<u64> = self.toasts.iter().filter(|t| t.note.app.eq_ignore_ascii_case(&app)).map(|t| t.key).collect();
            for k in keys {
                self.dismiss(k);
            }
            self.queue.retain(|t| !t.note.app.eq_ignore_ascii_case(&app));
        } else {
            self.cfg.blocked.retain(|b| !b.eq_ignore_ascii_case(&app));
        }
        if let Ok((mut doc, _)) = chickenwm_core::config::load_or_init_config() {
            doc.set_category_key("notifications", "blocked", &self.cfg.blocked.join(", "));
            if let Err(e) = chickenwm_core::config::save_config_document(&doc) {
                tracing::warn!("couldn't save blocked apps: {e}");
            }
        }
    }

    /// Adds or removes an app from DND exceptions (`notifications { dnd_exceptions = … }`).
    fn set_dnd_exception(&mut self, app: &str, allow: bool) {
        let app = app.trim().to_string();
        if allow {
            if !self.is_dnd_exception(&app) {
                self.cfg.dnd_exceptions.push(app);
            }
        } else {
            self.cfg.dnd_exceptions.retain(|e| !e.eq_ignore_ascii_case(&app));
        }
        if let Ok((mut doc, _)) = chickenwm_core::config::load_or_init_config() {
            doc.set_category_key("notifications", "dnd_exceptions", &self.cfg.dnd_exceptions.join(", "));
            if let Err(e) = chickenwm_core::config::save_config_document(&doc) {
                tracing::warn!("couldn't save DND exceptions: {e}");
            }
        }
    }

    fn save_quiet_hours(&mut self, schedule: String) {
        self.cfg.quiet_hours = schedule.trim().to_string();
        if let Ok((mut doc, _)) = chickenwm_core::config::load_or_init_config() {
            doc.set_category_key("notifications", "quiet_hours", &self.cfg.quiet_hours);
            let _ = chickenwm_core::config::save_config_document(&doc);
        }
    }

    fn clear_app_history(&mut self, app: &str) {
        let mut toast_ids = Vec::new();
        self.stored.history.retain(|e| {
            let matches = e.app.eq_ignore_ascii_case(app);
            if matches {
                if let Some(id) = e.toast_id {
                    toast_ids.push(id);
                }
            }
            !matches
        });
        if !toast_ids.is_empty() {
            std::thread::spawn(move || {
                for id in toast_ids {
                    toasts::remove(id);
                }
            });
        }
        self.save_store();
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Incoming::Note { note, toast_id, created } => self.push(note, toast_id, created),
                Incoming::History { note, created } => {
                    if !self.is_blocked(&note.app) {
                        self.record(&note, created, None);
                    }
                }
                Incoming::DismissAll => self.dismiss_all(),
                Incoming::ToggleCenter => {
                    self.center_open = !self.center_open;
                    self.center_since = Instant::now();
                    if self.center_open {
                        UNREAD.store(0, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Incoming::Reload => {
                    self.cfg = load_config();
                    self.app_mode = load_app_mode();
                }
            }
        }
        // Fill free slots from the queue.
        let max = self.cfg.max_visible.max(1) as usize;
        while self.toasts.iter().filter(|t| t.leaving.is_none()).count() < max {
            let Some(mut t) = self.queue.pop_front() else { break };
            t.born = Instant::now();
            self.toasts.insert(0, t);
        }
    }

    fn side(&self) -> (f32, f32) {
        // (horizontal: -1 left, 0 center, 1 right; vertical: -1 top, 1 bottom)
        let p = self.cfg.position.as_str();
        let h = if p.contains("left") { -1.0 } else if p.contains("center") { 0.0 } else { 1.0 };
        let v = if p.starts_with("bottom") { 1.0 } else { -1.0 };
        (h, v)
    }
}

fn load_app_mode() -> chickenwm_core::theme::AppMode {
    chickenwm_core::config::load_or_init_config()
        .map(|(_, c)| c.theme.app_mode("notifications"))
        .unwrap_or(chickenwm_core::theme::AppMode::System)
}

fn load_config() -> NotificationsConfig {
    chickenwm_core::config::load_or_init_config().map(|(_, c)| c.notifications).unwrap_or_default()
}

/// Primary monitor work area in physical pixels.
fn primary_work_area() -> [f32; 4] {
    let mons = chickenwm_core::monitor::enumerate_monitors();
    mons.iter()
        .find(|m| m.is_primary)
        .or(mons.first())
        .map(|m| [m.work_rect[0] as f32, m.work_rect[1] as f32, m.work_rect[2] as f32, m.work_rect[3] as f32])
        .unwrap_or([0.0, 0.0, 1920.0, 1040.0])
}

const PAD: f32 = 18.0; // room for card shadows inside the window
const CENTER_W: f32 = 400.0;
/// Diameter of the "clear all" button under the notification stack.
const CLEAR_BTN: f32 = 32.0;

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        DND.store(self.effective_dnd(), std::sync::atomic::Ordering::Relaxed);
        if !self.window_ready {
            self.window_ready = prepare_window(frame);
        }
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.1);
        self.last_frame = Instant::now();
        self.drain();
        let style = Style::from_css(&self.css.lock());
        self.font.sync(ctx, &self.css.lock(), false);
        if style != self.style || !self.visuals_set {
            self.style = style;
            apply_visuals(ctx, &style);
            self.visuals_set = true;
        }
        if self.work_at.elapsed() > Duration::from_secs(5) {
            self.work_area = primary_work_area();
            self.work_at = Instant::now();
        }
        let st = self.style;
        let ppp = ctx.pixels_per_point();
        let work = [self.work_area[0] / ppp, self.work_area[1] / ppp, self.work_area[2] / ppp, self.work_area[3] / ppp];
        let (hside, vside) = self.side();
        let (mx, my) = (self.cfg.margin_x as f32, self.cfg.margin_y as f32);

        // Card heights (needed to size the window before drawing).
        let text_w = st.card_w - st.padding * 2.0 - if st.icon > 0.0 { st.icon + 10.0 } else { 0.0 };
        let heights: Vec<f32> = self.toasts.iter().map(|t| card_height(ctx, &st, &t.note, text_w, 4)).collect();
        let stack_h: f32 = heights.iter().sum::<f32>() + st.gap * heights.len().saturating_sub(1) as f32;
        // Room for the single "clear all" button below the stack.
        let clear_h = CLEAR_BTN + st.gap;

        // Window geometry.
        let (size, passthrough) = if self.center_open {
            (egui::vec2(CENTER_W + PAD * 2.0, (work[3] - my * 2.0).max(200.0)), false)
        } else if self.toasts.is_empty() {
            (egui::vec2(1.0, 1.0), true)
        } else {
            (egui::vec2(st.card_w + PAD * 2.0, stack_h + clear_h + PAD * 2.0), false)
        };
        let center_h = if self.center_open { if hside < 0.0 { -1.0 } else { 1.0 } } else { hside };
        let x = match center_h {
            h if h < 0.0 => work[0] + mx - PAD,
            h if h > 0.0 => work[0] + work[2] - mx - size.x + PAD,
            _ => work[0] + (work[2] - size.x) / 2.0,
        };
        let y = if self.center_open || vside < 0.0 {
            work[1] + my - if self.center_open { 0.0 } else { PAD }
        } else {
            work[1] + work[3] - my - size.y + PAD
        };
        let geometry = (Pos2::new(x.round(), y.round()), size, passthrough);
        if self.last_geometry != Some(geometry) {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(geometry.0));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(geometry.1));
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(geometry.2));
            self.last_geometry = Some(geometry);
        }

        let mut actions: Vec<UiAction> = Vec::new();
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
            if self.center_open {
                self.draw_center(ui, &st, &mut actions);
                return;
            }
            // Timers pause while the pointer is over the stack.
            let hovering = ui.rect_contains_pointer(ui.max_rect()) && !self.toasts.is_empty();
            // Bottom-anchored stacks grow upward, so the button sits below them at the bottom.
            let mut y = if vside < 0.0 { PAD } else { size.y - PAD - clear_h };
            for (t, &h) in self.toasts.iter_mut().zip(&heights) {
                if !hovering && t.leaving.is_none() {
                    if let Some(limit) = t.timeout {
                        t.elapsed += dt;
                        if t.elapsed >= limit {
                            t.leaving = Some(Instant::now());
                        }
                    }
                }
                let top = if vside < 0.0 { y } else { y - h };
                y = if vside < 0.0 { y + h + st.gap } else { y - h - st.gap };
                // Smooth reflow when cards above appear or leave.
                let top = ui.ctx().animate_value_with_time(egui::Id::new(("toast-y", t.key)), top, 0.22);
                let enter = ease_out(t.born.elapsed().as_secs_f32() / st.anim_secs);
                let exit = t.leaving.map_or(0.0, |at| ease_out(at.elapsed().as_secs_f32() / (st.anim_secs * 0.8)));
                let alpha = enter * (1.0 - exit);
                let slide_dir = if hside == 0.0 { 0.0 } else { hside };
                let dx = (1.0 - enter) * 40.0 * slide_dir + exit * 60.0 * slide_dir;
                let dy = if hside == 0.0 { (1.0 - enter) * 16.0 * -vside } else { 0.0 };
                let rect = Rect::from_min_size(Pos2::new(PAD + dx, top + dy), egui::vec2(st.card_w, h));
                draw_toast(ui, &st, t, rect, alpha, text_w, &mut actions);
            }

            // One "clear all" button below every notification.
            let any_live = self.toasts.iter().any(|t| t.leaving.is_none());
            let shown = ui.ctx().animate_bool_with_time(egui::Id::new("clear-all-shown"), any_live, st.anim_secs);
            if shown > 0.01 {
                let btn_y = if vside < 0.0 { PAD + stack_h + st.gap } else { size.y - PAD - CLEAR_BTN };
                let btn_y = ui.ctx().animate_value_with_time(egui::Id::new("clear-all-y"), btn_y, 0.22);
                let center = Pos2::new(PAD + st.card_w / 2.0, btn_y + CLEAR_BTN / 2.0);
                let hit = Rect::from_center_size(center, Vec2::splat(CLEAR_BTN));
                let resp = ui.interact(hit, egui::Id::new("clear-all"), Sense::click()).on_hover_text("Clear all notifications");
                let hover = ui.ctx().animate_bool_with_time(egui::Id::new("clear-all-hover"), resp.hovered(), 0.12);
                let r = CLEAR_BTN / 2.0 * (0.85 + 0.15 * shown) * (1.0 + 0.08 * hover);
                let painter = ui.painter();
                let shadow = egui::epaint::Shadow { offset: [0, 4], blur: 14, spread: 0, color: st.shadow.gamma_multiply(shown) };
                painter.add(shadow.as_shape(Rect::from_center_size(center, Vec2::splat(r * 2.0)), CornerRadius::same(r as u8)));
                painter.circle(center, r, st.card_bg.gamma_multiply(shown), Stroke::new(1.0_f32, st.border.gamma_multiply(shown)));
                let glyph = if hover > 0.5 { st.fg } else { st.sub };
                painter.text(center, Align2::CENTER_CENTER, "×", FontId::proportional(15.0 + 2.0 * hover), glyph.gamma_multiply(shown));
                if resp.clicked() && any_live {
                    actions.push(UiAction::DismissAll);
                }
            }
        });

        // Remove finished exits.
        let exit_secs = st.anim_secs * 0.8;
        self.toasts.retain(|t| t.leaving.is_none_or(|at| at.elapsed().as_secs_f32() < exit_secs));

        for a in actions {
            match a {
                UiAction::Dismiss(k) => self.dismiss(k),
                UiAction::DismissAll => self.dismiss_all(),
                UiAction::Block(app) => self.set_blocked(&app, true),
                UiAction::Unblock(app) => self.set_blocked(&app, false),
                UiAction::AllowInDnd(app, allow) => self.set_dnd_exception(&app, allow),
                UiAction::SetQuietHours(s) => self.save_quiet_hours(s),
                UiAction::ClearAppHistory(app) => self.clear_app_history(&app),
                UiAction::FocusAppWindow(app) => {
                    focus_source_window(&app);
                    self.center_open = false;
                }
                UiAction::OpenCenter => {
                    self.center_open = true;
                    self.center_since = Instant::now();
                }
                UiAction::CloseCenter => self.center_open = false,
                UiAction::ClearHistory => {
                    self.stored.history.clear();
                    self.save_store();
                }
                UiAction::DeleteEntry(i) => {
                    if i < self.stored.history.len() {
                        let e = self.stored.history.remove(i);
                        if let Some(id) = e.toast_id {
                            std::thread::spawn(move || toasts::remove(id));
                        }
                        self.save_store();
                    }
                }
                UiAction::CycleAppearance => {
                    use chickenwm_core::theme::AppMode;
                    self.app_mode = match self.app_mode {
                        AppMode::System => AppMode::Light,
                        AppMode::Light => AppMode::Dark,
                        AppMode::Dark => AppMode::System,
                    };
                    if let Err(e) = chickenwm_core::theme::set_app_mode("notifications", self.app_mode) {
                        tracing::warn!("couldn't change appearance: {e}");
                    }
                }
                UiAction::ToggleDnd => {
                    self.stored.do_not_disturb = !self.stored.do_not_disturb;
                    self.save_store();
                }
            }
        }

        let animating = !self.toasts.is_empty() || self.center_open && self.center_since.elapsed().as_secs_f32() < st.anim_secs;
        if animating {
            ctx.request_repaint();
        } else if !self.queue.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

enum UiAction {
    Dismiss(u64),
    DismissAll,
    Block(String),
    Unblock(String),
    AllowInDnd(String, bool),
    SetQuietHours(String),
    ClearAppHistory(String),
    FocusAppWindow(String),
    OpenCenter,
    CloseCenter,
    ClearHistory,
    DeleteEntry(usize),
    ToggleDnd,
    CycleAppearance,
}

fn body_job(text: &str, size: f32, color: Color32, width: f32, rows: usize) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(FontId::proportional(size), color));
    job.wrap = egui::text::TextWrapping { max_width: width, max_rows: rows, break_anywhere: false, overflow_character: Some('…') };
    job
}

fn card_height(ctx: &egui::Context, st: &Style, note: &Notification, text_w: f32, rows: usize) -> f32 {
    let title = ctx.fonts(|f| f.layout_job(body_job(&note.title, st.title_size, Color32::WHITE, text_w, 2)));
    let body_h = if note.body.trim().is_empty() {
        0.0
    } else {
        ctx.fonts(|f| f.layout_job(body_job(&note.body, st.body_size, Color32::WHITE, text_w, rows))).size().y + 3.0
    };
    let text_h = st.app_size + 6.0 + title.size().y + body_h;
    st.padding * 2.0 + text_h.max(st.icon)
}

fn paint_card(painter: &egui::Painter, st: &Style, rect: Rect, alpha: f32, critical: bool) {
    let shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 22,
        spread: 0,
        color: st.shadow.gamma_multiply(alpha),
    };
    painter.add(shadow.as_shape(rect, CornerRadius::same(st.radius)));
    painter.rect(
        rect,
        CornerRadius::same(st.radius),
        st.card_bg.gamma_multiply(alpha),
        Stroke::new(1.0_f32, if critical { st.critical.gamma_multiply(alpha) } else { st.border.gamma_multiply(alpha) }),
        egui::StrokeKind::Inside,
    );
}

fn paint_avatar(painter: &egui::Painter, st: &Style, center: Pos2, app: &str, alpha: f32, critical: bool) {
    if st.icon <= 0.0 {
        return;
    }
    let fill = if critical { st.critical } else { st.accent };
    painter.circle_filled(center, st.icon / 2.0, fill.gamma_multiply(alpha));
    let letter = app.trim().chars().next().unwrap_or('•').to_uppercase().to_string();
    painter.text(center, Align2::CENTER_CENTER, letter, FontId::proportional(st.icon * 0.48), st.accent_fg.gamma_multiply(alpha));
}

/// Draws the text block of a card; returns nothing (layout matches `card_height`).
#[allow(clippy::too_many_arguments)]
fn paint_text(painter: &egui::Painter, st: &Style, origin: Pos2, text_w: f32, app: &str, when: &str, note_title: &str, body: &str, alpha: f32, rows: usize) {
    painter.text(origin, Align2::LEFT_TOP, app, FontId::proportional(st.app_size), st.sub.gamma_multiply(alpha));
    painter.text(origin + egui::vec2(text_w, 0.0), Align2::RIGHT_TOP, when, FontId::proportional(st.app_size), st.sub.gamma_multiply(alpha));
    let mut y = origin.y + st.app_size + 6.0;
    let title = painter.layout_job(body_job(note_title, st.title_size, st.fg.gamma_multiply(alpha), text_w, 2));
    let th = title.size().y;
    painter.galley(Pos2::new(origin.x, y), title, st.fg);
    y += th + 3.0;
    if !body.trim().is_empty() {
        let g = painter.layout_job(body_job(body, st.body_size, st.sub.gamma_multiply(alpha), text_w, rows));
        painter.galley(Pos2::new(origin.x, y), g, st.sub);
    }
}

fn draw_toast(ui: &mut egui::Ui, st: &Style, t: &Toast, rect: Rect, alpha: f32, text_w: f32, actions: &mut Vec<UiAction>) {
    let painter = ui.painter();
    paint_card(painter, st, rect, alpha, t.critical);
    let icon_c = Pos2::new(rect.left() + st.padding + st.icon / 2.0, rect.top() + st.padding + st.icon / 2.0);
    paint_avatar(painter, st, icon_c, &t.note.app, alpha, t.critical);
    let text_x = rect.left() + st.padding + if st.icon > 0.0 { st.icon + 10.0 } else { 0.0 };
    let resp = ui.interact(rect, egui::Id::new(("toast", t.key)), Sense::click());
    let close_rect = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.top() + 16.0), Vec2::splat(20.0));
    let when = if resp.hovered() { String::new() } else { "now".to_string() };
    paint_text(painter, st, Pos2::new(text_x, rect.top() + st.padding), text_w, &t.note.app, &when, &t.note.title, &t.note.body, alpha, 4);
    // Countdown line along the bottom edge.
    if let Some(limit) = t.timeout {
        if st.progress_h > 0.0 {
            let left = 1.0 - (t.elapsed / limit).clamp(0.0, 1.0);
            let bar = Rect::from_min_size(
                Pos2::new(rect.left() + st.radius as f32, rect.bottom() - st.progress_h - 1.0),
                egui::vec2((rect.width() - st.radius as f32 * 2.0) * left, st.progress_h),
            );
            painter.rect_filled(bar, CornerRadius::same(1), st.accent.gamma_multiply(alpha * 0.55));
        }
    }
    if resp.hovered() {
        let over_close = ui.rect_contains_pointer(close_rect);
        if over_close {
            painter.circle_filled(close_rect.center(), 10.0, st.border.gamma_multiply(alpha));
        }
        painter.text(close_rect.center(), Align2::CENTER_CENTER, "×", FontId::proportional(11.0), st.sub.gamma_multiply(alpha));
    }
    if resp.clicked() {
        let over_close = ui.rect_contains_pointer(close_rect);
        if !over_close {
            actions.push(UiAction::FocusAppWindow(t.note.app.clone()));
        }
        actions.push(UiAction::Dismiss(t.key));
    }
    let app = t.note.app.clone();
    let key = t.key;
    resp.context_menu(|ui| {
        if ui.button(format!("Focus {app}")).clicked() {
            actions.push(UiAction::FocusAppWindow(app.clone()));
            actions.push(UiAction::Dismiss(key));
            ui.close_menu();
        }
        if ui.button("Dismiss").clicked() {
            actions.push(UiAction::Dismiss(key));
            ui.close_menu();
        }
        if ui.button("Dismiss all").clicked() {
            actions.push(UiAction::DismissAll);
            ui.close_menu();
        }
        if ui.button(format!("Allow {app} during Do Not Disturb")).clicked() {
            actions.push(UiAction::AllowInDnd(app.clone(), true));
            ui.close_menu();
        }
        if ui.button(format!("Block notifications from {app}")).clicked() {
            actions.push(UiAction::Block(app.clone()));
            ui.close_menu();
        }
        ui.separator();
        if ui.button("Notification center  (Super+N)").clicked() {
            actions.push(UiAction::OpenCenter);
            ui.close_menu();
        }
    });
}

/// Checks whether `minute` (0..1440 after local midnight) falls inside `schedule` (`"22:00-07:00"`).
fn is_in_quiet_hours(schedule: &str, minute: u32) -> bool {
    let Some((start_s, end_s)) = schedule.trim().split_once('-') else {
        return false;
    };
    let (Some(start), Some(end)) = (
        chickenwm_core::theme_tools::parse_hm(start_s),
        chickenwm_core::theme_tools::parse_hm(end_s),
    ) else {
        return false;
    };
    if start == end {
        return false;
    }
    if start < end {
        minute >= start && minute < end
    } else {
        minute >= start || minute < end
    }
}

/// Groups history entries by app (case-insensitive), preserving the order of each app's most recent notification.
fn group_history_by_app(history: &[HistoryEntry]) -> Vec<(String, Vec<(usize, &HistoryEntry)>)> {
    let mut groups: Vec<(String, Vec<(usize, &HistoryEntry)>)> = Vec::new();
    for (idx, entry) in history.iter().enumerate() {
        let app_key = if entry.app.trim().is_empty() { "System" } else { entry.app.trim() };
        if let Some((_, items)) = groups.iter_mut().find(|(g, _)| g.eq_ignore_ascii_case(app_key)) {
            items.push((idx, entry));
        } else {
            groups.push((app_key.to_string(), vec![(idx, entry)]));
        }
    }
    groups
}

/// Focuses the open window belonging to `app` (matching process name, class, or window title) via WM IPC.
fn focus_source_window(app: &str) {
    let app_q = app.trim().to_ascii_lowercase();
    if app_q.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        use chickenwm_core::ipc::{send_ipc_command_blocking, PIPE_NAME_WM};
        let Ok(IpcResponse::Clients(clients)) =
            send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::QueryClients)
        else {
            return;
        };
        let target = clients.iter().find(|c| {
            let proc_stem = c.process_name.trim_end_matches(".exe").to_ascii_lowercase();
            proc_stem == app_q
                || proc_stem.contains(&app_q)
                || app_q.contains(&proc_stem) && !proc_stem.is_empty()
                || c.class.to_ascii_lowercase().contains(&app_q)
                || c.title.to_ascii_lowercase().contains(&app_q)
        });
        if let Some(c) = target {
            let _ = send_ipc_command_blocking(
                PIPE_NAME_WM,
                &IpcCommand::Dispatch(chickenwm_core::ipc::Action::Workspace(c.workspace_id)),
            );
            unsafe {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::UI::WindowsAndMessaging::{
                    IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
                };
                let hwnd = HWND(c.hwnd as *mut core::ffi::c_void);
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
            }
        }
    });
}

impl App {
    fn draw_center(&self, ui: &mut egui::Ui, st: &Style, actions: &mut Vec<UiAction>) {
        let full = ui.max_rect().shrink(PAD);
        let appear = ease_out(self.center_since.elapsed().as_secs_f32() / st.anim_secs);
        let (hside, _) = self.side();
        let dir = if hside < 0.0 { -1.0 } else { 1.0 };
        let panel = full.translate(egui::vec2((1.0 - appear) * 40.0 * dir, 0.0));
        paint_card(ui.painter(), st, panel, appear, false);

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(panel.shrink(st.padding)));
        let ui = &mut child;
        ui.visuals_mut().override_text_color = Some(st.fg.gamma_multiply(appear));
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Notifications").size(st.title_size + 4.0).color(st.fg.gamma_multiply(appear)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new("×").frame(false)).on_hover_text("Close (Super+N)").clicked() {
                    actions.push(UiAction::CloseCenter);
                }
                if ui.add(egui::Button::new("Clear all").frame(false)).clicked() {
                    actions.push(UiAction::ClearHistory);
                }
                let mode_label = match self.app_mode {
                    chickenwm_core::theme::AppMode::System => "Appearance: follow system (click for light)",
                    chickenwm_core::theme::AppMode::Light => "Appearance: light (click for dark)",
                    chickenwm_core::theme::AppMode::Dark => "Appearance: dark (click to follow system)",
                };
                if ui.add(egui::Button::new("◐").frame(false)).on_hover_text(mode_label).clicked() {
                    actions.push(UiAction::CycleAppearance);
                }
            });
        });
        ui.add_space(4.0);
        let quiet_now = self.is_quiet_hours_now();
        let mut dnd = self.stored.do_not_disturb;
        let dnd_label = if quiet_now && !dnd {
            format!("Do not disturb (active via quiet hours {})", self.cfg.quiet_hours)
        } else {
            "Do not disturb (only critical / exception apps pop up)".to_string()
        };
        if ui.checkbox(&mut dnd, dnd_label).changed() {
            actions.push(UiAction::ToggleDnd);
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Quiet hours:").size(st.app_size).color(st.sub));
            let current_qh = if self.cfg.quiet_hours.is_empty() { "Off" } else { &self.cfg.quiet_hours };
            ui.label(egui::RichText::new(current_qh).size(st.app_size).color(st.fg));
            for (label, sched) in [("Off", ""), ("22:00–07:00", "22:00-07:00"), ("23:00–08:00", "23:00-08:00")] {
                let selected = self.cfg.quiet_hours == sched;
                if ui.selectable_label(selected, egui::RichText::new(label).size(st.app_size)).clicked() {
                    actions.push(UiAction::SetQuietHours(sched.to_string()));
                }
            }
        });
        ui.add_space(6.0);
        ui.separator();

        let has_footer = !self.cfg.blocked.is_empty() || !self.cfg.dnd_exceptions.is_empty();
        let footer_h = if has_footer { 84.0 } else { 0.0 };
        let list_h = (ui.available_height() - footer_h).max(60.0);
        let text_w = ui.available_width() - st.padding * 2.0 - if st.icon > 0.0 { st.icon + 10.0 } else { 0.0 } - 8.0;
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
            if self.stored.history.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| ui.label(egui::RichText::new("No notifications").color(st.sub)));
            }
            for (group_app, entries) in group_history_by_app(&self.stored.history) {
                ui.horizontal(|ui| {
                    let is_exc = self.is_dnd_exception(&group_app);
                    let hdr = if is_exc {
                        format!("{} ({}) • DND allowed", group_app, entries.len())
                    } else {
                        format!("{} ({})", group_app, entries.len())
                    };
                    ui.label(egui::RichText::new(hdr).size(st.app_size + 0.5).strong().color(st.sub.gamma_multiply(appear)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(egui::RichText::new("Clear app").size(st.app_size)).frame(false))
                            .on_hover_text(format!("Clear all notifications from {group_app}"))
                            .clicked()
                        {
                            actions.push(UiAction::ClearAppHistory(group_app.clone()));
                        }
                        if ui
                            .add(egui::Button::new(egui::RichText::new("Focus").size(st.app_size)).frame(false))
                            .on_hover_text(format!("Switch to {group_app} window"))
                            .clicked()
                        {
                            actions.push(UiAction::FocusAppWindow(group_app.clone()));
                        }
                    });
                });
                ui.add_space(2.0);
                for (i, e) in entries {
                    let note = Notification { app: e.app.clone(), title: e.title.clone(), body: e.body.clone(), ..Default::default() };
                    let h = card_height(ui.ctx(), st, &note, text_w, 6);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width() - 4.0, h), Sense::click());
                    let painter = ui.painter();
                    painter.rect(
                        rect,
                        CornerRadius::same(st.radius.saturating_sub(2)),
                        if resp.hovered() { st.border.gamma_multiply(0.35 * appear) } else { Color32::TRANSPARENT },
                        Stroke::new(1.0_f32, st.border.gamma_multiply(appear)),
                        egui::StrokeKind::Inside,
                    );
                    let icon_c = Pos2::new(rect.left() + st.padding + st.icon / 2.0, rect.top() + st.padding + st.icon / 2.0);
                    paint_avatar(painter, st, icon_c, &e.app, appear, false);
                    let text_x = rect.left() + st.padding + if st.icon > 0.0 { st.icon + 10.0 } else { 0.0 };
                    let close_rect = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.top() + 14.0), Vec2::splat(20.0));
                    let when = if resp.hovered() { "×".to_string() } else { time_ago(e.time) };
                    paint_text(painter, st, Pos2::new(text_x, rect.top() + st.padding), text_w, &e.app, &when, &e.title, &e.body, appear, 6);
                    if resp.clicked() {
                        if ui.rect_contains_pointer(close_rect) {
                            actions.push(UiAction::DeleteEntry(i));
                        } else {
                            actions.push(UiAction::FocusAppWindow(e.app.clone()));
                        }
                    }
                    let app = e.app.clone();
                    let is_exc = self.is_dnd_exception(&app);
                    resp.context_menu(|ui| {
                        if ui.button(format!("Focus {app} window")).clicked() {
                            actions.push(UiAction::FocusAppWindow(app.clone()));
                            ui.close_menu();
                        }
                        if ui.button("Delete notification").clicked() {
                            actions.push(UiAction::DeleteEntry(i));
                            ui.close_menu();
                        }
                        if ui.button(format!("Clear all from {app}")).clicked() {
                            actions.push(UiAction::ClearAppHistory(app.clone()));
                            ui.close_menu();
                        }
                        let exc_label = if is_exc {
                            format!("Remove {app} from DND exceptions")
                        } else {
                            format!("Allow {app} during Do Not Disturb")
                        };
                        if ui.button(exc_label).clicked() {
                            actions.push(UiAction::AllowInDnd(app.clone(), !is_exc));
                            ui.close_menu();
                        }
                        if ui.button(format!("Block notifications from {app}")).clicked() {
                            actions.push(UiAction::Block(app.clone()));
                            ui.close_menu();
                        }
                    });
                    ui.add_space(st.gap * 0.5);
                }
                ui.add_space(st.gap * 0.4);
            }
        });

        if has_footer {
            ui.separator();
            if !self.cfg.dnd_exceptions.is_empty() {
                ui.label(egui::RichText::new("Allowed during DND (click to remove)").size(st.app_size).color(st.sub));
                ui.horizontal_wrapped(|ui| {
                    for app in &self.cfg.dnd_exceptions {
                        if ui.button(format!("{app}  ×")).on_hover_text("Stop allowing during DND").clicked() {
                            actions.push(UiAction::AllowInDnd(app.clone(), false));
                        }
                    }
                });
            }
            if !self.cfg.blocked.is_empty() {
                ui.label(egui::RichText::new("Blocked apps (click to unblock)").size(st.app_size).color(st.sub));
                ui.horizontal_wrapped(|ui| {
                    for app in &self.cfg.blocked {
                        if ui.button(format!("{app}  ×")).on_hover_text("Allow notifications again").clicked() {
                            actions.push(UiAction::Unblock(app.clone()));
                        }
                    }
                });
            }
        }
    }
}

/// Widget colors (center panel controls, context menus) from the notification palette.
fn apply_visuals(ctx: &egui::Context, st: &Style) {
    let [r, g, b, _] = st.card_bg.to_array();
    let light = (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0 > 0.5;
    let mut v = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
    let opaque = Color32::from_rgb(r, g, b);
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = opaque;
    v.extreme_bg_color = opaque;
    v.override_text_color = Some(st.fg);
    v.window_stroke = Stroke::new(1.0_f32, st.border);
    v.selection.bg_fill = st.accent.gamma_multiply(0.35);
    v.widgets.inactive.bg_fill = st.border;
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, st.fg);
    v.widgets.hovered.weak_bg_fill = st.border;
    v.widgets.hovered.bg_fill = st.border;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, st.fg);
    v.widgets.active.bg_fill = st.accent;
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, st.accent_fg);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, st.fg);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, st.border);
    ctx.set_visuals(v);
}

/// Tool window that never takes focus, without Windows' open/close animation.
fn prepare_window(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{BOOL, HWND};
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW};
    let Ok(handle) = frame.window_handle() else { return false };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return false };
    let hwnd = HWND(h.hwnd.get() as _);
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize);
        let off = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &off as *const _ as *const core::ffi::c_void, 4);
    }
    true
}

/// Notifications that arrived while the center was closed (the bar's bell shows the count).
static UNREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Mirror of `do_not_disturb`, readable from the IPC thread.
static DND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn spawn_ipc_server(tx: Sender<Incoming>) {
    std::thread::Builder::new()
        .name("notify-ipc".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
            rt.block_on(async move {
                let tx = Arc::new(Mutex::new(tx));
                let result = chickenwm_core::ipc::serve_ipc(PIPE_NAME_NOTIFY, move |cmd| {
                    let tx = tx.clone();
                    async move {
                        let msg = match cmd {
                            IpcCommand::Notify(note) => Some(Incoming::Note { note, toast_id: None, created: None }),
                            IpcCommand::DismissNotifications => Some(Incoming::DismissAll),
                            IpcCommand::QueryNotificationStatus => {
                                return IpcResponse::NotificationStatus {
                                    unread: UNREAD.load(std::sync::atomic::Ordering::Relaxed),
                                    dnd: DND.load(std::sync::atomic::Ordering::Relaxed),
                                };
                            }
                            IpcCommand::ToggleNotificationCenter => Some(Incoming::ToggleCenter),
                            IpcCommand::ReloadConfig => Some(Incoming::Reload),
                            _ => None,
                        };
                        match msg {
                            Some(m) => {
                                let _ = tx.lock().send(m);
                                wake();
                                IpcResponse::Ok { message: "ok".into() }
                            }
                            None => IpcResponse::Error { message: "not handled by chickenwm-notify".into() },
                        }
                    }
                })
                .await;
                if let Err(e) = result {
                    tracing::error!("notification IPC server stopped: {e}");
                }
            });
        })
        .expect("spawn IPC thread");
}

fn main() -> anyhow::Result<()> {
    chickenwm_core::init_tracing("chickenwm_notify");
    let _failsafe = chickenwm_core::failsafe::spawn_emergency_failsafe_thread();
    let (_doc, cfg) = chickenwm_core::bootstrap_config_and_themes()?;

    let (tx, rx) = mpsc::channel::<Incoming>();
    spawn_ipc_server(tx.clone());
    {
        let tx = tx.clone();
        let windows_toasts = cfg.notifications.windows_toasts;
        std::thread::Builder::new()
            .name("notify-toasts".into())
            .spawn(move || {
                if windows_toasts {
                    toasts::run(tx, || true);
                }
            })?;
    }

    let options = eframe::NativeOptions {
        // glow, not wgpu: egui_kittest (headless feature) also enables eframe's wgpu backend,
        // which eframe would then prefer, and wgpu draws transparent windows black on Windows.
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("ChickenWM Notifications")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_active(false)
            .with_resizable(false)
            .with_mouse_passthrough(true)
            .with_inner_size([1.0, 1.0])
            .with_position([-32000.0, -32000.0]),
        ..Default::default()
    };
    eframe::run_native(
        "chickenwm-notify",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, rx, tx)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_hours_handles_overnight_and_daytime_ranges() {
        // Overnight: 22:00 (1320) to 07:00 (420)
        assert!(is_in_quiet_hours("22:00-07:00", 23 * 60 + 15));
        assert!(is_in_quiet_hours("22:00-07:00", 6 * 60 + 30));
        assert!(!is_in_quiet_hours("22:00-07:00", 12 * 60));
        assert!(!is_in_quiet_hours("22:00-07:00", 7 * 60));

        // Daytime: 09:00 (540) to 17:00 (1020)
        assert!(is_in_quiet_hours("09:00-17:00", 10 * 60));
        assert!(!is_in_quiet_hours("09:00-17:00", 20 * 60));
        assert!(!is_in_quiet_hours("", 10 * 60));
    }

    #[test]
    fn history_groups_by_app_preserving_recency_order() {
        let entries = vec![
            HistoryEntry { app: "Discord".into(), title: "msg 1".into(), body: String::new(), time: 30, toast_id: None },
            HistoryEntry { app: "Steam".into(), title: "update".into(), body: String::new(), time: 20, toast_id: None },
            HistoryEntry { app: "discord".into(), title: "msg 2".into(), body: String::new(), time: 10, toast_id: None },
        ];
        let groups = group_history_by_app(&entries);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "Discord");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "Steam");
        assert_eq!(groups[1].1.len(), 1);
    }
}
