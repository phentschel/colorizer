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
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC, HDC};
use windows::Win32::UI::ColorSystem::{GetDeviceGammaRamp, SetDeviceGammaRamp};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY};


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
    register_hotkey(hotkey.0, hotkey.1);

    // Transition state: eased ramp from `start` to `target` beginning at `t0`.
    let mut cur = 1.0f32;
    let mut start = 1.0f32;
    let mut target = 1.0f32;
    let mut t0 = Instant::now();
    let mut dur_s = 0.001f32;
    let mut last_measured = f32::MIN;

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
            if cap.is_none() && now >= next_reinit {
                match Capture::new(cfg.zone_sizes) {
                    Ok(c) => {
                        shared.screen_dims.store(
                            ((c.width as u64) << 32) | c.height as u64,
                            Relaxed,
                        );
                        cap = Some(c);
                        shared.capture_ok.store(true, Relaxed);
                        reinit_backoff = Duration::from_secs(2);
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
                        for (slot, m) in shared.zone_medians.iter().zip(&medians) {
                            slot.store(*m as u32, Relaxed);
                        }
                        let m = combine(&medians, &cfg.zone_weights);
                        shared.measured.store(m as u32, Relaxed);
                        let mf = m as f32;
                        // Deadband: ±3 counts of flicker never retargets.
                        if (mf - last_measured).abs() >= 3.0 {
                            last_measured = mf;
                            let want = target_gamma(&cfg, mf);
                            if (want - target).abs() > 0.005 {
                                start = cur;
                                target = want;
                                t0 = now;
                                // Less boost = the scene got brighter: ramp
                                // down fast so it doesn't glare.
                                let ms = if want < cur {
                                    cfg.transition_down_ms
                                } else {
                                    cfg.transition_ms
                                };
                                dur_s = ms.max(1) as f32 / 1000.0;
                            }
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
            if cap.is_some() {
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
            gamma.set(cur);
            shared.gamma_milli.store((cur * 1000.0).round() as u32, Relaxed);
            shared.gamma_ok.store(gamma.ok, Relaxed);
        } else {
            gamma.restore();
            shared.gamma_milli.store(1000, Relaxed);
            shared.gamma_ok.store(true, Relaxed);
            cur = 1.0;
            start = 1.0;
            target = 1.0;
            last_measured = f32::MIN;
        }

        std::thread::sleep(Duration::from_millis(if transitioning {
            16
        } else if active {
            50
        } else {
            100
        }));
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
    original: [u16; 768],
    /// The exact ramp we last wrote to the hardware (original when neutral).
    last_written: [u16; 768],
    applied: Option<f32>,
    dir: Option<std::path::PathBuf>,
    marker: bool,
    /// False when the driver rejected our last SetDeviceGammaRamp.
    ok: bool,
}

impl Gamma {
    fn new() -> Self {
        let hdc = unsafe { GetDC(None) };
        let mut captured = [0u16; 768];
        unsafe {
            let _ = GetDeviceGammaRamp(hdc, captured.as_mut_ptr() as *mut _);
        }
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
        Self { hdc, original, last_written: original, applied: None, dir, marker, ok: true }
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
            unsafe {
                let _ = SetDeviceGammaRamp(self.hdc, self.original.as_ptr() as *const _);
            }
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
        if !unsafe { GetDeviceGammaRamp(self.hdc, hw.as_mut_ptr() as *mut _) }.as_bool() {
            return;
        }
        if self.applied.is_some() {
            if hw != self.last_written {
                self.ok = unsafe {
                    SetDeviceGammaRamp(self.hdc, self.last_written.as_ptr() as *const _)
                }
                .as_bool();
            }
        } else if !self.marker && hw != self.original {
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
            ReleaseDC(None, self.hdc);
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
