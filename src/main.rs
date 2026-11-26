//! Pencil: full-screen drawing overlay for Windows.
//!
//! In the background only a hidden window (receives the global hotkey and the tray
//! icon events) and the tray icon exist. The thread sleeps in `GetMessageW`, so CPU
//! usage is 0 % until the hotkey is pressed.
//!
//! Ways out of the overlay (any of them destroys it completely):
//!   * the global hotkey again (works even if the overlay does not have focus),
//!   * Esc or Alt+F4,
//!   * the toolbar X button,
//!   * losing focus (Alt+Tab, Windows key, Ctrl+Alt+Del, session lock...),
//!   * a display/monitor configuration change,
//!   * any panic (`panic = "abort"` profile: the process dies and Windows removes
//!     its windows).

#![windows_subsystem = "windows"]

mod config;
mod gdi;
mod overlay;
mod raster;
mod settings;

use std::cell::RefCell;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use config::{hotkey_text, Config, Hotkey};
use overlay::{Effect, Overlay};
use raster::{Rgb, Surface};

const HOTKEY_ID: i32 = 1;

/// Default pen: blue, 2nd size.
const DEFAULT_COLOR: usize = 0;
const DEFAULT_SIZE: usize = 1;

const WM_APP_HIDE: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;
const ID_SETTINGS: usize = 3;

/// Control (hidden) window. Kept outside the `RefCell` so messages can be posted
/// to it from anywhere without taking a borrow.
static CTRL: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

