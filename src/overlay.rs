//! On-screen zone overlay: a click-through, topmost, colour-keyed window over
//! the measured display that outlines each zone and labels it with its
//! influence and live brightness.
//!
//! Runs on its own thread with plain Win32 + GDI rather than as an egui
//! viewport, so it keeps working while the settings window is hidden in the
//! tray (a hidden eframe window stops running `update()`). The thread only
//! exists while the overlay is switched on; it exits as soon as it is
//! switched off.
//!
//! The overlay must never change what it is showing. Two layers keep it out
//! of the measurement: the window asks to be excluded from screen capture
//! (Windows 10 2004+), and independently `overlay_layout` places every pixel
//! outside the zones.

use crate::overlay_layout::{self, Layout, Rect, KEY_RGB};
use crate::{Shared, ZONE_NAMES};
use std::cell::RefCell;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetDC,
    GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    TextOutW, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FW_BOLD,
    HFONT, HGDIOBJ, NONANTIALIASED_QUALITY, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetWindowLongPtrW,
    LoadCursorW, PeekMessageW, SetCursor, SetWindowLongPtrW, GWL_EXSTYLE, IDC_SIZEALL, WM_CAPTURECHANGED,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_SETCURSOR,
    RegisterClassW, SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowPos,
    ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_COLORKEY, MSG, PM_REMOVE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, WDA_EXCLUDEFROMCAPTURE, WM_ERASEBKGND, WM_PAINT, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

const CLASS: PCWSTR = w!("AutoBrightZoneOverlay");
/// Dark plate behind label text so it reads on any background.
const LABEL_BG: (u8, u8, u8) = (24, 24, 24);
const LABEL_PAD: i32 = 4;

/// What is currently on screen. A new snapshot that compares equal means
/// nothing to redraw, so a static screen costs one comparison per tick.
#[derive(Clone, PartialEq)]
struct Snapshot {
    origin: (i32, i32),
    dims: (u32, u32),
    sizes: [u32; 5],
    pos: [(u32, u32); 5],
    weights: [u32; 5],
    medians: Option<[u32; 5]>,
}

/// Everything WM_PAINT needs, positioned in window-client coordinates.
struct Scene {
    layout: Layout,
    texts: [Vec<u16>; 5],
    colors: [COLORREF; 5],
    font: HFONT,
}

impl Drop for Scene {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.font.0));
        }
    }
}

/// A zone being dragged: which one, and where the cursor grabbed it relative
/// to the zone's centre (output-local pixels), so it doesn't jump.
#[derive(Clone, Copy)]
struct Drag {
    zone: usize,
    dx: i32,
    dy: i32,
}

thread_local! {
    static SCENE: RefCell<Option<Scene>> = const { RefCell::new(None) };
    /// The wndproc has no user data; it reaches shared state through this.
    static SHARED: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
    static DRAG: std::cell::Cell<Option<Drag>> = const { std::cell::Cell::new(None) };
}

/// Slack around a thin border bar that still counts as grabbing it.
const GRAB_SLOP: i32 = 5;

fn rgb((r, g, b): (u8, u8, u8)) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn win_rect(r: &Rect) -> RECT {
    RECT { left: r.x, top: r.y, right: r.right(), bottom: r.bottom() }
}

/// Start the overlay thread. It runs until `shared.show_overlay` goes false
/// or the app shuts down, then destroys its window and exits.
pub fn spawn(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("autobright-overlay".into())
        .spawn(move || {
            // Zone rects are in physical pixels; this thread's window must be
            // too. winit already makes the process DPI-aware, but don't rely
            // on initialisation order.
            unsafe {
                let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            if let Some(hwnd) = create_window() {
                SHARED.with(|s| *s.borrow_mut() = Some(shared.clone()));
                run(&shared, hwnd);
                SCENE.with(|s| s.borrow_mut().take());
                SHARED.with(|s| s.borrow_mut().take());
                DRAG.with(|d| d.set(None));
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
        })
        .expect("spawn overlay thread")
}

fn create_window() -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        // Fails harmlessly with "already registered" on a second spawn.
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            CLASS,
            w!("AutoBright zones"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;
        // Key colour = see-through and click-through.
        let _ = SetLayeredWindowAttributes(hwnd, rgb(KEY_RGB), 0, LWA_COLORKEY);
        // Keep the overlay out of Desktop Duplication so it can't skew the
        // samples. Unsupported before Windows 10 2004 — the layout keeps its
        // pixels outside the zones regardless.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        Some(hwnd)
    }
}

fn run(shared: &Shared, hwnd: HWND) {
    let mut shown: Option<Snapshot> = None;
    let mut editing = false;
    while shared.running.load(Relaxed) && shared.show_overlay.load(Relaxed) {
        // Click-through normally; grabbable while the user is placing zones.
        let want_edit = shared.edit_zones.load(Relaxed);
        if want_edit != editing {
            set_click_through(hwnd, !want_edit);
            editing = want_edit;
        }
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let snap = snapshot(shared);
        if snap != shown {
            match &snap {
                Some(s) => apply(hwnd, s),
                None => unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                },
            }
            shown = snap;
        }
        // Poll quickly while dragging so the zone keeps up with the cursor.
        std::thread::sleep(Duration::from_millis(if editing { 16 } else { 100 }));
    }
    if editing {
        set_click_through(hwnd, true);
    }
}

