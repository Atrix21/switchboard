//! Windows clipboard: text (CF_UNICODETEXT), images ("PNG" + CF_DIB), files (CF_HDROP).

use std::io;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::wide;
use crate::clipboard::{Backend, Clip};

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const CF_HDROP: u32 = 15;

struct Hwnd(HWND);
unsafe impl Send for Hwnd {}
unsafe impl Sync for Hwnd {}

pub struct WinClipboard {
    hwnd: Hwnd,
    png: u32,
    drop_effect: u32,
}

pub fn new() -> io::Result<Arc<dyn Backend>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("sb-clipwin".into()).spawn(move || unsafe {
        let cls = wide("SwitchboardClipboard");
        let hinst = GetModuleHandleW(null());
        let wc = WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), hInstance: hinst, lpszClassName: cls.as_ptr(), ..std::mem::zeroed() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(0, cls.as_ptr(), cls.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), hinst, null());
        let _ = tx.send(Hwnd(h));
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    })?;
    let hwnd = rx.recv().map_err(io::Error::other)?;
    if hwnd.0.is_null() {
        return Err(io::Error::other("clipboard window"));
    }
    let png = unsafe { RegisterClipboardFormatW(wide("PNG").as_ptr()) };
    let drop_effect = unsafe { RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr()) };
    Ok(Arc::new(WinClipboard { hwnd, png, drop_effect }))
}

struct Open;
impl Open {
    fn new(h: HWND) -> io::Result<Open> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(h) } != 0 {
                return Ok(Open);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        Err(io::Error::other("clipboard is busy (another app has it open)"))
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

unsafe fn global_bytes(h: HANDLE) -> Option<Vec<u8>> {
    if h.is_null() {
        return None;
    }
    let p = GlobalLock(h) as *const u8;
    if p.is_null() {
        return None;
    }
    let n = GlobalSize(h);
    let v = std::slice::from_raw_parts(p, n).to_vec();
    GlobalUnlock(h);
    Some(v)
}

unsafe fn set_bytes(fmt: u32, b: &[u8]) -> io::Result<()> {
    let h = GlobalAlloc(GMEM_MOVEABLE, b.len().max(1));
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    let p = GlobalLock(h) as *mut u8;
    std::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
    GlobalUnlock(h);
    if SetClipboardData(fmt, h).is_null() {
        GlobalFree(h);
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Backend for WinClipboard {
    fn change_count(&self) -> u64 {
        unsafe { GetClipboardSequenceNumber() as u64 }
    }

    fn read(&self) -> Option<Clip> {
        let _o = Open::new(self.hwnd.0).ok()?;
        unsafe {
            if IsClipboardFormatAvailable(CF_HDROP) != 0 {
                let h = GetClipboardData(CF_HDROP) as HDROP;
                if !h.is_null() {
                    let n = DragQueryFileW(h, u32::MAX, null_mut(), 0);
                    let mut v = vec![];
                    for i in 0..n {
                        let len = DragQueryFileW(h, i, null_mut(), 0);
                        let mut buf = vec![0u16; len as usize + 1];
                        DragQueryFileW(h, i, buf.as_mut_ptr(), len + 1);
                        v.push(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])));
                    }
                    if !v.is_empty() {
                        return Some(Clip::Files(v));
                    }
                }
            }
            if IsClipboardFormatAvailable(self.png) != 0 {
                if let Some(b) = global_bytes(GetClipboardData(self.png)) {
                    if b.starts_with(b"\x89PNG") {
                        return Some(Clip::Png(b));
                    }
                }
            }
            for f in [CF_DIBV5, CF_DIB] {
                if IsClipboardFormatAvailable(f) != 0 {
                    if let Some(png) = global_bytes(GetClipboardData(f)).and_then(|d| dib_to_png(&d)) {
                        return Some(Clip::Png(png));
                    }
                }
            }
            if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
                if let Some(b) = global_bytes(GetClipboardData(CF_UNICODETEXT)) {
                    let w: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                    return Some(Clip::Text(String::from_utf16_lossy(&w)));
                }
            }
        }
        None
    }

    fn write(&self, c: &Clip) -> io::Result<()> {
        let _o = Open::new(self.hwnd.0)?;
        unsafe {
            EmptyClipboard();
            match c {
                Clip::Text(t) => {
                    let w: Vec<u16> = t.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().chain(Some(0)).collect();
                    set_bytes(CF_UNICODETEXT, std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2))
                }
                Clip::Png(p) => {
                    set_bytes(self.png, p)?;
                    if let Some(d) = png_to_dib(p) {
                        set_bytes(CF_DIB, &d)?;
                    }
                    Ok(())
                }
                Clip::Files(fs) => {
                    // DROPFILES { pFiles: u32, pt: POINT, fNC: BOOL, fWide: BOOL } + double-NUL list
                    let mut b = Vec::new();
                    b.extend_from_slice(&20u32.to_le_bytes());
                    b.extend_from_slice(&[0u8; 12]);
                    b.extend_from_slice(&1u32.to_le_bytes());
                    for f in fs {
                        for u in f.to_string_lossy().encode_utf16().chain(Some(0)) {
                            b.extend_from_slice(&u.to_le_bytes());
                        }
                    }
                    b.extend_from_slice(&[0, 0]);
                    set_bytes(CF_HDROP, &b)?;
                    set_bytes(self.drop_effect, &1u32.to_le_bytes()) // DROPEFFECT_COPY
                }
            }
        }
    }
}