struct App {
    hinst: HINSTANCE,
    overlay_class: Vec<u16>,
    overlay: Option<Overlay>,
    tray: Option<Tray>,
    config: Config,
    /// Whether `config.hotkey` is currently registered.
    hotkey_active: bool,
    /// Color and size remembered between activations.
    color: usize,
    size: usize,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` with the global state if it is available. If the state is already
/// borrowed (re-entrant call from Win32) it returns `None` instead of panicking;
/// callers treat that as "do nothing".
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Null-terminated UTF-16 string for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ctrl() -> HWND {
    CTRL.load(Ordering::Relaxed)
}

/// Requests hiding the overlay asynchronously (safe from any context).
fn post_hide() {
    unsafe { PostMessageW(ctrl(), WM_APP_HIDE, 0, 0) };
}

fn show() {
    // Not while the settings window is open: the global hotkey is unregistered then.
    if settings::hwnd().is_some() {
        return;
    }
    let Some(Some((hinst, class, color, size, cfg))) = with_app(|a| {
        a.overlay.is_none().then_some((a.hinst, a.overlay_class.as_ptr(), a.color, a.size, a.config))
    }) else {
        return;
    };
    // Created with no active borrow: CreateWindowEx sends synchronous messages.
    let Some(ov) = Overlay::new(hinst, class, color, size, &cfg) else { return };
    let hwnd = ov.hwnd;
    if with_app(|a| a.overlay = Some(ov)).is_none() {
        return; // practically impossible; if it happens, `ov` has already been destroyed
    }
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}

fn hide() {
    // Take the overlay out of the state and destroy it outside the borrow
    // (DestroyWindow sends messages that re-enter the wndproc).
    let taken = APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => Ok(g.as_mut().and_then(|app| {
            let ov = app.overlay.take();
            if let Some(o) = &ov {
                app.color = o.color;
                app.size = o.size;
            }
            ov
        })),
        Err(_) => Err(()),
    });
    match taken {
        Ok(ov) => drop(ov),
        // State busy: retry as soon as the message loop comes back.
        Err(()) => post_hide(),
    }
}

fn toggle() {
    match with_app(|a| a.overlay.is_some()) {
        Some(true) => hide(),
        Some(false) => show(),
        None => post_hide(),
    }
}

// --------------------------------------------------------------- settings

fn register_hotkey(h: Hotkey) -> bool {
    unsafe { RegisterHotKey(ctrl(), HOTKEY_ID, h.mods | MOD_NOREPEAT, h.vk) != 0 }
}

fn open_settings() {
    let Some((hinst, cfg)) = with_app(|a| (a.hinst, a.config)) else { return };
    if settings::hwnd().is_none() {
        hide();
        // Free the hotkey so it can be typed into the hotkey field.
        unsafe { UnregisterHotKey(ctrl(), HOTKEY_ID) };
        with_app(|a| a.hotkey_active = false);
    }
    settings::open(hinst, ctrl(), &cfg);
}

/// Called by the settings window on OK. Registers the new hotkey (the old one is
/// already unregistered) and stores the config. `false` if the hotkey is taken.
pub fn apply_settings(cfg: Config) -> bool {
    if !register_hotkey(cfg.hotkey) {
        return false;
    }
    cfg.save();
    with_app(|a| {
        a.config = cfg;
        a.hotkey_active = true;
        if let Some(t) = a.tray.as_mut() {
            t.set_tip(&tray_tip(&cfg));
        }
    });
    true
}

/// Called when the settings window is destroyed (OK, Cancel or app exit):
/// restores the previous hotkey if no new one was applied.
pub fn settings_closed() {
    let Some((active, cfg)) = with_app(|a| (a.hotkey_active, a.config)) else { return };
    if active {
        return;
    }
    if register_hotkey(cfg.hotkey) {
        with_app(|a| a.hotkey_active = true);
    } else {
        error_box(&hotkey_taken_text(cfg.hotkey));
    }
}

fn hotkey_taken_text(h: Hotkey) -> String {
    format!(
        "Could not register {}.\nIt may be in use by another program; choose another one in Settings.",
        hotkey_text(h)
    )
}

fn tray_tip(cfg: &Config) -> String {
    format!("Pencil ({})", hotkey_text(cfg.hotkey))
}

// --------------------------------------------------------------- wndprocs

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // Safety net: if the overlay loses focus for any reason, it closes.
        WM_ACTIVATE => {
            if (wp & 0xFFFF) as u32 == WA_INACTIVE {
                post_hide();
            }
            0
        }
        WM_CLOSE | WM_DISPLAYCHANGE => {
            post_hide();
            0
        }
        // Avoid entering the system menu modal loop when Alt is released.
        WM_SYSCOMMAND if (wp & 0xFFF0) as u32 == SC_KEYMENU => 0,
        WM_DPICHANGED => 0,
        WM_SETCURSOR if (lp & 0xFFFF) as u32 == HTCLIENT => {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
            let hand = with_app(|a| a.overlay.as_ref().is_some_and(|o| o.over_button(pt.x, pt.y)))
                .unwrap_or(false);
            SetCursor(LoadCursorW(null_mut(), if hand { IDC_HAND } else { IDC_CROSS }));
            1
        }
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_CAPTURECHANGED | WM_KEYDOWN => {
            let fx = with_app(|a| match a.overlay.as_mut() {
                Some(o) if o.hwnd == hwnd => o.handle(msg, wp, lp),
                _ => Effect::None,
            })
            .unwrap_or(Effect::None);
            // Effects that send synchronous messages: always without an active borrow.
            match fx {
                Effect::None => {}
                Effect::Capture => {
                    SetCapture(hwnd);
                }
                Effect::Release => {
                    ReleaseCapture();
                }
                Effect::Hide => hide(),
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn ctrl_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wp as i32 == HOTKEY_ID => {
            toggle();
            0
        }
        WM_APP_HIDE | WM_DISPLAYCHANGE => {
            hide();
            0
        }
        WM_APP_TRAY => {
            match (lp & 0xFFFF) as u32 {
                WM_LBUTTONUP => show(),
                WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            hide();
            let tray = with_app(|a| a.tray.take()).flatten();
            drop(tray);
            UnregisterHotKey(hwnd, HOTKEY_ID);
            PostQuitMessage(0);
            0
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // Explorer restarted: the icon has to be added again.
            let old = with_app(|a| a.tray.take()).flatten();
            drop(old);
            let tip = with_app(|a| tray_tip(&a.config)).unwrap_or_default();
            let t = Tray::new(hwnd, &tip);
            with_app(|a| a.tray = t);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn tray_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let hk = with_app(|a| hotkey_text(a.config.hotkey)).unwrap_or_default();
    let show_txt = wide(&format!("Draw\t{hk}"));
    let settings_txt = wide("Settings");
    let quit_txt = wide("Quit");
    let draw_flags = if settings::hwnd().is_some() { MF_STRING | MF_GRAYED } else { MF_STRING };
    AppendMenuW(menu, draw_flags, ID_SHOW, show_txt.as_ptr());
    AppendMenuW(menu, MF_STRING, ID_SETTINGS, settings_txt.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null_mut());
    AppendMenuW(menu, MF_STRING, ID_QUIT, quit_txt.as_ptr());
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd); // required so the menu closes when clicking outside it
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null_mut());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match cmd as usize {
        ID_SHOW => show(),
        ID_SETTINGS => open_settings(),
        ID_QUIT => {
            DestroyWindow(hwnd);
        }
        _ => {}
    }
}

// --------------------------------------------------------------- tray

struct Tray {
    nid: NOTIFYICONDATAW,
}

impl Tray {
    fn new(hwnd: HWND, tip: &str) -> Option<Tray> {
        unsafe {
            let icon = make_icon();
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_APP_TRAY;
            nid.hIcon = if icon.is_null() { LoadIconW(null_mut(), IDI_APPLICATION) } else { icon };
            let mut tray = Tray { nid };
            tray.write_tip(tip);
            if Shell_NotifyIconW(NIM_ADD, &tray.nid) == 0 {
                return None; // Drop releases the icon
            }
            Some(tray)
        }
    }
}

impl Tray {
    fn write_tip(&mut self, tip: &str) {
        let t: Vec<u16> = tip.encode_utf16().take(self.nid.szTip.len() - 1).collect();
        self.nid.szTip.fill(0);
        self.nid.szTip[..t.len()].copy_from_slice(&t);
    }

    fn set_tip(&mut self, tip: &str) {
        self.write_tip(tip);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.nid) };
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &self.nid);
            // System icons (IDI_APPLICATION) are shared and must not be destroyed.
            if self.nid.hIcon != LoadIconW(null_mut(), IDI_APPLICATION) {
                DestroyIcon(self.nid.hIcon);
            }
        }
    }
}

/// Builds the tray icon (blue circle with a white pencil) without external resources.
unsafe fn make_icon() -> HICON {
    let sz = GetSystemMetrics(SM_CXSMICON).clamp(16, 64);
    let Some(mut dib) = gdi::Dib::new(sz, sz, false) else { return null_mut() };
    {
        let mut sf: Surface = dib.surface();
        sf.fill(0);
        let f = sz as f32;
        sf.circle(f * 0.5, f * 0.5, f * 0.5 - 0.5, Rgb(0x1E, 0x6F, 0xE8));
        sf.capsule((f * 0.32, f * 0.68), (f * 0.70, f * 0.30), f * 0.09, Rgb(255, 255, 255));
    }
    // Zeroed monochrome mask (rows aligned to 16 bits); ignored with 32 bpp alpha.
    let mask_bits = vec![0u8; (((sz + 15) / 16) * 2 * sz) as usize];
    let mask = windows_sys::Win32::Graphics::Gdi::CreateBitmap(sz, sz, 1, 1, mask_bits.as_ptr().cast());
    if mask.is_null() {
        return null_mut();
    }
    let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: dib.bmp };
    let icon = CreateIconIndirect(&ii); // copies the bitmaps
    windows_sys::Win32::Graphics::Gdi::DeleteObject(mask);
    icon
}

// --------------------------------------------------------------- main

fn error_box(text: &str) {
    let t = wide(text);
    let c = wide("Pencil");
    unsafe { MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONERROR) };
}

fn main() {
    unsafe {
        // Single instance: a second launch just tells the user and exits.
        let mutex_name = wide("Local\\PencilSingleInstance");
        let mutex = CreateMutexW(null_mut(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !mutex.is_null() {
                CloseHandle(mutex);
            }
            return error_box("Pencil is already running (see its icon in the notification area).");
        }

        // Physical pixel coordinates on every monitor.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hinst = GetModuleHandleW(null_mut());
        let ctrl_class = wide("PencilCtrl");
        let overlay_class = wide("PencilOverlay");

        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = hinst;
        wc.lpfnWndProc = Some(ctrl_proc);
        wc.lpszClassName = ctrl_class.as_ptr();
        if RegisterClassExW(&wc) == 0 {
            return error_box("Could not register the window class.");
        }
        wc.lpfnWndProc = Some(overlay_proc);
        wc.lpszClassName = overlay_class.as_ptr();
        if RegisterClassExW(&wc) == 0 || !settings::register_class(hinst) {
            return error_box("Could not register the window class.");
        }

        // Hidden top-level window (not "message-only") so it can receive broadcasts
        // such as TaskbarCreated and own the tray popup menu.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW, ctrl_class.as_ptr(), ctrl_class.as_ptr(), WS_POPUP,
            0, 0, 0, 0, null_mut(), null_mut(), hinst, null_mut(),
        );
        if hwnd.is_null() {
            return error_box("Could not create the control window.");
        }
        CTRL.store(hwnd, Ordering::Relaxed);

        // A taken hotkey is not fatal: the tray icon still works and Settings can change it.
        let config = Config::load();
        let hotkey_active = register_hotkey(config.hotkey);
        if !hotkey_active {
            error_box(&hotkey_taken_text(config.hotkey));
        }
        let tb = wide("TaskbarCreated");
        TASKBAR_CREATED.store(RegisterWindowMessageW(tb.as_ptr()), Ordering::Relaxed);

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hinst,
                overlay_class,
                overlay: None,
                tray: None,
                config,
                hotkey_active,
                color: DEFAULT_COLOR,
                size: DEFAULT_SIZE,
            })
        });
        let tray = Tray::new(hwnd, &tray_tip(&config));
        with_app(|a| a.tray = tray);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            // Tab / Enter / Esc navigation inside the settings window.
            if let Some(h) = settings::hwnd() {
                if IsDialogMessageW(h, &msg) != 0 {
                    continue;
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Final cleanup (normally already done in WM_DESTROY).
        let app = APP.with(|a| a.borrow_mut().take());
        drop(app);
        CloseHandle(mutex);
    }
}
