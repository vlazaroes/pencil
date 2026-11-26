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

mod capture;
mod config;
mod gdi;
mod overlay;
mod raster;
mod settings;
mod sound;
mod updater;

use std::cell::RefCell;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};
use windows_sys::Win32::UI::Controls::{LoadIconMetric, LIM_SMALL};
use windows_sys::Win32::UI::Input::Ime::ImmDisableIME;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
    NIIF_NOSOUND, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use config::{hotkey_text, Config, Hotkey};
use overlay::{Effect, Overlay};

const HOTKEY_ID: i32 = 1;

/// Default pen: blue, 2nd size.
const DEFAULT_COLOR: usize = 0;
const DEFAULT_SIZE: usize = 1;

const WM_APP_HIDE: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
// WM_APP + 3 and + 4 are used by the update checker (see updater.rs).
/// A background PNG save finished (see capture.rs).
const WM_APP_SHOT_SAVED: u32 = WM_APP + 5;
/// Tray notification clicked (shell `NIN_BALLOONUSERCLICK`).
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;
/// Retry timer for an update that arrived while the user was busy.
const TIMER_INSTALL: usize = 1;
/// Timer that launches the next update check.
const TIMER_CHECK: usize = 2;
const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;
const ID_SETTINGS: usize = 3;

/// Control (hidden) window. Kept outside the `RefCell` so messages can be posted
/// to it from anywhere without taking a borrow.
static CTRL: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// Update checks launched so far in this run.
static UPDATE_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

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
    /// Last saved screenshot, opened when its notification is clicked.
    last_shot: Option<std::path::PathBuf>,
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
        Ok(ov) => {
            if ov.is_some() {
                drop(ov);
                trim_heap();
            }
        }
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

// --------------------------------------------------------------- screenshots

/// Grabs the finished selection, copies it right away and saves the PNG in the
/// background; the notice comes when the file is written.
fn take_screenshot() {
    let Some(mut img) = with_app(|a| a.overlay.as_mut().and_then(|o| o.screenshot())).flatten() else {
        return;
    };
    sound::shutter();
    let copied = capture::to_clipboard(ctrl(), &mut img);
    capture::save_async(img, copied, ctrl(), WM_APP_SHOT_SAVED);
}

/// Tells the user how a screenshot went: in the overlay if it is still open
/// (Windows mutes its own notifications while a full-screen window is in front),
/// otherwise with a tray notification.
fn shot_saved(saved: capture::Saved) {
    let ok = saved.copied && saved.path.is_some();
    let text = match (saved.copied, saved.path.is_some()) {
        (true, true) => "Copied to clipboard \u{b7} Saved to Pictures\\Pencil",
        (true, false) => "Copied to clipboard \u{b7} Could not save the PNG",
        (false, true) => "Could not copy to clipboard \u{b7} Saved to Pictures\\Pencil",
        (false, false) => "Could not copy or save the screenshot",
    };
    let shown = with_app(|a| {
        a.last_shot = saved.path;
        a.overlay.as_mut().map(|o| o.notify(ok, text)).is_some()
    })
    .unwrap_or(false);
    if !shown {
        notify("Pencil screenshot", text);
    }
}