/// CF_DIB / CF_DIBV5 (24/32 bpp) -> PNG.
pub fn dib_to_png(d: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |o: usize| d.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    let hsize = u32_at(0)? as usize;
    let w = u32_at(4)? as i32;
    let h = u32_at(8)? as i32;
    let bpp = u16::from_le_bytes(d.get(14..16)?.try_into().ok()?);
    let comp = u32_at(16)?;
    let clr_used = u32_at(32)? as usize;
    if w <= 0 || h == 0 || !(bpp == 24 || bpp == 32) || !(comp == 0 || comp == 3) {
        return None;
    }
    let masks = if comp == 3 && hsize == 40 { 12 } else { 0 };
    let off = hsize + masks + clr_used * 4;
    let stride = ((w as usize * bpp as usize + 31) / 32) * 4;
    let ah = h.unsigned_abs() as usize;
    if d.len() < off + stride * ah {
        return None;
    }
    let bytes = (bpp / 8) as usize;
    let mut rgba = vec![0u8; w as usize * ah * 4];
    let mut any_alpha = false;
    for y in 0..ah {
        let src_row = if h > 0 { ah - 1 - y } else { y };
        let row = &d[off + src_row * stride..];
        for x in 0..w as usize {
            let p = &row[x * bytes..];
            let o = (y * w as usize + x) * 4;
            rgba[o] = p[2];
            rgba[o + 1] = p[1];
            rgba[o + 2] = p[0];
            rgba[o + 3] = if bytes == 4 { p[3] } else { 255 };
            any_alpha |= bytes == 4 && p[3] != 0;
        }
    }
    if !any_alpha {
        // 32-bpp BI_RGB with an unused (zero) alpha channel: opaque.
        rgba.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, ah as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header().ok()?;
        wr.write_image_data(&rgba).ok()?;
    }
    Some(out)
}

/// PNG -> 32-bpp top-down CF_DIB.
pub fn png_to_dib(p: &[u8]) -> Option<Vec<u8>> {
    let mut dec = png::Decoder::new(p);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0u8; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        _ => return None,
    };
    let mut d = Vec::with_capacity(40 + w * h * 4);
    d.extend_from_slice(&40u32.to_le_bytes());
    d.extend_from_slice(&(w as i32).to_le_bytes());
    d.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&32u16.to_le_bytes());
    d.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    d.extend_from_slice(&((w * h * 4) as u32).to_le_bytes());
    d.extend_from_slice(&[0u8; 16]);
    for y in 0..h {
        for x in 0..w {
            let s = &buf[(y * w + x) * ch..];
            let (r, g, b, a) = match ch {
                4 => (s[0], s[1], s[2], s[3]),
                3 => (s[0], s[1], s[2], 255),
                2 => (s[0], s[0], s[0], s[1]),
                _ => (s[0], s[0], s[0], 255),
            };
            d.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dib_png_roundtrip() {
        // 2x2 image through PNG -> DIB -> PNG -> DIB.
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 10, 20, 30, 128];
        let mut png0 = Vec::new();
        {
            let mut e = png::Encoder::new(&mut png0, 2, 2);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
        let dib = super::png_to_dib(&png0).unwrap();
        let png1 = super::dib_to_png(&dib).unwrap();
        assert_eq!(super::png_to_dib(&png1).unwrap(), dib);
    }
}
