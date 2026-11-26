//! Screenshots: selection veil, screen grab, clipboard and PNG.
//!
//! The screen is grabbed as displayed, so the strokes come out exactly as drawn. The
//! selection veil is excluded from capture (`WDA_EXCLUDEFROMCAPTURE`) and the overlay
//! removes its toolbar and notice for the instant of the grab, so neither reaches
//! the image.
//!
//! Memory: the grabbed pixels stay in their DIB section (no copy) until the PNG is
//! written, the PNG is filtered, compressed and written row by row, and the
//! clipboard copy is a system bitmap held by Windows. Once the PNG is saved, the
//! process is back to where it was before the capture.

use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Mutex;

use miniz_oxide::deflate::core::{compress, create_comp_flags_from_zip_params, CompressorOxide, TDEFLFlush, TDEFLStatus};
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, SYSTEMTIME};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CombineRgn, CreateCompatibleBitmap, CreateCompatibleDC, CreateRectRgn, DeleteDC,
    DeleteObject, GetDC, GetStockObject, ReleaseDC, SelectObject, SetWindowRgn, BLACK_BRUSH,
    CAPTUREBLT, RGN_DIFF, SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::read_reg_sz;
use crate::gdi::Dib;
use crate::raster::Rect;
use crate::wide;

const VEIL_CLASS: &str = "PencilVeil";
/// Darkness of the area outside the selection (0-255).
const VEIL_ALPHA: u8 = 90;
/// Standard clipboard format for a bitmap handle.
const CF_BITMAP: u32 = 2;
/// Where Windows keeps the (possibly relocated, e.g. OneDrive) Pictures folder.
/// Read from the registry: the Known Folder API would load the shell storage stack
/// (several MB) into this long-running process for good.
const SHELL_FOLDERS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders";

// ---------------------------------------------------------------- veil

pub fn register_class(hinst: HINSTANCE) -> bool {
    unsafe {
        let class = wide(VEIL_CLASS);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = hinst;
        wc.lpfnWndProc = Some(DefWindowProcW);
        wc.lpszClassName = class.as_ptr();
        wc.hbrBackground = GetStockObject(BLACK_BRUSH);
        RegisterClassExW(&wc) != 0
    }
}

/// Translucent black layer over the screen while selecting, with the selection cut
/// out of its window region. Uses no pixel buffer of its own (constant alpha), lets
/// the mouse through to the overlay, and is never focused nor captured.
pub struct Veil {
    hwnd: HWND,
    w: i32,
    h: i32,
}

impl Veil {
    /// `owner` keeps it above the overlay; `(x, y, w, h)` is the virtual screen.
    pub fn new(hinst: HINSTANCE, owner: HWND, x: i32, y: i32, w: i32, h: i32) -> Option<Veil> {
        unsafe {
            let class = wide(VEIL_CLASS);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP,
                x,
                y,
                w,
                h,
                owner,
                null_mut(),
                hinst,
                null_mut(),
            );
            if hwnd.is_null() {
                return None;
            }
            SetLayeredWindowAttributes(hwnd, 0, VEIL_ALPHA, LWA_ALPHA);
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Some(Veil { hwnd, w, h })
        }
    }

    /// Cuts `hole` (client coordinates) out of the veil; `None` covers everything.
    pub fn set_hole(&self, hole: Option<Rect>) {
        unsafe {
            let rgn = CreateRectRgn(0, 0, self.w, self.h);
            if rgn.is_null() {
                return;
            }
            if let Some(r) = hole.filter(|r| !r.is_empty()) {
                let cut = CreateRectRgn(r.l, r.t, r.r, r.b);
                if !cut.is_null() {
                    CombineRgn(rgn, rgn, cut, RGN_DIFF);
                    DeleteObject(cut);
                }
            }
            // On success the system owns the region.
            if SetWindowRgn(self.hwnd, rgn, 1) == 0 {
                DeleteObject(rgn);
            }
        }
    }
}

impl Drop for Veil {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.hwnd) };
    }
}

// ---------------------------------------------------------------- grab