fn set_click_through(hwnd: HWND, on: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let flag = WS_EX_TRANSPARENT.0 as isize;
        let ex = if on { ex | flag } else { ex & !flag };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        if on {
            DRAG.with(|d| d.set(None));
            let _ = ReleaseCapture();
        }
    }
}

/// Which zone a client-area point grabs: label first, then its border.
fn hit_zone(x: i32, y: i32) -> Option<usize> {
    SCENE.with(|sc| {
        let sc = sc.borrow();
        let scene = sc.as_ref()?;
        let inside = |r: &Rect, slop: i32| {
            x >= r.x - slop && x < r.right() + slop && y >= r.y - slop && y < r.bottom() + slop
        };
        scene
            .layout
            .labels
            .iter()
            .find(|(_, r)| inside(r, 0))
            .or_else(|| scene.layout.bars.iter().find(|(_, r)| inside(r, GRAB_SLOP)))
            .map(|(i, _)| *i)
    })
}

/// Cursor in output-local pixels, plus that output's size and desktop origin.
fn cursor_local(shared: &Shared) -> Option<((i32, i32), (u32, u32))> {
    let dims = shared.screen_dims.load(Relaxed);
    if dims == 0 {
        return None;
    }
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    let (ox, oy) = overlay_layout::unpack_origin(shared.screen_origin.load(Relaxed));
    Some(((p.x - ox, p.y - oy), ((dims >> 32) as u32, dims as u32)))
}

fn begin_drag(hwnd: HWND, x: i32, y: i32) {
    let Some(zone) = hit_zone(x, y) else { return };
    let Some(shared) = SHARED.with(|s| s.borrow().clone()) else { return };
    let Some(((cx, cy), (w, h))) = cursor_local(&shared) else { return };
    let cfg = *shared.cfg.lock().unwrap();
    let (zx, zy, zs) = crate::overlay_layout::zone_rects(w, h, &cfg.zone_sizes, &cfg.zone_pos)[zone];
    let centre = (zx as i32 + zs as i32 / 2, zy as i32 + zs as i32 / 2);
    DRAG.with(|d| d.set(Some(Drag { zone, dx: cx - centre.0, dy: cy - centre.1 })));
    unsafe {
        SetCapture(hwnd);
    }
}

fn drag_to_cursor(d: Drag) {
    let Some(shared) = SHARED.with(|s| s.borrow().clone()) else { return };
    let Some(((cx, cy), (w, h))) = cursor_local(&shared) else { return };
    let pos = overlay_layout::pos_from_center(cx - d.dx, cy - d.dy, w, h);
    shared.cfg.lock().unwrap().zone_pos[d.zone] = pos;
}

fn end_drag() {
    if DRAG.with(|d| d.take()).is_none() {
        return;
    }
    unsafe {
        let _ = ReleaseCapture();
    }
    // The settings window may be hidden in the tray (its update() isn't
    // running), so persist here instead of relying on it.
    if let Some(shared) = SHARED.with(|s| s.borrow().clone()) {
        let cfg = *shared.cfg.lock().unwrap();
        crate::save_config(&cfg);
    }
}

