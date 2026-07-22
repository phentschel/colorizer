#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;

use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
pub struct Config {
    pub enabled: bool,
    /// Measured screen value (0-255) at/below which dark_gamma applies.
    pub dark_point: f32,
    /// Measured screen value (0-255) at/above which bright_gamma applies.
    pub bright_point: f32,
    /// Boost applied for dark scenes; never below 1.0 (neutral).
    pub dark_gamma: f32,
    /// Gamma at/above the bright point; capped at 1.0 so already-bright
    /// screens never get brighter.
    pub bright_gamma: f32,
    /// Time to reach a new brightness target when the boost increases.
    pub transition_ms: u32,
    /// Time when the boost decreases (scene got brighter) — kept short so a
    /// suddenly bright screen stops glaring quickly.
    pub transition_down_ms: u32,
    /// Screen sampling interval.
    pub sample_ms: u32,
    /// Influence of each zone (center, TL, TR, BL, BR), 0-10.
    pub zone_weights: [u32; 5],
    /// Side length of each zone as % of the smaller screen dimension, 2-20.
    pub zone_sizes: [u32; 5],
    /// Toggle-hotkey modifiers as raw MOD_* bits (Alt=1, Ctrl=2, Shift=4,
    /// Win=8), without MOD_NOREPEAT. 0 = no hotkey.
    pub hotkey_mods: u32,
    /// Toggle-hotkey virtual-key code. 0 = no hotkey.
    pub hotkey_vk: u32,
}

// MODIFIERKEYS_FLAGS bits, kept as plain constants so the UI and config don't
// depend on the windows crate. MOD_NOREPEAT is added only at register time.
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CTRL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;

pub const ZONE_NAMES: [&str; 5] = ["Center", "Top-left", "Top-right", "Bottom-left", "Bottom-right"];

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            dark_point: 30.0,
            bright_point: 130.0,
            dark_gamma: 1.6,
            bright_gamma: 1.0,
            transition_ms: 800,
            transition_down_ms: 200,
            sample_ms: 500,
            zone_weights: [2, 1, 1, 1, 1],
            zone_sizes: [8; 5],
            hotkey_mods: MOD_CTRL | MOD_ALT,
            hotkey_vk: 0x42, // 'B'
        }
    }
}

/// "Ctrl+Alt+B" for the current hotkey, or "none" when unbound.
pub fn hotkey_label(mods: u32, vk: u32) -> String {
    if mods == 0 || vk == 0 {
        return "none".into();
    }
    let mut s = String::new();
    if mods & MOD_CTRL != 0 {
        s.push_str("Ctrl+");
    }
    if mods & MOD_ALT != 0 {
        s.push_str("Alt+");
    }
    if mods & MOD_SHIFT != 0 {
        s.push_str("Shift+");
    }
    if mods & MOD_WIN != 0 {
        s.push_str("Win+");
    }
    s.push_str(&key_name(vk));
    s
}

/// Human label for a virtual-key code from the pickable set.
pub fn key_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 => ((b'0' + (vk - 0x30) as u8) as char).to_string(),
        0x41..=0x5A => ((b'A' + (vk - 0x41) as u8) as char).to_string(),
        0x70..=0x7B => format!("F{}", vk - 0x70 + 1),
        _ => format!("0x{vk:02X}"),
    }
}

/// Virtual-key codes offered in the hotkey picker: 0-9, A-Z, F1-F12.
pub fn pickable_keys() -> Vec<u32> {
    (0x30..=0x39).chain(0x41..=0x5A).chain(0x70..=0x7B).collect()
}

pub struct Shared {
    pub cfg: Mutex<Config>,
    pub running: AtomicBool,
    /// Last measured screen value 0-255; u32::MAX = no sample yet.
    pub measured: AtomicU32,
    /// Currently applied gamma * 1000.
    pub gamma_milli: AtomicU32,
    pub capture_ok: AtomicBool,
    /// False while the driver rejects our gamma ramp.
    pub gamma_ok: AtomicBool,
    /// Primary screen size, packed (width << 32) | height; 0 = unknown.
    pub screen_dims: AtomicU64,
    /// Delayed calibration capture: (is_dark_point, fire_at). Set by the UI,
    /// executed by the engine so it works while the window is minimized.
    pub pending: Mutex<Option<(bool, std::time::Instant)>>,
    /// Last median brightness per zone (center, TL, TR, BL, BR).
    pub zone_medians: [AtomicU32; 5],
    /// Set when paused for a while: the engine re-enables at this instant.
    /// While set, `enabled` is held false so the boost is off.
    pub pause_until: Mutex<Option<std::time::Instant>>,
    /// One-shot: the engine restores the original ramp once, then clears it.
    pub force_restore: AtomicBool,
    /// Outcome of the last delayed calibration, for a UI confirmation:
    /// (is_dark, captured_value, completed_at). value = f32::NAN on timeout
    /// (no screen sample arrived). Set by the engine, shown then ignored by UI.
    pub calib_result: Mutex<Option<(bool, f32, std::time::Instant)>>,
}

