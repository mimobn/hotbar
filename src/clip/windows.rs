//! Windows clipboard: a message-only window listening for WM_CLIPBOARDUPDATE.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, KillTimer, RegisterClassW,
    SetTimer, TranslateMessage, HWND_MESSAGE, MSG, WM_CLIPBOARDUPDATE, WNDCLASSW,
};

use super::{ClipEvent, Clipboard};
use crate::content::{to_png, Content};

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;

struct Formats {
    png: u32,
    exclude: u32,
    can_include: u32,
    can_upload: u32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn formats() -> &'static Formats {
    static F: OnceLock<Formats> = OnceLock::new();
    F.get_or_init(|| unsafe {
        Formats {
            png: RegisterClipboardFormatW(wide("PNG").as_ptr()),
            exclude: RegisterClipboardFormatW(
                wide("ExcludeClipboardContentFromMonitorProcessing").as_ptr(),
            ),
            can_include: RegisterClipboardFormatW(wide("CanIncludeInClipboardHistory").as_ptr()),
            can_upload: RegisterClipboardFormatW(wide("CanUploadToCloudClipboard").as_ptr()),
        }
    })
}

static OWN_HWND: AtomicIsize = AtomicIsize::new(0);
static SINK: OnceLock<Mutex<Sender<ClipEvent>>> = OnceLock::new();

fn own_hwnd() -> HWND {
    OWN_HWND.load(Ordering::SeqCst) as HWND
}

/// Holds the clipboard open and closes it on drop.
struct Open;

impl Open {
    fn new() -> Result<Open> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(own_hwnd()) } != 0 {
                return Ok(Open);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!("clipboard is busy")
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

unsafe fn read_bytes(fmt: u32) -> Option<Vec<u8>> {
    if IsClipboardFormatAvailable(fmt) == 0 {
        return None;
    }
    let h = GetClipboardData(fmt);
    if h.is_null() {
        return None;
    }
    let size = GlobalSize(h);
    let p = GlobalLock(h) as *const u8;
    if p.is_null() {
        return None;
    }
    let v = std::slice::from_raw_parts(p, size).to_vec();
    GlobalUnlock(h);
    Some(v)
}

unsafe fn read_text() -> Option<String> {
    let b = read_bytes(CF_UNICODETEXT)?;
    let u: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let end = u.iter().position(|&c| c == 0).unwrap_or(u.len());
    Some(String::from_utf16_lossy(&u[..end]))
}

/// Wrap a packed DIB in a BMP file header so the image crate can read it.
fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes(dib[i..i + 4].try_into().unwrap());
    let header = u32_at(0);
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]) as u32;
    let compression = u32_at(16);
    let clr_used = u32_at(32);
    let colors = if clr_used > 0 {
        clr_used
    } else if bit_count <= 8 {
        1 << bit_count
    } else {
        0
    };
    let masks = if header == 40 {
        match compression {
            3 => 12,
            6 => 16,
            _ => 0,
        }
    } else {
        0
    };
    let offset = 14 + header + masks + colors * 4;
    let mut out = Vec::with_capacity(dib.len() + 14);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((dib.len() + 14) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(dib);
    Some(out)
}

/// PNG bytes to a 32 bit bottom-up DIB.
fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut out = Vec::with_capacity(40 + (w * h * 4) as usize);
    for v in [40u32, w] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for v in [0u32, w * h * 4, 2835, 2835, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.get_pixel(x, y).0;
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    Some(out)
}

enum Raw {
    Text(String),
    Png(Vec<u8>),
    Dib(Vec<u8>),
}

unsafe fn read_current() -> Option<ClipEvent> {
    let f = formats();
    // Copy the raw bytes and let go of the clipboard right away; other apps
    // cannot copy while we hold it. Image conversion happens afterwards.
    let (raw, sensitive) = {
        let _open = Open::new().ok()?;
        let mut sensitive = IsClipboardFormatAvailable(f.exclude) != 0;
        if let Some(b) = read_bytes(f.can_include) {
            if b.len() >= 4 && u32::from_le_bytes(b[..4].try_into().unwrap()) == 0 {
                sensitive = true;
            }
        }
        let raw = if let Some(t) = read_text() {
            Raw::Text(t)
        } else if let Some(png) = read_bytes(f.png) {
            Raw::Png(png)
        } else {
            Raw::Dib(read_bytes(CF_DIB)?)
        };
        (raw, sensitive)
    };
    let content = match raw {
        Raw::Text(t) => Content::Text(t),
        Raw::Png(p) => Content::Image(p),
        Raw::Dib(d) => Content::Image(dib_to_bmp(&d).and_then(|b| to_png(&b))?),
    };
    Some(ClipEvent { content, sensitive })
}

