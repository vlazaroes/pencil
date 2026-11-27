//! Full-screen overlay window: canvas, bottom toolbar and input handling.
//!
//! The overlay only exists while it is visible. Hiding it destroys it completely
//! (window, DC, bitmap and buffers), so in the background the program holds no
//! screen-sized memory and uses no CPU/GPU.

use std::ptr::null_mut;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, ExtTextOutW, GdiFlush, GetMonitorInfoW, GetTextExtentPoint32W,
    MonitorFromPoint, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    BLENDFUNCTION, ETO_CLIPPED, ETO_IGNORELANGUAGE, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    TRANSPARENT,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, SystemParametersInfoForDpi, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_MENU,
    VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetCursorPos, GetSystemMetrics, GetWindowLongW, KillTimer,
    SetTimer, SetWindowLongW, SetWindowsHookExW, UnhookWindowsHookEx, GWL_EXSTYLE, HHOOK,
    WH_MOUSE_LL, WS_EX_TRANSPARENT,
    UpdateLayeredWindowIndirect, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, WM_TIMER,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    UPDATELAYEREDWINDOWINFO, ULW_ALPHA, WM_CAPTURECHANGED, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::{self, Image, Veil};
use crate::config::{Config, Keys, ThemePref};
use crate::gdi::Dib;
use crate::laser::{self, Laser};
use crate::raster::{blend, sd_round_rect, Rect, Rgb, Surface};
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

/// Highlighter colors: yellow, green, pink, sky blue.
pub const HL_COLORS: [Rgb; 4] = [Rgb(255, 221, 0), Rgb(80, 220, 70), Rgb(255, 90, 170), Rgb(40, 180, 255)];
/// Highlighter nib height in logical pixels, one per size preset.
const HL_SIZES: [f32; 4] = [8.0, 14.0, 22.0, 32.0];
/// Opacity of the highlighter ink. Within one stroke it never builds up.
const HL_ALPHA: f32 = 0.4;
/// Chisel nib: width as a fraction of its height, and its tilt from the vertical
/// (radians, top leaning right) like a highlighter held in the right hand.
const NIB_RATIO: f32 = 0.3;
const NIB_TILT: f32 = 0.35;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Highlighter,
}

/// Drawing tool state, remembered between activations of the overlay.
#[derive(Clone, Copy)]
pub struct PenState {
    pub tool: Tool,
    pub color: usize,
    pub hl_color: usize,
    pub size: usize,
}

const ERASER_TIP: Rgb = Rgb(242, 139, 130);
const CLOSE_HOVER: Rgb = Rgb(229, 72, 77);
const NOTICE_OK: Rgb = Rgb(46, 160, 67);
const NOTICE_WARN: Rgb = Rgb(230, 150, 30);
/// Timer on the overlay window that dismisses the notice.
const NOTICE_TIMER: usize = 1;
const NOTICE_MS: u32 = 4000;
/// Timer on the overlay window that, while the laser is on, follows the cursor
/// and animates the fading trail.
const LASER_TIMER: usize = 2;
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
    Highlighter,
    Laser,
    Size,
    Clear,
    Capture,
    Close,
    /// The screenshot notice above the toolbar (not part of `BTNS`).
    Notice,
}
const BTNS: [Btn; 7] =
    [Btn::Pencil, Btn::Highlighter, Btn::Size, Btn::Clear, Btn::Laser, Btn::Capture, Btn::Close];
/// Buttons inside the main panel (the close button sits apart).
const MAIN_BTNS: usize = 6;
/// The drawing tools come before this index; a vertical separator sets the
/// screenshot button apart from them.
const SEP_BEFORE: usize = 5;

/// Screenshot selection state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shot {
    Off,
    /// Waiting for the user to press and drag.
    Armed,
    /// Dragging from `start` to `cur` (client coordinates).
    Dragging { start: (i32, i32), cur: (i32, i32) },
    /// Ctrl held: pointing at windows; `target` is the one a click would capture.
    Window { target: Rect },
}

/// One undoable action. Strokes are kept as data (a few KB each) rather than
/// pixels, so undoing re-renders the remaining ones instead of storing copies of
/// the full-screen canvas.
enum Op {
    Stroke { tool: Tool, color: usize, size: usize, points: Vec<(f32, f32)> },
    Clear,
}

/// `MK_CONTROL` in the wParam of mouse messages: Ctrl is down.
const MK_CONTROL: usize = 0x0008;

/// Selections smaller than this (in pixels) in both directions count as a click,
/// which captures the whole monitor.
const MIN_SHOT: i32 = 4;

/// Action the caller must run *after* releasing the state borrow (these are
/// Win32 calls that send synchronous messages and could re-enter the wndproc).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Start receiving mouse input outside the window (SetCapture).
    MouseCapture,
    /// Stop it (ReleaseCapture).
    MouseRelease,
    /// Release the mouse and take the screenshot of the finished selection.
    Screenshot,
    /// Open the last saved screenshot (its notice was clicked).
    OpenShot,
    Hide,
}

type FRect = (f32, f32, f32, f32);

struct Layout {
    s: f32,
    main: FRect,
    close: FRect,
    btn: [FRect; 7],
    hit: [Rect; 7],
    /// Vertical separator line before the screenshot button: x, top and bottom.
    sep: (f32, f32, f32),
    /// Toolbar panels: clicks here never draw.
    bar: Rect,
    /// Notice slot: horizontal center, bottom edge, height and maximum width.
    notice: (f32, f32, f32, f32),
    /// Area that can hold toolbar or notice pixels (with a margin for antialiasing);
    /// the strokes under it are kept in `under`.
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
        let n = MAIN_BTNS as f32;
        // Extra room around the separator, on top of the usual gap.
        let sep_room = (10.0 * s).round();
        let main_w = n * btn + (n - 1.0) * gap + sep_room + 2.0 * pad;
        let x0 = (cx - main_w / 2.0).round();
        let main = (x0, bottom - h, x0 + main_w, bottom);
        let cl = main.2 + (14.0 * s).round();
        let close = (cl, bottom - h, cl + h, bottom);