/// Disable the boost now and schedule the engine to switch it back on after
/// `mins` minutes. Reuses the ordinary enabled/restore path — a pause is just
/// a timed re-enable.
pub fn pause_for(shared: &Shared, mins: u64) {
    let snap = {
        let mut c = shared.cfg.lock().unwrap();
        c.enabled = false;
        *c
    };
    save_config(&snap);
    *shared.pause_until.lock().unwrap() =
        Some(std::time::Instant::now() + Duration::from_secs(mins * 60));
}

/// Cancel any pending pause and turn the boost back on now.
pub fn resume(shared: &Shared) {
    *shared.pause_until.lock().unwrap() = None;
    let snap = {
        let mut c = shared.cfg.lock().unwrap();
        c.enabled = true;
        *c
    };
    save_config(&snap);
}

/// Audible calibration feedback. The window is minimized during the 3s capture
/// — usually behind an exclusive-fullscreen game where no overlay draws — so
/// sound is the only reliable channel. Three countdown ticks, then a rising
/// chime on success or a low buzz if no screen sample was captured.
fn spawn_calib_beeps(shared: Arc<Shared>, is_dark: bool) {
    use windows::Win32::System::Diagnostics::Debug::Beep;
    std::thread::spawn(move || unsafe {
        for _ in 0..3 {
            let _ = Beep(660, 120);
            std::thread::sleep(Duration::from_millis(880));
        }
        // Capture fires at ~3s; give the engine a beat to record the result.
        std::thread::sleep(Duration::from_millis(400));
        let captured = shared
            .calib_result
            .lock()
            .unwrap()
            .map(|(d, v, _)| d == is_dark && !v.is_nan())
            .unwrap_or(false);
        if captured {
            let _ = Beep(880, 120);
            let _ = Beep(1320, 200);
        } else {
            let _ = Beep(300, 500);
        }
    });
}

pub fn config_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("AutoBright");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.txt"))
}

pub fn save_config(cfg: &Config) {
    let Some(path) = config_path() else { return };
    let list = |a: &[u32; 5]| a.map(|v| v.to_string()).join(",");
    let s = format!(
        "enabled={}\ndark_point={}\nbright_point={}\ndark_gamma={}\nbright_gamma={}\ntransition_ms={}\ntransition_down_ms={}\nsample_ms={}\nzone_weights={}\nzone_sizes={}\nhotkey_mods={}\nhotkey_vk={}\n",
        cfg.enabled, cfg.dark_point, cfg.bright_point, cfg.dark_gamma, cfg.bright_gamma,
        cfg.transition_ms, cfg.transition_down_ms, cfg.sample_ms, list(&cfg.zone_weights), list(&cfg.zone_sizes),
        cfg.hotkey_mods, cfg.hotkey_vk
    );
    let _ = std::fs::write(path, s);
}

fn load_config() -> Config {
    let mut cfg = Config::default();
    let Some(path) = config_path() else { return cfg };
    let Ok(text) = std::fs::read_to_string(path) else { return cfg };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "enabled" => cfg.enabled = v.trim() == "true",
            "dark_point" => cfg.dark_point = v.trim().parse().unwrap_or(cfg.dark_point),
            "bright_point" => cfg.bright_point = v.trim().parse().unwrap_or(cfg.bright_point),
            "dark_gamma" => cfg.dark_gamma = v.trim().parse().unwrap_or(cfg.dark_gamma),
            "bright_gamma" => cfg.bright_gamma = v.trim().parse().unwrap_or(cfg.bright_gamma),
            "transition_ms" => cfg.transition_ms = v.trim().parse().unwrap_or(cfg.transition_ms),
            "transition_down_ms" => {
                cfg.transition_down_ms = v.trim().parse().unwrap_or(cfg.transition_down_ms)
            }
            "sample_ms" => cfg.sample_ms = v.trim().parse().unwrap_or(cfg.sample_ms),
            "zone_weights" => parse_list(v, &mut cfg.zone_weights),
            "zone_sizes" => parse_list(v, &mut cfg.zone_sizes),
            "hotkey_mods" => cfg.hotkey_mods = v.trim().parse().unwrap_or(cfg.hotkey_mods),
            "hotkey_vk" => cfg.hotkey_vk = v.trim().parse().unwrap_or(cfg.hotkey_vk),
            _ => {}
        }
    }
    sanitize(&mut cfg);
    cfg
}

