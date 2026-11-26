//! Self-update from GitHub Releases, checked once per program start.
//!
//! `https://github.com/<repo>/releases/latest` redirects to the tag of the newest
//! published release, so reading that redirect gives the latest version without the
//! REST API, JSON or tokens. If it is newer than this build, the release installer
//! is downloaded to %TEMP% and the main process runs it silently.
//!
//! The network work runs in a short-lived child process (`pencil.exe --update-check
//! <hwnd>`): WinHTTP, TLS and their worker threads stay loaded in whatever process
//! uses them, so keeping them out of the long-running process keeps its idle memory
//! at the minimum. The child makes one attempt, posts the result to the main
//! process's control window and exits; the main process schedules retries.
//!
//! Only installed copies update, so development builds never replace themselves.
//! Any failure simply means "no update this time".

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_APP};

use crate::config::read_reg_sz;
use crate::wide;

const HOST: &str = "github.com";
const REPO: &str = "vlazaroes/pencil";
/// Release asset name produced by the release workflow.
const ASSET: &str = "PencilSetup.exe";
/// Must match `AppId` in installer/pencil.iss (Inno Setup appends "_is1").
const UNINSTALL_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{950E1D4D-F50A-497C-846C-8EBDC0033C1E}_is1";
const TEMP_PREFIX: &str = "PencilSetup-";
/// Upper bound for the download, as a guard against unexpected responses.
const MAX_SIZE: usize = 64 * 1024 * 1024;

/// Command line switch that turns the process into a one-shot update checker.
pub const CHECK_ARG: &str = "--update-check";
/// Delay (ms) before each attempt: right after logon the network may not be up yet.
pub const ATTEMPT_DELAYS_MS: [u32; 3] = [15_000, 60_000, 300_000];
/// Posted by the checker: a newer installer is ready at `installer_path()`.
pub const WM_UPDATE_READY: u32 = WM_APP + 3;
/// Posted by the checker: network failure, worth retrying later.
pub const WM_UPDATE_FAILED: u32 = WM_APP + 4;

/// Main process: whether this copy should look for updates (installed copies only).
pub fn enabled() -> bool {
    is_installed_copy()
}

/// Main process: starts one check in a child process that reports back to `notify`.
pub fn spawn_check(notify: *mut c_void) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    // The child handle is dropped (closed) right away; the child runs on its own.
    std::process::Command::new(exe)
        .arg(CHECK_ARG)
        .arg((notify as usize).to_string())
        .spawn()
        .is_ok()
}

/// Child process entry point: one attempt, report, return (and the process exits).
pub fn run_check(notify: Option<&OsStr>) {
    let Some(hwnd) = notify.and_then(|s| s.to_str()?.parse::<usize>().ok()) else { return };
    let hwnd = hwnd as *mut c_void;
    if unsafe { IsWindow(hwnd) } == 0 {
        return;
    }
    remove_old_installers();
    let msg = match check() {
        Check::UpToDate => return,
        Check::Failed => WM_UPDATE_FAILED,
        Check::Ready => WM_UPDATE_READY,
    };
    unsafe { PostMessageW(hwnd, msg, 0, 0) };
}

/// Main process: the downloaded installer, if there is one.
pub fn installer() -> Option<PathBuf> {
    let p = installer_path();
    p.is_file().then_some(p)
}

/// Runs the installer silently. It closes this process, installs and relaunches Pencil.
pub fn run_installer(path: &Path) -> bool {
    std::process::Command::new(path)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .spawn()
        .is_ok()
}

/// Fixed location, so the main process knows what to run without extra IPC.
fn installer_path() -> PathBuf {
    std::env::temp_dir().join(format!("{TEMP_PREFIX}update.exe"))
}

enum Check {
    UpToDate,
    Failed,
    Ready,
}

fn check() -> Check {
    let Some(session) = Handle::session() else { return Check::Failed };
    let tag = match latest_tag(&session, REPO) {
        Err(()) => return Check::Failed,
        Ok(None) => return Check::UpToDate, // no published release yet
        Ok(Some(tag)) => tag,
    };
    let Some(latest) = parse_version(&tag) else { return Check::UpToDate };
    let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));
    if latest <= current {
        return Check::UpToDate;
    }
    let url = format!("/{REPO}/releases/download/{tag}/{ASSET}");
    if download(&session, &url, &installer_path()) {
        Check::Ready
    } else {
        Check::Failed
    }
}

/// Tag of the latest release, read from the `releases/latest` redirect.
/// `Err` = network/HTTP failure; `Ok(None)` = the repository has no release.
fn latest_tag(session: &Handle, repo: &str) -> Result<Option<String>, ()> {
    let req = Request::get(session, &format!("/{repo}/releases/latest"), false).ok_or(())?;
    let status = req.status().ok_or(())?;
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return if status < 500 { Ok(None) } else { Err(()) };
    }
    let Some(location) = req.header(WINHTTP_QUERY_LOCATION) else { return Err(()) };
    // ".../releases/tag/v1.2.3"; without any release it redirects to ".../releases".
    let Some(tag) = location.split("/releases/tag/").nth(1).map(|t| t.trim_end_matches('/')) else {
        return Ok(None);
    };
    let safe = !tag.is_empty() && tag.len() <= 32 && tag.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    Ok(safe.then(|| tag.to_string()))
}

