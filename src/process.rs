//! Launching helper processes (update check, shutter sound, installer, explorer).
//!
//! `CreateProcessW` is called directly instead of `std::process::Command` for two
//! reasons:
//! - `STARTF_FORCEOFFFEEDBACK`: otherwise Windows shows the "app starting" busy
//!   cursor until the new process opens a window, which these background helpers
//!   never do, so the cursor would spin for seconds for no visible reason.
//! - The program is always given as a full path: nothing is looked up in the
//!   search path, so a same-named executable placed elsewhere is never picked.

use std::path::{Path, PathBuf};
use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, WaitForSingleObject, PROCESS_INFORMATION, STARTF_FORCEOFFFEEDBACK,
    STARTUPINFOW,
};

use crate::wide;

/// A started child process; the handle is closed on drop.
pub struct Child(HANDLE);

impl Child {
    /// Waits up to `ms` for the process to end and returns its exit code, or
    /// `None` if it is still running.
    pub fn exit_code(&self, ms: u32) -> Option<u32> {
        let mut code = 0u32;
        unsafe {
            (WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 && GetExitCodeProcess(self.0, &mut code) != 0)
                .then_some(code)
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Starts `program` (a full path) with `args` and returns at once; the child runs
/// on its own and its handles are closed right away.
pub fn spawn_quiet(program: &Path, args: &[&str]) -> bool {
    spawn(program, args).is_some()
}

/// Starts this same executable with `args` (helper modes such as `--play-shutter`).
pub fn spawn_self(args: &[&str]) -> bool {
    spawn_self_child(args).is_some()
}

/// Like `spawn_self`, but keeps the process handle to read its exit code later.
pub fn spawn_self_child(args: &[&str]) -> Option<Child> {
    spawn(&std::env::current_exe().ok()?, args)
}

fn spawn(program: &Path, args: &[&str]) -> Option<Child> {
    let mut cmd = quote(&program.to_string_lossy());
    for a in args {
        cmd.push(' ');
        cmd.push_str(&quote(a));
    }
    let app = wide(&program.to_string_lossy());
    let mut cmd = wide(&cmd); // CreateProcessW may write to the command line buffer
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_FORCEOFFFEEDBACK;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessW(app.as_ptr(), cmd.as_mut_ptr(), null(), null(), 0, 0, null(), null(), &si, &mut pi) == 0 {
            return None;
        }
        CloseHandle(pi.hThread);
        Some(Child(pi.hProcess))
    }
}

/// `%SystemRoot%\explorer.exe`, resolved from the system rather than the search path.
pub fn explorer() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let n = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])).join("explorer.exe"))
}

/// Quotes an argument containing spaces. Only paths and fixed switches are passed,
/// which never contain `"` nor end in a backslash, so no further escaping is needed.
fn quote(s: &str) -> String {
    if s.is_empty() || s.contains([' ', '\t']) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_system_explorer() {
        let e = explorer().expect("windows directory");
        assert!(e.is_file(), "{}", e.display());
    }

    #[test]
    fn spawns_a_process() {
        let cmd = explorer().unwrap().with_file_name("System32").join("cmd.exe");
        assert!(spawn_quiet(&cmd, &["/c", "exit"]));
    }

    #[test]
    fn reads_the_exit_code() {
        let cmd = explorer().unwrap().with_file_name("System32").join("cmd.exe");
        let child = spawn(&cmd, &["/c", "exit", "7"]).expect("spawn");
        assert_eq!(child.exit_code(10_000), Some(7));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("--play-shutter"), "--play-shutter");
        assert_eq!(quote(r"C:\Users\A B\x.png"), r#""C:\Users\A B\x.png""#);
        assert_eq!(quote(""), "\"\"");
    }
}