/// Screenshot pixels (0x??RRGGBB, alpha undefined, top-down rows) kept in the DIB
/// section they were grabbed into. Released with the DIB, outside the heap.
pub struct Image {
    dib: Dib,
}

// SAFETY: the DIB has no device context left (see `Image::grab`), and a bitmap
// handle and its memory may be used and freed from any thread.
unsafe impl Send for Image {}

impl Image {
    /// Bitmap to grab into. Created ahead of time so that, while the toolbar is
    /// hidden for the grab, nothing but the copy itself happens.
    pub fn target(w: i32, h: i32) -> Option<Dib> {
        Dib::new(w, h, true)
    }

    /// Copies the screen area at `(x, y)` (screen coordinates, size of `dib`) as it is
    /// displayed. With DWM, reading the screen already waits for the composed frame
    /// that includes the latest window updates (measured: an extra `DwmFlush` only
    /// added a frame, and the hidden toolbar never leaked without it).
    pub fn grab(mut dib: Dib, x: i32, y: i32) -> Option<Image> {
        unsafe {
            let screen = GetDC(null_mut());
            // CAPTUREBLT includes other layered windows (menus, tooltips...).
            let ok = BitBlt(dib.dc, 0, 0, dib.w, dib.h, screen, x, y, SRCCOPY | CAPTUREBLT) != 0;
            ReleaseDC(null_mut(), screen);
            if !ok {
                return None;
            }
        }
        dib.drop_dc();
        Some(Image { dib })
    }

    pub fn w(&self) -> i32 {
        self.dib.w
    }

    pub fn h(&self) -> i32 {
        self.dib.h
    }

    pub fn px(&self) -> &[u32] {
        self.dib.pixels()
    }
}

// ---------------------------------------------------------------- clipboard

/// Puts the image on the clipboard. It is handed over as a system bitmap
/// (`CF_BITMAP`): Windows then keeps the data on its side and converts it to
/// whatever format the pasting app asks for, instead of the copy living inside this
/// process (as a `CF_DIB` memory block would) until something else is copied.
pub fn to_clipboard(owner: HWND, img: &mut Image) -> bool {
    let (w, h) = (img.w(), img.h());
    // Screen pixels carry an undefined alpha byte; some apps read it when pasting.
    for p in img.dib.surface().px.iter_mut() {
        *p |= 0xFF00_0000;
    }
    unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return false;
        }
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let (dst, src) = (CreateCompatibleDC(screen), CreateCompatibleDC(screen));
        let copied = !bmp.is_null() && !dst.is_null() && !src.is_null() && {
            let old_dst = SelectObject(dst, bmp);
            let old_src = SelectObject(src, img.dib.bmp);
            let ok = BitBlt(dst, 0, 0, w, h, src, 0, 0, SRCCOPY) != 0;
            SelectObject(dst, old_dst);
            SelectObject(src, old_src);
            ok
        };
        for dc in [dst, src] {
            if !dc.is_null() {
                DeleteDC(dc);
            }
        }
        ReleaseDC(null_mut(), screen);
        if !copied {
            if !bmp.is_null() {
                DeleteObject(bmp);
            }
            return false;
        }

        // Another app may hold the clipboard for a moment.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(owner) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !opened {
            DeleteObject(bmp);
            return false;
        }
        EmptyClipboard();
        // On success the system owns the bitmap.
        let ok = !SetClipboardData(CF_BITMAP, bmp).is_null();
        if !ok {
            DeleteObject(bmp);
        }
        CloseClipboard();
        ok
    }
}

// ---------------------------------------------------------------- PNG file

/// Outcome of one screenshot, reported once its PNG save has finished.
pub struct Saved {
    /// Whether the image reached the clipboard.
    pub copied: bool,
    /// The PNG file, or `None` if saving failed.
    pub path: Option<PathBuf>,
}

static SAVED: Mutex<Vec<Saved>> = Mutex::new(Vec::new());

