//! Pure geometry for the on-screen zone overlay. No Win32 in here, so the
//! layout rules can be unit-tested on any platform.
//!
//! Everything is drawn *outside* the measured zones: the overlay window is
//! also excluded from capture where Windows supports it, but on older builds
//! Desktop Duplication would see it, and a border painted inside a zone would
//! bias that zone's median. Keeping the pixels out of the zones makes the
//! overlay harmless either way.

/// Axis-aligned rectangle in output-local physical pixels. `w`/`h` may be 0
/// (an item clipped away entirely); such items are dropped from a layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// True when the two rectangles share at least one pixel.
    #[cfg(test)]
    pub fn intersects(&self, o: &Rect) -> bool {
        !self.is_empty()
            && !o.is_empty()
            && self.x < o.right()
            && o.x < self.right()
            && self.y < o.bottom()
            && o.y < self.bottom()
    }

    #[cfg(test)]
    pub fn contains(&self, o: &Rect) -> bool {
        o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }

    /// Intersection with `[0, w) x [0, h)`; may come back empty.
    pub fn clip(&self, w: i32, h: i32) -> Rect {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = self.right().min(w);
        let y1 = self.bottom().min(h);
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        Rect::new(x0, y0, self.right().max(o.right()) - x0, self.bottom().max(o.bottom()) - y0)
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

/// Zone rectangle as produced by `engine::zone_rects`: (x, y, side).
pub fn zone_rect(z: (u32, u32, u32)) -> Rect {
    Rect::new(z.0 as i32, z.1 as i32, z.2 as i32, z.2 as i32)
}

/// Overlay content for one screen, in output-local pixels.
#[derive(Debug, Default, PartialEq)]
pub struct Layout {
    /// Border bars, tagged with their zone index.
    pub bars: Vec<(usize, Rect)>,
    /// Label boxes, tagged with their zone index.
    pub labels: Vec<(usize, Rect)>,
    /// Smallest rectangle holding every bar and label; the overlay window is
    /// sized to this rather than the whole screen, so the shell never mistakes
    /// it for a fullscreen app. `None` when there is nothing to draw.
    pub bounds: Option<Rect>,
}

/// Gap between a zone's border and its label.
const LABEL_GAP: i32 = 2;

/// Lay out a border of `stroke` px just outside each zone plus a label box of
/// `label_sizes[i]` next to it: above the zone when there is room, otherwise
/// below, and only as a last resort inside its top-left corner.
pub fn layout(
    screen_w: u32,
    screen_h: u32,
    zones: &[(u32, u32, u32); 5],
    stroke: i32,
    label_sizes: &[(i32, i32); 5],
) -> Layout {
    let (sw, sh) = (screen_w as i32, screen_h as i32);
    let s = stroke.max(1);
    let mut out = Layout::default();

    for (i, &z) in zones.iter().enumerate() {
        let r = zone_rect(z);
        let sides = [
            Rect::new(r.x - s, r.y - s, r.w + 2 * s, s), // top (with corners)
            Rect::new(r.x - s, r.bottom(), r.w + 2 * s, s), // bottom (with corners)
            Rect::new(r.x - s, r.y, s, r.h),             // left
            Rect::new(r.right(), r.y, s, r.h),           // right
        ];
        for bar in sides {
            let c = bar.clip(sw, sh);
            if !c.is_empty() {
                out.bars.push((i, c));
            }
        }

        let (lw, lh) = label_sizes[i];
        if lw <= 0 || lh <= 0 {
            continue;
        }
        let above = r.y - s - LABEL_GAP - lh;
        let below = r.bottom() + s + LABEL_GAP;
        let (lx, ly) = if above >= 0 {
            (r.x, above)
        } else if below + lh <= sh {
            (r.x, below)
        } else {
            (r.x + s, r.y + s) // zone fills the height: nowhere else to go
        };
        let lx = lx.min(sw - lw).max(0);
        let label = Rect::new(lx, ly, lw, lh).clip(sw, sh);
        if !label.is_empty() {
            out.labels.push((i, label));
        }
    }

    out.bounds = out
        .bars
        .iter()
        .chain(&out.labels)
        .map(|(_, r)| *r)
        .reduce(|a, b| a.union(&b));
    out
}

/// Colour key for the overlay window: pixels of exactly this colour are fully
/// transparent and click-through. Magenta, which no zone colour uses.
pub const KEY_RGB: (u8, u8, u8) = (255, 0, 255);

/// Border/label colour per zone (center, TL, TR, BL, BR). Zones with no
/// influence are drawn grey so it's obvious they're ignored.
pub fn zone_color(i: usize, weight: u32) -> (u8, u8, u8) {
    if weight == 0 {
        return (140, 140, 140);
    }
    const COLORS: [(u8, u8, u8); 5] = [
        (255, 170, 0),  // center: amber
        (0, 200, 255),  // top-left: cyan
        (80, 220, 80),  // top-right: green
        (255, 90, 90),  // bottom-left: red
        (190, 140, 255), // bottom-right: violet
    ];
    COLORS[i % 5]
}

/// "Center ×2 · 42" — zone name, influence and the last measured median
/// (`None` before the first sample).
pub fn label_text(name: &str, weight: u32, median: Option<u32>) -> String {
    match median {
        Some(m) => format!("{name} \u{00D7}{weight} \u{00B7} {m}"),
        None => format!("{name} \u{00D7}{weight}"),
    }
}

/// Desktop position of the measured output packed into one u64 (x high, y
/// low) so it fits an atomic. Coordinates can be negative for a monitor left
/// of or above the primary one.
pub fn pack_origin(x: i32, y: i32) -> u64 {
    ((x as u32 as u64) << 32) | y as u32 as u64
}

pub fn unpack_origin(v: u64) -> (i32, i32) {
    ((v >> 32) as u32 as i32, v as u32 as i32)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Same as engine::zone_rects; duplicated here only so this module's tests
    /// run without the Windows-only engine. `zone_rects_matches_engine` in
    /// engine.rs keeps the two in lockstep.
    pub(crate) fn zones(w: u32, h: u32, sizes: &[u32; 5]) -> [(u32, u32, u32); 5] {
        const PATCH: u32 = 16;
        let dim = w.min(h).max(PATCH);
        let px = sizes.map(|pct| (dim * pct.clamp(2, 100) / 100).clamp(PATCH, dim));
        let inset_x = w / 20;
        let inset_y = h / 20;
        [
            ((w - px[0]) / 2, (h - px[0]) / 2, px[0]),
            (inset_x.min(w - px[1]), inset_y.min(h - px[1]), px[1]),
            (w.saturating_sub(inset_x + px[2]), inset_y.min(h - px[2]), px[2]),
            (inset_x.min(w - px[3]), h.saturating_sub(inset_y + px[3]), px[3]),
            (w.saturating_sub(inset_x + px[4]), h.saturating_sub(inset_y + px[4]), px[4]),
        ]
    }

    const SCREENS: [(u32, u32); 6] =
        [(1920, 1080), (2560, 1440), (3840, 2160), (1080, 1920), (800, 800), (100, 60)];
    const LABEL: [(i32, i32); 5] = [(120, 18); 5];

    #[test]
    fn borders_never_touch_their_own_zone() {
        for &(w, h) in &SCREENS {
            for pct in [2u32, 8, 20, 50, 100] {
                let zs = zones(w, h, &[pct; 5]);
                let l = layout(w, h, &zs, 3, &LABEL);
                for (i, bar) in &l.bars {
                    let z = zone_rect(zs[*i]);
                    assert!(!bar.intersects(&z), "zone {i} border inside zone at {w}x{h} {pct}%: {bar:?}");
                }
            }
        }
    }

    #[test]
    fn default_overlay_stays_out_of_every_zone() {
        // With the default sizes nothing the overlay draws — borders or
        // labels — may land in any measured zone, or it would skew sampling.
        for &(w, h) in &SCREENS[..5] {
            let zs = zones(w, h, &[8; 5]);
            let l = layout(w, h, &zs, 3, &LABEL);
            for (_, item) in l.bars.iter().chain(&l.labels) {
                for (j, z) in zs.iter().enumerate() {
                    assert!(!item.intersects(&zone_rect(*z)), "{item:?} overlaps zone {j} at {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn everything_stays_on_screen() {
        for &(w, h) in &SCREENS {
            for pct in [2u32, 8, 50, 100] {
                let zs = zones(w, h, &[pct; 5]);
                let l = layout(w, h, &zs, 4, &[(300, 30); 5]);
                let screen = Rect::new(0, 0, w as i32, h as i32);
                for (_, r) in l.bars.iter().chain(&l.labels) {
                    assert!(!r.is_empty());
                    assert!(screen.contains(r), "{r:?} off-screen at {w}x{h} {pct}%");
                }
            }
        }
    }

    #[test]
    fn every_zone_gets_a_border_and_a_label() {
        let zs = zones(1920, 1080, &[8; 5]);
        let l = layout(1920, 1080, &zs, 2, &LABEL);
        for i in 0..5 {
            assert_eq!(l.bars.iter().filter(|(z, _)| *z == i).count(), 4, "zone {i} bars");
            assert_eq!(l.labels.iter().filter(|(z, _)| *z == i).count(), 1, "zone {i} label");
        }
    }

    #[test]
    fn label_goes_above_or_falls_back_below() {
        let s = 2;
        // Center zone has plenty of room above.
        let zs = zones(1920, 1080, &[8; 5]);
        let l = layout(1920, 1080, &zs, s, &LABEL);
        let center = zone_rect(zs[0]);
        let lab = l.labels.iter().find(|(i, _)| *i == 0).unwrap().1;
        assert_eq!(lab.bottom(), center.y - s - LABEL_GAP);
        assert_eq!(lab.x, center.x);

        // A zone hugging the top edge puts its label underneath instead.
        let top = [(500, 0, 100); 5];
        let l = layout(1920, 1080, &top, s, &LABEL);
        let lab = l.labels[0].1;
        assert_eq!(lab.y, 100 + s + LABEL_GAP);
    }

    #[test]
    fn label_falls_inside_when_zone_fills_height() {
        let full = [(0, 0, 60); 5];
        let l = layout(100, 60, &full, 1, &[(40, 10); 5]);
        let lab = l.labels[0].1;
        assert_eq!((lab.x, lab.y), (1, 1));
    }

    #[test]
    fn label_is_pulled_back_from_the_right_edge() {
        let z = [(1900, 500, 20); 5];
        let l = layout(1920, 1080, &z, 1, &[(200, 20); 5]);
        assert_eq!(l.labels[0].1.right(), 1920);
    }

    #[test]
    fn edge_borders_are_clipped_not_wrapped() {
        // Zone touching the top-left corner: the top and left bars are fully
        // off-screen and must be dropped, not drawn at negative coordinates.
        let z = [(0, 0, 100); 5];
        let l = layout(1920, 1080, &z, 3, &[(0, 0); 5]);
        let mine: Vec<_> = l.bars.iter().filter(|(i, _)| *i == 0).map(|(_, r)| *r).collect();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|r| r.x >= 0 && r.y >= 0));
    }

    #[test]
    fn bounds_cover_everything_but_not_the_whole_screen() {
        let zs = zones(1920, 1080, &[8; 5]);
        let l = layout(1920, 1080, &zs, 3, &LABEL);
        let b = l.bounds.expect("non-empty layout");
        for (_, r) in l.bars.iter().chain(&l.labels) {
            assert!(b.contains(r), "{r:?} outside bounds {b:?}");
        }
        // Default zones are inset 5%, so the window never spans the monitor
        // (a monitor-sized topmost window can trip fullscreen detection).
        assert!(b.w < 1920 && b.h < 1080, "bounds {b:?}");
    }

    #[test]
    fn empty_layout_has_no_bounds() {
        assert_eq!(Layout::default().bounds, None);
    }

    #[test]
    fn rect_helpers() {
        let a = Rect::new(0, 0, 10, 10);
        assert!(a.intersects(&Rect::new(9, 9, 5, 5)));
        assert!(!a.intersects(&Rect::new(10, 0, 5, 5))); // touching edges don't overlap
        assert!(!a.intersects(&Rect::new(2, 2, 0, 5))); // empty never overlaps
        assert_eq!(Rect::new(-5, -5, 10, 10).clip(100, 100), Rect::new(0, 0, 5, 5));
        assert!(Rect::new(200, 0, 10, 10).clip(100, 100).is_empty());
        assert_eq!(a.offset(3, -2), Rect::new(3, -2, 10, 10));
        assert_eq!(a.union(&Rect::new(20, 5, 5, 20)), Rect::new(0, 0, 25, 25));
    }

    #[test]
    fn zone_colors_are_distinct_and_never_the_key() {
        let cs: Vec<_> = (0..5).map(|i| zone_color(i, 1)).collect();
        for (i, c) in cs.iter().enumerate() {
            assert_ne!(*c, KEY_RGB, "zone {i} colour would be transparent");
            assert_eq!(cs.iter().filter(|o| *o == c).count(), 1, "zone {i} colour reused");
        }
        assert_eq!(zone_color(0, 0), zone_color(3, 0)); // ignored zones look alike
        assert_ne!(zone_color(0, 0), KEY_RGB);
    }

    #[test]
    fn origin_roundtrips_including_negative() {
        for (x, y) in [(0, 0), (1920, 0), (-2560, -300), (i32::MIN, i32::MAX), (-1, 1)] {
            assert_eq!(unpack_origin(pack_origin(x, y)), (x, y));
        }
        assert_eq!(pack_origin(0, 0), 0);
    }

    #[test]
    fn label_text_formats() {
        assert_eq!(label_text("Center", 2, Some(42)), "Center \u{00D7}2 \u{00B7} 42");
        assert_eq!(label_text("Top-left", 0, None), "Top-left \u{00D7}0");
    }
}
