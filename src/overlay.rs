//! Full-screen overlay window: canvas, bottom toolbar and input handling.
//!
//! The overlay only exists while it is visible. Hiding it destroys it completely
//! (window, DC, bitmap and buffers), so in the background the program holds no
//! screen-sized memory and uses no CPU/GPU.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_MENU, VK_NUMPAD1, VK_NUMPAD4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetCursorPos, GetSystemMetrics, UpdateLayeredWindowIndirect,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    UPDATELAYEREDWINDOWINFO, ULW_ALPHA, WM_CAPTURECHANGED, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::config::{Config, Keys, ThemePref};
use crate::gdi::Dib;
use crate::raster::{Rect, Rgb, Surface};
use crate::wide;

/// Overlay background: black with alpha 1/255. It is visually invisible, but since
/// the alpha is not 0 Windows treats the pixel as "solid" and captures the clicks,
/// so they never reach the windows underneath. (Raise it, e.g. to 0x2000_0000, if
/// you prefer a slight dimming as a visual cue.)
const BG: u32 = 0x0100_0000;

/// Pen colors: blue, red, black, white.
pub const COLORS: [Rgb; 4] = [
    Rgb(0x1E, 0x6F, 0xE8),
    Rgb(0xE5, 0x39, 0x35),
    Rgb(0x11, 0x11, 0x11),
    Rgb(0xFF, 0xFF, 0xFF),
];
/// Stroke diameter in logical pixels (scaled with the monitor DPI).
pub const SIZES: [f32; 4] = [2.0, 4.0, 8.0, 14.0];
const SIZE_ICON_R: [f32; 4] = [2.0, 3.5, 5.5, 8.0];

const ERASER_TIP: Rgb = Rgb(242, 139, 130);
const CLOSE_HOVER: Rgb = Rgb(229, 72, 77);
const WHITE: Rgb = Rgb(255, 255, 255);

/// Toolbar palette, picked from the system light/dark app mode.
struct Theme {
    panel: Rgb,
    hover: Rgb,
    selected: Rgb,
    selected_hover: Rgb,
    icon: Rgb,
    dot_off: Rgb,
    eraser_body: Rgb,
}

const LIGHT: Theme = Theme {
    panel: Rgb(250, 250, 251),
    hover: Rgb(232, 232, 237),
    selected: Rgb(221, 233, 252),
    selected_hover: Rgb(204, 221, 250),
    icon: Rgb(60, 60, 68),
    dot_off: Rgb(200, 200, 206),
    eraser_body: Rgb(96, 96, 106),
};

const DARK: Theme = Theme {
    panel: Rgb(36, 36, 40),
    hover: Rgb(58, 58, 64),
    selected: Rgb(40, 58, 92),
    selected_hover: Rgb(50, 72, 112),
    icon: Rgb(228, 228, 234),
    dot_off: Rgb(92, 92, 100),
    eraser_body: Rgb(176, 176, 186),
};

/// Palette for the user preference; `System` follows the "default app mode"
/// setting (Settings > Personalization > Colors).
fn theme_for(pref: ThemePref) -> &'static Theme {
    match pref {
        ThemePref::Light => return &LIGHT,
        ThemePref::Dark => return &DARK,
        ThemePref::System => {}
    }
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide("AppsUseLightTheme");
    let mut value: u32 = 1;
    let mut len = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut value as *mut u32).cast(),
            &mut len,
        )
    };
    // Missing value (old Windows builds) => light, the Windows default.
    if rc == 0 && value == 0 {
        &DARK
    } else {
        &LIGHT
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Pencil,
    Size,
    Clear,
    Close,
}
const BTNS: [Btn; 4] = [Btn::Pencil, Btn::Size, Btn::Clear, Btn::Close];

/// Action the caller must run *after* releasing the state borrow (these are
/// Win32 calls that send synchronous messages and could re-enter the wndproc).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Capture,
    Release,
    Hide,
}

type FRect = (f32, f32, f32, f32);

struct Layout {
    s: f32,
    main: FRect,
    close: FRect,
    btn: [FRect; 4],
    hit: [Rect; 4],
    /// Area covered by the toolbar (with a margin for antialiasing).
    area: Rect,
}