fn parse_list(v: &str, out: &mut [u32; 5]) {
    for (slot, part) in out.iter_mut().zip(v.trim().split(',')) {
        if let Ok(n) = part.trim().parse() {
            *slot = n;
        }
    }
}

fn sanitize(cfg: &mut Config) {
    cfg.dark_point = cfg.dark_point.clamp(0.0, 250.0);
    cfg.bright_point = cfg.bright_point.clamp(cfg.dark_point + 5.0, 255.0);
    cfg.dark_gamma = cfg.dark_gamma.clamp(1.0, 3.0);
    // Bright scenes must never be boosted above neutral.
    cfg.bright_gamma = cfg.bright_gamma.clamp(0.5, 1.0);
    cfg.transition_ms = cfg.transition_ms.clamp(0, 5000);
    cfg.transition_down_ms = cfg.transition_down_ms.clamp(0, 5000);
    cfg.sample_ms = cfg.sample_ms.clamp(100, 5000);
    for w in &mut cfg.zone_weights {
        *w = (*w).min(10);
    }
    if cfg.zone_weights.iter().sum::<u32>() == 0 {
        cfg.zone_weights[0] = 1; // at least one zone must count
    }
    for s in &mut cfg.zone_sizes {
        *s = (*s).clamp(2, 100);
    }
    // Only the four real modifiers; a bare key with no modifier would swallow
    // that key globally, so drop the whole binding if no modifier survives.
    cfg.hotkey_mods &= MOD_ALT | MOD_CTRL | MOD_SHIFT | MOD_WIN;
    if cfg.hotkey_mods == 0 {
        cfg.hotkey_vk = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clamps_everything() {
        let mut cfg = Config {
            enabled: true,
            dark_point: 300.0,
            bright_point: 0.0,
            dark_gamma: 9.0,
            bright_gamma: 2.0,
            transition_ms: 99999,
            transition_down_ms: 99999,
            sample_ms: 1,
            zone_weights: [99; 5],
            zone_sizes: [0; 5],
            hotkey_mods: MOD_CTRL | MOD_ALT,
            hotkey_vk: 0x42,
        };
        sanitize(&mut cfg);
        assert_eq!(cfg.dark_point, 250.0);
        assert_eq!(cfg.bright_point, 255.0); // >= dark_point + 5
        assert_eq!(cfg.dark_gamma, 3.0);
        assert_eq!(cfg.bright_gamma, 1.0); // bright never boosts
        assert_eq!(cfg.transition_ms, 5000);
        assert_eq!(cfg.sample_ms, 100);
        assert_eq!(cfg.zone_weights, [10; 5]);
        assert_eq!(cfg.zone_sizes, [2; 5]);
    }

    #[test]
    fn sanitize_keeps_one_active_zone() {
        let mut cfg = Config { zone_weights: [0; 5], ..Config::default() };
        sanitize(&mut cfg);
        assert_eq!(cfg.zone_weights[0], 1);
    }

    #[test]
    fn sanitize_leaves_valid_config_alone() {
        let mut cfg = Config::default();
        let before = cfg;
        sanitize(&mut cfg);
        assert!(cfg == before);
    }

    #[test]
    fn sanitize_drops_modifierless_hotkey() {
        // A key with no modifier would swallow that key globally.
        let mut cfg = Config { hotkey_mods: 0, hotkey_vk: 0x42, ..Config::default() };
        sanitize(&mut cfg);
        assert_eq!(cfg.hotkey_vk, 0, "bare key must be cleared");
        // Stray non-modifier bits are masked off but a real modifier survives.
        let mut cfg = Config { hotkey_mods: MOD_CTRL | 0x4000, hotkey_vk: 0x42, ..Config::default() };
        sanitize(&mut cfg);
        assert_eq!(cfg.hotkey_mods, MOD_CTRL);
        assert_eq!(cfg.hotkey_vk, 0x42);
    }

    #[test]
    fn hotkey_label_formats() {
        assert_eq!(hotkey_label(MOD_CTRL | MOD_ALT, 0x42), "Ctrl+Alt+B");
        assert_eq!(hotkey_label(MOD_SHIFT | MOD_WIN, 0x70), "Shift+Win+F1");
        assert_eq!(hotkey_label(0, 0x42), "none");
        assert_eq!(hotkey_label(MOD_CTRL, 0), "none");
        assert_eq!(key_name(0x30), "0"); // digit
        assert_eq!(key_name(0x7B), "F12");
    }

    #[test]
    fn fmt_dur_rounds_up_minutes() {
        assert_eq!(fmt_dur(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_dur(Duration::from_secs(60)), "1m");
        assert_eq!(fmt_dur(Duration::from_secs(61)), "2m"); // never reads 0m with time left
    }

    #[test]
    fn parse_list_partial_and_garbage() {
        let mut out = [7u32; 5];
        parse_list("1,2,3", &mut out);
        assert_eq!(out, [1, 2, 3, 7, 7]); // short list keeps tail defaults

        let mut out = [7u32; 5];
        parse_list("1,x,3,4,5,6,7", &mut out);
        assert_eq!(out, [1, 7, 3, 4, 5]); // garbage keeps slot, extras ignored

        let mut out = [7u32; 5];
        parse_list(" 1 , 2 ,3,4,5", &mut out);
        assert_eq!(out, [1, 2, 3, 4, 5]); // whitespace tolerated
    }
}

struct App {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
    restore_after_capture: bool,
    tray: Option<tray_icon::TrayIcon>,
    /// Last icon/tooltip pushed to the tray, so they are only re-set on change.
    icon_enabled: bool,
    tooltip: String,
    /// Set by the tray Quit handler; lets the close request through.
    quit_flag: Arc<AtomicBool>,
    tray_toggle: tray_icon::menu::CheckMenuItem,
    /// Hiding and restoring both go through Win32 so winit's cached visibility
    /// can't drift out of sync with the window's real state.
    hwnd: isize,
    /// Mirrors the tray checkmark so it is only re-set when it actually flips.
    toggle_checked: bool,
    quitting: bool,
    autostart: bool,
    /// Watchdog: last engine respawn, rate-limited against panic storms.
    last_respawn: Option<std::time::Instant>,
    /// Set when the UI edited the config; saved to disk 500ms after the last
    /// edit so slider drags don't write every frame.
    dirty_at: Option<std::time::Instant>,
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn reg(args: &[&str]) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()
}

fn autostart_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", "AutoBright"]).is_some_and(|o| o.status.success())
}

fn set_autostart(on: bool) {
    if on {
        let Ok(exe) = std::env::current_exe() else { return };
        // Boot straight to the tray — it's a background tool.
        let d = format!("\"{}\" --hidden", exe.display());
        let _ = reg(&["add", RUN_KEY, "/v", "AutoBright", "/t", "REG_SZ", "/d", &d, "/f"]);
    } else {
        let _ = reg(&["delete", RUN_KEY, "/v", "AutoBright", "/f"]);
    }
}

/// Simple sun disc, built in code so no icon asset is needed. Warm when the
/// boost is enabled, grey when off/paused so tray state reads at a glance.
fn tray_icon(enabled: bool) -> tray_icon::Icon {
    const N: u32 = 32;
    let rgb = if enabled { [255u8, 200, 60] } else { [130, 130, 130] };
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            let a = if d < 10.0 { 255 } else { (255.0 * (12.0 - d) / 2.0).clamp(0.0, 255.0) as u8 };
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, N, N).expect("tray icon")
}