/// Downloads to `<path>.part` and renames on success, so a partial file is never run.
fn download(session: &Handle, url: &str, path: &Path) -> bool {
    let Some(req) = Request::get(session, url, true) else { return false };
    if req.status() != Some(200) {
        return false;
    }
    let part = path.with_extension("part");
    let ok = (|| -> Option<()> {
        let mut file = File::create(&part).ok()?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        loop {
            let mut read = 0u32;
            let ok = unsafe { WinHttpReadData(req.0.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) };
            if ok == 0 {
                return None;
            }
            if read == 0 {
                break;
            }
            // Must be a Windows executable.
            if total == 0 && !buf.starts_with(b"MZ") {
                return None;
            }
            total += read as usize;
            if total > MAX_SIZE {
                return None;
            }
            file.write_all(&buf[..read as usize]).ok()?;
        }
        (total > 0).then_some(())?;
        file.sync_all().ok()?;
        drop(file);
        std::fs::rename(&part, path).ok()
    })()
    .is_some();
    if !ok {
        let _ = std::fs::remove_file(&part);
    }
    ok
}

/// Deletes installers left in %TEMP% by previous updates.
fn remove_old_installers() {
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in dir.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(TEMP_PREFIX) && (name.ends_with(".exe") || name.ends_with(".part")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// "v1.2.3" / "1.2.3" -> (1, 2, 3). Anything else (e.g. pre-release suffixes) -> None.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.strip_prefix('v').unwrap_or(s).split('.').map(|p| p.parse::<u32>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// True when the running executable lives in the folder the installer registered.
fn is_installed_copy() -> bool {
    let Some(dir) = read_reg_sz(UNINSTALL_KEY, "InstallLocation") else { return false };
    let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) else {
        return false;
    };
    let norm = |s: &str| s.trim_end_matches('\\').to_lowercase();
    norm(&dir) == norm(&exe_dir.to_string_lossy())
}

// ---------------------------------------------------------------- WinHTTP

/// Owned WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Handle {
    fn session() -> Option<Handle> {
        let agent = wide(concat!("Pencil/", env!("CARGO_PKG_VERSION")));
        let h = unsafe { WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0) };
        if h.is_null() {
            return None;
        }
        // resolve, connect, send, receive (ms)
        unsafe { WinHttpSetTimeouts(h, 10_000, 10_000, 15_000, 30_000) };
        Some(Handle(h))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// Request plus the connection it depends on (dropped in this field order).
struct Request(Handle, #[allow(dead_code)] Handle);

impl Request {
    fn get(session: &Handle, path: &str, follow_redirects: bool) -> Option<Request> {
        unsafe {
            let host = wide(HOST);
            let conn = WinHttpConnect(session.0, host.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0);
            if conn.is_null() {
                return None;
            }
            let conn = Handle(conn);
            let verb = wide("GET");
            let path = wide(path);
            let req = WinHttpOpenRequest(conn.0, verb.as_ptr(), path.as_ptr(), null(), null(), null(), WINHTTP_FLAG_SECURE);
            if req.is_null() {
                return None;
            }
            let req = Request(Handle(req), conn);
            if !follow_redirects {
                let v: u32 = WINHTTP_DISABLE_REDIRECTS;
                WinHttpSetOption(req.0.0, WINHTTP_OPTION_DISABLE_FEATURE, (&v as *const u32).cast(), 4);
            }
            if WinHttpSendRequest(req.0.0, null(), 0, null(), 0, 0, 0) == 0
                || WinHttpReceiveResponse(req.0.0, null_mut()) == 0
            {
                return None;
            }
            Some(req)
        }
    }

    fn status(&self) -> Option<u32> {
        let mut code: u32 = 0;
        let mut len = 4u32;
        let ok = unsafe {
            WinHttpQueryHeaders(
                self.0.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&mut code as *mut u32).cast(),
                &mut len,
                null_mut(),
            )
        };
        (ok != 0).then_some(code)
    }

    fn header(&self, which: u32) -> Option<String> {
        let mut buf = [0u16; 2048];
        let mut len = std::mem::size_of_val(&buf) as u32;
        let ok = unsafe { WinHttpQueryHeaders(self.0.0, which, null(), buf.as_mut_ptr().cast(), &mut len, null_mut()) };
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize / 2]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network test against a public repo that ships an .exe asset:
    /// `cargo test --release -- --ignored`
    #[test]
    #[ignore]
    fn github_latest_and_download() {
        let session = Handle::session().unwrap();
        let tag = latest_tag(&session, "jqlang/jq").expect("network").expect("latest tag");
        assert!(tag.starts_with("jq-"), "{tag}");
        let path = std::env::temp_dir().join("pencil-updater-test.exe");
        let url = format!("/jqlang/jq/releases/download/{tag}/jq-windows-amd64.exe");
        assert!(download(&session, &url, &path), "download");
        let len = std::fs::metadata(&path).unwrap().len();
        std::fs::remove_file(&path).unwrap();
        assert!(len > 100_000, "{len}");
        // This repo has no releases yet: no tag, no update, no error.
        assert_eq!(latest_tag(&session, REPO), Ok(None));
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert!(parse_version("v1.2.3") > parse_version("v1.2.2"));
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3-beta"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
    }
}
