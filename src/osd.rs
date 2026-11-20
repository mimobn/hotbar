//! Windows on-screen message: a small rounded popup near the bottom of the
//! screen that never takes focus and disappears on its own.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, Once};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SetLayeredWindowAttributes, SetTimer,
    SetWindowPos, SystemParametersInfoW, TranslateMessage, MSG, WNDCLASSW,
};

const CLASS: &str = "hotbar_osd";
const WIDTH: i32 = 380;
const HEIGHT: i32 = 52;
const SHOW_MS: u32 = 1500;

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
const DT_END_ELLIPSIS: u32 = 0x8000;

// COLORREF is 0x00BBGGRR.
const BG: u32 = 0x002e_1e1e;
const FG: u32 = 0x00f4_d6cd;

static TEXT: Mutex<Vec<u16>> = Mutex::new(Vec::new());
static CURRENT: AtomicIsize = AtomicIsize::new(0);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
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
            let face = wide("Segoe UI");
            let font = CreateFontW(-18, 0, 0, 0, 500, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
            let old = SelectObject(hdc, font);
            SetBkMode(hdc, 1);
            SetTextColor(hdc, FG);
            rc.left += 16;
            rc.right -= 16;
            let mut text = TEXT.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let len = text.len().saturating_sub(1) as i32;
            DrawTextW(
                hdc,
                text.as_mut_ptr(),
                len,
                &mut rc,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            SelectObject(hdc, old);
            DeleteObject(font);
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

/// Show `text` for a moment. Returns right away.
pub fn show(text: &str) {
    *TEXT.lock().unwrap_or_else(|e| e.into_inner()) = wide(text);
    // Replace a popup that is still up.
    let old = CURRENT.swap(0, Ordering::SeqCst);
    if old != 0 {
        unsafe {
            PostMessageW(old as HWND, WM_CLOSE, 0, 0);
        }
    }
    std::thread::spawn(|| unsafe {
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
        let x = work.left + (work.right - work.left - WIDTH) / 2;
        let y = work.bottom - HEIGHT - 72;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            class.as_ptr(),
            wide("hotbar").as_ptr(),
            WS_POPUP,
            x,
            y,
            WIDTH,
            HEIGHT,
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
        SetWindowRgn(
            hwnd,
            CreateRoundRectRgn(0, 0, WIDTH + 1, HEIGHT + 1, 18, 18),
            1,
        );
        // SetWindowPos ignores the "start hidden" flag a parent process may pass.
        SetWindowPos(
            hwnd,
            -1isize as HWND,
            x,
            y,
            WIDTH,
            HEIGHT,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        SetTimer(hwnd, 1, SHOW_MS, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}
