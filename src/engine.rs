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

use crate::{Config, Shared};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadInformation, SetThreadPriority, ThreadPowerThrottling,
    THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_BELOW_NORMAL,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT, DXGI_ERROR_WAS_STILL_DRAWING,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, EnumDisplayDevicesW, GetDC, ReleaseDC, DISPLAY_DEVICEW, HDC,
};
use windows::Win32::UI::ColorSystem::{GetDeviceGammaRamp, SetDeviceGammaRamp};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
    MsgWaitForMultipleObjects, PeekMessageW, RegisterClassW, SetWindowLongPtrW, GWLP_USERDATA, MSG,
    PM_REMOVE, QS_HOTKEY, QS_POSTMESSAGE, WINDOW_EX_STYLE, WM_DISPLAYCHANGE, WM_ENDSESSION,
    WM_HOTKEY, WM_POWERBROADCAST, WNDCLASSW, WS_POPUP,
};

/// Set by the event window when the display stack is about to change or just
/// changed (suspend/resume, lock/unlock, mode change). The engine loop drops
/// the capture and re-reads the ramp. Only a flag is stored here: the handler
/// never calls into the display driver itself.
static DISPLAY_EVENT: AtomicBool = AtomicBool::new(false);

const PBT_APMSUSPEND: usize = 0x4;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;

/// Window procedure of the hidden event window (engine thread only).
unsafe extern "system" fn event_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST if wp.0 == PBT_APMSUSPEND || wp.0 == PBT_APMRESUMEAUTOMATIC => {
            DISPLAY_EVENT.store(true, Relaxed);
        }
        WM_DISPLAYCHANGE => DISPLAY_EVENT.store(true, Relaxed),
        WM_WTSSESSION_CHANGE if wp.0 == WTS_SESSION_LOCK || wp.0 == WTS_SESSION_UNLOCK => {
            DISPLAY_EVENT.store(true, Relaxed);
        }
        // Logoff/shutdown: nothing runs after the system kills us, so put the
        // ramp back now. The Gamma pointer is set once the engine owns one.
        WM_ENDSESSION if wp.0 != 0 => {
            let g = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Gamma;
            if let Some(g) = g.as_mut() {
                g.restore();
            }
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Hidden top-level window on the engine thread. It must be top-level, not
/// message-only: broadcasts such as WM_DISPLAYCHANGE and WM_ENDSESSION are not
/// delivered to message-only windows. Never shown.
fn create_event_window() -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(PCWSTR::null()).ok()?.into();
        let class = windows::core::w!("AutoBrightEvents");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(event_proc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // 0 if already registered (engine restart): fine
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            class,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )
        .ok()?;
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        Some(hwnd)
    }
}