impl Layout {
    /// Toolbar centered at the bottom of the monitor under the cursor.
    fn new(vx: i32, vy: i32, screen: Rect) -> Layout {
        let (work, dpi) = unsafe {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            let work = if GetMonitorInfoW(mon, &mut mi) != 0 {
                mi.rcWork
            } else {
                RECT { left: vx, top: vy, right: vx + screen.r, bottom: vy + screen.b }
            };
            let (mut dx, mut dy) = (96u32, 96u32);
            if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) != 0 {
                dx = 96;
            }
            (work, dx)
        };
        let s = (dpi as f32 / 96.0).clamp(1.0, 4.0);
        let cx = ((work.left + work.right) / 2 - vx) as f32;
        let bottom = (work.bottom - vy) as f32 - (24.0 * s).round();

        let btn = (40.0 * s).round();
        let gap = (4.0 * s).round();
        let pad = (6.0 * s).round();
        let h = btn + 2.0 * pad;
        let main_w = 3.0 * btn + 2.0 * gap + 2.0 * pad;
        let x0 = (cx - main_w / 2.0).round();
        let main = (x0, bottom - h, x0 + main_w, bottom);
        let cl = main.2 + (14.0 * s).round();
        let close = (cl, bottom - h, cl + h, bottom);

        let mut b = [(0.0, 0.0, 0.0, 0.0); 4];
        for (i, r) in b.iter_mut().take(3).enumerate() {
            let x = main.0 + pad + i as f32 * (btn + gap);
            *r = (x, main.1 + pad, x + btn, main.1 + pad + btn);
        }
        b[3] = close;
        let hit = b.map(|r| Rect::new(r.0 as i32, r.1 as i32, r.2 as i32, r.3 as i32));
        let area = Rect::bounds(main.0 - 2.0, main.1 - 2.0, close.2 + 2.0, close.3 + 2.0).intersect(&screen);
        Layout { s, main, close, btn: b, hit, area }
    }

    fn hit(&self, x: i32, y: i32) -> Option<Btn> {
        if !self.area.contains(x, y) {
            return None;
        }
        BTNS.iter().zip(self.hit.iter()).find(|(_, r)| r.contains(x, y)).map(|(b, _)| *b)
    }
}

pub struct Overlay {
    pub hwnd: HWND,
    dib: Dib,
    /// Copy of the strokes lying under the toolbar, so the toolbar can be redrawn
    /// on top without losing them or duplicating the full-screen buffer.
    under: Vec<u32>,
    lay: Layout,
    theme: &'static Theme,
    keys: Keys,
    pub color: usize,
    pub size: usize,
    hover: Option<Btn>,
    pressed: Option<Btn>,
    drawing: bool,
    last: (f32, f32),
    dirty: Rect,
    menu_dirty: bool,
    first_flush: bool,
}