/// Current state from the engine; `None` until the screen size is known.
fn snapshot(shared: &Shared) -> Option<Snapshot> {
    let dims = shared.screen_dims.load(Relaxed);
    if dims == 0 {
        return None;
    }
    let cfg = *shared.cfg.lock().unwrap();
    let medians = (shared.measured.load(Relaxed) != u32::MAX)
        .then(|| std::array::from_fn(|i| shared.zone_medians[i].load(Relaxed)));
    Some(Snapshot {
        origin: overlay_layout::unpack_origin(shared.screen_origin.load(Relaxed)),
        dims: ((dims >> 32) as u32, dims as u32),
        sizes: cfg.zone_sizes,
        pos: cfg.zone_pos,
        weights: cfg.zone_weights,
        medians,
    })
}

/// Rebuild the scene for `s`, move/resize the window over it and repaint.
fn apply(hwnd: HWND, s: &Snapshot) {
    unsafe {
        // Park the window on the target display first so its DPI is right.
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), s.origin.0, s.origin.1, 1, 1, SWP_NOACTIVATE);
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let stroke = (2 * dpi + 48) / 96; // 2px at 100%, 3px at 150%, 4px at 200%
        let font = CreateFontW(
            -(13 * dpi / 96),
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            // Antialiased text would fringe against the magenta colour key.
            NONANTIALIASED_QUALITY,
            FF_DONTCARE.0 as u32,
            w!("Segoe UI"),
        );

        let texts: [Vec<u16>; 5] = std::array::from_fn(|i| {
            overlay_layout::label_text(ZONE_NAMES[i], s.weights[i], s.medians.map(|m| m[i]))
                .encode_utf16()
                .collect()
        });
        let mut sizes = [(0, 0); 5];
        let hdc = GetDC(Some(hwnd));
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        for (size, text) in sizes.iter_mut().zip(&texts) {
            let mut ext = SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, text, &mut ext);
            *size = (ext.cx + 2 * LABEL_PAD, ext.cy + LABEL_PAD);
        }
        SelectObject(hdc, old);
        ReleaseDC(Some(hwnd), hdc);

        let zones = crate::overlay_layout::zone_rects(s.dims.0, s.dims.1, &s.sizes, &s.pos);
        let mut layout = overlay_layout::layout(s.dims.0, s.dims.1, &zones, stroke, &sizes);
        let Some(bounds) = layout.bounds else {
            let _ = ShowWindow(hwnd, SW_HIDE);
            let _ = DeleteObject(HGDIOBJ(font.0));
            return;
        };
        // Output-local -> window-client coordinates.
        for (_, r) in layout.bars.iter_mut().chain(layout.labels.iter_mut()) {
            *r = r.offset(-bounds.x, -bounds.y);
        }
        let colors = std::array::from_fn(|i| rgb(overlay_layout::zone_color(i, s.weights[i])));
        SCENE.with(|sc| *sc.borrow_mut() = Some(Scene { layout, texts, colors, font }));

        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            s.origin.0 + bounds.x,
            s.origin.1 + bounds.y,
            bounds.w,
            bounds.h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1), // WM_PAINT covers every pixel
        WM_LBUTTONDOWN => {
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
            begin_drag(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(d) = DRAG.with(|d| d.get()) {
                drag_to_cursor(d);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            end_drag();
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // Only reachable while grabbable (click-through windows get no
            // mouse messages), so a "move" cursor is always right.
            SetCursor(LoadCursorW(None, IDC_SIZEALL).ok());
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let key = CreateSolidBrush(rgb(KEY_RGB));
            FillRect(hdc, &ps.rcPaint, key);
            let _ = DeleteObject(HGDIOBJ(key.0));
            SCENE.with(|sc| {
                let sc = sc.borrow();
                let Some(scene) = sc.as_ref() else { return };
                let brushes = scene.colors.map(|c| CreateSolidBrush(c));
                for (i, r) in &scene.layout.bars {
                    FillRect(hdc, &win_rect(r), brushes[*i]);
                }
                let plate = CreateSolidBrush(rgb(LABEL_BG));
                let old = SelectObject(hdc, HGDIOBJ(scene.font.0));
                SetBkMode(hdc, TRANSPARENT);
                for (i, r) in &scene.layout.labels {
                    FillRect(hdc, &win_rect(r), plate);
                    SetTextColor(hdc, scene.colors[*i]);
                    let _ = TextOutW(hdc, r.x + LABEL_PAD, r.y + LABEL_PAD / 2, &scene.texts[*i]);
                }
                SelectObject(hdc, old);
                let _ = DeleteObject(HGDIOBJ(plate.0));
                for b in brushes {
                    let _ = DeleteObject(HGDIOBJ(b.0));
                }
            });
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
