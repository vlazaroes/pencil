//! Screenshot shutter sound, embedded in the executable (no external files).
//!
//! It plays in a short-lived child process (`pencil.exe --play-shutter`): the first
//! sound a process plays loads the Windows audio stack (about 1 MB and dozens of
//! handles) and keeps it loaded for good, so this keeps the long-running process at
//! its minimum. The child plays the sound and exits, releasing everything.

use std::ptr::null_mut;

use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT};

/// Command line switch that turns the process into a one-shot sound player.
pub const PLAY_ARG: &str = "--play-shutter";

/// 16-bit PCM, mono, 22050 Hz WAV (~0.5 s, 23 KB), played natively by Windows.
static SHUTTER: &[u8] = include_bytes!("../assets/screenshot.wav");

/// Main process: plays the shutter sound without blocking.
pub fn shutter() {
    crate::process::spawn_self(&[PLAY_ARG]);
}

/// Child process entry point: plays the sound to the end, then returns (and the
/// process exits). `SND_MEMORY` reads the WAV straight from the executable's image;
/// `SND_NODEFAULT` stays silent if it cannot play.
pub fn play_and_exit() {
    unsafe { PlaySoundW(SHUTTER.as_ptr().cast(), null_mut(), SND_MEMORY | SND_NODEFAULT) };
}