        let mut b = [(0.0, 0.0, 0.0, 0.0); 7];
        for (i, r) in b.iter_mut().take(MAIN_BTNS).enumerate() {
            let extra = if i >= SEP_BEFORE { sep_room } else { 0.0 };
            let x = main.0 + pad + i as f32 * (btn + gap) + extra;
            *r = (x, main.1 + pad, x + btn, main.1 + pad + btn);
        }
        b[MAIN_BTNS] = close;
        // Centered in the space between the two buttons, shorter than them.
        let sep_x = (b[SEP_BEFORE - 1].2 + b[SEP_BEFORE].0) * 0.5;
        let inset = (10.0 * s).round();
        let sep = (sep_x, main.1 + pad + inset, main.3 - pad - inset);
        let hit = b.map(|r| Rect::new(r.0 as i32, r.1 as i32, r.2 as i32, r.3 as i32));
        let bar = Rect::bounds(main.0 - 2.0, main.1 - 2.0, close.2 + 2.0, close.3 + 2.0).intersect(&screen);
        let (nh, nw) = ((34.0 * s).round(), (440.0 * s).round());
        let nbottom = main.1 - (10.0 * s).round();
        let ncx = ((main.0 + close.2) * 0.5).round();
        let slot = Rect::bounds(ncx - nw * 0.5 - 2.0, nbottom - nh - 2.0, ncx + nw * 0.5 + 2.0, nbottom + 2.0);
        let area = bar.union(&slot).intersect(&screen);
        Layout { s, main, close, btn: b, hit, sep, bar, notice: (ncx, nbottom, nh, nw), area }
    }

    fn hit(&self, x: i32, y: i32) -> Option<Btn> {
        if !self.bar.contains(x, y) {
            return None;
        }
        BTNS.iter().zip(self.hit.iter()).find(|(_, r)| r.contains(x, y)).map(|(b, _)| *b)
    }
}

pub struct Overlay {
    pub hwnd: HWND,
    hinst: HINSTANCE,
    /// Screen position of the client origin (top-left of the virtual screen).
    origin: (i32, i32),
    dib: Dib,
    /// Copy of the strokes lying under the toolbar, so the toolbar can be redrawn
    /// on top without losing them or duplicating the full-screen buffer.
    under: Vec<u32>,
    lay: Layout,
    theme: &'static Theme,
    keys: Keys,
    tool: Tool,
    color: usize,
    hl_color: usize,
    size: usize,
    hover: Option<Btn>,
    pressed: Option<Btn>,
    drawing: bool,
    /// Coverage (0-255) of the highlighter stroke being painted, one byte per
    /// screen pixel, so its ink never builds up where the stroke overlaps itself.
    /// Empty outside a highlighter stroke.
    mask: Vec<u8>,
    /// Laser pointer mode: a fading trail follows the cursor and, outside the
    /// toolbar, clicks go through to the windows underneath.
    laser_on: bool,
    laser: Laser,
    /// Whether the window currently lets the mouse through (`WS_EX_TRANSPARENT`).
    click_through: bool,
    /// Low-level mouse hook that feeds the laser every move, while it is on.
    hook: HHOOK,
    /// When the laser was last drawn.
    last_frame: Instant,
    last: (f32, f32),
    /// Undo / redo stacks, oldest first. Freed with the overlay.
    history: Vec<Op>,
    redo: Vec<Op>,
    dirty: Rect,
    menu_dirty: bool,
    first_flush: bool,
    shot: Shot,
    veil: Option<Veil>,
    /// Finished selection waiting for `screenshot()`.
    pending_shot: Option<Rect>,
    /// Window bounds (client coordinates, topmost first) while pointing at windows.
    windows: Vec<Rect>,
    notice: Option<Notice>,
}

/// Pre-rendered notice pill (opaque pixels) and where it sits.
struct Notice {
    px: Vec<u32>,
    rect: Rect,
}

