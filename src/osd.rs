//! Windows on-screen message: a small rounded popup near the bottom of the
//! screen that never takes focus and disappears on its own.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, Once};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, HDC,
    HFONT, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SetLayeredWindowAttributes, SetTimer,
    SetWindowPos, SystemParametersInfoW, TranslateMessage, MSG, WNDCLASSW,
};

const CLASS: &str = "hotbar_osd";

const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_SHOWWINDOW: u32 = 0x0040;
const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_PAINT: u32 = 0x000F;
const WM_TIMER: u32 = 0x0113;
const LWA_ALPHA: u32 = 0x2;
const SPI_GETWORKAREA: u32 = 0x0030;
const DT_CENTER: u32 = 0x1;
const DT_VCENTER: u32 = 0x4;
const DT_SINGLELINE: u32 = 0x20;
const DT_CALCRECT: u32 = 0x400;
const DT_END_ELLIPSIS: u32 = 0x8000;

// COLORREF is 0x00BBGGRR.
const BG: u32 = 0x002e_1e1e;
const FG: u32 = 0x00f4_d6cd;

static TEXT: Mutex<Vec<u16>> = Mutex::new(Vec::new());
static CURRENT: AtomicIsize = AtomicIsize::new(0);
static DPI: AtomicU32 = AtomicU32::new(96);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Draw with real pixels on scaled displays instead of a blurry stretched bitmap.
pub fn init_dpi() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Logical pixels to device pixels.
fn px(v: i32) -> i32 {
    v * DPI.load(Ordering::Relaxed) as i32 / 96
}

unsafe fn font() -> HFONT {
    let face = wide("Segoe UI");
    CreateFontW(-px(17), 0, 0, 0, 500, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr())
}

unsafe fn draw_text(hdc: HDC, rc: &mut RECT, flags: u32) {
    let mut text = TEXT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let len = text.len().saturating_sub(1) as i32;
    DrawTextW(hdc, text.as_mut_ptr(), len, rc, flags | DT_SINGLELINE);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let brush = CreateSolidBrush(BG);
            FillRect(hdc, &rc, brush);
            DeleteObject(brush);
            let f = font();
            let old = SelectObject(hdc, f);
            SetBkMode(hdc, 1);
            SetTextColor(hdc, FG);
            rc.left += px(18);
            rc.right -= px(18);
            draw_text(hdc, &mut rc, DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS);
            SelectObject(hdc, old);
            DeleteObject(f);
            EndPaint(hwnd, &ps);
            0
        }
        WM_TIMER | WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            let _ = CURRENT.compare_exchange(hwnd as isize, 0, Ordering::SeqCst, Ordering::SeqCst);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// How long a message stays up: longer text gets more time to read.
fn duration_ms(text: &str) -> u32 {
    let n = text.chars().count() as u32;
    (1400 + n.saturating_sub(30) * 45).min(6000)
}

/// Show `text` for a moment. Returns right away.
pub fn show(text: &str) {
    *TEXT.lock().unwrap_or_else(|e| e.into_inner()) = wide(text);
    let show_ms = duration_ms(text);
    // Replace a popup that is still up.
    let old = CURRENT.swap(0, Ordering::SeqCst);
    if old != 0 {
        unsafe {
            PostMessageW(old as HWND, WM_CLOSE, 0, 0);
        }
    }
    std::thread::spawn(move || unsafe {
        DPI.store(GetDpiForSystem().max(96), Ordering::Relaxed);
        let hinst = GetModuleHandleW(null());
        let class = wide(CLASS);
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
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
        });

        let mut work: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut RECT as *mut _, 0);

        // Size the popup to its text, within limits.
        let screen = GetDC(null_mut());
        let f = font();
        let old = SelectObject(screen, f);
        let mut measure: RECT = std::mem::zeroed();
        draw_text(screen, &mut measure, DT_CALCRECT);
        SelectObject(screen, old);
        DeleteObject(f);
        ReleaseDC(null_mut(), screen);
        let max_w = (work.right - work.left) - px(80);
        let width = (measure.right - measure.left + px(48)).clamp(px(240), max_w.max(px(240)));
        let height = px(48);

        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.bottom - height - px(72);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            class.as_ptr(),
            wide("hotbar").as_ptr(),
            WS_POPUP,
            x,
            y,
            width,
            height,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        CURRENT.store(hwnd as isize, Ordering::SeqCst);
        SetLayeredWindowAttributes(hwnd, 0, 240, LWA_ALPHA);
        let r = px(18);
        SetWindowRgn(
            hwnd,
            CreateRoundRectRgn(0, 0, width + 1, height + 1, r, r),
            1,
        );
        // SetWindowPos ignores the "start hidden" flag a parent process may pass.
        SetWindowPos(
            hwnd,
            -1isize as HWND,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        SetTimer(hwnd, 1, show_ms, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longer_messages_stay_longer() {
        assert_eq!(duration_ms("Slot 2   empty"), 1400);
        assert!(duration_ms(&"x".repeat(100)) > 4000);
        assert_eq!(duration_ms(&"x".repeat(1000)), 6000);
    }
}
