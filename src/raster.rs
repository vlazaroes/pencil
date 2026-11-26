//! Minimal software rasterizer over premultiplied ARGB buffers.
//! Every shape is drawn through a signed distance function (SDF),
//! which gives antialiasing for free with very little code.

/// Opaque RGB color.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Half-open integer rectangle [l, r) x [t, b).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Rect {
    pub const EMPTY: Rect = Rect { l: 0, t: 0, r: 0, b: 0 };

    pub fn new(l: i32, t: i32, r: i32, b: i32) -> Self {
        Rect { l, t, r, b }
    }

    /// Rectangle that contains the float box [x0,x1]x[y0,y1].
    pub fn bounds(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Rect {
            l: x0.floor() as i32,
            t: y0.floor() as i32,
            r: x1.ceil() as i32 + 1,
            b: y1.ceil() as i32 + 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }

    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect {
            l: self.l.min(o.l),
            t: self.t.min(o.t),
            r: self.r.max(o.r),
            b: self.b.max(o.b),
        }
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let r = Rect {
            l: self.l.max(o.l),
            t: self.t.max(o.t),
            r: self.r.min(o.r),
            b: self.b.min(o.b),
        };
        if r.is_empty() {
            Rect::EMPTY
        } else {
            r
        }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }

    pub fn width(&self) -> i32 {
        self.r - self.l
    }

    pub fn height(&self) -> i32 {
        self.b - self.t
    }
}

/// Mutable view over a pixel buffer. `ox/oy` is the global coordinate of pixel
/// (0,0), so several surfaces share the same coordinate system.
pub struct Surface<'a> {
    pub px: &'a mut [u32],
    pub w: i32,
    pub h: i32,
    pub ox: i32,
    pub oy: i32,
}

impl<'a> Surface<'a> {
    pub fn rect(&self) -> Rect {
        Rect::new(self.ox, self.oy, self.ox + self.w, self.oy + self.h)
    }

    pub fn fill(&mut self, v: u32) {
        self.px.fill(v);
    }

    /// Fills the shape described by `sdf` (signed distance, negative inside)
    /// restricted to `bounds` (global coordinates).
    #[inline]
    pub fn fill_sdf(&mut self, bounds: Rect, c: Rgb, opacity: f32, sdf: impl Fn(f32, f32) -> f32) {
        let area = bounds.intersect(&self.rect());
        if area.is_empty() {
            return;
        }
        let w = self.w as usize;
        for y in area.t..area.b {
            let py = y as f32 + 0.5;
            let row = (y - self.oy) as usize * w;
            for x in area.l..area.r {
                let cov = (0.5 - sdf(x as f32 + 0.5, py)).clamp(0.0, 1.0) * opacity;
                let a = (cov * 255.0 + 0.5) as u32;
                if a != 0 {
                    let i = row + (x - self.ox) as usize;
                    self.px[i] = blend(self.px[i], c, a);
                }
            }
        }
    }

    /// Segment with rounded caps (also works as a dot when a == b).
    pub fn capsule(&mut self, a: (f32, f32), b: (f32, f32), radius: f32, c: Rgb) -> Rect {
        let m = radius + 1.0;
        let bounds = Rect::bounds(a.0.min(b.0) - m, a.1.min(b.1) - m, a.0.max(b.0) + m, a.1.max(b.1) + m);
        self.fill_sdf(bounds, c, 1.0, |x, y| sd_segment(x, y, a, b) - radius);
        bounds
    }

    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, c: Rgb) {
        self.capsule((cx, cy), (cx, cy), radius, c);
    }

    pub fn round_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32, c: Rgb) {
        let (cx, cy, hw, hh) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5, (x1 - x0) * 0.5, (y1 - y0) * 0.5);
        let bounds = Rect::bounds(x0 - 1.0, y0 - 1.0, x1 + 1.0, y1 + 1.0);
        self.fill_sdf(bounds, c, 1.0, |x, y| sd_round_rect(x - cx, y - cy, hw, hh, radius));
    }

    /// Rotated box: center `c`, unit axis `u`, half extents `hw` (along u) and `hh`.
    pub fn oriented_box(&mut self, c: (f32, f32), u: (f32, f32), hw: f32, hh: f32, radius: f32, col: Rgb) {
        let m = hw.max(hh) * 1.5 + 1.0;
        let bounds = Rect::bounds(c.0 - m, c.1 - m, c.0 + m, c.1 + m);
        self.fill_sdf(bounds, col, 1.0, |x, y| {
            let (dx, dy) = (x - c.0, y - c.1);
            // Project onto the local frame of the box.
            let lx = dx * u.0 + dy * u.1;
            let ly = -dx * u.1 + dy * u.0;
            sd_round_rect(lx, ly, hw, hh, radius)
        });
    }

    /// Copies this surface into `dst`, in global coordinates.
    pub fn blit_into(&self, dst: &mut Surface) {
        let area = self.rect().intersect(&dst.rect());
        if area.is_empty() {
            return;
        }
        let n = area.width() as usize;
        for y in area.t..area.b {
            let s = (y - self.oy) as usize * self.w as usize + (area.l - self.ox) as usize;
            let d = (y - dst.oy) as usize * dst.w as usize + (area.l - dst.ox) as usize;
            dst.px[d..d + n].copy_from_slice(&self.px[s..s + n]);
        }
    }
}

/// `src over dst` blend in premultiplied ARGB; `a` is the coverage 0..=255.
#[inline]
fn blend(dst: u32, c: Rgb, a: u32) -> u32 {
    let (r, g, b) = (c.0 as u32, c.1 as u32, c.2 as u32);
    if a >= 255 {
        return 0xFF00_0000 | (r << 16) | (g << 8) | b;
    }
    let ia = 255 - a;
    let mix = |s: u32, d: u32| (s * a + d * ia + 127) / 255;
    let da = dst >> 24;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    (mix(255, da) << 24) | (mix(r, dr) << 16) | (mix(g, dg) << 8) | mix(b, db)
}

#[inline]
fn sd_segment(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pax, pay) = (px - a.0, py - a.1);
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let len2 = bax * bax + bay * bay;
    let h = if len2 > 0.0 { ((pax * bax + pay * bay) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (pax - bax * h, pay - bay * h);
    (dx * dx + dy * dy).sqrt()
}

/// Rounded-rectangle SDF centered at the origin.
#[inline]
fn sd_round_rect(px: f32, py: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = px.abs() - (hw - r);
    let qy = py.abs() - (hh - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}
