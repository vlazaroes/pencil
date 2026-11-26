//! User settings, persisted as DWORD values under `HKCU\Software\Pencil`.

use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

use crate::wide;

const REG_KEY: &str = "Software\\Pencil";
/// Per-user "run at logon" list. The installer adds the value on a fresh install;
/// afterwards only the Settings window changes it (updates never touch it).
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Pencil";

/// Whether Pencil is set to start with Windows.
pub fn autostart_enabled() -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    rc == 0
}

/// Adds (pointing at the running executable) or removes the logon entry.
pub fn set_autostart(on: bool) -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return false };
            let cmd = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                cmd.as_ptr().cast(),
                (cmd.len() * 2) as u32,
            ) == 0
        } else {
            let rc = RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
            rc == 0 || rc == 2 // ERROR_FILE_NOT_FOUND: already off
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePref {
    System = 0,
    Light = 1,
    Dark = 2,
}

impl ThemePref {
    pub fn from_index(i: u32) -> ThemePref {
        match i {
            1 => ThemePref::Light,
            2 => ThemePref::Dark,
            _ => ThemePref::System,
        }
    }
}

/// Global show/hide hotkey: `MOD_*` flags plus a virtual key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Hotkey {
    /// Must include Ctrl, Alt or Win so it never swallows plain typing.
    pub fn is_valid(&self) -> bool {
        let known = MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN;
        (1..=0xFE).contains(&self.vk)
            && !is_modifier(self.vk as u16)
            && self.mods & !known == 0
            && self.mods & (MOD_CONTROL | MOD_ALT | MOD_WIN) != 0
    }
}

/// Single-key tool shortcuts, only active while the overlay is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Keys {
    pub pencil: u16,
    pub size: u16,
    pub clear: u16,
}

impl Keys {
    pub fn all(&self) -> [u16; 3] {
        [self.pencil, self.size, self.clear]
    }

    pub fn is_valid(&self) -> bool {
        let k = self.all();
        k.iter().all(|&v| v != 0 && v <= 0xFE && !is_reserved(v))
            && k[0] != k[1]
            && k[0] != k[2]
            && k[1] != k[2]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    pub hotkey: Hotkey,
    pub keys: Keys,
    pub theme: ThemePref,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey: Hotkey { mods: MOD_CONTROL | MOD_ALT, vk: b'D' as u32 },
            keys: Keys { pencil: b'P' as u16, size: b'S' as u16, clear: b'E' as u16 },
            theme: ThemePref::System,
        }
    }
}

impl Config {
    /// Loads the saved settings; any missing or invalid group falls back to its default.
    pub fn load() -> Config {
        let d = Config::default();
        let hotkey = read("Hotkey")
            .map(|v| Hotkey { mods: v >> 16, vk: v & 0xFFFF })
            .filter(Hotkey::is_valid)
            .unwrap_or(d.hotkey);
        let keys = match (read("PencilKey"), read("SizeKey"), read("ClearKey")) {
            (Some(p), Some(s), Some(c)) => {
                Some(Keys { pencil: p as u16, size: s as u16, clear: c as u16 }).filter(Keys::is_valid)
            }
            _ => None,
        }
        .unwrap_or(d.keys);
        let theme = read("Theme").map(ThemePref::from_index).unwrap_or(d.theme);
        Config { hotkey, keys, theme }
    }

    pub fn save(&self) {
        write("Hotkey", (self.hotkey.mods << 16) | self.hotkey.vk);
        write("PencilKey", self.keys.pencil as u32);
        write("SizeKey", self.keys.size as u32);
        write("ClearKey", self.keys.clear as u32);
        write("Theme", self.theme as u32);
    }
}

fn read(name: &str) -> Option<u32> {
    let key = wide(REG_KEY);
    let name = wide(name);
    let mut value: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(value)
}

fn write(name: &str, value: u32) {
    let key = wide(REG_KEY);
    let name = wide(name);
    // Creates the key if it does not exist yet.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_DWORD,
            (&value as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
    }
}

fn is_modifier(vk: u16) -> bool {
    matches!(
        vk,
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL
            | VK_RCONTROL | VK_LMENU | VK_RMENU
    )
}

/// Keys with a fixed meaning inside the overlay (or modifiers), which tool
/// shortcuts may not reuse.
pub fn is_reserved(vk: u16) -> bool {
    is_modifier(vk)
        || matches!(vk, VK_ESCAPE | VK_DELETE | VK_BACK | VK_RETURN | VK_TAB | VK_SPACE)
        || (0x31..=0x34).contains(&vk)
        || (VK_NUMPAD1..=VK_NUMPAD4).contains(&vk)
}

/// Keys that need the "extended" flag for their name and for the hotkey control.
pub fn is_extended(vk: u16) -> bool {
    matches!(
        vk,
        VK_INSERT | VK_DELETE | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_LEFT | VK_RIGHT | VK_UP
            | VK_DOWN | VK_DIVIDE | VK_NUMLOCK | VK_SNAPSHOT | VK_RCONTROL | VK_RMENU
    )
}

/// Name of a key as Windows shows it for the current keyboard layout.
pub fn key_name(vk: u16) -> String {
    unsafe {
        let sc = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC);
        let lp = ((sc as i32) << 16) | if is_extended(vk) { 1 << 24 } else { 0 };
        let mut buf = [0u16; 64];
        let n = GetKeyNameTextW(lp, buf.as_mut_ptr(), buf.len() as i32);
        if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            format!("0x{vk:02X}")
        }
    }
}

/// Human readable hotkey, e.g. "Ctrl+Alt+D".
pub fn hotkey_text(h: Hotkey) -> String {
    let mut s = String::new();
    for (flag, name) in [(MOD_CONTROL, "Ctrl+"), (MOD_ALT, "Alt+"), (MOD_SHIFT, "Shift+"), (MOD_WIN, "Win+")] {
        if h.mods & flag != 0 {
            s.push_str(name);
        }
    }
    s + &key_name(h.vk as u16)
}