impl Overlay {
    pub fn new(hinst: HINSTANCE, class: *const u16, pen: PenState, cfg: &Config) -> Option<Overlay> {
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
            hinst,
            origin: (vx, vy),
            dib,
            under,
            lay,
            theme: theme_for(cfg.theme),
            keys: cfg.keys,
            tool: pen.tool,
            color: pen.color % COLORS.len(),
            hl_color: pen.hl_color % HL_COLORS.len(),
            size: pen.size % SIZES.len(),
            hover: None,
            pressed: None,
            drawing: false,
            mask: Vec::new(),
            laser_on: false,
            laser: Laser::new(),
            click_through: false,
            hook: null_mut(),
            last_frame: Instant::now(),
            history: Vec::new(),
            redo: Vec::new(),
            last: (0.0, 0.0),
            dirty: Rect::EMPTY,
            menu_dirty: true,
            first_flush: true,
            shot: Shot::Off,
            veil: None,
            pending_shot: None,
            windows: Vec::new(),
            notice: None,
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
        if msg == WM_TIMER && wp == LASER_TIMER {
            self.laser_tick();
            return Effect::None;
        }
        let mut fx = Effect::None;
        self.erase_laser();
        match msg {
            WM_MOUSEMOVE => {
                // Ctrl state rides on every mouse message, so the window mode stays in
                // sync even if a key event was missed (e.g. Ctrl pressed before arming).
                self.sync_window_mode(wp & MK_CONTROL != 0, x, y);
                if let Shot::Window { target } = self.shot {
                    let t = self.window_at(x, y);
                    if t != target {
                        self.set_shot(Shot::Window { target: t });
                    }
                    self.set_hover(self.hit(x, y));
                } else if let Shot::Dragging { start, .. } = self.shot {
                    self.shot = Shot::Dragging { start, cur: (x, y) };
                    if let Some(v) = &self.veil {
                        v.set_hole(Some(self.selection(start, (x, y))));
                    }
                } else if self.drawing {
                    let p = (x as f32 + 0.5, y as f32 + 0.5);
                    self.paint(self.last, p);
                    self.last = p;
                    if let Some(Op::Stroke { points, .. }) = self.history.last_mut() {
                        points.push(p);
                    }
                } else {
                    self.set_hover(self.hit(x, y));
                }
            }
            WM_LBUTTONDOWN => match self.hit(x, y) {
                Some(b) => self.pressed = Some(b),
                // Pointing at windows: a click captures the highlighted one (no dragging).
                None if matches!(self.shot, Shot::Window { .. }) => {
                    if let Shot::Window { target } = self.shot {
                        self.pending_shot = Some(target);
                        self.set_shot(Shot::Off);
                        fx = Effect::Screenshot;
                    }
                }
                None if self.shot == Shot::Armed => {
                    self.shot = Shot::Dragging { start: (x, y), cur: (x, y) };
                    self.set_hover(None);
                    fx = Effect::MouseCapture;
                }
                None if !self.lay.bar.contains(x, y) && self.shot == Shot::Off && !self.laser_on => {
                    self.drawing = true;
                    self.last = (x as f32 + 0.5, y as f32 + 0.5);
                    self.paint(self.last, self.last);
                    let color = self.tool_color();
                    self.history.push(Op::Stroke { tool: self.tool, color, size: self.size, points: vec![self.last] });
                    self.redo.clear();
                    self.set_hover(None);
                    fx = Effect::MouseCapture;
                }
                _ => {}
            },
            WM_LBUTTONUP => {
                if let Shot::Dragging { start, .. } = self.shot {
                    let sel = self.selection(start, (x, y));
                    self.set_shot(Shot::Off);
                    let (wide, tall) = (sel.width() >= MIN_SHOT, sel.height() >= MIN_SHOT);
                    // A plain click (no real drag) captures the whole monitor under it.
                    self.pending_shot = match (wide, tall) {
                        (true, true) => Some(sel),
                        (false, false) => Some(self.monitor_rect(x, y)),
                        _ => None, // a thin line: nothing sensible to capture
                    };
                    fx = if self.pending_shot.is_some() { Effect::Screenshot } else { Effect::MouseRelease };
                } else if self.drawing {
                    self.drawing = false;
                    self.mask = Vec::new();
                    fx = Effect::MouseRelease;
                } else if let Some(p) = self.pressed.take() {
                    if self.hit(x, y) == Some(p) {
                        fx = self.press(p);
                    }
                }
                // Releasing the button always frees the mouse, so a capture can never
                // outlive the gesture that took it (e.g. a selection cancelled with Esc).
                if fx == Effect::None {
                    fx = Effect::MouseRelease;
                }
            }
            // `lp` is the window gaining the capture: when it is this one (SetCapture
            // while already holding it) nothing was lost.
            WM_CAPTURECHANGED if lp as HWND != self.hwnd => {
                self.drawing = false;
                self.mask = Vec::new();
                // Lost the mouse mid-selection (e.g. another window took it): start over.
                if matches!(self.shot, Shot::Dragging { .. }) {
                    self.set_shot(Shot::Armed);
                }
            }
            WM_KEYDOWN => fx = self.key(wp as u16, lp),
            WM_KEYUP if wp as u16 == VK_CONTROL => {
                let (cx, cy) = self.cursor_client();
                self.sync_window_mode(false, cx, cy);
            }
            WM_TIMER if wp == NOTICE_TIMER => self.set_notice(None),
            _ => {}
        }
        if fx != Effect::Hide {
            self.draw_laser();
            self.flush();
        }
        fx
    }

    /// Is the (client) point over something clickable (toolbar button or notice)?
    pub fn over_button(&self, x: i32, y: i32) -> bool {
        self.hit(x, y).is_some()
    }

    /// Tool, colors and size, to restore them next time.
    pub fn pen(&self) -> PenState {
        PenState { tool: self.tool, color: self.color, hl_color: self.hl_color, size: self.size }
    }

    /// Color index of the current tool.
    fn tool_color(&self) -> usize {
        match self.tool {
            Tool::Pen => self.color,
            Tool::Highlighter => self.hl_color,
        }
    }

    /// Is the laser on? Then the overlay stays open when other windows get focus.
    pub fn laser_on(&self) -> bool {
        self.laser_on
    }

    fn hit(&self, x: i32, y: i32) -> Option<Btn> {
        // While selecting a screenshot only the capture button (to leave the mode)
        // reacts; anywhere else, toolbar included, starts the selection.
        if self.shot != Shot::Off {
            return self.lay.hit(x, y).filter(|b| *b == Btn::Capture);
        }
        match &self.notice {
            Some(n) if n.rect.contains(x, y) => Some(Btn::Notice),
            _ => self.lay.hit(x, y),
        }
    }

    fn key(&mut self, vk: u16, lp: isize) -> Effect {
        if vk == VK_ESCAPE {
            // Esc first leaves the screenshot selection; a second Esc closes the overlay.
            if self.shot != Shot::Off {
                return self.cancel_shot();
            }
            return Effect::Hide;
        }
        let repeat = (lp >> 30) & 1 != 0;
        if vk == VK_CONTROL {
            if !repeat {
                let (cx, cy) = self.cursor_client();
                self.sync_window_mode(true, cx, cy);
            }
            return Effect::None;
        }
        let (ctrl, alt, shift) =
            unsafe { (GetKeyState(VK_CONTROL as i32) < 0, GetKeyState(VK_MENU as i32) < 0, GetKeyState(VK_SHIFT as i32) < 0) };
        // Undo / redo while drawing mode is idle (in screenshot mode Ctrl points at
        // windows). Auto-repeat is allowed: holding Ctrl+Z keeps undoing.
        if ctrl && !alt && self.shot == Shot::Off && !self.drawing {
            match vk {
                0x5A /* Z */ if shift => return self.redo(),
                0x5A /* Z */ => return self.undo(),
                0x59 /* Y */ => return self.redo(),
                _ => {}
            }
        }
        let mods = ctrl || alt;
        if repeat || mods {
            return Effect::None; // keeps the global hotkey itself from triggering actions
        }
        let k = self.keys;
        match vk {
            v if v == k.pencil => self.press(Btn::Pencil),
            v if v == k.highlight => self.press(Btn::Highlighter),
            v if v == k.size => self.press(Btn::Size),
            v if v == k.clear => self.press(Btn::Clear),
            v if v == k.capture => self.press(Btn::Capture),
            v if v == k.laser => self.press(Btn::Laser),
            VK_DELETE | VK_BACK => self.press(Btn::Clear),
            _ => Effect::None,
        }
    }