const WM_TIMER: u32 = 0x0113;
const READ_TIMER: usize = 7;
/// Wait a little after a change before reading: many apps (anything using
/// OleFlushClipboard) open the clipboard again right after setting it, and
/// reading at that moment makes their copy fail.
const READ_DELAY_MS: u32 = 60;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            // Our own writes are transient (slot pastes and restores): skip them.
            if GetClipboardOwner() != own_hwnd() {
                SetTimer(hwnd, READ_TIMER, READ_DELAY_MS, None);
            }
            0
        }
        WM_TIMER if wp == READ_TIMER => {
            KillTimer(hwnd, READ_TIMER);
            if GetClipboardOwner() != own_hwnd() {
                if let Some(ev) = read_current() {
                    if let Some(s) = SINK.get() {
                        let _ = s.lock().unwrap_or_else(|e| e.into_inner()).send(ev);
                    }
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn set_bytes(fmt: u32, bytes: &[u8]) -> bool {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
    if h.is_null() {
        return false;
    }
    let p = GlobalLock(h) as *mut u8;
    if p.is_null() {
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    GlobalUnlock(h);
    !SetClipboardData(fmt, h as HANDLE).is_null()
}

struct WinClipboard;

impl Clipboard for WinClipboard {
    fn write(&self, c: &Content, transient: bool) -> Result<()> {
        let f = formats();
        // Prepare every format before opening the clipboard, so it stays locked
        // only for the copy itself.
        let formats: Vec<(u32, Vec<u8>)> = match c {
            Content::Text(s) => vec![(
                CF_UNICODETEXT,
                wide(s).iter().flat_map(|u| u.to_le_bytes()).collect(),
            )],
            Content::Image(png) => {
                let mut v = vec![(f.png, png.clone())];
                if let Some(d) = png_to_dib(png) {
                    v.push((CF_DIB, d));
                }
                v
            }
        };
        let _open = Open::new()?;
        unsafe {
            EmptyClipboard();
            let mut ok = false;
            for (fmt, bytes) in &formats {
                ok |= set_bytes(*fmt, bytes);
            }
            if transient {
                set_bytes(f.exclude, &[0]);
                set_bytes(f.can_include, &0u32.to_le_bytes());
                set_bytes(f.can_upload, &0u32.to_le_bytes());
            }
            if !ok {
                bail!("SetClipboardData failed");
            }
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "windows"
    }
}

pub fn start(tx: Sender<ClipEvent>) -> Result<Arc<dyn Clipboard>> {
    let _ = SINK.set(Mutex::new(tx));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || unsafe {
            let hinst = GetModuleHandleW(null());
            let class = wide("hotbar_clipboard");
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: null_mut(),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("hotbar").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                hinst,
                null(),
            );
            if hwnd.is_null() {
                let _ = ready_tx.send(Err("could not create clipboard window".into()));
                return;
            }
            OWN_HWND.store(hwnd as isize, Ordering::SeqCst);
            if AddClipboardFormatListener(hwnd) == 0 {
                let _ = ready_tx.send(Err("AddClipboardFormatListener failed".into()));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            // Record what is on the clipboard right now.
            if let Some(ev) = read_current() {
                if let Some(s) = SINK.get() {
                    let _ = s.lock().unwrap_or_else(|e| e.into_inner()).send(ev);
                }
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Arc::new(WinClipboard)),
        Ok(Err(e)) => bail!(e),
        Err(_) => bail!("clipboard thread died"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dib_roundtrip_through_bmp() {
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let dib = png_to_dib(png.get_ref()).unwrap();
        let back = to_png(&dib_to_bmp(&dib).unwrap()).unwrap();
        let decoded = image::load_from_memory(&back).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(decoded.get_pixel(1, 1).0[..3], [10, 20, 30]);
    }
}