/// Compact "5m" / "42s" for the pause countdown; minutes round up so it never
/// reads 0m while time is left.
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}m", s.div_ceil(60))
    } else {
        format!("{s}s")
    }
}

impl App {
    /// Miniature of the screen showing where each zone sits, how big it is,
    /// how much influence it has (fill opacity) and its live measured value.
    fn zone_preview(&self, ui: &mut egui::Ui, cfg: &Config) {
        let dims = self.shared.screen_dims.load(Ordering::Relaxed);
        let (sw, sh) = if dims == 0 {
            (1920u32, 1080u32) // no capture yet: assume 16:9 for the sketch
        } else {
            ((dims >> 32) as u32, (dims & 0xFFFF_FFFF) as u32)
        };
        let avail = ui.available_width().min(370.0);
        let scale = avail / sw as f32;
        let (resp, p) = ui.allocate_painter(
            egui::vec2(sw as f32 * scale, sh as f32 * scale),
            egui::Sense::hover(),
        );
        let origin = resp.rect.min;
        p.rect_filled(resp.rect, 3.0, ui.visuals().extreme_bg_color);

        for (i, &(x, y, z)) in engine::zone_rects(sw, sh, &cfg.zone_sizes).iter().enumerate() {
            let rect = egui::Rect::from_min_size(
                origin + egui::vec2(x as f32 * scale, y as f32 * scale),
                egui::Vec2::splat(z as f32 * scale),
            );
            let alpha = 25 + 18 * cfg.zone_weights[i] as u8; // influence -> opacity
            p.rect_filled(rect, 2.0, egui::Color32::from_rgba_unmultiplied(90, 150, 250, alpha));
            p.rect_stroke(
                rect,
                2.0,
                egui::Stroke::new(1.0_f32, egui::Color32::from_gray(200)),
                egui::StrokeKind::Inside,
            );
            let med = self.shared.zone_medians[i].load(Ordering::Relaxed);
            let text = if self.shared.measured.load(Ordering::Relaxed) == u32::MAX {
                format!("{}", cfg.zone_weights[i])
            } else {
                format!("{med}")
            };
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
        }
        ui.label("Fill = influence, number = measured brightness of that zone.");
    }
}

