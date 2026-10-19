//! Sampling + gamma worker. Everything here runs on one dedicated thread so the
//! UI and (more importantly) games never wait on us.
//!
//! Stutter-avoidance design, learned from BrightRaider's bug reports:
//! - No GDI BitBlt readbacks: DXGI Desktop Duplication with AcquireNextFrame(0)
//!   — a static screen produces no frame and costs nothing.
//! - Only 5 small zones are copied GPU-side into one tiny staging texture; the
//!   CPU readback is ~200 KB per sample at any screen resolution.
//! - SetDeviceGammaRamp is called only when the value actually changed.
//! - The device is created on the adapter that owns the primary output, which
//!   keeps hybrid-GPU laptops on the fast path.

use crate::overlay_layout::{zone_rects, PATCH};
use crate::{Config, Shared};
use std::sync::atomic::Ordering::Relaxed;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput6,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, GetDC, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Threading::{
    OpenMutexW, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, INFINITE,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::ColorSystem::{GetDeviceGammaRamp, SetDeviceGammaRamp};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, MsgWaitForMultipleObjects, PeekMessageW, MSG,
    PM_REMOVE, QS_HOTKEY, WM_HOTKEY,
};


pub fn spawn(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("autobright-engine".into())
        .spawn(move || {
            // Gamma's Drop restores the original ramp even if run() panics.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&shared)));
        })
        .expect("spawn engine thread")
}

/// `--restore`: put the saved original ramp back and clear the marker, for when
/// a hard kill (Task Manager, crash, power loss) left the boost applied. A killed
/// process cannot restore its own ramp — nothing runs on TerminateProcess — so
/// this is the out-of-band repair. No-op when the marker says nothing was boosted.
pub fn restore_ramp() {
    let mut g = Gamma::new();
    let had_boost = g.marker;
    g.restore();
    if had_boost {
        println!("restore: original gamma ramp put back");
    } else {
        println!("restore: nothing was boosted, ramp left alone");
    }
}

/// Called by the watchdog after the owning process is gone. Same repair as
/// `--restore`, but silent and only acts when the marker says a boost was left.
fn restore_if_orphaned() {
    let mut g = Gamma::new();
    g.restore();
}

/// `--watchdog <pid>`: a tiny sibling process the app spawns at startup. It
/// sleeps until the app exits and then puts the original ramp back if the app
/// died mid-boost (Task Manager, crash, ...). Nothing runs inside a
/// TerminateProcess'd process, so the repair has to live outside it. A normal
/// exit clears the marker first, which makes this a no-op.
pub fn watchdog(parent_pid: u32) {
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, parent_pid) {
            WaitForSingleObject(h, INFINITE);
            let _ = CloseHandle(h);
        }
        // Give a relaunch (autostart, user) a moment to take the single-instance
        // mutex; a live instance owns the ramp and must not be fought.
        std::thread::sleep(Duration::from_millis(1500));
        if !instance_running() {
            restore_if_orphaned();
        }
    }
}

/// True while another AutoBright process owns the single-instance mutex.
pub fn instance_running() -> bool {
    unsafe {
        match OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            windows::core::w!("AutoBright_SingleInstance"),
        ) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// `--probe-ramp`: verifies the assumption the whole feedback loop rests on —
/// that Desktop Duplication sees the framebuffer *before* the gamma ramp. If it
/// saw the boosted image, brightening would darken the measurement and the
/// boost would oscillate. Applies a strong temporary boost (restored on exit)
/// and checks that the measured value does not follow it.
pub fn probe_ramp() {
    let cfg = Config::default();
    let mut cap = match Capture::new((cfg.zone_sizes, cfg.zone_pos)) {
        Ok(c) => c,
        Err(e) => {
            println!("probe-ramp: capture init failed: {e}");
            return;
        }
    };
    // A fresh sample needs a new desktop frame, so a static screen can stall.
    let next = |cap: &mut Capture| -> Option<u8> {
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            match cap.sample() {
                Ok(Some(m)) => return Some(combine(&m, &cfg.zone_weights)),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        None
    };
    let Some(base) = next(&mut cap) else {
        println!("probe-ramp: no baseline frame — move the mouse or play something, then retry");
        return;
    };
    let mut gamma = Gamma::new();
    gamma.set(2.0);
    println!("probe-ramp: driver accepted boost 2.0: {}", gamma.ok);
    let after = next(&mut cap);
    drop(gamma); // restores the original ramp
    match after {
        Some(v) if (v as i32 - base as i32).abs() <= 6 => {
            println!("probe-ramp: OK — capture is pre-ramp (baseline {base}, boosted {v})")
        }
        Some(v) => println!(
            "probe-ramp: WARNING — measurement followed the ramp (baseline {base}, boosted {v}); \
             the boost loop may oscillate. (Screen content changing between frames can cause \
             this too: retry on a quiet desktop.)"
        ),
        None => println!("probe-ramp: inconclusive — no frame after the ramp change (static screen)"),
    }
}

/// `--probe`: one capture + sample, printed to the console. Never touches gamma.
pub fn probe() {
    let cfg = Config::default();
    match Capture::new((cfg.zone_sizes, cfg.zone_pos)) {
        Ok(mut cap) => {
            for attempt in 1..=20 {
                std::thread::sleep(Duration::from_millis(100));
                match cap.sample() {
                    Ok(Some(m)) => {
                        let v = combine(&m, &cfg.zone_weights);
                        println!("probe ok: zones {m:?}, weighted {v}/255 (attempt {attempt})");
                        return;
                    }
                    Ok(None) => continue, // no new frame yet
                    Err(e) => {
                        println!("probe: sample failed: {e}");
                        return;
                    }
                }
            }
            println!("probe: capture initialized but no frame arrived (static screen?)");
        }
        Err(e) => println!("probe: capture init failed: {e}"),
    }
}

/// (Re)register the global toggle hotkey on id 1. Always unregisters first so a
/// changed binding replaces the old one; a zero binding just clears it.
/// Returns false when Windows refused the binding (another program owns it).
fn register_hotkey(mods: u32, vk: u32) -> bool {
    unsafe {
        let _ = UnregisterHotKey(None, 1);
        if mods != 0 && vk != 0 {
            let flags = HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT;
            return RegisterHotKey(None, 1, flags, vk).is_ok();
        }
        true
    }
}

/// File name (lowercase) of the process owning the foreground window.
fn foreground_exe() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(|s| s.to_ascii_lowercase())
    }
}