/// Encodes and saves the PNG on a background thread, then posts `msg` to `notify`.
pub fn save_async(img: Image, copied: bool, notify: HWND, msg: u32) {
    let notify = notify as usize; // HWND is not Send; the value is just an id
    let report = move |path| {
        SAVED.lock().unwrap_or_else(|e| e.into_inner()).push(Saved { copied, path });
        unsafe { PostMessageW(notify as *mut c_void, msg, 0, 0) };
    };
    let spawned = std::thread::Builder::new().name("png".into()).stack_size(256 * 1024).spawn(move || {
        let path = save_png(&img);
        drop(img); // free the pixels before notifying
        report(path);
    });
    if spawned.is_err() {
        report(None);
    }
}

/// Results of finished saves, oldest first.
pub fn take_saved() -> Vec<Saved> {
    std::mem::take(&mut *SAVED.lock().unwrap_or_else(|e| e.into_inner()))
}

fn save_png(img: &Image) -> Option<PathBuf> {
    let dir = pictures_dir()?.join("Pencil");
    std::fs::create_dir_all(&dir).ok()?;
    let t = unsafe {
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    let stem = format!(
        "Pencil {:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let mut path = dir.join(format!("{stem}.png"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).png"));
        n += 1;
    }
    let written = File::create(&path).ok().and_then(|f| {
        let mut out = BufWriter::with_capacity(64 * 1024, f);
        write_png(&mut out, img.w() as usize, img.h() as usize, img.px()).ok()?;
        out.into_inner().ok()?.sync_all().ok()
    });
    if written.is_none() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

/// The user's Pictures folder (follows OneDrive or custom locations).
fn pictures_dir() -> Option<PathBuf> {
    // REG_EXPAND_SZ like "%USERPROFILE%\Pictures"; read_reg_sz expands it.
    read_reg_sz(SHELL_FOLDERS, "My Pictures")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| Path::new(&p).join("Pictures")))
}

/// Streaming PNG encoder: 8-bit RGB, adaptive per-row filters, zlib via miniz_oxide.
/// Each row is filtered and compressed as it goes and every compressed block is
/// written as its own IDAT chunk, so memory stays at a few rows plus the compressor.
fn write_png(out: &mut impl Write, w: usize, h: usize, px: &[u32]) -> std::io::Result<()> {
    out.write_all(b"\x89PNG\r\n\x1a\n")?;
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&(w as u32).to_be_bytes());
    ihdr[4..8].copy_from_slice(&(h as u32).to_be_bytes());
    ihdr[8..].copy_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolor, deflate, adaptive, no interlace
    chunk(out, b"IHDR", &ihdr)?;

    // window_bits > 0 => zlib header and checksum, as PNG requires.
    let mut z = CompressorOxide::new(create_comp_flags_from_zip_params(6, 15, 0));
    let mut zbuf = vec![0u8; 64 * 1024];
    let stride = w * 3;
    let mut prev = vec![0u8; stride];
    let mut cur = vec![0u8; stride];
    // Filter type byte followed by the filtered row, for each of the 5 filters.
    let mut cand: [Vec<u8>; 5] = std::array::from_fn(|f| {
        let mut v = vec![0u8; stride + 1];
        v[0] = f as u8;
        v
    });
    for y in 0..h {
        for (x, p) in px[y * w..(y + 1) * w].iter().enumerate() {
            cur[x * 3] = (p >> 16) as u8;
            cur[x * 3 + 1] = (p >> 8) as u8;
            cur[x * 3 + 2] = *p as u8;
        }
        for i in 0..stride {
            let a = if i >= 3 { cur[i - 3] } else { 0 };
            let b = prev[i];
            let c = if i >= 3 { prev[i - 3] } else { 0 };
            let x = cur[i];
            cand[0][i + 1] = x;
            cand[1][i + 1] = x.wrapping_sub(a);
            cand[2][i + 1] = x.wrapping_sub(b);
            cand[3][i + 1] = x.wrapping_sub(((a as u16 + b as u16) / 2) as u8);
            cand[4][i + 1] = x.wrapping_sub(paeth(a, b, c));
        }
        // Keep the filter with the smallest sum of magnitudes (standard heuristic).
        let score = |v: &[u8]| v[1..].iter().map(|&b| (b as i8).unsigned_abs() as u32).sum::<u32>();
        let best = (0..5).min_by_key(|&f| score(&cand[f])).unwrap_or(0);
        deflate(&mut z, &cand[best], &mut zbuf, TDEFLFlush::None, out)?;
        std::mem::swap(&mut prev, &mut cur);
    }
    deflate(&mut z, &[], &mut zbuf, TDEFLFlush::Finish, out)?;
    chunk(out, b"IEND", &[])
}