impl App {
    /// Asks Win32 directly rather than tracking a flag, so it can't drift out
    /// of sync with the window the way winit's cached visibility did.
    fn window_visible(&self) -> bool {
        if self.hwnd == 0 {
            return true;
        }
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
        unsafe { IsWindowVisible(HWND(self.hwnd as *mut _)) }.as_bool()
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.quit_flag.load(Ordering::Relaxed) {
            self.quitting = true;
        }
        // Engine watchdog: a panicked worker restored the ramp (Drop) but left
        // sampling dead — respawn it, at most every 30s.
        if !self.quitting
            && self.worker.as_ref().is_some_and(|h| h.is_finished())
            && self.last_respawn.is_none_or(|t| t.elapsed() > Duration::from_secs(30))
        {
            let _ = self.worker.take().map(|h| h.join());
            self.worker = Some(engine::spawn(self.shared.clone()));
            self.last_respawn = Some(std::time::Instant::now());
        }
        // Closing the window hides to tray; only the tray Quit really exits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.hwnd != 0 {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
                unsafe {
                    let _ = ShowWindow(HWND(self.hwnd as *mut _), SW_HIDE);
                }
            }
        }

        // Un-minimize once the delayed calibration capture went through.
        if self.restore_after_capture && self.shared.pending.lock().unwrap().is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            self.restore_after_capture = false;
        }
        let measured = self.shared.measured.load(Ordering::Relaxed);
        let gamma = self.shared.gamma_milli.load(Ordering::Relaxed) as f32 / 1000.0;
        let capture_ok = self.shared.capture_ok.load(Ordering::Relaxed);

        let mut cfg = *self.shared.cfg.lock().unwrap();
        let mut before = cfg;

        // Button intents, applied after the panel closes so they don't race the
        // config-diff write below.
        let mut act_pause: Option<u64> = None;
        let mut act_resume = false;
        let mut act_reset_defaults = false;
        let mut act_reset_display = false;
        let paused_left = self
            .shared
            .pause_until
            .lock()
            .unwrap()
            .map(|t| t.saturating_duration_since(std::time::Instant::now()));

        egui::CentralPanel::default().show(ctx, |ui| {
            let hk = hotkey_label(cfg.hotkey_mods, cfg.hotkey_vk);
            ui.checkbox(&mut cfg.enabled, format!("Auto brightness enabled ({hk})"));
            if ui.checkbox(&mut self.autostart, "Start with Windows").changed() {
                set_autostart(self.autostart);
            }
            if let Some(left) = paused_left {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        egui::Color32::LIGHT_BLUE,
                        format!("Paused — resumes in {}", fmt_dur(left)),
                    );
                    if ui.button("Resume now").clicked() {
                        act_resume = true;
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label("Pause:");
                    if ui.button("30 min").clicked() {
                        act_pause = Some(30);
                    }
                    if ui.button("1 hour").clicked() {
                        act_pause = Some(60);
                    }
                    if ui.button("2 hours").clicked() {
                        act_pause = Some(120);
                    }
                });
            }
            ui.separator();

            if cfg.enabled && !self.shared.gamma_ok.load(Ordering::Relaxed) {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Driver rejected the gamma ramp — try a lower gamma value",
                );
            }
            if !capture_ok && cfg.enabled {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Screen capture unavailable (exclusive fullscreen or HDR?) — retrying",
                );
            } else if measured == u32::MAX {
                ui.label("Screen brightness: (no sample yet)");
            } else {
                ui.label(format!("Screen brightness: {measured} / 255"));
            }
            ui.label(format!("Applied gamma: {gamma:.2}"));
            ui.separator();

            ui.heading("Calibration");
            ui.label("Capture a dark and a bright reference scene, or type values.");
            let pending = *self.shared.pending.lock().unwrap();
            if let Some((is_dark, at)) = pending {
                let left = at.saturating_duration_since(std::time::Instant::now());
                ui.colored_label(
                    egui::Color32::LIGHT_BLUE,
                    format!(
                        "Capturing {} point in {:.1}s — bring the game to the front",
                        if is_dark { "dark" } else { "bright" },
                        left.as_secs_f32()
                    ),
                );
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            } else if let Some((is_dark, val, at)) = *self.shared.calib_result.lock().unwrap() {
                // Confirm the finished capture for a few seconds — the window was
                // minimized during the countdown, so this is the user's "done" signal.
                let left = Duration::from_secs(6).saturating_sub(at.elapsed());
                if !left.is_zero() {
                    if val.is_nan() {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            "Calibration failed — no screen sample (fullscreen/HDR?)",
                        );
                    } else {
                        ui.colored_label(
                            egui::Color32::GREEN,
                            format!(
                                "\u{2713} {} point captured: {val:.0}",
                                if is_dark { "Dark" } else { "Bright" }
                            ),
                        );
                    }
                    ui.ctx().request_repaint_after(left);
                }
            }
            let mut arm = |ui: &mut egui::Ui, is_dark: bool| {
                *self.shared.calib_result.lock().unwrap() = None;
                *self.shared.pending.lock().unwrap() =
                    Some((is_dark, std::time::Instant::now() + Duration::from_secs(3)));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.restore_after_capture = true;
                spawn_calib_beeps(self.shared.clone(), is_dark);
            };
            ui.horizontal(|ui| {
                if ui.button("Set dark point from screen (3s)").clicked() {
                    arm(ui, true);
                }
                ui.add(egui::DragValue::new(&mut cfg.dark_point).range(0.0..=250.0).speed(1));
            });
            ui.horizontal(|ui| {
                if ui.button("Set bright point from screen (3s)").clicked() {
                    arm(ui, false);
                }
                ui.add(egui::DragValue::new(&mut cfg.bright_point).range(5.0..=255.0).speed(1));
            });
            ui.add(egui::Slider::new(&mut cfg.dark_gamma, 1.0..=3.0).text("Gamma at dark point"));
            ui.add(egui::Slider::new(&mut cfg.bright_gamma, 0.5..=1.0).text("Gamma at bright point"));
            ui.label("Bright scenes are never boosted: 1.0 = leave untouched.");
            ui.separator();

            ui.heading("Zones");
            egui::Grid::new("zones").num_columns(3).show(ui, |ui| {
                ui.label("");
                ui.label("Influence (0-10)");
                ui.label("Size (% of screen)");
                ui.end_row();
                for i in 0..5 {
                    ui.label(ZONE_NAMES[i]);
                    ui.add(egui::DragValue::new(&mut cfg.zone_weights[i]).range(0..=10));
                    ui.add(egui::DragValue::new(&mut cfg.zone_sizes[i]).range(2..=100));
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            self.zone_preview(ui, &cfg);
            ui.separator();

            ui.add(
                egui::Slider::new(&mut cfg.transition_ms, 0..=5000)
                    .text("Transition up (ms, boost increase)")
                    .step_by(50.0),
            );
            ui.add(
                egui::Slider::new(&mut cfg.transition_down_ms, 0..=5000)
                    .text("Transition down (ms, glare reduction)")
                    .step_by(50.0),
            );
            ui.add(
                egui::Slider::new(&mut cfg.sample_ms, 100..=5000)
                    .text("Sample interval (ms)")
                    .step_by(50.0),
            );
            ui.separator();

            ui.heading("Toggle hotkey");
            ui.horizontal(|ui| {
                let mut ctrl = cfg.hotkey_mods & MOD_CTRL != 0;
                let mut alt = cfg.hotkey_mods & MOD_ALT != 0;
                let mut shift = cfg.hotkey_mods & MOD_SHIFT != 0;
                let mut win = cfg.hotkey_mods & MOD_WIN != 0;
                ui.checkbox(&mut ctrl, "Ctrl");
                ui.checkbox(&mut alt, "Alt");
                ui.checkbox(&mut shift, "Shift");
                ui.checkbox(&mut win, "Win");
                let mut m = 0;
                for (on, bit) in [(ctrl, MOD_CTRL), (alt, MOD_ALT), (shift, MOD_SHIFT), (win, MOD_WIN)] {
                    if on {
                        m |= bit;
                    }
                }
                cfg.hotkey_mods = m;
                let sel = if cfg.hotkey_vk == 0 { "—".to_string() } else { key_name(cfg.hotkey_vk) };
                egui::ComboBox::from_id_salt("hotkey_key")
                    .selected_text(sel)
                    .show_ui(ui, |ui| {
                        for vk in pickable_keys() {
                            ui.selectable_value(&mut cfg.hotkey_vk, vk, key_name(vk));
                        }
                    });
            });
            ui.label(format!(
                "Current: {} (needs at least one modifier)",
                hotkey_label(cfg.hotkey_mods, cfg.hotkey_vk)
            ));
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("Reset settings to defaults").clicked() {
                    act_reset_defaults = true;
                }
                if ui
                    .button("Reset display now")
                    .on_hover_text("Put the original gamma ramp back if it looks stuck")
                    .clicked()
                {
                    act_reset_display = true;
                }
            });
        });

        if let Some(mins) = act_pause {
            pause_for(&self.shared, mins);
            cfg = *self.shared.cfg.lock().unwrap();
            before = cfg;
        }
        if act_resume {
            resume(&self.shared);
            cfg = *self.shared.cfg.lock().unwrap();
            before = cfg;
        }
        if act_reset_defaults {
            *self.shared.pause_until.lock().unwrap() = None;
            cfg = Config::default(); // saved by the diff block below
        }
        if act_reset_display {
            self.shared.force_restore.store(true, Ordering::Relaxed);
        }
        // Manually re-enabling (checkbox) cancels a running pause.
        if cfg.enabled && !before.enabled {
            *self.shared.pause_until.lock().unwrap() = None;
        }

        if cfg != before {
            sanitize(&mut cfg);
            *self.shared.cfg.lock().unwrap() = cfg;
            self.dirty_at = Some(std::time::Instant::now());
        }
        if self.dirty_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
            save_config(&self.shared.cfg.lock().unwrap());
            self.dirty_at = None;
        }
        let enabled = self.shared.cfg.lock().unwrap().enabled;
        if enabled != self.toggle_checked {
            self.tray_toggle.set_checked(enabled);
            self.toggle_checked = enabled;
        }

        // Live tray icon + tooltip: colour tracks enabled, tooltip shows the
        // current boost or the pause countdown. Only pushed on change.
        let paused_now = self
            .shared
            .pause_until
            .lock()
            .unwrap()
            .map(|t| t.saturating_duration_since(std::time::Instant::now()));
        let tip = if let Some(left) = paused_now {
            format!("AutoBright — paused, {} left", fmt_dur(left))
        } else if !enabled {
            "AutoBright — off".to_string()
        } else {
            format!("AutoBright — boost {gamma:.2}x")
        };
        if let Some(tray) = &self.tray {
            if enabled != self.icon_enabled {
                let _ = tray.set_icon(Some(tray_icon(enabled)));
                self.icon_enabled = enabled;
            }
            if tip != self.tooltip {
                let _ = tray.set_tooltip(Some(&tip));
                self.tooltip = tip;
            }
        }

        // Faster ticks while a capture countdown runs (also keeps update()
        // firing while minimized so the restore above happens promptly).
        // Hidden in the tray there is nothing to draw, so drop to a slow
        // heartbeat instead of relayouting the whole UI four times a second
        // behind an invisible window. Restoring posts a paint, which brings
        // the normal cadence back immediately; the heartbeat is only a
        // self-heal in case it doesn't.
        let ms = if self.restore_after_capture {
            100
        } else if self.window_visible() {
            250
        } else {
            2000
        };
        ctx.request_repaint_after(Duration::from_millis(ms));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.dirty_at.take().is_some() {
            save_config(&self.shared.cfg.lock().unwrap());
        }
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join(); // worker restores the original gamma ramp on exit
        }
    }
}