impl Overlay {
    pub fn new(hinst: HINSTANCE, class: *const u16, color: usize, size: usize, cfg: &Config) -> Option<Overlay> {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let dib = Dib::new(vw, vh, true)?;
        let screen = Rect::new(0, 0, vw, vh);
        let lay = Layout::new(vx, vy, screen);

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class,
                class,
                WS_POPUP,
                vx,
                vy,
                vw,
                vh,
                null_mut(),
                null_mut(),
                hinst,
                null_mut(),
            )
        };
        if hwnd.is_null() {
            return None; // `dib` is released on return
        }

        let under = vec![BG; lay.area.width() as usize * lay.area.height() as usize];
        let mut ov = Overlay {
            hwnd,
            dib,
            under,
            lay,
            theme: theme_for(cfg.theme),
            keys: cfg.keys,
            color: color % COLORS.len(),
            size: size % SIZES.len(),
            hover: None,
            pressed: None,
            drawing: false,
            last: (0.0, 0.0),
            dirty: Rect::EMPTY,
            menu_dirty: true,
            first_flush: true,
        };
        ov.dib.surface().fill(BG);
        ov.dirty = screen;
        ov.draw_menu();
        // If the first composition fails nothing is shown: `ov` is destroyed.
        if !ov.flush() {
            return None;
        }
        Some(ov)
    }

    // ------------------------------------------------------------------ input

    pub fn handle(&mut self, msg: u32, wp: usize, lp: isize) -> Effect {
        let x = (lp & 0xFFFF) as i16 as i32;
        let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
        let mut fx = Effect::None;
        match msg {
            WM_MOUSEMOVE => {
                if self.drawing {
                    let p = (x as f32 + 0.5, y as f32 + 0.5);
                    self.paint(self.last, p);
                    self.last = p;
                } else {
                    self.set_hover(self.lay.hit(x, y));
                }
            }
            WM_LBUTTONDOWN => match self.lay.hit(x, y) {
                Some(b) => self.pressed = Some(b),
                None if !self.lay.area.contains(x, y) => {
                    self.drawing = true;
                    self.last = (x as f32 + 0.5, y as f32 + 0.5);
                    self.paint(self.last, self.last);
                    self.set_hover(None);
                    fx = Effect::Capture;
                }
                None => {}
            },
            WM_LBUTTONUP => {
                if self.drawing {
                    self.drawing = false;
                    fx = Effect::Release;
                } else if let Some(p) = self.pressed.take() {
                    if self.lay.hit(x, y) == Some(p) {
                        fx = self.press(p);
                    }
                }
            }
            WM_CAPTURECHANGED => self.drawing = false,
            WM_KEYDOWN => fx = self.key(wp as u16, lp),
            _ => {}
        }
        if fx != Effect::Hide {
            self.flush();
        }
        fx
    }

    /// Is the (client) point over a toolbar button?
    pub fn over_button(&self, x: i32, y: i32) -> bool {
        self.lay.hit(x, y).is_some()
    }

    fn key(&mut self, vk: u16, lp: isize) -> Effect {
        if vk == VK_ESCAPE {
            return Effect::Hide;
        }
        let repeat = (lp >> 30) & 1 != 0;
        let mods = unsafe { GetKeyState(VK_CONTROL as i32) < 0 || GetKeyState(VK_MENU as i32) < 0 };
        if repeat || mods {
            return Effect::None; // keeps the global hotkey itself from triggering actions
        }
        let k = self.keys;
        match vk {
            v if v == k.pencil => self.press(Btn::Pencil),
            v if v == k.size => self.press(Btn::Size),
            v if v == k.clear => self.press(Btn::Clear),
            VK_DELETE | VK_BACK => self.press(Btn::Clear),
            0x31..=0x34 /* 1-4 */ => self.set_size((vk - 0x31) as usize),
            VK_NUMPAD1..=VK_NUMPAD4 => self.set_size((vk - VK_NUMPAD1) as usize),
            _ => Effect::None,
        }
    }

    fn press(&mut self, b: Btn) -> Effect {
        match b {
            Btn::Pencil => {
                self.color = (self.color + 1) % COLORS.len();
                self.draw_menu();
                Effect::None
            }
            Btn::Size => self.set_size((self.size + 1) % SIZES.len()),
            Btn::Clear => {
                self.clear();
                Effect::None
            }
            Btn::Close => Effect::Hide,
        }
    }

    fn set_size(&mut self, i: usize) -> Effect {
        self.size = i;
        self.draw_menu();
        Effect::None
    }

    fn set_hover(&mut self, h: Option<Btn>) {
        if h != self.hover {
            self.hover = h;
            self.draw_menu();
        }
    }

    // ---------------------------------------------------------------- drawing

    fn clear(&mut self) {
        self.dib.surface().fill(BG);
        self.under.fill(BG);
        self.dirty = Rect::new(0, 0, self.dib.w, self.dib.h);
        self.draw_menu();
    }

    fn under_surface(&mut self) -> Surface<'_> {
        let a = self.lay.area;
        Surface { px: &mut self.under, w: a.width(), h: a.height(), ox: a.l, oy: a.t }
    }

    /// Paints a stroke segment from `a` to `b`.
    fn paint(&mut self, a: (f32, f32), b: (f32, f32)) {
        let r = (SIZES[self.size] * self.lay.s * 0.5).max(0.75);
        let c = COLORS[self.color];
        // Split long segments so the box to scan stays small.
        let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        let n = (len / (r * 4.0).max(24.0)).ceil().max(1.0) as usize;
        let screen = Rect::new(0, 0, self.dib.w, self.dib.h);
        let lerp = |t: f32| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        for i in 0..n {
            let (p0, p1) = (lerp(i as f32 / n as f32), lerp((i + 1) as f32 / n as f32));
            let bounds = self.dib.surface().capsule(p0, p1, r, c);
            if !bounds.intersect(&self.lay.area).is_empty() {
                self.under_surface().capsule(p0, p1, r, c);
                self.menu_dirty = true;
            }
            self.dirty = self.dirty.union(&bounds.intersect(&screen));
        }
        if self.menu_dirty {
            self.draw_menu();
        }
    }

    /// Recomposes the toolbar: restores the strokes beneath it and paints the panel on top.
    fn draw_menu(&mut self) {
        self.menu_dirty = false;
        let lay = &self.lay;
        let t = self.theme;
        let a = lay.area;
        let mut cv = self.dib.surface();
        Surface { px: &mut self.under, w: a.width(), h: a.height(), ox: a.l, oy: a.t }.blit_into(&mut cv);

        let s = lay.s;
        let (m, c) = (lay.main, lay.close);
        cv.round_rect(m.0, m.1, m.2, m.3, 14.0 * s, t.panel);

        for (i, b) in BTNS.iter().enumerate() {
            let r = lay.btn[i];
            let (cx, cy) = ((r.0 + r.2) * 0.5, (r.1 + r.3) * 0.5);
            let hover = self.hover == Some(*b);
            match b {
                Btn::Pencil => {
                    // The pencil is the active tool: always highlighted.
                    let bg = if hover { t.selected_hover } else { t.selected };
                    cv.round_rect(r.0, r.1, r.2, r.3, 8.0 * s, bg);
                    let col = COLORS[self.color];
                    let y = cy - 3.0 * s;
                    let (p0, p1) = ((cx - 4.0 * s, y + 4.0 * s), (cx + 6.0 * s, y - 6.0 * s));
                    cv.capsule(p0, p1, 3.2 * s, col);
                    cv.capsule((cx - 8.0 * s, y + 8.0 * s), p0, 1.4 * s, t.icon);
                    for (k, kc) in COLORS.iter().enumerate() {
                        let rad = if k == self.color { 2.6 } else { 1.8 };
                        let dx = cx + (k as f32 - 1.5) * 7.0 * s;
                        cv.circle(dx, cy + 13.0 * s, rad * s, *kc);
                    }
                }
                Btn::Size => {
                    if hover {
                        cv.round_rect(r.0, r.1, r.2, r.3, 8.0 * s, t.hover);
                    }
                    cv.circle(cx, cy - 3.0 * s, SIZE_ICON_R[self.size] * s, t.icon);
                    for k in 0..SIZES.len() {
                        let (rad, col) = if k == self.size { (2.4, t.icon) } else { (1.5, t.dot_off) };
                        cv.circle(cx + (k as f32 - 1.5) * 6.5 * s, cy + 13.0 * s, rad * s, col);
                    }
                }
                Btn::Clear => {
                    if hover {
                        cv.round_rect(r.0, r.1, r.2, r.3, 8.0 * s, t.hover);
                    }
                    let u = (std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2);
                    cv.oriented_box((cx, cy), u, 10.0 * s, 5.5 * s, 1.5 * s, t.eraser_body);
                    let tip = (cx - u.0 * 5.5 * s, cy - u.1 * 5.5 * s);
                    cv.oriented_box(tip, u, 4.5 * s, 5.5 * s, 1.5 * s, ERASER_TIP);
                }
                Btn::Close => {
                    let rad = (c.2 - c.0) * 0.5;
                    cv.circle(cx, cy, rad, if hover { CLOSE_HOVER } else { t.panel });
                    let col = if hover { WHITE } else { t.icon };
                    let d = 6.0 * s;
                    cv.capsule((cx - d, cy - d), (cx + d, cy + d), 1.6 * s, col);
                    cv.capsule((cx - d, cy + d), (cx + d, cy - d), 1.6 * s, col);
                }
            }
        }
        self.dirty = self.dirty.union(&a);
    }

    /// Sends only the modified area to the compositor. Returns `false` on failure.
    fn flush(&mut self) -> bool {
        if self.dirty.is_empty() {
            return true;
        }
        let d = self.dirty;
        let dirty = RECT { left: d.l, top: d.t, right: d.r, bottom: d.b };
        let size = SIZE { cx: self.dib.w, cy: self.dib.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: null_mut(),
            pptDst: null_mut(),
            // Always pass the size: with NULL the call "succeeds" but does not refresh.
            psize: &size,
            hdcSrc: self.dib.dc,
            pptSrc: &src,
            crKey: 0,
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: if self.first_flush { std::ptr::null() } else { &dirty },
        };
        let ok = unsafe { UpdateLayeredWindowIndirect(self.hwnd, &info) } != 0;
        if ok {
            self.first_flush = false;
            self.dirty = Rect::EMPTY;
        }
        ok
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // Window first; `dib` and `under` are released automatically afterwards.
        unsafe { DestroyWindow(self.hwnd) };
    }
}