    /// A tool's button selects it (leaving the laser); once selected, it cycles its colors.
    fn select_tool(&mut self, tool: Tool) {
        if self.laser_on {
            self.set_laser(false);
        } else if self.tool == tool {
            match tool {
                Tool::Pen => self.color = (self.color + 1) % COLORS.len(),
                Tool::Highlighter => self.hl_color = (self.hl_color + 1) % HL_COLORS.len(),
            }
        }
        self.tool = tool;
        self.draw_menu();
    }

    /// Is `tool` the one a click would draw with (so its button shows as selected)?
    fn drawing_with(&self, tool: Tool) -> bool {
        self.tool == tool && !self.laser_on && self.shot == Shot::Off
    }

    fn press(&mut self, b: Btn) -> Effect {
        match b {
            Btn::Pencil => {
                self.select_tool(Tool::Pen);
                Effect::None
            }
            Btn::Highlighter => {
                self.select_tool(Tool::Highlighter);
                Effect::None
            }
            Btn::Laser => {
                self.set_laser(!self.laser_on);
                Effect::None
            }
            Btn::Size => self.set_size((self.size + 1) % SIZES.len()),
            Btn::Clear => {
                // Only when something is visible, so undo never has empty steps.
                if matches!(self.history.last(), Some(Op::Stroke { .. })) {
                    self.history.push(Op::Clear);
                    self.redo.clear();
                    self.clear();
                }
                Effect::None
            }
            Btn::Capture => {
                // The button (or its key) toggles the selection mode.
                if self.shot == Shot::Off {
                    self.set_laser(false);
                    self.set_shot(Shot::Armed);
                    Effect::None
                } else {
                    self.cancel_shot()
                }
            }
            Btn::Close => Effect::Hide,
            Btn::Notice => Effect::OpenShot,
        }
    }

    /// Leaves screenshot mode; if a selection was being dragged, the mouse capture
    /// taken for it is released too.
    fn cancel_shot(&mut self) -> Effect {
        let dragging = matches!(self.shot, Shot::Dragging { .. });
        self.set_shot(Shot::Off);
        if dragging {
            Effect::MouseRelease
        } else {
            Effect::None
        }
    }

    // ------------------------------------------------------------------ laser

    fn set_laser(&mut self, on: bool) {
        if self.laser_on == on {
            return;
        }
        self.laser_on = on;
        self.laser.clear();
        if on {
            let (x, y) = self.cursor_client();
            self.laser.push((x as f32 + 0.5, y as f32 + 0.5));
            // Outside the toolbar the mouse belongs to the windows underneath, so no
            // WM_MOUSEMOVE arrives here: a low-level hook reports every move as it
            // happens, and the timer fades the trail once the mouse stops.
            unsafe {
                self.hook = SetWindowsHookExW(WH_MOUSE_LL, Some(crate::laser_hook), self.hinst, 0);
                SetTimer(self.hwnd, LASER_TIMER, laser::FRAME_MS, None);
            }
        } else {
            unsafe { KillTimer(self.hwnd, LASER_TIMER) };
            self.unhook();
            self.set_click_through(false);
        }
        self.draw_menu(); // the highlighted tool changes
    }

    /// One laser frame: follows the cursor, lets the clicks through outside the
    /// toolbar and notice, and fades the trail. Does nothing while all is still.
    fn unhook(&mut self) {
        if !self.hook.is_null() {
            unsafe { UnhookWindowsHookEx(self.hook) };
            self.hook = null_mut();
        }
    }

    /// The mouse moved to the screen point `(sx, sy)` (from the hook). Every move
    /// joins the trail; a frame is drawn unless the last one is too recent.
    pub fn laser_move(&mut self, sx: i32, sy: i32) {
        if !self.laser_on {
            return;
        }
        let (x, y) = (sx - self.origin.0, sy - self.origin.1);
        self.laser.push((x as f32 + 0.5, y as f32 + 0.5));
        if self.last_frame.elapsed() >= laser::MIN_FRAME {
            self.laser_frame(x, y);
        }
    }

    fn laser_tick(&mut self) {
        let (x, y) = self.cursor_client();
        self.laser_frame(x, y);
    }

    /// Draws the laser with its dot at the client point `(x, y)`.
    fn laser_frame(&mut self, x: i32, y: i32) {
        let solid = self.lay.bar.contains(x, y) || self.notice.as_ref().is_some_and(|n| n.rect.contains(x, y));
        self.set_click_through(!solid);
        let p = (x as f32 + 0.5, y as f32 + 0.5);
        // Away from the toolbar no WM_MOUSEMOVE clears the hover: do it here.
        let unhover = !solid && self.hover.is_some();
        if self.laser.head() == Some(p) && !self.laser.trailing() && !unhover {
            return;
        }
        self.erase_laser();
        if unhover {
            self.set_hover(None);
        }
        self.laser.push(p);
        self.laser.prune(Instant::now());
        self.draw_laser();
        self.flush();
        self.last_frame = Instant::now();
    }