/// Feeds `input` to the compressor and writes whatever comes out as IDAT chunks.
fn deflate(
    z: &mut CompressorOxide,
    mut input: &[u8],
    zbuf: &mut [u8],
    flush: TDEFLFlush,
    out: &mut impl Write,
) -> std::io::Result<()> {
    loop {
        let (status, used, produced) = compress(z, input, zbuf, flush);
        if produced > 0 {
            chunk(out, b"IDAT", &zbuf[..produced])?;
        }
        input = &input[used..];
        match status {
            TDEFLStatus::Done => return Ok(()),
            TDEFLStatus::Okay if flush == TDEFLFlush::None && input.is_empty() => return Ok(()),
            TDEFLStatus::Okay => {}
            _ => return Err(std::io::Error::other("deflate failed")),
        }
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn chunk(out: &mut impl Write, kind: &[u8; 4], data: &[u8]) -> std::io::Result<()> {
    out.write_all(&(data.len() as u32).to_be_bytes())?;
    out.write_all(kind)?;
    out.write_all(data)?;
    let crc = crc32(crc32(0xFFFF_FFFF, kind), data) ^ 0xFFFF_FFFF;
    out.write_all(&crc.to_be_bytes())
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
};

fn crc32(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_png_spec() {
        // CRC of the IEND chunk type is a well-known constant.
        assert_eq!(crc32(0xFFFF_FFFF, b"IEND") ^ 0xFFFF_FFFF, 0xAE42_6082);
    }

    /// Encodes an image, then parses the chunks back, checks every CRC and inflates
    /// all IDAT data to verify the exact filtered pixel bytes.
    #[test]
    fn png_roundtrip() {
        let (w, h) = (37, 23);
        let px: Vec<u32> = (0..w * h).map(|i| (i as u32).wrapping_mul(2_654_435_761) & 0xFF_FFFF).collect();
        let mut png = Vec::new();
        write_png(&mut png, w, h, &px).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");

        let (mut pos, mut zdata, mut kinds) = (8, Vec::new(), Vec::new());
        while pos < png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &png[pos + 4..pos + 8];
            let data = &png[pos + 8..pos + 8 + len];
            let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32(crc32(0xFFFF_FFFF, kind), data) ^ 0xFFFF_FFFF);
            if kind == b"IDAT" {
                zdata.extend_from_slice(data);
            }
            kinds.push(kind.to_vec());
            pos += 12 + len;
        }
        assert_eq!(kinds.first().unwrap(), b"IHDR");
        assert_eq!(kinds.last().unwrap(), b"IEND");

        // Undo the filters and compare with the source pixels.
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&zdata).unwrap();
        let stride = w * 3;
        assert_eq!(raw.len(), h * (stride + 1));
        let mut prev = vec![0u8; stride];
        for y in 0..h {
            let f = raw[y * (stride + 1)];
            let row = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
            let mut cur = vec![0u8; stride];
            for i in 0..stride {
                let a = if i >= 3 { cur[i - 3] } else { 0 };
                let b = prev[i];
                let c = if i >= 3 { prev[i - 3] } else { 0 };
                let pred = match f {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((a as u16 + b as u16) / 2) as u8,
                    4 => paeth(a, b, c),
                    _ => panic!("bad filter {f}"),
                };
                cur[i] = row[i].wrapping_add(pred);
            }
            for x in 0..w {
                let p = px[y * w + x];
                assert_eq!(cur[x * 3..x * 3 + 3], [(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
            prev = cur;
        }
    }
}
