//! RAII wrappers over GDI resources: everything created here is released in `Drop`,
//! so handles cannot leak even if an initialization fails halfway.

use std::ptr::null_mut;

use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};

use crate::raster::Surface;

/// 32 bpp top-down DIB with direct access to its pixels.
pub struct Dib {
    pub bmp: HBITMAP,
    /// Memory DC with the bitmap selected into it (optional).
    pub dc: HDC,
    old: HGDIOBJ,
    bits: *mut u32,
    pub w: i32,
    pub h: i32,
}

impl Dib {
    pub fn new(w: i32, h: i32, with_dc: bool) -> Option<Dib> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative => rows go top to bottom
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits: *mut core::ffi::c_void = null_mut();
            let bmp = CreateDIBSection(null_mut(), &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                if !bmp.is_null() {
                    DeleteObject(bmp);
                }
                return None;
            }
            let mut dib = Dib { bmp, dc: null_mut(), old: null_mut(), bits: bits.cast(), w, h };
            if with_dc {
                let dc = CreateCompatibleDC(null_mut());
                if dc.is_null() {
                    return None; // `dib` is released by its Drop
                }
                dib.dc = dc;
                dib.old = SelectObject(dc, bmp);
            }
            Some(dib)
        }
    }

    /// Releases the memory DC, keeping only the bitmap (e.g. to hand it to another thread).
    pub fn drop_dc(&mut self) {
        if !self.dc.is_null() {
            unsafe {
                SelectObject(self.dc, self.old);
                DeleteDC(self.dc);
            }
            self.dc = null_mut();
        }
    }

    /// Read-only view of the pixels.
    pub fn pixels(&self) -> &[u32] {
        // SAFETY: as in `surface`; a shared borrow only allows reads.
        unsafe { std::slice::from_raw_parts(self.bits, self.w as usize * self.h as usize) }
    }

    /// Drawing surface over the whole bitmap (global origin at 0,0).
    pub fn surface(&mut self) -> Surface<'_> {
        let len = self.w as usize * self.h as usize;
        // SAFETY: `bits` points to w*h valid pixels for the lifetime of the DIB, and
        // the mutable borrow of `self` guarantees exclusive access.
        let px = unsafe { std::slice::from_raw_parts_mut(self.bits, len) };
        Surface { px, w: self.w, h: self.h, ox: 0, oy: 0 }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.dc.is_null() {
                SelectObject(self.dc, self.old);
                DeleteDC(self.dc);
            }
            DeleteObject(self.bmp);
        }
    }
}
