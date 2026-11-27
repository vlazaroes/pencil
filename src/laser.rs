//! Laser pointer: a glowing dot that follows the cursor and leaves a short trail
//! that fades away.
//!
//! The laser is drawn on top of the canvas only for the current frame: the pixels
//! it covers are saved first and put back before anything else changes, so the
//! strokes underneath are never modified.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::raster::{blend, sd_segment, Rect, Rgb, Surface};

pub const COLOR: Rgb = Rgb(255, 38, 38);
/// How long a point of the trail stays visible.
const LIFE: Duration = Duration::from_millis(150);
/// Radii in logical pixels (scaled with the monitor DPI).
const CORE: f32 = 3.0;
/// The dot is a pixel wider than the thickest (newest) part of the trail.
const HEAD_CORE: f32 = CORE + 0.5;
const GLOW: f32 = 10.0;
const GLOW_ALPHA: f32 = 0.3;
/// Interval of the fading animation (and of the cursor polling, a fallback for
/// the mouse hook).
pub const FRAME_MS: u32 = 16;
/// Shortest time between two frames driven by mouse moves (about 250 per second):
/// a fast mouse reports up to 1000 moves per second, more than the screen shows.
pub const MIN_FRAME: Duration = Duration::from_millis(4);

pub struct Laser {
    /// Cursor positions, oldest first; the last one is the dot.
    points: VecDeque<((f32, f32), Instant)>,
    /// Canvas pixels under the last drawn frame, and where they go.
    saved: Option<(Rect, Vec<u32>)>,
}

impl Laser {
    pub fn new() -> Laser {
        Laser { points: VecDeque::new(), saved: None }
    }

    pub fn push(&mut self, p: (f32, f32)) {
        if self.points.back().is_some_and(|(q, _)| *q == p) {
            return;
        }
        self.points.push_back((p, Instant::now()));
    }

    pub fn clear(&mut self) {
        self.points.clear();
    }

    /// Current dot position.
    pub fn head(&self) -> Option<(f32, f32)> {
        self.points.back().map(|(p, _)| *p)
    }

    /// Drops the points that have faded out; the newest one (the dot) always stays.
    pub fn prune(&mut self, now: Instant) {
        while self.points.len() >= 2 && now - self.points[1].1 >= LIFE {
            self.points.pop_front();
        }
    }

    /// Is there a trail behind the dot (still fading, so frames must go on)?
    pub fn trailing(&self) -> bool {
        self.points.len() >= 2
    }

    /// Puts back the pixels covered by the last `draw`. Returns that area.
    pub fn erase(&mut self, cv: &mut Surface) -> Rect {
        let Some((r, mut px)) = self.saved.take() else { return Rect::EMPTY };
        Surface { px: &mut px, w: r.width(), h: r.height(), ox: r.l, oy: r.t }.blit_into(cv);
        r
    }

    /// Draws the trail (and the dot unless `head` is false) for the instant `now`,
    /// saving the pixels it covers. Returns the modified area.
    pub fn draw(&mut self, cv: &mut Surface, s: f32, head: bool, now: Instant) -> Rect {
        debug_assert!(self.saved.is_none(), "erase before drawing again");
        let life = LIFE.as_secs_f32();
        // Position and remaining strength (1 = new, 0 = gone) of every point.
        let pts: Vec<((f32, f32), f32)> = self
            .points
            .iter()
            .map(|(p, t)| (*p, (1.0 - (now - *t).as_secs_f32() / life).clamp(0.0, 1.0)))
            .collect();
        if pts.is_empty() || (pts.len() == 1 && !head) {
            return Rect::EMPTY;
        }
        let m = GLOW * s + 2.0;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for ((x, y), _) in &pts {
            (x0, y0, x1, y1) = (x0.min(*x), y0.min(*y), x1.max(*x), y1.max(*y));
        }
        let area = Rect::bounds(x0 - m, y0 - m, x1 + m, y1 + m).intersect(&cv.rect());
        if area.is_empty() {
            return Rect::EMPTY;
        }

        // Coverage per pixel, combined with `max` so overlapping segments never
        // add up into darker beads along the trail.
        let w = area.width() as usize;
        let mut cov = vec![0f32; w * area.height() as usize];
        let mut stamp = |a: (f32, f32), b: (f32, f32), r: f32, rg: f32, f: f32| {
            let bounds = Rect::bounds(a.0.min(b.0) - rg - 1.0, a.1.min(b.1) - rg - 1.0, a.0.max(b.0) + rg + 1.0, a.1.max(b.1) + rg + 1.0)
                .intersect(&area);
            for y in bounds.t..bounds.b {
                let row = (y - area.t) as usize * w;
                for x in bounds.l..bounds.r {
                    let d = sd_segment(x as f32 + 0.5, y as f32 + 0.5, a, b);
                    let core = (0.5 - (d - r)).clamp(0.0, 1.0);
                    let glow = (1.0 - d / rg).max(0.0);
                    let v = core.max(glow * glow * GLOW_ALPHA) * f;
                    let c = &mut cov[row + (x - area.l) as usize];
                    *c = c.max(v);
                }
            }
        };
        for seg in pts.windows(2) {
            // A segment fades with its newer end, so it is gone just as that point is dropped.
            let f = seg[1].1;
            if f > 0.0 {
                stamp(seg[0].0, seg[1].0, CORE * s * (0.3 + 0.7 * f), GLOW * s * (0.5 + 0.5 * f), f);
            }
        }
        if head {
            let p = pts[pts.len() - 1].0;
            stamp(p, p, HEAD_CORE * s, GLOW * s, 1.0);
        }

        let mut saved = vec![0u32; cov.len()];
        cv.blit_into(&mut Surface { px: &mut saved, w: area.width(), h: area.height(), ox: area.l, oy: area.t });
        for y in area.t..area.b {
            let src = (y - area.t) as usize * w;
            let dst = ((y - cv.oy) * cv.w + (area.l - cv.ox)) as usize;
            for (i, c) in cov[src..src + w].iter().enumerate() {
                let a = (c * 255.0 + 0.5) as u32;
                if a != 0 {
                    cv.px[dst + i] = blend(cv.px[dst + i], COLOR, a);
                }
            }
        }
        self.saved = Some((area, saved));
        area
    }
}
