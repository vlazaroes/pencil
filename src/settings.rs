//! Small native settings window: shortcuts and theme.
//!
//! While it is open the global hotkey is unregistered, so the user can type any
//! combination (including the current one) into the hotkey field. It is registered
//! again when the window closes, with the new value on OK or the old one on Cancel.

use std::cell::Cell;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetMonitorInfoW, COLOR_BTNFACE, MonitorFromPoint, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, BST_UNCHECKED, ICC_HOTKEY_CLASS, INITCOMMONCONTROLSEX,
};
use windows_sys::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForMonitor, GetDpiForWindow, SystemParametersInfoForDpi, MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, MOD_ALT, MOD_CONTROL, MOD_SHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::{
    autostart_enabled, hotkey_text, is_extended, set_autostart, Config, Hotkey, Keys, ThemePref,
};
use crate::wide;

const CLASS: &str = "PencilSettings";
const TITLE: &str = "Settings";

/// Labels in `ctl` index order (hotkeys, then theme). The theme row is shown first.
const LABELS: [&str; ROWS] = ["Show / Hide", "Color", "Size", "Erase", "Capture", "Theme"];
/// Visual row of each `ctl` index.
const ROW: [i32; ROWS] = [1, 2, 3, 4, 5, 0];
/// Form rows: the show/hide hotkey, the tool keys, then the theme.
const ROWS: usize = 6;
/// Index of the theme combo box in `ctl` (the hotkey controls come before it).
const THEME: usize = ROWS - 1;
const THEMES: [&str; 3] = ["System", "Light", "Dark"];

// Hotkey control messages and flags (commctrl.h).
const HOTKEY_CLASS: &str = "msctls_hotkey32";
const HKM_SETHOTKEY: u32 = WM_USER + 1;
const HKM_GETHOTKEY: u32 = WM_USER + 2;
const HKM_SETRULES: u32 = WM_USER + 3;
const HOTKEYF_SHIFT: u32 = 0x01;
const HOTKEYF_CONTROL: u32 = 0x02;
const HOTKEYF_ALT: u32 = 0x04;
const HOTKEYF_EXT: u32 = 0x08;
const HKCOMB_NONE: usize = 0x01;
const HKCOMB_S: usize = 0x02;
/// Every combination that includes at least one modifier.
const HKCOMB_ANY_MOD: usize = 0xFE;

/// Static control style: vertically center single-line text.
const SS_CENTERIMAGE: u32 = 0x200;

const STYLE: u32 = WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;
const EX_STYLE: u32 = WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW;

/// All handles of the open window. Plain `Copy` handles, so they can be read
/// without holding any borrow while calling into Win32.
#[derive(Clone, Copy)]
struct Win {
    hwnd: HWND,
    font: HFONT,
    labels: [HWND; ROWS],
    /// Show/hide, pencil, size, clear and capture hotkey controls, then the theme combo box.
    ctl: [HWND; ROWS],
    /// "Start with Windows" check box.
    autostart: HWND,
    ok: HWND,
    cancel: HWND,
}

thread_local! {
    static WIN: Cell<Option<Win>> = const { Cell::new(None) };
}

/// Handle of the settings window if it is open.
pub fn hwnd() -> Option<HWND> {
    WIN.with(|w| w.get()).map(|w| w.hwnd)
}

pub fn register_class(hinst: HINSTANCE) -> bool {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_HOTKEY_CLASS,
        };
        InitCommonControlsEx(&icc);
        let class = wide(CLASS);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = hinst;
        (wc.hIcon, wc.hIconSm) = crate::class_icons();
        wc.lpfnWndProc = Some(settings_proc);
        wc.lpszClassName = class.as_ptr();
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hbrBackground = (COLOR_BTNFACE + 1) as usize as _;
        RegisterClassExW(&wc) != 0
    }
}