    /// Makes the whole window transparent to the mouse, or solid again.
    fn set_click_through(&mut self, on: bool) {
        if self.click_through == on {
            return;
        }
        self.click_through = on;
        unsafe {
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE) as u32;
            let ex = if on { ex | WS_EX_TRANSPARENT } else { ex & !WS_EX_TRANSPARENT };
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex as i32);
        }
    }

    /// Removes the laser from the canvas, leaving the strokes as they were.
    fn erase_laser(&mut self) {
        let r = self.laser.erase(&mut self.dib.surface());
        self.dirty = self.dirty.union(&r);
    }

    /// Draws the laser for the current instant. The dot hides over the toolbar,
    /// where the normal cursor is shown instead.
    fn draw_laser(&mut self) {
        if !self.laser_on {
            return;
        }
        let head = self.laser.head().is_some_and(|(x, y)| !self.lay.bar.contains(x as i32, y as i32));
        let r = self.laser.draw(&mut self.dib.surface(), self.lay.s, head, Instant::now());
        self.dirty = self.dirty.union(&r);
    }

    // ------------------------------------------------------------- screenshot

    /// Enters or leaves screenshot mode, showing the dimming veil while active.
    fn set_shot(&mut self, shot: Shot) {
        let was_off = self.shot == Shot::Off;
        self.shot = shot;
        // Only the capture button reacts in this mode: drop any other highlight.
        if shot != Shot::Off && self.hover != Some(Btn::Capture) {
            self.hover = None;
        }
        if !matches!(shot, Shot::Window { .. }) {
            self.windows = Vec::new(); // free the snapshot
        }
        match shot {
            Shot::Off => self.veil = None,
            _ if was_off => {
                self.veil = Veil::new(self.hinst, self.hwnd, self.origin.0, self.origin.1, self.dib.w, self.dib.h);
            }
            _ => {
                if let Some(v) = &self.veil {
                    // The highlighted window shows through; otherwise all is dimmed.
                    let hole = if let Shot::Window { target } = shot { Some(target) } else { None };
                    v.set_hole(hole);
                }
            }
        }
        if was_off != (shot == Shot::Off) {
            self.draw_menu(); // the highlighted tool changes
        }
    }

    /// Enters (Ctrl down, while armed) or leaves (Ctrl up) the window pointing mode.
    /// Never during a drag: then Ctrl is simply ignored.
    fn sync_window_mode(&mut self, ctrl: bool, x: i32, y: i32) {
        match (ctrl, self.shot) {
            (true, Shot::Armed) => {
                // Windows do not move while the overlay covers the screen: one
                // snapshot per Ctrl press makes hit-testing on mouse move trivial.
                let (ox, oy) = self.origin;
                let screen = Rect::new(0, 0, self.dib.w, self.dib.h);
                self.windows = capture::visible_windows()
                    .into_iter()
                    .map(|r| Rect::new(r.left - ox, r.top - oy, r.right - ox, r.bottom - oy).intersect(&screen))
                    .filter(|r| !r.is_empty())
                    .collect();
                let target = self.window_at(x, y);
                self.set_shot(Shot::Window { target });
            }
            (false, Shot::Window { .. }) => self.set_shot(Shot::Armed),
            _ => {}
        }
    }

    /// Topmost window under the client point, or its whole monitor (desktop).
    fn window_at(&self, x: i32, y: i32) -> Rect {
        self.windows.iter().find(|r| r.contains(x, y)).copied().unwrap_or_else(|| self.monitor_rect(x, y))
    }

    /// Cursor position in client coordinates.
    fn cursor_client(&self) -> (i32, i32) {
        let mut pt = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut pt) };
        (pt.x - self.origin.0, pt.y - self.origin.1)
    }

    /// Full area of the monitor containing the client point, in client coordinates.
    fn monitor_rect(&self, x: i32, y: i32) -> Rect {
        let screen = Rect::new(0, 0, self.dib.w, self.dib.h);
        unsafe {
            let pt = POINT { x: x + self.origin.0, y: y + self.origin.1 };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(mon, &mut mi) == 0 {
                return screen;
            }
            let m = mi.rcMonitor;
            let (ox, oy) = self.origin;
            Rect::new(m.left - ox, m.top - oy, m.right - ox, m.bottom - oy).intersect(&screen)
        }
    }

    /// Normalized selection rectangle between two client points, clipped to the screen.
    fn selection(&self, a: (i32, i32), b: (i32, i32)) -> Rect {
        Rect::new(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
            .intersect(&Rect::new(0, 0, self.dib.w, self.dib.h))
    }

    /// Grabs the finished selection with the strokes exactly as they are shown, but
    /// without the toolbar, the notice or the veil.
    ///
    /// Windows refuses to exclude `UpdateLayeredWindow` windows from capture, so for
    /// the grab the toolbar area briefly shows the strokes kept beneath it; the
    /// strokes themselves never disappear.
    ///
    /// The hidden time is kept to the physical minimum: nothing is hidden when the
    /// selection does not reach the toolbar/notice area; otherwise everything is
    /// prepared beforehand (target bitmap, copy of the toolbar pixels) so that only
    /// "copy the screen (which waits for the composed frame), paste the toolbar back"
    /// happens in between: about one frame (measured 7-38 ms, 19 ms on average at 60 Hz).
    pub fn screenshot(&mut self) -> Option<Image> {
        let r = self.pending_shot.take()?;
        self.veil = None; // excluded from capture: no need to wait for it to go
        let target = Image::target(r.width(), r.height())?;
        let (x, y) = (self.origin.0 + r.l, self.origin.1 + r.t);
        let a = self.lay.area;
        if r.intersect(&a).is_empty() {
            return Image::grab(target, x, y);
        }

        // Keep the toolbar pixels, then show the strokes beneath them instead.
        let mut toolbar = vec![0u32; a.width() as usize * a.height() as usize];
        let mut saved = Surface { px: &mut toolbar, w: a.width(), h: a.height(), ox: a.l, oy: a.t };
        let mut cv = self.dib.surface();
        cv.blit_into(&mut saved);
        Surface { px: &mut self.under, w: a.width(), h: a.height(), ox: a.l, oy: a.t }.blit_into(&mut cv);
        self.dirty = self.dirty.union(&a);
        self.flush();

        let img = Image::grab(target, x, y);

        Surface { px: &mut toolbar, w: a.width(), h: a.height(), ox: a.l, oy: a.t }.blit_into(&mut self.dib.surface());
        self.dirty = self.dirty.union(&a);
        self.flush();
        img
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

    // ---------------------------------------------------------------- notice

    /// Shows a short message above the toolbar for a few seconds. `ok` picks the
    /// icon (check mark or warning); clicking it yields `Effect::OpenShot`.
    pub fn notify(&mut self, ok: bool, text: &str) {
        let n = render_notice(text, ok, self.theme, &self.lay);
        self.erase_laser();
        self.set_notice(n);
        if self.notice.is_some() {
            unsafe { SetTimer(self.hwnd, NOTICE_TIMER, NOTICE_MS, None) };
        }
        self.draw_laser();
        self.flush();
    }

    fn set_notice(&mut self, n: Option<Notice>) {
        if n.is_none() {
            unsafe { KillTimer(self.hwnd, NOTICE_TIMER) };
        }
        self.notice = n;
        self.draw_menu();
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

    /// Removes the last stroke (or brings back what the eraser cleared).
    fn undo(&mut self) -> Effect {
        if let Some(op) = self.history.pop() {
            self.redo.push(op);
            self.rerender();
        }
        Effect::None
    }

    /// Re-applies the last undone action; no full re-render needed.
    fn redo(&mut self) -> Effect {
        let Some(op) = self.redo.pop() else { return Effect::None };
        match &op {
            Op::Stroke { tool, color, size, points } => {
                self.paint_stroke(*tool, *color, *size, points);
                self.draw_menu();
            }
            Op::Clear => self.clear(),
        }
        self.history.push(op);
        Effect::None
    }

    /// Redraws the canvas from the history: only what came after the last clear.
    fn rerender(&mut self) {
        self.dib.surface().fill(BG);
        self.under.fill(BG);
        let start = self.history.iter().rposition(|op| matches!(op, Op::Clear)).map_or(0, |i| i + 1);
        let ops = std::mem::take(&mut self.history);
        for op in &ops[start..] {
            if let Op::Stroke { tool, color, size, points } = op {
                self.paint_stroke(*tool, *color, *size, points);
            }
        }
        self.history = ops;
        self.dirty = Rect::new(0, 0, self.dib.w, self.dib.h);
        self.draw_menu();
    }

    /// Paints a whole recorded stroke exactly as it was drawn live.
    fn paint_stroke(&mut self, tool: Tool, color: usize, size: usize, points: &[(f32, f32)]) {
        let mut prev = points[0];
        self.paint_with(tool, color, size, prev, prev);
        for &p in &points[1..] {
            self.paint_with(tool, color, size, prev, p);
            prev = p;
        }
        self.mask = Vec::new();
    }

    /// Paints a stroke segment from `a` to `b` with the current tool.
    fn paint(&mut self, a: (f32, f32), b: (f32, f32)) {
        self.paint_with(self.tool, self.tool_color(), self.size, a, b);
        if self.menu_dirty {
            self.draw_menu();
        }
    }

    /// Paints a segment; the caller redraws the toolbar if `menu_dirty` was set.
    fn paint_with(&mut self, tool: Tool, color: usize, size: usize, a: (f32, f32), b: (f32, f32)) {
        if tool == Tool::Highlighter {
            return self.highlight(color, size, a, b);
        }
        let r = (SIZES[size] * self.lay.s * 0.5).max(0.75);
        let c = COLORS[color];
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
    }

    /// Paints a highlighter segment: the chisel nib swept from `a` to `b`, in
    /// translucent ink that does not build up where the stroke overlaps itself.
    fn highlight(&mut self, color: usize, size: usize, a: (f32, f32), b: (f32, f32)) {
        let (w, h) = (self.dib.w, self.dib.h);
        if self.mask.is_empty() {
            self.mask = vec![0; w as usize * h as usize];
        }
        // The nib is a tilted rectangle; swept along a segment it covers the convex
        // hull of its two copies.
        let half_h = HL_SIZES[size] * self.lay.s * 0.5;
        let half_w = (half_h * NIB_RATIO).max(0.75);
        let (sin, cos) = NIB_TILT.sin_cos();
        let (v, u) = ((sin * half_h, -cos * half_h), (cos * half_w, sin * half_w));
        let mut corners = Vec::with_capacity(8);
        for p in [a, b] {
            for (sv, su) in [(1.0, 1.0), (1.0, -1.0), (-1.0, -1.0), (-1.0, 1.0)] {
                corners.push((p.0 + sv * v.0 + su * u.0, p.1 + sv * v.1 + su * u.1));
            }
        }
        let hull = convex_hull(corners);
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in &hull {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        let screen = Rect::new(0, 0, w, h);
        let bounds = Rect::bounds(x0 - 1.0, y0 - 1.0, x1 + 1.0, y1 + 1.0).intersect(&screen);
        if bounds.is_empty() {
            return;
        }

        let c = HL_COLORS[color];
        let area = self.lay.area;
        let cv = self.dib.surface();
        let mut under_touched = false;
        for y in bounds.t..bounds.b {
            for x in bounds.l..bounds.r {
                let cov = (0.5 - sd_polygon(x as f32 + 0.5, y as f32 + 0.5, &hull)).clamp(0.0, 1.0);
                let new = (cov * 255.0 + 0.5) as u8;
                let i = (y * w + x) as usize;
                let old = self.mask[i];
                if new <= old {
                    continue;
                }
                self.mask[i] = new;
                // The pixel already holds the ink at `old` coverage: blending by `k`
                // takes it to `new` coverage over the same background.
                let (ao, an) = (HL_ALPHA * old as f32 / 255.0, HL_ALPHA * new as f32 / 255.0);
                let k = (((1.0 - (1.0 - an) / (1.0 - ao)) * 255.0) + 0.5) as u32;
                cv.px[i] = blend(cv.px[i], c, k);
                if area.contains(x, y) {
                    let j = ((y - area.t) * area.width() + (x - area.l)) as usize;
                    self.under[j] = blend(self.under[j], c, k);
                    under_touched = true;
                }
            }
        }
        if under_touched {
            self.menu_dirty = true;
        }
        self.dirty = self.dirty.union(&bounds);
    }

    /// Recomposes the toolbar: restores the strokes beneath it and paints the panel on top.
    fn draw_menu(&mut self) {
        self.menu_dirty = false;
        let lay = &self.lay;
        let t = self.theme;
        let a = lay.area;
        // Read before `cv` borrows the canvas.
        let (pen_on, hl_on) = (self.drawing_with(Tool::Pen), self.drawing_with(Tool::Highlighter));
        let mut cv = self.dib.surface();
        Surface { px: &mut self.under, w: a.width(), h: a.height(), ox: a.l, oy: a.t }.blit_into(&mut cv);

        let s = lay.s;
        let (m, c) = (lay.main, lay.close);
        if let Some(n) = &self.notice {
            draw_pill(&mut cv, n);
        }
        cv.round_rect(m.0, m.1, m.2, m.3, 14.0 * s, t.panel);
        let (sx, st, sb) = lay.sep;
        cv.capsule((sx, st), (sx, sb), 0.75 * s, t.dot_off);

        for (i, b) in BTNS.iter().enumerate() {
            let r = lay.btn[i];
            let (cx, cy) = ((r.0 + r.2) * 0.5, (r.1 + r.3) * 0.5);
            let hover = self.hover == Some(*b);
            match b {
                Btn::Highlighter => {
                    let bg = match (hl_on, hover) {
                        (true, false) => Some(t.selected),
                        (true, true) => Some(t.selected_hover),
                        (false, true) => Some(t.hover),
                        (false, false) => None,
                    };
                    if let Some(bg) = bg {
                        cv.round_rect(r.0, r.1, r.2, r.3, 8.0 * s, bg);
                    }
                    // Marker leaning like the pencil: body, then a chisel tip in the ink color.
                    let col = HL_COLORS[self.hl_color];
                    let u = (std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2);
                    let y = cy - 3.0 * s;
                    cv.oriented_box((cx + 1.5 * s, y - 1.5 * s), u, 6.5 * s, 3.6 * s, 1.2 * s, t.icon);
                    cv.oriented_box((cx - 6.0 * s, y + 6.0 * s), u, 2.4 * s, 2.6 * s, 0.6 * s, col);
                    for (k, kc) in HL_COLORS.iter().enumerate() {
                        let rad = if k == self.hl_color { 2.6 } else { 1.8 };
                        let dx = cx + (k as f32 - 1.5) * 7.0 * s;
                        cv.circle(dx, cy + 13.0 * s, rad * s, *kc);
                    }
                }
                Btn::Pencil => {
                    // The active tool is highlighted unless using the laser or selecting
                    // a screenshot.
                    let bg = match (pen_on, hover) {
                        (true, false) => Some(t.selected),
                        (true, true) => Some(t.selected_hover),
                        (false, true) => Some(t.hover),
                        (false, false) => None,
                    };
                    if let Some(bg) = bg {
                        cv.round_rect(r.0, r.1, r.2, r.3, 8.0 * s, bg);
                    }
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
                Btn::Laser => {
                    let bg = match (self.laser_on, hover) {
                        (true, false) => Some(t.selected),
                        (true, true) => Some(t.selected_hover),
                        (false, true) => Some(t.hover),
                        (false, false) => None,
                    };
                    if let Some(bg) = bg {
                        cv.round_rect(r.0, r.1, r.2, r.3, 8.0 * s, bg);
                    }
                    // Pointer body with a glowing red beam spot at its tip.
                    let (dx, dy) = (cx + 5.5 * s, cy - 5.5 * s);
                    cv.capsule((cx - 8.0 * s, cy + 8.0 * s), (cx + 0.5 * s, cy - 0.5 * s), 2.6 * s, t.icon);
                    let g = 6.0 * s;
                    let glow = Rect::bounds(dx - g - 1.0, dy - g - 1.0, dx + g + 1.0, dy + g + 1.0);
                    cv.fill_sdf(glow, laser::COLOR, 0.3, |x, y| ((x - dx).powi(2) + (y - dy).powi(2)).sqrt() - g);
                    cv.circle(dx, dy, 2.8 * s, laser::COLOR);
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
                Btn::Capture => {
                    let bg = match (self.shot != Shot::Off, hover) {
                        (true, false) => t.selected,
                        (true, true) => t.selected_hover,
                        (false, true) => t.hover,
                        (false, false) => t.panel,
                    };
                    if bg != t.panel {
                        cv.round_rect(r.0, r.1, r.2, r.3, 8.0 * s, bg);
                    }
                    // Camera: body, viewfinder bump and a ring-shaped lens.
                    cv.round_rect(cx - 6.0 * s, cy - 9.5 * s, cx + 1.0 * s, cy - 5.0 * s, 1.5 * s, t.icon);
                    cv.round_rect(cx - 10.5 * s, cy - 6.5 * s, cx + 10.5 * s, cy + 8.0 * s, 3.0 * s, t.icon);
                    cv.circle(cx, cy + 0.75 * s, 5.0 * s, bg);
                    cv.circle(cx, cy + 0.75 * s, 3.0 * s, t.icon);
                }
                Btn::Notice => {}
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
        self.unhook();
        // The veil is owned by the overlay window: destroy it first.
        self.veil = None;
        // Then the window; `dib` and `under` are released automatically afterwards.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Renders the notice pill: icon plus one line of text in the system UI font.
/// Text goes through GDI on a small opaque bitmap, then becomes plain pixels.
fn render_notice(text: &str, ok: bool, t: &Theme, lay: &Layout) -> Option<Notice> {
    let s = lay.s;
    let (cx, bottom, h, max_w) = lay.notice;
    let (pad, icon_w) = ((16.0 * s).round() as i32, (22.0 * s).round() as i32);
    let h = h as i32;
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        let dpi = (s * 96.0).round() as u32;
        if SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, (&mut ncm as *mut NONCLIENTMETRICSW).cast(), 0, dpi) == 0 {
            return None;
        }
        let font = CreateFontIndirectW(&ncm.lfMessageFont);
        if font.is_null() {
            return None;
        }
        let text_w: Vec<u16> = text.encode_utf16().collect();
        let rendered = (|| {
            // Measure on a 1x1 bitmap, then render on one of the final size.
            let probe = Dib::new(1, 1, true)?;
            SelectObject(probe.dc, font);
            let mut sz = SIZE { cx: 0, cy: 0 };
            GetTextExtentPoint32W(probe.dc, text_w.as_ptr(), text_w.len() as i32, &mut sz);
            let w = (pad * 2 + icon_w + sz.cx).min(max_w as i32);
            let mut dib = Dib::new(w, h, true)?;
            dib.surface().fill(0xFF00_0000 | rgb_u32(t.panel));
            SelectObject(dib.dc, font);
            SetBkMode(dib.dc, TRANSPARENT as i32);
            SetTextColor(dib.dc, colorref(t.icon));
            // Plain Latin text: ETO_IGNORELANGUAGE skips complex-script shaping, which
            // would otherwise load the text shaping engine into the process for good.
            let r = RECT { left: pad + icon_w, top: 0, right: w - pad, bottom: h };
            let y = (h - sz.cy) / 2;
            ExtTextOutW(dib.dc, r.left, y, ETO_CLIPPED | ETO_IGNORELANGUAGE, &r, text_w.as_ptr(), text_w.len() as u32, std::ptr::null());
            GdiFlush();
            let mut sf = dib.surface();
            for p in sf.px.iter_mut() {
                *p |= 0xFF00_0000; // GDI leaves alpha at 0 where it draws text
            }
            // Icon: check mark, or an exclamation mark for a partial failure.
            let (ix, iy) = (pad as f32 + 8.0 * s, h as f32 * 0.5);
            if ok {
                sf.capsule((ix - 6.0 * s, iy), (ix - 2.0 * s, iy + 4.0 * s), 1.6 * s, NOTICE_OK);
                sf.capsule((ix - 2.0 * s, iy + 4.0 * s), (ix + 6.0 * s, iy - 5.0 * s), 1.6 * s, NOTICE_OK);
            } else {
                sf.capsule((ix, iy - 6.0 * s), (ix, iy + 1.5 * s), 1.7 * s, NOTICE_WARN);
                sf.circle(ix, iy + 5.5 * s, 1.8 * s, NOTICE_WARN);
            }
            let px = sf.px.to_vec();
            let x0 = (cx - w as f32 * 0.5).round() as i32;
            let y0 = bottom as i32 - h;
            Some(Notice { px, rect: Rect::new(x0, y0, x0 + w, y0 + h) })
        })();
        DeleteObject(font); // the DCs that held it are already gone
        rendered
    }
}

/// Copies the notice into the canvas clipped to a pill shape (antialiased edges).
fn draw_pill(cv: &mut Surface, n: &Notice) {
    let r = n.rect;
    let (w, h) = (r.width(), r.height());
    let (cx, cy) = (r.l as f32 + w as f32 * 0.5, r.t as f32 + h as f32 * 0.5);
    let (hw, hh, rad) = (w as f32 * 0.5, h as f32 * 0.5, h as f32 * 0.5);
    let area = r.intersect(&cv.rect());
    for y in area.t..area.b {
        for x in area.l..area.r {
            let d = sd_round_rect(x as f32 + 0.5 - cx, y as f32 + 0.5 - cy, hw, hh, rad);
            let a = ((0.5 - d).clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
            if a == 0 {
                continue;
            }
            let p = n.px[((y - r.t) * w + (x - r.l)) as usize];
            let c = Rgb((p >> 16) as u8, (p >> 8) as u8, p as u8);
            let i = ((y - cv.oy) * cv.w + (x - cv.ox)) as usize;
            cv.px[i] = blend(cv.px[i], c, a);
        }
    }
}

/// Convex hull of a set of points (monotone chain), counterclockwise.
fn convex_hull(mut p: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    p.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let cross = |o: (f32, f32), a: (f32, f32), b: (f32, f32)| (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0);
    let mut hull: Vec<(f32, f32)> = Vec::with_capacity(p.len() + 1);
    for pass in 0..2 {
        let start = hull.len();
        for &q in &p {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], q) <= 0.0 {
                hull.pop();
            }
            hull.push(q);
        }
        hull.pop(); // the last point starts the other half
        if pass == 0 {
            p.reverse();
        }
    }
    hull
}

/// Signed distance from a point to a closed polygon, negative inside.
fn sd_polygon(x: f32, y: f32, v: &[(f32, f32)]) -> f32 {
    let mut d = f32::MAX;
    let mut sign = 1.0;
    let mut j = v.len() - 1;
    for i in 0..v.len() {
        let (a, b) = (v[i], v[j]);
        let e = (b.0 - a.0, b.1 - a.1);
        let w = (x - a.0, y - a.1);
        let len2 = e.0 * e.0 + e.1 * e.1;
        let t = if len2 > 0.0 { ((w.0 * e.0 + w.1 * e.1) / len2).clamp(0.0, 1.0) } else { 0.0 };
        let q = (w.0 - e.0 * t, w.1 - e.1 * t);
        d = d.min(q.0 * q.0 + q.1 * q.1);
        // Crossing count (even-odd rule) decides inside / outside.
        let c = (y >= a.1, y < b.1, e.0 * w.1 > e.1 * w.0);
        if (c.0 && c.1 && c.2) || (!c.0 && !c.1 && !c.2) {
            sign = -sign;
        }
        j = i;
    }
    sign * d.sqrt()
}

fn rgb_u32(c: Rgb) -> u32 {
    ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | c.2 as u32
}

/// GDI COLORREF (0x00BBGGRR).
fn colorref(c: Rgb) -> u32 {
    ((c.2 as u32) << 16) | ((c.1 as u32) << 8) | c.0 as u32
}