/// Opens the image with its default app. Done through explorer.exe so the shell's
/// file association machinery (several MB) is never loaded into this process.
fn open_last_shot() {
    let Some(Some(path)) = with_app(|a| a.last_shot.clone()) else { return };
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// Gives freed heap memory back to the system. Large one-off work (a screenshot,
/// the overlay) leaves free blocks committed otherwise.
fn trim_heap() {
    unsafe { HeapCompact(GetProcessHeap(), 0) };
}

fn notify(title: &str, text: &str) {
    with_app(|a| {
        if let Some(t) = a.tray.as_ref() {
            t.notify(title, text);
        }
    });
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

/// Tray tooltip: just the app name and version.
fn tray_tip() -> String {
    format!("Pencil {}", env!("CARGO_PKG_VERSION"))
}

/// Resource id of the app icon embedded by build.rs: MAKEINTRESOURCE(1), an
/// integer passed where Windows expects a name pointer.
const APP_ICON: *const u16 = std::ptr::without_provenance(1);

/// Large and small app icons for window classes. Shared (LR_SHARED): Windows owns
/// them, so they are never destroyed by the app.
pub fn class_icons() -> (HICON, HICON) {
    unsafe {
        let hinst = GetModuleHandleW(null_mut());
        let big = LoadImageW(hinst, APP_ICON, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE | LR_SHARED);
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        let small = LoadImageW(hinst, APP_ICON, IMAGE_ICON, cx, cy, LR_SHARED);
        (big, small)
    }
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
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_CAPTURECHANGED | WM_KEYDOWN | WM_KEYUP | WM_TIMER => {
            let fx = with_app(|a| match a.overlay.as_mut() {
                Some(o) if o.hwnd == hwnd => o.handle(msg, wp, lp),
                _ => Effect::None,
            })
            .unwrap_or(Effect::None);
            // Effects that send synchronous messages: always without an active borrow.
            match fx {
                Effect::None => {}
                Effect::MouseCapture => {
                    SetCapture(hwnd);
                }
                Effect::MouseRelease => {
                    ReleaseCapture();
                }
                Effect::Screenshot => {
                    ReleaseCapture();
                    take_screenshot();
                }
                // Clicking the notice leaves drawing mode and shows the image.
                Effect::OpenShot => {
                    hide();
                    open_last_shot();
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
        WM_TIMER if wp == TIMER_CHECK => {
            KillTimer(hwnd, TIMER_CHECK);
            UPDATE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            updater::spawn_check(hwnd);
            0
        }
        // The checker hit a network error: schedule the next attempt, if any is left.
        updater::WM_UPDATE_FAILED => {
            schedule_update_check();
            0
        }
        updater::WM_UPDATE_READY | WM_TIMER => {
            KillTimer(hwnd, TIMER_INSTALL);
            // Never interrupt a drawing or an open settings window: retry in a minute.
            let busy = settings::hwnd().is_some() || with_app(|a| a.overlay.is_some()).unwrap_or(true);
            if busy {
                SetTimer(hwnd, TIMER_INSTALL, 60_000, None);
            } else if let Some(path) = updater::installer() {
                // The installer closes this process anyway; exit right away so it does not wait.
                if updater::run_installer(&path) {
                    DestroyWindow(hwnd);
                }
            }
            0
        }
        // Windows is shutting down, or an installer asked us to close (Restart Manager).
        WM_ENDSESSION => {
            if wp != 0 {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_APP_HIDE | WM_DISPLAYCHANGE => {
            hide();
            0
        }
        WM_APP_SHOT_SAVED => {
            for saved in capture::take_saved() {
                shot_saved(saved);
            }
            trim_heap();
            0
        }
        WM_APP_TRAY => {
            match (lp & 0xFFFF) as u32 {
                WM_LBUTTONUP => show(),
                NIN_BALLOONUSERCLICK => open_last_shot(),
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
            let t = Tray::new(hwnd, &tray_tip());
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
            // Small icon at the size the notification area uses for the current DPI.
            let mut icon: HICON = null_mut();
            LoadIconMetric(GetModuleHandleW(null_mut()), APP_ICON, LIM_SMALL, &mut icon);
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

    /// Shows a notification from the tray icon (a toast on Windows 10/11). Works on a
    /// copy so later tooltip updates do not show it again.
    fn notify(&self, title: &str, text: &str) {
        let mut nid = self.nid;
        nid.uFlags = NIF_INFO;
        let copy = |dst: &mut [u16], s: &str| {
            let v: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
            dst.fill(0);
            dst[..v.len()].copy_from_slice(&v);
        };
        copy(&mut nid.szInfoTitle, title);
        copy(&mut nid.szInfo, text);
        nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
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

// --------------------------------------------------------------- main

fn error_box(text: &str) {
    let t = wide(text);
    let c = wide("Pencil");
    unsafe { MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONERROR) };
}

/// Arms the timer for the next update check, or does nothing when all attempts are used.
fn schedule_update_check() {
    let n = UPDATE_ATTEMPTS.load(Ordering::Relaxed);
    if let Some(&delay) = updater::ATTEMPT_DELAYS_MS.get(n) {
        unsafe { SetTimer(ctrl(), TIMER_CHECK, delay, None) };
    }
}

fn main() {
    // One-shot helper child processes: no window, tray or single-instance lock.
    let mut args = std::env::args_os().skip(1);
    let arg = args.next();
    if arg.as_deref() == Some(std::ffi::OsStr::new(updater::CHECK_ARG)) {
        return updater::run_check(args.next().as_deref());
    }
    if arg.as_deref() == Some(std::ffi::OsStr::new(sound::PLAY_ARG)) {
        return sound::play_and_exit();
    }

    unsafe {
        // Pencil never takes text input, so keep the IME / Text Services Framework
        // out of the process (it would load as soon as a window gets keyboard focus).
        ImmDisableIME(u32::MAX);

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
        (wc.hIcon, wc.hIconSm) = class_icons();
        wc.lpfnWndProc = Some(ctrl_proc);
        wc.lpszClassName = ctrl_class.as_ptr();
        if RegisterClassExW(&wc) == 0 {
            return error_box("Could not register the window class.");
        }
        wc.lpfnWndProc = Some(overlay_proc);
        wc.lpszClassName = overlay_class.as_ptr();
        if RegisterClassExW(&wc) == 0 || !settings::register_class(hinst) || !capture::register_class(hinst) {
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
                last_shot: None,
            })
        });
        let tray = Tray::new(hwnd, &tray_tip());
        with_app(|a| a.tray = tray);

        // Check for a newer release (installed copies only, once per start).
        if updater::enabled() {
            schedule_update_check();
        }

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