/// Efficiency for a background sampler: below-normal priority so a game never
/// waits on us, and EcoQoS so the scheduler may run us on efficient cores at a
/// lower clock. Only this thread; the UI thread keeps normal behaviour. Both
/// calls are best effort.
fn lower_thread_priority() {
    unsafe {
        let t = GetCurrentThread();
        let _ = SetThreadPriority(t, THREAD_PRIORITY_BELOW_NORMAL);
        let state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetThreadInformation(
            t,
            ThreadPowerThrottling,
            &state as *const _ as *const _,
            std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
    }
}


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

/// `--probe`: one capture + sample, printed to the console. Never touches gamma.
pub fn probe() {
    let cfg = Config::default();
    match Capture::new(cfg.zone_sizes) {
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
fn register_hotkey(mods: u32, vk: u32) {
    unsafe {
        let _ = UnregisterHotKey(None, 1);
        if mods != 0 && vk != 0 {
            let flags = HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT;
            let _ = RegisterHotKey(None, 1, flags, vk);
        }
    }
}

fn run(shared: &Shared) {
    lower_thread_priority();
    let mut gamma = Gamma::new();
    // The event window lets logoff/shutdown restore the ramp from inside the
    // handler. It borrows `gamma` through GWLP_USERDATA, but only while a
    // message is dispatched from this loop, never during a Gamma call.
    let event_hwnd = create_event_window();
    if let Some(h) = event_hwnd {
        unsafe { SetWindowLongPtrW(h, GWLP_USERDATA, &mut gamma as *mut Gamma as isize) };
    }
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
    register_hotkey(hotkey.0, hotkey.1);

    // Transition state: eased ramp from `start` to `target` beginning at `t0`.
    // These are blend positions (0 = dark point, 1 = bright point), not gamma.
    let mut cur = 1.0f32;
    let mut start = 1.0f32;
    let mut target = 1.0f32;
    let mut t0 = Instant::now();
    let mut dur_s = 0.001f32;
    let mut last_measured = f32::MIN;
    // Retarget waiting to prove stable: (blend position, since when).
    let mut cand: Option<(f32, Instant)> = None;

    // Crash bisection switches: exercise only one half of the GPU/driver work.
    let no_gamma = std::env::args().any(|a| a == "--no-gamma");
    let no_capture = std::env::args().any(|a| a == "--no-capture");
    let mut next_flip = Instant::now();
    let mut next_write = Instant::now();

    // Wall clock, not Instant: Instant may not advance across sleep, which is
    // exactly the gap we want to see.
    let mut last_tick = SystemTime::now();

    while shared.running.load(Relaxed) {
        // Suspend/resume, lock/unlock or a mode change (DISPLAY_EVENT), or, as a
        // fallback, a long gap between ticks (the machine slept or we were
        // frozen). The display driver resets, so the duplication device and the
        // display DC are stale: drop them and give the driver a few seconds to
        // settle instead of hammering it with re-creation while it recovers.
        let wall = SystemTime::now();
        let event = DISPLAY_EVENT.swap(false, Relaxed);
        if event || wall.duration_since(last_tick).is_ok_and(|d| d > Duration::from_secs(5)) {
            let t = Instant::now();
            cap = None;
            shared.capture_ok.store(false, Relaxed);
            next_reinit = t + Duration::from_secs(4);
            reinit_backoff = Duration::from_secs(2);
            next_heal = t + Duration::from_secs(6);
            gamma.reopen();
        }
        last_tick = wall;

        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_HOTKEY {
                let snap = {
                    let mut c = shared.cfg.lock().unwrap();
                    c.enabled = !c.enabled;
                    *c
                };
                crate::save_config(&snap);
            } else {
                unsafe { DispatchMessageW(&msg) };
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
            register_hotkey(cfg.hotkey_mods, cfg.hotkey_vk);
            hotkey = (cfg.hotkey_mods, cfg.hotkey_vk);
        }

        // UI asked to put the original ramp back right now (stuck-gamma kick).
        // If still enabled, the next tick recomputes cleanly from the original.
        if shared.force_restore.swap(false, Relaxed) {
            gamma.restore();
            shared.gamma_milli.store(1000, Relaxed);
            cur = 1.0;
            start = 1.0;
            target = 1.0;
            last_measured = f32::MIN;
            cand = None;
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
            next_reinit = Instant::now();
            reinit_backoff = Duration::from_secs(2);
        }

        let now = Instant::now();

        if active && now >= next_sample {
            next_sample = now + Duration::from_millis(cfg.sample_ms as u64);

            // Zone sizes changed in the UI: just recompute the rectangles.
            if let Some(c) = cap.as_mut() {
                if c.built_sizes != cfg.zone_sizes {
                    c.rects = zone_rects(c.width, c.height, &cfg.zone_sizes);
                    c.built_sizes = cfg.zone_sizes;
                }
            }
            // --no-capture: no sampling, so fake scene changes to keep the
            // gamma path busy with real fades.
            if no_capture && now >= next_flip {
                next_flip = now + Duration::from_secs(6);
                start = cur;
                target = if target > 0.5 { 0.0 } else { 1.0 };
                t0 = now;
                dur_s = cfg.transition_ms.max(1) as f32 / 1000.0;
            }
            if cap.is_none() && !no_capture && now >= next_reinit {
                match Capture::new(cfg.zone_sizes) {
                    Ok(c) => {
                        shared.screen_dims.store(
                            ((c.width as u64) << 32) | c.height as u64,
                            Relaxed,
                        );
                        cap = Some(c);
                        shared.capture_ok.store(true, Relaxed);
                        // Capture coming back often means the ramp was reset
                        // too (game exit, resume from sleep) — heal now.
                        next_heal = now;
                    }
                    Err(_) => {
                        shared.capture_ok.store(false, Relaxed);
                        next_reinit = now + reinit_backoff;
                        reinit_backoff = (reinit_backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }

            if let Some(c) = cap.as_mut() {
                match c.sample() {
                    Ok(Some(medians)) => {
                        reinit_backoff = Duration::from_secs(2); // capture proven healthy
                        for (slot, m) in shared.zone_medians.iter().zip(&medians) {
                            slot.store(*m as u32, Relaxed);
                        }
                        let m = combine(&medians, &cfg.zone_weights);
                        shared.measured.store(m as u32, Relaxed);
                        let mf = m as f32;
                        // Deadband: ±3 counts of flicker never retargets.
                        if (mf - last_measured).abs() >= 3.0 {
                            last_measured = mf;
                            let want = blend_pos(&cfg, mf);
                            // Don't retarget yet: a new value must hold
                            // still for cfg.settle_ms first (applied below). Sudden
                            // swings like a round-start load never reach
                            // the driver as ramp writes.
                            if (want - target).abs() > 0.005 {
                                if cand.is_none_or(|(w, _)| (w - want).abs() > 0.05) {
                                    cand = Some((want, now));
                                }
                            } else {
                                cand = None;
                            }
                        }
                    }
                    Ok(None) => {} // no new frame: screen static, nothing to do
                    Err(_) => {
                        // Mode change / fullscreen handoff: rebuild duplication.
                        // Mode changes often reset the hardware ramp too, so
                        // heal immediately instead of waiting for the timer.
                        // The backoff is deliberately not reset: a capture
                        // that keeps dying right after creation (lock screen,
                        // UAC, driver reset) must not recreate the device in
                        // a tight loop.
                        cap = None;
                        shared.capture_ok.store(false, Relaxed);
                        next_reinit = now + reinit_backoff;
                        reinit_backoff = (reinit_backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }

        if let Some((want, since)) = cand {
            if now.duration_since(since) >= Duration::from_millis(cfg.settle_ms as u64) {
                cand = None;
                start = cur;
                target = want;
                t0 = now;
                // Moving toward the bright point = the scene got brighter:
                // ramp down fast so it doesn't glare.
                let ms = if want > cur { cfg.transition_down_ms } else { cfg.transition_ms };
                dur_s = ms.max(1) as f32 / 1000.0;
            }
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
            // Only while capture works: with capture broken (exclusive
            // fullscreen) the game owns the ramp — fighting it flickers.
            if cap.is_some() && !no_gamma {
                gamma.heal();
            }
            next_heal = now + Duration::from_secs(10);
        }

        let mut transitioning = false;
        if cfg.enabled {
            let e = (now - t0).as_secs_f32() / dur_s;
            transitioning = e < 1.0 && (target - start).abs() > 1e-4;
            cur = if transitioning {
                start + (target - start) * smoothstep(e)
            } else {
                target
            };
            let g = gamma_at(&cfg, cur);
            // Each ramp write is a driver call that can stall a game's present,
            // so fades are rate-limited; the settled value is always written.
            if !no_gamma && (!transitioning || now >= next_write) {
                gamma.set(g, cfg.black_lift * (1.0 - cur), cfg.white_drop * cur);
                next_write = now + Duration::from_millis(100);
            }
            shared.gamma_milli.store((g * 1000.0).round() as u32, Relaxed);
            shared.gamma_ok.store(gamma.ok, Relaxed);
        } else {
            gamma.restore();
            shared.gamma_milli.store(1000, Relaxed);
            shared.gamma_ok.store(true, Relaxed);
            cur = 1.0;
            start = 1.0;
            target = 1.0;
            last_measured = f32::MIN;
            cand = None;
        }

        // Wait on the message queue instead of sleeping so the hotkey wakes us
        // at once; the timeout is the tick. Idle ticks are long: when off,
        // nothing needs sampling, only the UI's toggle/restore flags are polled.
        let wait_ms = if transitioning {
            40 // ~25 ramp writes/s is smooth enough for an eased fade
        } else if active {
            50
        } else {
            250
        };
        unsafe { MsgWaitForMultipleObjects(None, false, wait_ms, QS_HOTKEY | QS_POSTMESSAGE) };
    }
    if let Some(h) = event_hwnd {
        unsafe {
            // Detach before `gamma` drops (which restores the ramp).
            SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            let _ = WTSUnRegisterSessionNotification(h);
            let _ = DestroyWindow(h);
        }
    }
    // Gamma::drop restores the original ramp.
}

fn combine(medians: &[u8; 5], weights: &[u32; 5]) -> u8 {
    let wsum: u32 = weights.iter().sum();
    if wsum == 0 {
        return medians[0]; // sanitize() prevents this, but never divide by zero
    }
    let sum: u32 = medians.iter().zip(weights).map(|(m, w)| *m as u32 * w).sum();
    (sum / wsum) as u8
}

/// Position between the dark point (0.0) and the bright point (1.0). Gamma,
/// black lift and white drop all follow it, so they ease together.
fn blend_pos(cfg: &Config, measured: f32) -> f32 {
    let span = cfg.bright_point - cfg.dark_point;
    if span < 1.0 {
        return if measured < cfg.dark_point { 0.0 } else { 1.0 };
    }
    ((measured - cfg.dark_point) / span).clamp(0.0, 1.0)
}

#[cfg(test)]
fn target_gamma(cfg: &Config, measured: f32) -> f32 {
    gamma_at(cfg, blend_pos(cfg, measured))
}

fn gamma_at(cfg: &Config, pos: f32) -> f32 {
    cfg.dark_gamma + (cfg.bright_gamma - cfg.dark_gamma) * pos
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

/// DC for the primary display device. The generic screen DC (GetDC(None))
/// fails gamma calls with ERROR_MOD_NOT_FOUND on current AMD drivers, while a
/// DC created for the display device itself works. Falls back to the screen DC.
fn open_gamma_dc() -> (HDC, bool) {
    const ATTACHED: u32 = 0x1;
    const PRIMARY: u32 = 0x4;
    unsafe {
        let mut i = 0;
        loop {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0).as_bool() {
                break;
            }
            if dd.StateFlags.0 & (ATTACHED | PRIMARY) == (ATTACHED | PRIMARY) {
                let name = PCWSTR(dd.DeviceName.as_ptr());
                let hdc = CreateDCW(name, name, PCWSTR::null(), None);
                if !hdc.is_invalid() {
                    return (hdc, true);
                }
            }
            i += 1;
        }
        (GetDC(None), false)
    }
}

/// Run `f` with a display DC that exists only for the call. Holding a DC on the
/// display device open for the whole process lifetime keeps a handle into the
/// display driver even while the boost is off, so every gamma read or write
/// opens a fresh one and closes it straight away.
fn with_dc<R>(f: impl FnOnce(HDC) -> R) -> R {
    with_dc_timed(f).0
}

/// `with_dc` that also returns (time to open the DC, time inside `f`).
fn with_dc_timed<R>(f: impl FnOnce(HDC) -> R) -> (R, Duration, Duration) {
    let t = Instant::now();
    let (hdc, owned) = open_gamma_dc();
    let opened = t.elapsed();
    let t = Instant::now();
    let r = f(hdc);
    let called = t.elapsed();
    unsafe {
        if owned {
            let _ = DeleteDC(hdc);
        } else {
            ReleaseDC(None, hdc);
        }
    }
    (r, opened, called)
}

struct Gamma {
    original: [u16; 768],
    /// The exact ramp we last wrote to the hardware (original when neutral).
    last_written: [u16; 768],
    /// (gamma, black lift, white drop) of the ramp we last wrote.
    applied: Option<(f32, f32, f32)>,
    dir: Option<std::path::PathBuf>,
    marker: bool,
    /// False when the driver rejected our last SetDeviceGammaRamp.
    ok: bool,
}

impl Gamma {
    fn new() -> Self {
        let mut captured = [0u16; 768];
        with_dc(|hdc| unsafe {
            let _ = GetDeviceGammaRamp(hdc, captured.as_mut_ptr() as *mut _);
        });
        let dir = crate::config_dir();
        let marker = dir.as_ref().is_some_and(|d| d.join(MARKER_FILE).exists());
        let mut original = captured;
        if marker {
            if let Some(saved) = dir.as_ref().and_then(|d| read_ramp(&d.join(RAMP_FILE))) {
                original = saved;
            }
        } else if let Some(d) = &dir {
            // Clean start: what we captured IS the user's real ramp (ICC,
            // Night Light, …) — refresh the saved copy.
            let _ = write_ramp(&d.join(RAMP_FILE), &captured);
        }
        Self { original, last_written: original, applied: None, dir, marker, ok: true }
    }

    /// `lift` raises black and `drop` lowers white, both on the 0-255 scale.
    fn set(&mut self, g: f32, lift: f32, drop: f32) {
        if let Some((pg, pl, pd)) = self.applied {
            if (pg - g).abs() < 0.001 && (pl - lift).abs() < 0.05 && (pd - drop).abs() < 0.05 {
                return;
            }
        }
        // Compose the boost through the original ramp so an ICC calibration
        // or Night Light tint survives while boosted. Neutral settings map
        // back to exactly the original ramp.
        let lo = lift as f64 / 255.0 * 65535.0;
        let hi = 65535.0 - drop as f64 / 255.0 * 65535.0;
        let mut ramp = [0u16; 768];
        let inv = 1.0 / g;
        for i in 0..256 {
            let idx = (((i as f32 / 255.0).powf(inv) * 255.0).round() as usize).min(255);
            for c in 0..3 {
                let v = self.original[idx + c * 256] as f64;
                ramp[i + c * 256] = (lo + v * (hi - lo) / 65535.0).round().clamp(0.0, 65535.0) as u16;
            }
        }
        // Each write reprograms the display LUT inside the driver; skip it
        // when rounding made the ramp identical to what is already there.
        if self.applied.is_some() && ramp == self.last_written {
            self.applied = Some((g, lift, drop));
            return;
        }
        let (ok, opened, wrote) =
            with_dc_timed(|hdc| unsafe { SetDeviceGammaRamp(hdc, ramp.as_ptr() as *const _) }.as_bool());
        self.ok = ok;
        self.log_write("set", opened, wrote);
        self.last_written = ramp;
        self.applied = Some((g, lift, drop));
        let neutral = (g - 1.0).abs() <= 0.001 && lift < 0.05 && drop < 0.05;
        self.set_marker(self.ok && !neutral);
    }

    /// Diagnostics: every ramp write is appended to gamma_timing.log as
    /// "unix_ms kind open_ms write_ms" (kind = set | heal | restore), so a GPU crash can
    /// be lined up against exactly what we did and why. Rotates at 200 KB.
    fn log_write(&self, kind: &str, opened: Duration, wrote: Duration) {
        use std::io::Write;
        let Some(d) = &self.dir else { return };
        let path = d.join("gamma_timing.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 200_000) {
            let _ = std::fs::rename(&path, d.join("gamma_timing.old.log"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            let _ = writeln!(
                f,
                "{ms} {kind} {:.1} {:.1}",
                opened.as_secs_f64() * 1000.0,
                wrote.as_secs_f64() * 1000.0
            );
        }
    }

    /// Forget what was applied (the driver reset on resume may have dropped
    /// it) so an active boost is rewritten on the next tick.
    fn reopen(&mut self) {
        self.applied = None;
    }

    fn restore(&mut self) {
        if self.applied.is_some() || self.marker {
            let (_, opened, wrote) = with_dc_timed(|hdc| unsafe {
                let _ = SetDeviceGammaRamp(hdc, self.original.as_ptr() as *const _);
            });
            self.log_write("restore", opened, wrote);
            self.last_written = self.original;
            self.applied = None;
            self.ok = true;
            self.set_marker(false);
        }
    }

    /// Periodic self-check against the actual hardware ramp.
    /// - Boosted and someone reset the ramp (game exit, resume from sleep):
    ///   rewrite our ramp — the boost wins while it is active.
    /// - Neutral and the ramp changed (Night Light, ICC loader): adopt it as
    ///   the new original so future boosts compose through it.
    fn heal(&mut self) {
        let mut hw = [0u16; 768];
        if !with_dc(|hdc| unsafe { GetDeviceGammaRamp(hdc, hw.as_mut_ptr() as *mut _) }.as_bool()) {
            return;
        }
        if self.applied.is_some() {
            if !ramps_close(&hw, &self.last_written) {
                let (ok, opened, wrote) = with_dc_timed(|hdc| unsafe {
                    SetDeviceGammaRamp(hdc, self.last_written.as_ptr() as *const _)
                }
                .as_bool());
                self.ok = ok;
                self.log_write("heal", opened, wrote);
            }
        } else if !self.marker && !ramps_close(&hw, &self.original) {
            self.original = hw;
            self.last_written = hw;
            if let Some(d) = &self.dir {
                let _ = write_ramp(&d.join(RAMP_FILE), &hw);
            }
        }
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
                std::fs::remove_file(&path).is_ok()
            };
            if ok {
                self.marker = on;
            }
        }
    }
}

/// Drivers quantize ramps (AMD keeps fewer than 16 bits), so a read-back never
/// equals what was written. Treat ~1.5% of full scale as "the same ramp" or
/// heal would rewrite the hardware every cycle.
fn ramps_close(a: &[u16; 768], b: &[u16; 768]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= 1024)
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
    }
}

// ---------------------------------------------------------------------------
// Screen sampling (DXGI Desktop Duplication, primary output)
// ---------------------------------------------------------------------------

/// Staging atlas cell per zone: a 4x4 grid of 16px patches (80 GPU copies per
/// sample in total — fewer commands for the driver than a denser grid). Sampling cost is
/// therefore constant regardless of configured zone size.
const CELL: u32 = 64;
const GRID: u32 = 4;
const PATCH: u32 = 16;

struct Capture {
    _device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    /// Per-zone (x, y, side) rectangles in desktop pixels.
    rects: [(u32, u32, u32); 5],
    /// The zone_sizes config `rects` was computed for.
    built_sizes: [u32; 5],
    width: u32,
    height: u32,
    rgba: bool,
}

impl Capture {
    fn new(sizes: [u32; 5]) -> windows::core::Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let (adapter, output) = find_primary_output(&factory)?;

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
            // Lowest GPU scheduling priority: our tiny copies must never queue
            // ahead of a game's work. Best effort — ignored if unsupported.
            if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
                let _ = dxgi.SetGPUThreadPriority(-7);
            }

            let output1: IDXGIOutput1 = output.cast()?;
            let dupl = output1.DuplicateOutput(&device)?;

            let dd: DXGI_OUTDUPL_DESC = dupl.GetDesc();
            let fmt: DXGI_FORMAT = dd.ModeDesc.Format;
            let rgba = match fmt {
                DXGI_FORMAT_B8G8R8A8_UNORM => false,
                DXGI_FORMAT_R8G8B8A8_UNORM => true,
                // ponytail: HDR desktops (R16G16B16A16_FLOAT) unsupported;
                // gamma ramps don't apply in HDR anyway.
                _ => return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                )),
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
                rects: zone_rects(w, h, &sizes),
                built_sizes: sizes,
                width: w,
                height: h,
                rgba,
            })
        }
    }

    /// Ok(None) = no new frame since last call (static screen).
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
            // 16 small patches spread evenly across each zone, packed into
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

            // A blocking Map waits on the GPU forever if the driver is mid-reset
            // (sleep/resume, TDR), hanging this thread and the quit join. Poll
            // with DO_NOT_WAIT for a bounded time, then fail so the caller
            // rebuilds the capture.
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            let mut tries = 0;
            loop {
                match self.ctx.Map(
                    &self.staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                ) {
                    Ok(()) => break,
                    Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING && tries < 25 => {
                        tries += 1;
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => return Err(e),
                }
            }
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
            self.ctx.Unmap(&self.staging, 0);

            Ok(Some(medians))
        }
    }
}

/// Square zone rectangles (x, y, side) in desktop pixels, from per-zone size
/// percentages of the smaller screen dimension (2-100%). Corners are inset 5%
/// when there is room and clamped on-screen when there isn't. Also used by
/// the UI to draw the zone preview.
pub fn zone_rects(w: u32, h: u32, sizes: &[u32; 5]) -> [(u32, u32, u32); 5] {
    let dim = w.min(h).max(PATCH);
    let px = sizes.map(|pct| (dim * pct.clamp(2, 100) / 100).clamp(PATCH, dim));
    let inset_x = w / 20;
    let inset_y = h / 20;
    [
        ((w - px[0]) / 2, (h - px[0]) / 2, px[0]),                                   // center
        (inset_x.min(w - px[1]), inset_y.min(h - px[1]), px[1]),                     // top-left
        (w.saturating_sub(inset_x + px[2]), inset_y.min(h - px[2]), px[2]),          // top-right
        (inset_x.min(w - px[3]), h.saturating_sub(inset_y + px[3]), px[3]),          // bottom-left
        (w.saturating_sub(inset_x + px[4]), h.saturating_sub(inset_y + px[4]), px[4]), // bottom-right
    ]
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
    fn zone_rects_stay_on_screen() {
        // Landscape, portrait, square, tiny, huge — and size extremes.
        for &(w, h) in &[(1920, 1080), (1080, 1920), (800, 800), (100, 60), (7680, 2160)] {
            for &pct in &[2u32, 8, 50, 100] {
                for (i, &(x, y, z)) in zone_rects(w, h, &[pct; 5]).iter().enumerate() {
                    assert!(z >= 1, "zone {i} empty at {w}x{h} pct {pct}");
                    assert!(
                        x + z <= w && y + z <= h,
                        "zone {i} off-screen at {w}x{h} pct {pct}: ({x},{y},{z})"
                    );
                }
            }
        }
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

unsafe fn find_primary_output(
    factory: &IDXGIFactory1,
) -> windows::core::Result<(IDXGIAdapter1, IDXGIOutput)> {
    let mut i = 0;
    loop {
        let adapter = factory.EnumAdapters1(i)?;
        let mut j = 0;
        while let Ok(output) = adapter.EnumOutputs(j) {
            if let Ok(desc) = output.GetDesc() {
                if desc.AttachedToDesktop.as_bool()
                    && desc.DesktopCoordinates.left == 0
                    && desc.DesktopCoordinates.top == 0
                {
                    return Ok((adapter, output));
                }
            }
            j += 1;
        }
        i += 1;
    }
}