/// Release builds are the `windows` subsystem (no console), so println! from a
/// CLI subcommand is otherwise lost. Reattach to the launching terminal's
/// console so `--probe`/`--restore` output is actually visible.
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--probe") {
        attach_console();
        engine::probe();
        return Ok(());
    }

    // Two instances would fight over the gamma ramp; the handle is held for
    // the process lifetime and released by the OS on exit.
    let already_running = unsafe {
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        let _ = windows::Win32::System::Threading::CreateMutexW(
            None,
            true,
            windows::core::w!("AutoBright_SingleInstance"),
        );
        GetLastError() == ERROR_ALREADY_EXISTS
    };

    // Repairs a boost left on screen by a killed process, so it only makes
    // sense when no process owns the ramp. Against a live instance it would
    // clear the marker out from under it, leaving that instance boosted with
    // no crash-recovery breadcrumb.
    if std::env::args().any(|a| a == "--restore") {
        attach_console();
        if already_running {
            println!("restore: AutoBright is running — quit it from the tray instead");
        } else {
            engine::restore_ramp();
        }
        return Ok(());
    }

    if already_running {
        return Ok(());
    }

    let shared = Arc::new(Shared {
        cfg: Mutex::new(load_config()),
        running: AtomicBool::new(true),
        measured: AtomicU32::new(u32::MAX),
        gamma_milli: AtomicU32::new(1000),
        capture_ok: AtomicBool::new(false),
        gamma_ok: AtomicBool::new(true),
        screen_dims: AtomicU64::new(0),
        zone_medians: Default::default(),
        pending: Mutex::new(None),
        pause_until: Mutex::new(None),
        force_restore: AtomicBool::new(false),
        calib_result: Mutex::new(None),
    });
    let worker = Some(engine::spawn(shared.clone()));

    let start_hidden = std::env::args().any(|a| a == "--hidden");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 780.0])
            .with_min_inner_size([360.0, 520.0])
            .with_visible(!start_hidden),
        ..Default::default()
    };
    eframe::run_native(
        "AutoBright",
        options,
        Box::new(move |cc| {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, Submenu};
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{
                PostMessageW, SetForegroundWindow, ShowWindow, SW_RESTORE, WM_CLOSE,
            };

            let enabled = shared.cfg.lock().unwrap().enabled;
            let quit_flag = Arc::new(AtomicBool::new(false));

            let show = MenuItem::new("Show settings", true, None);
            let toggle = CheckMenuItem::new("Auto brightness", true, enabled, None);
            let pause30 = MenuItem::new("30 minutes", true, None);
            let pause60 = MenuItem::new("1 hour", true, None);
            let pause120 = MenuItem::new("2 hours", true, None);
            let pause_menu = Submenu::new("Pause", true);
            let _ = pause_menu.append_items(&[&pause30, &pause60, &pause120]);
            let resume_item = MenuItem::new("Resume now", true, None);
            let quit = MenuItem::new("Quit", true, None);
            let menu = Menu::new();
            let _ = menu.append_items(&[&show, &toggle, &pause_menu, &resume_item, &quit]);

            // A hidden window gets no redraw events, so update() stops running
            // and can't process anything — the handlers below therefore act
            // directly via Win32 instead of forwarding to the UI thread.
            let hwnd: isize = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                _ => 0,
            };
            let show_window = move || unsafe {
                if hwnd != 0 {
                    let h = HWND(hwnd as *mut _);
                    let _ = ShowWindow(h, SW_RESTORE);
                    let _ = SetForegroundWindow(h);
                }
            };

            let show_id = show.id().clone();
            let toggle_id = toggle.id().clone();
            let (p30_id, p60_id, p120_id) =
                (pause30.id().clone(), pause60.id().clone(), pause120.id().clone());
            let resume_id = resume_item.id().clone();
            {
                let shared = shared.clone();
                let quit_flag = quit_flag.clone();
                let repaint = cc.egui_ctx.clone();
                MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                    let id = e.id();
                    if *id == show_id {
                        show_window();
                    } else if *id == toggle_id {
                        // Toggling on also cancels any running pause.
                        let snap = {
                            let mut c = shared.cfg.lock().unwrap();
                            c.enabled = !c.enabled;
                            *c
                        };
                        if snap.enabled {
                            *shared.pause_until.lock().unwrap() = None;
                        }
                        save_config(&snap);
                    } else if *id == p30_id {
                        pause_for(&shared, 30);
                    } else if *id == p60_id {
                        pause_for(&shared, 60);
                    } else if *id == p120_id {
                        pause_for(&shared, 120);
                    } else if *id == resume_id {
                        resume(&shared);
                    } else {
                        quit_flag.store(true, Ordering::Relaxed);
                        // WM_CLOSE reaches the queue even while hidden.
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(hwnd as *mut _)),
                                WM_CLOSE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                    repaint.request_repaint();
                }));
            }
            let repaint = cc.egui_ctx.clone();
            tray_icon::TrayIconEvent::set_event_handler(Some(move |e: tray_icon::TrayIconEvent| {
                if matches!(
                    e,
                    tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    } | tray_icon::TrayIconEvent::DoubleClick { .. }
                ) {
                    show_window();
                    repaint.request_repaint();
                }
            }));

            let tray = tray_icon::TrayIconBuilder::new()
                .with_tooltip("AutoBright")
                .with_icon(tray_icon(enabled))
                .with_menu(Box::new(menu))
                .build()
                .ok();

            Ok(Box::new(App {
                shared,
                worker,
                restore_after_capture: false,
                tray,
                icon_enabled: enabled,
                tooltip: "AutoBright".to_string(),
                quit_flag,
                tray_toggle: toggle,
                hwnd,
                toggle_checked: enabled,
                quitting: false,
                autostart: autostart_enabled(),
                last_respawn: None,
                dirty_at: None,
            }))
        }),
    )
}