/// `list` is user text: exe names separated by commas, semicolons or newlines,
/// compared case-insensitively against the foreground exe's file name.
fn is_excluded(list: &str, exe: &str) -> bool {
    list.split([',', ';', '\n'])
        .map(|s| s.trim().trim_matches('"'))
        .any(|s| !s.is_empty() && s.eq_ignore_ascii_case(exe))
}

fn run(shared: &Shared) {
    let mut gamma = Gamma::new();
    let mut cap: Option<Capture> = None;
    let mut next_sample = Instant::now();
    let mut next_reinit = Instant::now();
    let mut next_heal = Instant::now();
    // Grows while capture keeps failing (exclusive fullscreen): repeated
    // D3D11CreateDevice attempts are driver work a loaded GPU can feel.
    let mut reinit_backoff = Duration::from_secs(2);

    // The toggle hotkey works from inside a fullscreen game. Registered on this
    // thread so WM_HOTKEY lands in this thread's message queue; re-registered
    // whenever the user changes it in the UI.
    let mut hotkey = {
        let c = shared.cfg.lock().unwrap();
        (c.hotkey_mods, c.hotkey_vk)
    };
    shared.hotkey_ok.store(register_hotkey(hotkey.0, hotkey.1), Relaxed);

    // Foreground-app exclusion: polled twice a second, not every tick.
    let own_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default();
    let mut next_fg = Instant::now();
    let mut suppressed = false;
    let mut was_suppressed = false;

    // Transition state: eased ramp from `start` to `target` beginning at `t0`.
    let mut cur = 1.0f32;
    let mut start = 1.0f32;
    let mut target = 1.0f32;
    let mut t0 = Instant::now();
    let mut dur_s = 0.001f32;
    let mut last_measured = f32::MIN;
    // A boost increase waiting out the hold: (wanted gamma, first seen).
    let mut pending_up: Option<(f32, Instant)> = None;

    while shared.running.load(Relaxed) {
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_HOTKEY {
                let snap = {
                    let mut c = shared.cfg.lock().unwrap();
                    c.enabled = !c.enabled;
                    *c
                };
                crate::save_config(&snap);
            }
        }

        // A scheduled pause elapsed: switch the boost back on before this tick
        // reads the config, so `enabled` already reflects the resume.
        {
            let mut p = shared.pause_until.lock().unwrap();
            if p.is_some_and(|t| Instant::now() >= t) {
                *p = None;
                drop(p);
                let snap = {
                    let mut c = shared.cfg.lock().unwrap();
                    c.enabled = true;
                    *c
                };
                crate::save_config(&snap);
            }
        }

        let cfg = *shared.cfg.lock().unwrap();
        let pending = *shared.pending.lock().unwrap();

        // Hotkey changed in the UI: rebind.
        if (cfg.hotkey_mods, cfg.hotkey_vk) != hotkey {
            shared
                .hotkey_ok
                .store(register_hotkey(cfg.hotkey_mods, cfg.hotkey_vk), Relaxed);
            hotkey = (cfg.hotkey_mods, cfg.hotkey_vk);
        }

        if Instant::now() >= next_fg {
            next_fg = Instant::now() + Duration::from_millis(500);
            if let Some(exe) = foreground_exe() {
                suppressed = is_excluded(&shared.exclude.lock().unwrap(), &exe);
                if exe != own_exe {
                    let mut last = shared.last_app.lock().unwrap();
                    if *last != exe {
                        *last = exe;
                    }
                }
            }
        }
        shared.suppressed.store(suppressed && cfg.enabled, Relaxed);

        // UI asked to put the original ramp back right now (stuck-gamma kick).
        // If still enabled, the next tick recomputes cleanly from the original.
        if shared.force_restore.swap(false, Relaxed) {
            gamma.restore();
            shared.gamma_milli.store(1000, Relaxed);
            cur = 1.0;
            start = 1.0;
            target = 1.0;
            last_measured = f32::MIN;
            pending_up = None;
        }

        // A pending calibration capture keeps sampling alive even while off;
        // otherwise, while disabled, skip sampling and idle (reset below).
        let active = cfg.enabled || pending.is_some();

        // Turned off: release the D3D11 device and the duplication handle
        // instead of holding them idle. Desktop Duplication keeps the capture
        // path alive on the GPU, which a disabled background app has no
        // business doing. Re-enabling pays one D3D11CreateDevice.
        if !active && cap.is_some() {
            cap = None;
            shared.capture_ok.store(false, Relaxed);
            shared.hdr_active.store(false, Relaxed);
            next_reinit = Instant::now();
            reinit_backoff = Duration::from_secs(2);
        }

        let now = Instant::now();

        // Fresh target from this tick's measurement, if any (see `gate`).
        let mut new_want: Option<f32> = None;

        if active && now >= next_sample {
            next_sample = now + Duration::from_millis(cfg.sample_ms as u64);

            // Zone sizes changed in the UI: just recompute the rectangles.
            if let Some(c) = cap.as_mut() {
                if c.built != (cfg.zone_sizes, cfg.zone_pos) {
                    c.rects = zone_rects(c.width, c.height, &cfg.zone_sizes, &cfg.zone_pos);
                    c.built = (cfg.zone_sizes, cfg.zone_pos);
                }
            }
            if cap.is_none() && now >= next_reinit {
                match Capture::new((cfg.zone_sizes, cfg.zone_pos)) {
                    Ok(c) => {
                        shared.screen_dims.store(
                            ((c.width as u64) << 32) | c.height as u64,
                            Relaxed,
                        );
                        shared.screen_origin.store(
                            crate::overlay_layout::pack_origin(c.origin.0, c.origin.1),
                            Relaxed,
                        );
                        cap = Some(c);
                        shared.capture_ok.store(true, Relaxed);
                        shared.hdr_active.store(false, Relaxed);
                        reinit_backoff = Duration::from_secs(2);
                        // Capture coming back often means the ramp was reset
                        // too (game exit, resume from sleep) — heal now.
                        next_heal = now;
                    }
                    Err(e) => {
                        shared.capture_ok.store(false, Relaxed);
                        shared.hdr_active.store(matches!(e, CaptureError::Hdr), Relaxed);
                        next_reinit = now + reinit_backoff;
                        reinit_backoff = (reinit_backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }

            if let Some(c) = cap.as_mut() {
                match c.sample() {
                    Ok(Some(medians)) => {
                        for (slot, m) in shared.zone_medians.iter().zip(&medians) {
                            slot.store(*m as u32, Relaxed);
                        }
                        let m = combine(&medians, &cfg.zone_weights);
                        shared.measured.store(m as u32, Relaxed);
                        let mf = m as f32;
                        // Deadband: ±3 counts of flicker never retargets.
                        if (mf - last_measured).abs() >= 3.0 {
                            last_measured = mf;
                            new_want = Some(target_gamma(&cfg, mf));
                        }
                    }
                    Ok(None) => {} // no new frame: screen static, nothing to do
                    Err(_) => {
                        // Mode change / fullscreen handoff: rebuild duplication.
                        // Mode changes often reset the hardware ramp too, so
                        // heal immediately instead of waiting for the timer.
                        cap = None;
                        shared.capture_ok.store(false, Relaxed);
                        next_reinit = now + Duration::from_millis(500);
                        reinit_backoff = Duration::from_secs(2);
                    }
                }
            }
        }

        // Boost decreases apply at once; increases must persist for hold_ms.
        // Runs every tick so a held increase still lands on a static screen.
        if let Some(want) = gate(
            &mut pending_up,
            new_want,
            target,
            now,
            Duration::from_millis(cfg.hold_ms as u64),
        ) {
            start = cur;
            target = want;
            t0 = now;
            // Less boost = the scene got brighter: ramp down fast so it
            // doesn't glare.
            let ms = if want < cur { cfg.transition_down_ms } else { cfg.transition_ms };
            dur_s = ms.max(1) as f32 / 1000.0;
        }

        // Delayed calibration capture: fire once the countdown elapsed and a
        // sample exists; give up 3s past the deadline if none ever arrives.
        if let Some((is_dark, at)) = pending {
            if now >= at {
                let m = shared.measured.load(Relaxed);
                if m != u32::MAX {
                    let snap = {
                        let mut c = shared.cfg.lock().unwrap();
                        if is_dark {
                            c.dark_point = (m as f32).min(250.0);
                        } else {
                            c.bright_point = m as f32;
                        }
                        c.bright_point = c.bright_point.max(c.dark_point + 5.0).min(255.0);
                        *c
                    };
                    crate::save_config(&snap);
                    let val = if is_dark { snap.dark_point } else { snap.bright_point };
                    *shared.calib_result.lock().unwrap() = Some((is_dark, val, now));
                    *shared.pending.lock().unwrap() = None;
                } else if now >= at + Duration::from_secs(3) {
                    *shared.calib_result.lock().unwrap() = Some((is_dark, f32::NAN, now));
                    *shared.pending.lock().unwrap() = None;
                }
            }
        }

        if now >= next_heal {
            // The primary display changed (user re-arranged monitors): the
            // ramp we hold and the output we duplicate belong to the old one.
            // Put the old display back, then rebuild both on the new primary.
            if let Some((m, _)) = primary_monitor() {
                if gamma.monitor.is_some_and(|g| g != m) {
                    gamma.restore();
                    gamma = Gamma::new();
                    cap = None;
                    shared.capture_ok.store(false, Relaxed);
                    next_reinit = now;
                    reinit_backoff = Duration::from_secs(2);
                    start = cur; // hold the current boost until new samples arrive
                    target = cur;
                    last_measured = f32::MIN;
                    pending_up = None;
                }
            }
            // Only while capture works: with capture broken (exclusive
            // fullscreen) the game owns the ramp — fighting it flickers.
            if cap.is_some() {
                gamma.heal();
            }
            next_heal = now + Duration::from_secs(5);
        }

        let mut transitioning = false;
        if cfg.enabled && !suppressed {
            if was_suppressed {
                // Excluded app lost the foreground: ease the boost back in.
                start = 1.0;
                t0 = now;
                dur_s = cfg.transition_ms.max(1) as f32 / 1000.0;
            }
            let e = (now - t0).as_secs_f32() / dur_s;
            transitioning = e < 1.0 && (target - start).abs() > 1e-4;
            cur = if transitioning {
                start + (target - start) * smoothstep(e)
            } else {
                target
            };
            gamma.set(cur);
            shared.gamma_milli.store((cur * 1000.0).round() as u32, Relaxed);
            shared.gamma_ok.store(gamma.ok, Relaxed);
        } else if cfg.enabled {
            // Excluded app in the foreground: neutral ramp, but keep the
            // target and last measurement so the boost resumes instantly.
            gamma.restore();
            shared.gamma_milli.store(1000, Relaxed);
            shared.gamma_ok.store(gamma.ok, Relaxed);
            cur = 1.0;
        } else {
            gamma.restore();
            shared.gamma_milli.store(1000, Relaxed);
            shared.gamma_ok.store(gamma.ok, Relaxed);
            cur = 1.0;
            start = 1.0;
            target = 1.0;
            last_measured = f32::MIN;
            pending_up = None;
        }
        was_suppressed = cfg.enabled && suppressed;
        shared.recovery_ok.store(gamma.recovery_ok(), Relaxed);

        // Sleep until the next tick, but wake at once for WM_HOTKEY so the
        // toggle hotkey doesn't wait out the idle interval.
        let wait_ms = if transitioning {
            16
        } else if active {
            50
        } else {
            100
        };
        unsafe {
            MsgWaitForMultipleObjects(None, false, wait_ms, QS_HOTKEY);
        }
    }
    // Gamma::drop restores the original ramp.
}

/// Gate for new gamma targets, to stop the boost "pumping" on fast cuts.
/// A decrease (scene got brighter) applies immediately so it never glares; an
/// increase is held until the darker reading has stayed above the current
/// target for `hold`. A reading that falls back cancels the pending increase.
fn gate(
    pending: &mut Option<(f32, Instant)>,
    new_want: Option<f32>,
    target: f32,
    now: Instant,
    hold: Duration,
) -> Option<f32> {
    if let Some(want) = new_want {
        if (want - target).abs() <= 0.005 {
            *pending = None;
        } else if want < target {
            *pending = None;
            return Some(want);
        } else {
            let since = pending.map_or(now, |(_, s)| s);
            *pending = Some((want, since));
        }
    }
    if let Some((want, since)) = *pending {
        if want > target + 0.005 && now.duration_since(since) >= hold {
            *pending = None;
            return Some(want);
        }
    }
    None
}

fn combine(medians: &[u8; 5], weights: &[u32; 5]) -> u8 {
    let wsum: u32 = weights.iter().sum();
    if wsum == 0 {
        return medians[0]; // sanitize() prevents this, but never divide by zero
    }
    let sum: u32 = medians.iter().zip(weights).map(|(m, w)| *m as u32 * w).sum();
    (sum / wsum) as u8
}

fn target_gamma(cfg: &Config, measured: f32) -> f32 {
    let span = cfg.bright_point - cfg.dark_point;
    if span < 1.0 {
        return if measured < cfg.dark_point { cfg.dark_gamma } else { cfg.bright_gamma };
    }
    let t = ((measured - cfg.dark_point) / span).clamp(0.0, 1.0);
    cfg.dark_gamma + (cfg.bright_gamma - cfg.dark_gamma) * t
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---------------------------------------------------------------------------
// Gamma ramp (GDI, primary display)
// ---------------------------------------------------------------------------

const RAMP_FILE: &str = "original_ramp.bin";
/// Exists on disk while the hardware ramp differs from the original. If it is
/// present at startup, the previous run died mid-boost and the currently
/// captured ramp is tainted — the saved one is the truth.
const MARKER_FILE: &str = "boost_active";

struct Gamma {
    hdc: HDC,
    /// True when `hdc` came from CreateDCW (DeleteDC) rather than GetDC.
    owned_dc: bool,
    /// The primary monitor this ramp belongs to; None if it couldn't be told.
    monitor: Option<HMONITOR>,
    original: [u16; 768],
    /// The exact ramp we last wrote to the hardware (original when neutral).
    last_written: [u16; 768],
    applied: Option<f32>,
    dir: Option<std::path::PathBuf>,
    marker: bool,
    /// False when the driver rejected our last SetDeviceGammaRamp.
    ok: bool,
    /// The saved original ramp (crash-recovery truth) is on disk.
    ramp_saved: bool,
    /// The last boost-marker write/removal succeeded.
    marker_ok: bool,
}

impl Gamma {
    fn new() -> Self {
        // A DC on the primary display's own device, so the ramp we read, write
        // and restore is that display's, not whatever GetDC(NULL) resolves to.
        let monitor = primary_monitor();
        // The screen DC is preferred: a DC opened on the display device itself
        // made the driver do mode-level work while games were running (GPU
        // crash dumps reported). But when the primary display isn't the one
        // GetDC(NULL) resolves to, the screen DC can't read or write a ramp at
        // all, so fall back to a DC on the primary's own device.
        let mut captured = [0u16; 768];
        let (mut hdc, mut owned_dc) = (unsafe { GetDC(None) }, false);
        let mut got = unsafe { GetDeviceGammaRamp(hdc, captured.as_mut_ptr() as *mut _) }.as_bool();
        if !got {
            if let Some((_, dev)) = &monitor {
                let dc = unsafe {
                    CreateDCW(PCWSTR(dev.as_ptr()), PCWSTR::null(), PCWSTR::null(), None)
                };
                if !dc.is_invalid() {
                    got = unsafe { GetDeviceGammaRamp(dc, captured.as_mut_ptr() as *mut _) }
                        .as_bool();
                    if got {
                        unsafe { ReleaseDC(None, hdc) };
                        (hdc, owned_dc) = (dc, true);
                    } else {
                        unsafe { let _ = DeleteDC(dc); }
                    }
                }
            }
        }
        // A failed read leaves zeros; composing a boost through those writes an
        // all-black ramp that every driver refuses. Fall back to identity.
        if !got || !ramp_sane(&captured) {
            captured = identity_ramp();
        }
        let dir = crate::config_dir();
        let marker = dir.as_ref().is_some_and(|d| d.join(MARKER_FILE).exists());
        let mut original = captured;
        let mut ramp_saved = dir.is_some();
        if marker {
            match dir
                .as_ref()
                .and_then(|d| read_ramp(&d.join(RAMP_FILE)))
                .filter(ramp_sane)
            {
                Some(saved) => original = saved,
                None => ramp_saved = false, // marker but no usable saved ramp
            }
        } else if let Some(d) = &dir {
            // Clean start: what we captured IS the user's real ramp (ICC,
            // Night Light, …) — refresh the saved copy.
            ramp_saved = write_ramp(&d.join(RAMP_FILE), &captured).is_ok();
        }
        Self {
            hdc,
            owned_dc,
            monitor: monitor.map(|(m, _)| m),
            original,
            last_written: original,
            applied: None,
            dir,
            marker,
            ok: true,
            ramp_saved,
            marker_ok: true,
        }
    }

    fn set(&mut self, g: f32) {
        if let Some(prev) = self.applied {
            if (prev - g).abs() < 0.001 {
                return;
            }
        }
        // Compose the boost through the original ramp so an ICC calibration
        // or Night Light tint survives while boosted. g == 1.0 maps back to
        // exactly the original ramp.
        let mut ramp = [0u16; 768];
        let inv = 1.0 / g;
        for i in 0..256 {
            let idx = (((i as f32 / 255.0).powf(inv) * 255.0).round() as usize).min(255);
            ramp[i] = self.original[idx];
            ramp[i + 256] = self.original[idx + 256];
            ramp[i + 512] = self.original[idx + 512];
        }
        self.ok = unsafe { SetDeviceGammaRamp(self.hdc, ramp.as_ptr() as *const _) }.as_bool();
        self.last_written = ramp;
        self.applied = Some(g);
        self.set_marker(self.ok && (g - 1.0).abs() > 0.001);
    }

    fn restore(&mut self) {
        if self.applied.is_some() || self.marker {
            let ok = unsafe { SetDeviceGammaRamp(self.hdc, self.original.as_ptr() as *const _) }
                .as_bool();
            self.ok = ok;
            // A rejected restore keeps `applied`/marker set so it is retried
            // (and so crash recovery still knows the screen is boosted).
            if ok {
                self.last_written = self.original;
                self.applied = None;
                self.set_marker(false);
            }
        }
    }

    /// Periodic self-check against the actual hardware ramp.
    /// - Boosted and the ramp is no longer ours: another tool wrote a fresh,
    ///   un-boosted ramp (Night Light, ICC loader, f.lux, or a plain reset).
    ///   That ramp is the new original: adopt it and recompose the boost
    ///   through it on the next `set`, instead of stomping it with a stale one.
    /// - Neutral and the ramp changed: adopt it as the new original so future
    ///   boosts compose through it.
    /// Differences within `RAMP_TOLERANCE` are driver rounding, not change.
    fn heal(&mut self) {
        let mut hw = [0u16; 768];
        if !unsafe { GetDeviceGammaRamp(self.hdc, hw.as_mut_ptr() as *mut _) }.as_bool() {
            return;
        }
        if self.applied.is_some() {
            // Adopting external ramps is disabled: just re-apply our own.
            if ramps_differ(&hw, &self.last_written) {
                self.applied = None; // forces the next set() to rewrite
            }
        }
    }

    /// Save `original` as the crash-recovery truth.
    fn persist_original(&mut self) {
        self.ramp_saved = match &self.dir {
            Some(d) => write_ramp(&d.join(RAMP_FILE), &self.original).is_ok(),
            None => false,
        };
    }

    /// False when crash recovery can't work: the saved original ramp or the
    /// boost marker could not be written.
    fn recovery_ok(&self) -> bool {
        self.ramp_saved && self.marker_ok
    }

    fn set_marker(&mut self, on: bool) {
        if on == self.marker {
            return;
        }
        if let Some(d) = &self.dir {
            let path = d.join(MARKER_FILE);
            let ok = if on {
                std::fs::write(&path, b"1").is_ok()
            } else {
                match std::fs::remove_file(&path) {
                    Ok(()) => true,
                    Err(e) => e.kind() == std::io::ErrorKind::NotFound,
                }
            };
            self.marker_ok = ok;
            if ok {
                self.marker = on;
            }
        } else {
            self.marker_ok = false;
        }
    }
}

/// Largest per-entry deviation (of 65535) still counted as "the same ramp":
/// drivers round what they are given, and a read-back must not look like an
/// external change, or adopting our own boosted ramp would stack the boost.
const RAMP_TOLERANCE: i32 = 1024;

fn ramps_differ(a: &[u16; 768], b: &[u16; 768]) -> bool {
    a.iter().zip(b).any(|(x, y)| (*x as i32 - *y as i32).abs() > RAMP_TOLERANCE)
}

/// The primary monitor and its GDI device name (NUL-terminated UTF-16).
fn primary_monitor() -> Option<(HMONITOR, Vec<u16>)> {
    unsafe {
        let m = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if m.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(m, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
            return None;
        }
        Some((m, mi.szDevice.to_vec()))
    }
}

fn identity_ramp() -> [u16; 768] {
    let mut r = [0u16; 768];
    for i in 0..256 {
        let v = (i * 257) as u16;
        r[i] = v;
        r[i + 256] = v;
        r[i + 512] = v;
    }
    r
}

/// A usable ramp: each channel is non-decreasing and spans most of the range.
fn ramp_sane(r: &[u16; 768]) -> bool {
    r.chunks_exact(256)
        .all(|ch| ch.windows(2).all(|w| w[0] <= w[1]) && ch[255] >= 32768 && ch[0] <= 32768)
}

fn write_ramp(path: &std::path::Path, ramp: &[u16; 768]) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(1536);
    for v in ramp {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

fn read_ramp(path: &std::path::Path) -> Option<[u16; 768]> {
    let b = std::fs::read(path).ok()?;
    if b.len() != 1536 {
        return None;
    }
    let mut r = [0u16; 768];
    for (i, c) in b.chunks_exact(2).enumerate() {
        r[i] = u16::from_le_bytes([c[0], c[1]]);
    }
    Some(r)
}

impl Drop for Gamma {
    fn drop(&mut self) {
        self.restore();
        unsafe {
            if self.owned_dc {
                let _ = DeleteDC(self.hdc);
            } else {
                ReleaseDC(None, self.hdc);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Screen sampling (DXGI Desktop Duplication, primary output)
// ---------------------------------------------------------------------------

/// Staging atlas cell per zone: a 6x6 grid of 16px patches. Sampling cost is
/// therefore constant regardless of configured zone size.
const CELL: u32 = 96;
const GRID: u32 = 6;


/// Why a capture could not start. HDR is its own case so the UI can say so
/// instead of guessing.
enum CaptureError {
    Hdr,
    Failed(windows::core::Error),
}

impl From<windows::core::Error> for CaptureError {
    fn from(e: windows::core::Error) -> Self {
        Self::Failed(e)
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hdr => f.write_str("HDR is on for the primary display (gamma ramps don't apply)"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

struct Capture {
    _device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    /// Per-zone (x, y, side) rectangles in desktop pixels.
    rects: [(u32, u32, u32); 5],
    /// The (zone_sizes, zone_pos) config `rects` was computed for.
    built: ([u32; 5], [(u32, u32); 5]),
    width: u32,
    height: u32,
    /// Desktop position of the duplicated output (where the overlay goes).
    origin: (i32, i32),
    rgba: bool,
}

impl Capture {
    fn new(layout: ([u32; 5], [(u32, u32); 5])) -> Result<Self, CaptureError> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let (adapter, output) = find_primary_output(&factory)?;
            // Checked before any device work: with HDR on the display ignores
            // the gamma ramp, so there is nothing to compensate.
            if output_is_hdr(&output) {
                return Err(CaptureError::Hdr);
            }

            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
            let device = device.unwrap();
            let ctx = ctx.unwrap();

            let coords = output.GetDesc()?.DesktopCoordinates;
            let output1: IDXGIOutput1 = output.cast()?;
            let dupl = output1.DuplicateOutput(&device)?;

            let dd: DXGI_OUTDUPL_DESC = dupl.GetDesc();
            let fmt: DXGI_FORMAT = dd.ModeDesc.Format;
            let rgba = match fmt {
                DXGI_FORMAT_B8G8R8A8_UNORM => false,
                DXGI_FORMAT_R8G8B8A8_UNORM => true,
                // Gamma ramps don't apply in HDR, so an FP16 desktop is not
                // sampled; it is reported as HDR rather than as a failure.
                DXGI_FORMAT_R16G16B16A16_FLOAT => return Err(CaptureError::Hdr),
                _ => {
                    return Err(CaptureError::Failed(windows::core::Error::from_hresult(
                        windows::Win32::Foundation::E_FAIL,
                    )))
                }
            };
            let (w, h) = (dd.ModeDesc.Width, dd.ModeDesc.Height);

            let desc = D3D11_TEXTURE2D_DESC {
                Width: CELL * 5,
                Height: CELL,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;

            Ok(Self {
                _device: device,
                ctx,
                dupl,
                staging: staging.unwrap(),
                rects: zone_rects(w, h, &layout.0, &layout.1),
                built: layout,
                width: w,
                height: h,
                origin: (coords.left, coords.top),
                rgba,
            })
        }
    }

    /// Ok(None) = nothing to report yet: no new frame (static screen), or the
    /// GPU copy is still in flight.
    /// Ok(Some(medians)) = per-zone median brightness (center, TL, TR, BL, BR).
    fn sample(&mut self) -> windows::core::Result<Option<[u8; 5]>> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dupl.AcquireNextFrame(0, &mut info, &mut res) {
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                r => r?,
            }
            let Some(res) = res.as_ref() else {
                // Success with no resource: nothing to read, but the frame is
                // still ours until released.
                let _ = self.dupl.ReleaseFrame();
                return Ok(None);
            };
            let tex: ID3D11Texture2D = match res.cast() {
                Ok(t) => t,
                Err(e) => {
                    let _ = self.dupl.ReleaseFrame();
                    return Err(e);
                }
            };
            // 36 small patches spread evenly across each zone, packed into
            // its atlas cell — constant cost for any zone size.
            for (i, &(zx, zy, z)) in self.rects.iter().enumerate() {
                let spread = z.saturating_sub(PATCH);
                for r in 0..GRID {
                    for c in 0..GRID {
                        let sx = zx + c * spread / (GRID - 1);
                        let sy = zy + r * spread / (GRID - 1);
                        let b = D3D11_BOX {
                            left: sx,
                            top: sy,
                            front: 0,
                            right: sx + PATCH,
                            bottom: sy + PATCH,
                            back: 1,
                        };
                        self.ctx.CopySubresourceRegion(
                            &self.staging,
                            0,
                            i as u32 * CELL + c * PATCH,
                            r * PATCH,
                            0,
                            &tex,
                            0,
                            Some(&b),
                        );
                    }
                }
            }
            drop(tex);
            let _ = self.dupl.ReleaseFrame();

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let medians = self.read_medians(&mapped);
            self.ctx.Unmap(&self.staging, 0);
            Ok(Some(medians))
        }
    }

    /// Median luma per zone from the mapped staging atlas.
    unsafe fn read_medians(&self, mapped: &D3D11_MAPPED_SUBRESOURCE) -> [u8; 5] {
        let base = mapped.pData as *const u8;
        let pitch = mapped.RowPitch as usize;

        let mut medians = [0u8; 5];
        let mut hist = [0u32; 256];
        for (zone, median) in medians.iter_mut().enumerate() {
            hist.fill(0);
            let mut count = 0u32;
            let x0 = zone * CELL as usize * 4;
            for y in (0..CELL as usize).step_by(2) {
                let row = base.add(y * pitch + x0);
                for x in (0..CELL as usize).step_by(2) {
                    let p = row.add(x * 4);
                    let (r, g, b) = if self.rgba {
                        (*p, *p.add(1), *p.add(2))
                    } else {
                        (*p.add(2), *p.add(1), *p)
                    };
                    // Rec. 709 luma, integer approximation.
                    let l = (54 * r as u32 + 183 * g as u32 + 19 * b as u32) >> 8;
                    hist[l as usize] += 1;
                    count += 1;
                }
            }
            let mut acc = 0u32;
            for (v, n) in hist.iter().enumerate() {
                acc += n;
                if acc * 2 >= count {
                    *median = v as u8;
                    break;
                }
            }
        }
        medians
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_weighted_average() {
        assert_eq!(combine(&[100, 0, 0, 0, 0], &[1, 0, 0, 0, 0]), 100);
        assert_eq!(combine(&[100, 200, 0, 0, 0], &[1, 1, 0, 0, 0]), 150);
        // Center weight 2 vs one corner weight 1.
        assert_eq!(combine(&[90, 0, 0, 0, 30], &[2, 0, 0, 0, 1]), 70);
        // All-zero weights: falls back to center, no division by zero.
        assert_eq!(combine(&[42, 1, 2, 3, 4], &[0; 5]), 42);
    }

    #[test]
    fn target_gamma_interpolates() {
        let cfg = Config::default(); // dark 30/1.6, bright 130/1.0
        assert_eq!(target_gamma(&cfg, 0.0), cfg.dark_gamma);
        assert_eq!(target_gamma(&cfg, 30.0), cfg.dark_gamma);
        assert_eq!(target_gamma(&cfg, 130.0), cfg.bright_gamma);
        assert_eq!(target_gamma(&cfg, 255.0), cfg.bright_gamma);
        let mid = target_gamma(&cfg, 80.0);
        assert!((mid - 1.3).abs() < 1e-4, "midpoint should interpolate, got {mid}");
    }

    #[test]
    fn target_gamma_degenerate_span_is_a_step() {
        let mut cfg = Config::default();
        cfg.dark_point = 100.0;
        cfg.bright_point = 100.5; // span < 1
        assert_eq!(target_gamma(&cfg, 99.0), cfg.dark_gamma);
        assert_eq!(target_gamma(&cfg, 101.0), cfg.bright_gamma);
    }

    #[test]
    fn gate_applies_decreases_now_and_holds_increases() {
        let t0 = Instant::now();
        let hold = Duration::from_millis(500);
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut p = None;
        // Brighter scene: less boost, immediate.
        assert_eq!(gate(&mut p, Some(1.0), 1.5, at(0), hold), Some(1.0));
        // Darker scene: held...
        assert_eq!(gate(&mut p, Some(1.5), 1.0, at(100), hold), None);
        assert_eq!(gate(&mut p, None, 1.0, at(400), hold), None);
        // ...and lands once it persisted, even with no new sample.
        assert_eq!(gate(&mut p, None, 1.0, at(600), hold), Some(1.5));
        assert!(p.is_none());
    }

    #[test]
    fn gate_cancels_increase_that_flips_back() {
        let t0 = Instant::now();
        let hold = Duration::from_millis(500);
        let mut p = None;
        assert_eq!(gate(&mut p, Some(1.5), 1.0, t0, hold), None);
        // Back to the current target before the hold elapsed: nothing fires.
        assert_eq!(gate(&mut p, Some(1.0), 1.0, t0 + Duration::from_millis(200), hold), None);
        assert_eq!(gate(&mut p, None, 1.0, t0 + Duration::from_millis(900), hold), None);
        // Zero hold = immediate.
        assert_eq!(gate(&mut p, Some(1.5), 1.0, t0, Duration::ZERO), Some(1.5));
    }

    #[test]
    fn smoothstep_shape() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert_eq!(smoothstep(0.5), 0.5);
        assert_eq!(smoothstep(-5.0), 0.0);
        assert_eq!(smoothstep(5.0), 1.0);
        assert!(smoothstep(0.25) < 0.25); // ease-in below linear
        assert!(smoothstep(0.75) > 0.75); // ease-out above linear
    }

    #[test]
    fn is_excluded_matches_exe_names() {
        assert!(is_excluded("photoshop.exe", "photoshop.exe"));
        assert!(is_excluded(" GIMP.exe ; photoshop.exe,\"krita.exe\"", "krita.exe"));
        assert!(is_excluded("a.exe\nb.exe", "b.exe"));
        assert!(!is_excluded("photoshop.exe", "photoshop"));
        assert!(!is_excluded("", "game.exe"));
        assert!(!is_excluded(" , ;", ""));
    }

    #[test]
    fn ramps_differ_ignores_rounding_only() {
        let a = [30000u16; 768];
        let mut b = a;
        b[100] += 800; // driver rounding: same ramp
        assert!(!ramps_differ(&a, &b));
        b[100] += 600; // real external change
        assert!(ramps_differ(&a, &b));
    }

    #[test]
    fn ramp_file_roundtrip() {
        let path = std::env::temp_dir().join(format!("autobright_test_{}.bin", std::process::id()));
        let mut ramp = [0u16; 768];
        for (i, v) in ramp.iter_mut().enumerate() {
            *v = (i as u16).wrapping_mul(257);
        }
        write_ramp(&path, &ramp).unwrap();
        assert_eq!(read_ramp(&path), Some(ramp));
        // Truncated file must be rejected, not misread.
        std::fs::write(&path, [0u8; 100]).unwrap();
        assert_eq!(read_ramp(&path), None);
        let _ = std::fs::remove_file(&path);
    }
}

/// HDR is on when the output's colour space is HDR10 (PQ / BT.2020).
unsafe fn output_is_hdr(output: &IDXGIOutput) -> bool {
    output
        .cast::<IDXGIOutput6>()
        .ok()
        .and_then(|o| o.GetDesc1().ok())
        .is_some_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
}

/// The output of the monitor Windows calls primary (the one the gamma ramp is
/// written to). Matched by monitor handle, not position: cloned displays share
/// (0,0). Falls back to "the attached output at the origin".
unsafe fn find_primary_output(
    factory: &IDXGIFactory1,
) -> windows::core::Result<(IDXGIAdapter1, IDXGIOutput)> {
    let primary = primary_monitor().map(|(m, _)| m);
    let mut fallback: Option<(IDXGIAdapter1, IDXGIOutput)> = None;
    let mut i = 0;
    while let Ok(adapter) = factory.EnumAdapters1(i) {
        let mut j = 0;
        while let Ok(output) = adapter.EnumOutputs(j) {
            if let Ok(desc) = output.GetDesc() {
                if desc.AttachedToDesktop.as_bool() {
                    if primary == Some(desc.Monitor) {
                        return Ok((adapter, output));
                    }
                    if fallback.is_none()
                        && desc.DesktopCoordinates.left == 0
                        && desc.DesktopCoordinates.top == 0
                    {
                        fallback = Some((adapter.clone(), output));
                    }
                }
            }
            j += 1;
        }
        i += 1;
    }
    fallback.ok_or_else(|| windows::Win32::Graphics::Dxgi::DXGI_ERROR_NOT_FOUND.into())
}