/// Opens the window (or brings it to front if it is already open).
pub fn open(hinst: HINSTANCE, owner: HWND, cfg: &Config) {
    unsafe {
        if let Some(h) = hwnd() {
            SetForegroundWindow(h);
            return;
        }
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let (mut dpi, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi, &mut dy) != 0 {
            dpi = 96;
        }

        let class = wide(CLASS);
        let title = wide(TITLE);
        let hwnd = CreateWindowExW(
            EX_STYLE, class.as_ptr(), title.as_ptr(), STYLE,
            0, 0, 0, 0, owner, null_mut(), hinst, null_mut(),
        );
        if hwnd.is_null() {
            return;
        }

        let child = |cls: &str, text: &str, style: u32, id: usize| {
            let c = wide(cls);
            let t = wide(text);
            CreateWindowExW(
                0, c.as_ptr(), t.as_ptr(), WS_CHILD | WS_VISIBLE | style,
                0, 0, 0, 0, hwnd, id as _, hinst, null_mut(),
            )
        };
        let labels = LABELS.map(|l| child("STATIC", l, SS_CENTERIMAGE, 0));
        // Creation order is the Tab order: theme first, matching the visual order.
        let mut ctl = [null_mut(); ROWS];
        ctl[THEME] = child("COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32, 110);
        for (i, c) in ctl.iter_mut().take(THEME).enumerate() {
            *c = child(HOTKEY_CLASS, "", WS_TABSTOP, 100 + i);
        }
        let autostart = child("BUTTON", "Start with Windows", WS_TABSTOP | BS_AUTOCHECKBOX as u32, 105);
        let ok = child("BUTTON", "OK", WS_TABSTOP | BS_DEFPUSHBUTTON as u32, IDOK as usize);
        let cancel = child("BUTTON", "Cancel", WS_TABSTOP | BS_PUSHBUTTON as u32, IDCANCEL as usize);

        // Show/hide must contain Ctrl or Alt: plain keys and Shift+key become Ctrl+Alt+key.
        SendMessageW(ctl[0], HKM_SETRULES, HKCOMB_NONE | HKCOMB_S, (HOTKEYF_CONTROL | HOTKEYF_ALT) as LPARAM);
        // Tool shortcuts are single keys: any modifier is dropped.
        for c in &ctl[1..THEME] {
            SendMessageW(*c, HKM_SETRULES, HKCOMB_ANY_MOD, 0);
        }
        set_hotkey(ctl[0], cfg.hotkey);
        for (c, vk) in ctl[1..THEME].iter().zip(cfg.keys.all()) {
            set_hotkey(*c, Hotkey { mods: 0, vk: vk as u32 });
        }
        for t in THEMES {
            let w = wide(t);
            SendMessageW(ctl[THEME], CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
        }
        SendMessageW(ctl[THEME], CB_SETCURSEL, cfg.theme as usize, 0);
        let checked = if autostart_enabled() { BST_CHECKED } else { BST_UNCHECKED };
        SendMessageW(autostart, BM_SETCHECK, checked as WPARAM, 0);

        let win = Win { hwnd, font: null_mut(), labels, ctl, autostart, ok, cancel };
        let win = layout(win, dpi);
        WIN.with(|w| w.set(Some(win)));

        // Center on the work area of the monitor under the cursor.
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(hwnd, &mut r);
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            let wa = mi.rcWork;
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let x = wa.left + (wa.right - wa.left - w) / 2;
            let y = wa.top + (wa.bottom - wa.top - h) / 2;
            SetWindowPos(hwnd, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(ctl[THEME]);
    }
}

/// Positions every control for `dpi`, applies a matching font and resizes the window.
unsafe fn layout(mut w: Win, dpi: u32) -> Win {
    let sc = |v: i32| (v * dpi as i32 + 48) / 96;

    let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
    ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
    let font = if SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, (&mut ncm as *mut NONCLIENTMETRICSW).cast(), 0, dpi) != 0 {
        CreateFontIndirectW(&ncm.lfMessageFont)
    } else {
        null_mut()
    };
    let all = w.labels.iter().chain(w.ctl.iter()).chain([&w.autostart, &w.ok, &w.cancel]);
    for c in all {
        SendMessageW(*c, WM_SETFONT, font as WPARAM, 0);
    }
    if !w.font.is_null() {
        DeleteObject(w.font);
    }
    w.font = font;

    let (margin, label_w, ctl_w, row_h, gap, btn_w) = (sc(12), sc(76), sc(170), sc(24), sc(8), sc(80));
    let ctl_x = margin + label_w + sc(8);
    for (i, row) in ROW.iter().enumerate() {
        let y = margin + row * (row_h + gap);
        MoveWindow(w.labels[i], margin, y, label_w, row_h, 1);
        // A combo box height includes its drop-down list.
        let h = if i == THEME { row_h + sc(120) } else { row_h };
        MoveWindow(w.ctl[i], ctl_x, y, ctl_w, h, 1);
    }
    let client_w = ctl_x + ctl_w + margin;
    let mut y = margin + ROWS as i32 * (row_h + gap);
    MoveWindow(w.autostart, ctl_x, y, ctl_w, row_h, 1);
    y += row_h + gap + sc(6);
    MoveWindow(w.cancel, client_w - margin - btn_w, y, btn_w, row_h + sc(2), 1);
    MoveWindow(w.ok, client_w - margin - 2 * btn_w - sc(8), y, btn_w, row_h + sc(2), 1);
    let client_h = y + row_h + sc(2) + margin;

    let mut r = RECT { left: 0, top: 0, right: client_w, bottom: client_h };
    AdjustWindowRectExForDpi(&mut r, STYLE, 0, EX_STYLE, dpi);
    SetWindowPos(w.hwnd, null_mut(), 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    w
}

unsafe fn set_hotkey(ctl: HWND, h: Hotkey) {
    let mut f = 0;
    if h.mods & MOD_SHIFT != 0 {
        f |= HOTKEYF_SHIFT;
    }
    if h.mods & MOD_CONTROL != 0 {
        f |= HOTKEYF_CONTROL;
    }
    if h.mods & MOD_ALT != 0 {
        f |= HOTKEYF_ALT;
    }
    if is_extended(h.vk as u16) {
        f |= HOTKEYF_EXT;
    }
    SendMessageW(ctl, HKM_SETHOTKEY, (h.vk & 0xFF | f << 8) as WPARAM, 0);
}

unsafe fn get_hotkey(ctl: HWND) -> Hotkey {
    let v = SendMessageW(ctl, HKM_GETHOTKEY, 0, 0) as u32;
    let f = (v >> 8) & 0xFF;
    let mut mods = 0;
    if f & HOTKEYF_SHIFT != 0 {
        mods |= MOD_SHIFT;
    }
    if f & HOTKEYF_CONTROL != 0 {
        mods |= MOD_CONTROL;
    }
    if f & HOTKEYF_ALT != 0 {
        mods |= MOD_ALT;
    }
    Hotkey { mods, vk: v & 0xFF }
}

/// Reads and validates the form. Returns the new config or a message for the user.
unsafe fn read_form(w: &Win) -> Result<Config, String> {
    let hotkey = get_hotkey(w.ctl[0]);
    if !hotkey.is_valid() {
        return Err("The Show / Hide shortcut must include Ctrl or Alt plus another key.".into());
    }
    let k = [1, 2, 3, 4].map(|i| get_hotkey(w.ctl[i]).vk as u16);
    let keys = Keys { pencil: k[0], size: k[1], clear: k[2], capture: k[3] };
    if k.contains(&0) {
        return Err("Every tool needs a shortcut.".into());
    }
    if !keys.is_valid() {
        return Err(
            "Tool shortcuts must be different from each other.\n\
             Esc, Delete, Backspace, Enter, Tab and Space are reserved."
                .into(),
        );
    }
    let sel = SendMessageW(w.ctl[THEME], CB_GETCURSEL, 0, 0);
    let theme = ThemePref::from_index(sel.max(0) as u32);
    Ok(Config { hotkey, keys, theme })
}

unsafe fn message(owner: HWND, text: &str) {
    let t = wide(text);
    let c = wide("Pencil");
    MessageBoxW(owner, t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONWARNING);
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match (wp & 0xFFFF) as i32 {
                IDOK => {
                    let Some(w) = WIN.with(|w| w.get()) else { return 0 };
                    let autostart = SendMessageW(w.autostart, BM_GETCHECK, 0, 0) == BST_CHECKED as LRESULT;
                    // Autostart first: it can be retried freely, registering the hotkey cannot.
                    let result = read_form(&w)
                        .and_then(|cfg| {
                            if autostart == autostart_enabled() || set_autostart(autostart) {
                                Ok(cfg)
                            } else {
                                Err("Could not change the \"Start with Windows\" setting.".into())
                            }
                        })
                        .and_then(|cfg| {
                            if crate::apply_settings(cfg) {
                                Ok(())
                            } else {
                                Err(format!("{} is already in use by another program.", hotkey_text(cfg.hotkey)))
                            }
                        });
                    match result {
                        Ok(()) => {
                            DestroyWindow(hwnd);
                        }
                        Err(e) => message(hwnd, &e),
                    }
                }
                IDCANCEL => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        // Neither parameter is trusted: any process can send this message, and its
        // lParam would be a pointer into that process. The window's real DPI is
        // asked instead and the window keeps its position; `layout` resizes it.
        WM_DPICHANGED => {
            if let Some(w) = WIN.with(|w| w.get()) {
                let w = layout(w, GetDpiForWindow(hwnd));
                WIN.with(|c| c.set(Some(w)));
            }
            0
        }
        WM_NCDESTROY => {
            // Children are gone by now, so the font can be released.
            if let Some(w) = WIN.with(|w| w.take()) {
                if !w.font.is_null() {
                    DeleteObject(w.font);
                }
            }
            crate::settings_closed();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
