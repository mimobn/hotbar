//! Windows: WH_KEYBOARD_LL hook and SendInput.

use std::ptr::{null, null_mut};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL,
};

use super::{numpad, on_key, symbol_key, Injector, SharedEngine};
use crate::chord::{Act, Key, KeyEvent, Mods};

const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_INJECTED: u32 = 0x10;
const WM_KEYDOWN: usize = 0x100;
const WM_SYSKEYDOWN: usize = 0x104;

const VK_LSHIFT: u16 = 0xA0;
const VK_RSHIFT: u16 = 0xA1;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LMENU: u16 = 0xA4;
const VK_RMENU: u16 = 0xA5;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;
/// Unassigned key code, the usual way to stop Win from opening Start.
const VK_MASK: u16 = 0xE8;
/// Marks events we inject, for anyone looking.
const EXTRA_MAGIC: usize = 0x484f_5442;

fn map_vk(vk: u32) -> Key {
    match vk {
        0x10 | 0xA0 | 0xA1 => Key::Shift,
        0x11 | 0xA2 | 0xA3 => Key::Ctrl,
        0x12 | 0xA4 | 0xA5 => Key::Alt,
        0x5B | 0x5C => Key::Meta,
        0x31..=0x39 => Key::Digit((vk - 0x30) as u8),
        0x61..=0x69 if numpad() => Key::Digit((vk - 0x60) as u8),
        0x41..=0x5A => symbol_key((vk as u8) as char),
        0xBA => Key::Char(';'),
        0xBC => Key::Char(','),
        0xBE => Key::Char('.'),
        0xBF => Key::Char('/'),
        _ => Key::Other,
    }
}

fn char_vk(c: char) -> u16 {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c.to_ascii_uppercase() as u16,
        ';' => 0xBA,
        ',' => 0xBC,
        '.' => 0xBE,
        '/' => 0xBF,
        _ => 'V' as u16,
    }
}

fn is_extended(vk: u16) -> bool {
    matches!(vk, VK_RCONTROL | VK_RMENU | VK_LWIN | VK_RWIN)
}

fn key_input(vk: u16, up: bool, scan: Option<u16>, ext: bool) -> INPUT {
    let scan = scan.unwrap_or_else(|| unsafe { MapVirtualKeyW(vk as u32, 0) as u16 });
    let mut flags = if up { KEYEVENTF_KEYUP } else { 0 };
    if ext {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: EXTRA_MAGIC,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

fn held(vk: u16) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0 }
}

struct WinInjector {
    hooked: bool,
}

impl Injector for WinInjector {
    fn tap(&self, key: char, want: Mods) {
        let groups = [
            (want.ctrl, [VK_LCONTROL, VK_RCONTROL]),
            (want.shift, [VK_LSHIFT, VK_RSHIFT]),
            (want.alt, [VK_LMENU, VK_RMENU]),
            (want.meta, [VK_LWIN, VK_RWIN]),
        ];
        let mut pre = Vec::new();
        let mut post = Vec::new();
        for (wanted, vks) in groups {
            let down: Vec<u16> = vks.iter().copied().filter(|&vk| held(vk)).collect();
            if wanted && down.is_empty() {
                pre.push(key_input(vks[0], false, None, is_extended(vks[0])));
                post.push(key_input(vks[0], true, None, is_extended(vks[0])));
            } else if !wanted && !down.is_empty() {
                if vks[0] == VK_LWIN {
                    pre.push(key_input(VK_MASK, false, None, false));
                    pre.push(key_input(VK_MASK, true, None, false));
                }
                for vk in down {
                    pre.push(key_input(vk, true, None, is_extended(vk)));
                    post.push(key_input(vk, false, None, is_extended(vk)));
                }
            }
        }
        let vk = char_vk(key);
        let mut seq = pre;
        seq.push(key_input(vk, false, None, false));
        seq.push(key_input(vk, true, None, false));
        post.reverse();
        seq.extend(post);
        send(&seq);
    }

    fn replay(&self, ev: &KeyEvent) {
        let vk = ev.raw as u16;
        let scan = (ev.extra & 0xFFFF) as u16;
        let ext = (ev.extra >> 32) & 1 != 0;
        send(&[key_input(vk, !ev.down, Some(scan), ext)]);
    }

    fn mask_menu(&self) {
        send(&[key_input(VK_MASK, false, None, false), key_input(VK_MASK, true, None, false)]);
    }

    fn hooked(&self) -> bool {
        self.hooked
    }
}

struct Ctx {
    engine: SharedEngine,
    inj: Arc<WinInjector>,
    tx: Mutex<Sender<Act>>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        if let Some(ctx) = CTX.get() {
            let k = &*(lp as *const KBDLLHOOKSTRUCT);
            // Skip only keys we sent ourselves; other injected input (remote
            // desktop, on-screen keyboards, automation) goes through the engine.
            let ours = k.flags & LLKHF_INJECTED != 0 && k.dwExtraInfo == EXTRA_MAGIC;
            if !ours {
                let down = wp == WM_KEYDOWN || wp == WM_SYSKEYDOWN;
                let ev = KeyEvent {
                    key: map_vk(k.vkCode),
                    down,
                    raw: k.vkCode,
                    extra: (k.scanCode as u64 & 0xFFFF) | (((k.flags & LLKHF_EXTENDED) as u64) << 32),
                };
                let tx = ctx.tx.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if on_key(&ctx.engine, ctx.inj.as_ref(), &tx, ev) {
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

pub fn start(engine: SharedEngine, tx: Sender<Act>, hook_keys: bool) -> Result<Arc<dyn Injector>> {
    let inj = Arc::new(WinInjector { hooked: hook_keys });
    if !hook_keys {
        return Ok(inj);
    }
    if CTX
        .set(Ctx {
            engine,
            inj: inj.clone(),
            tx: Mutex::new(tx),
        })
        .is_err()
    {
        bail!("keyboard hook already started");
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
    std::thread::Builder::new().name("keyboard-hook".into()).spawn(move || unsafe {
        let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), GetModuleHandleW(null()), 0);
        let _ = ready_tx.send(!h.is_null());
        if h.is_null() {
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    })?;
    if !ready_rx.recv().unwrap_or(false) {
        bail!("could not install the keyboard hook");
    }
    Ok(inj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_table() {
        assert_eq!(map_vk(0x56), Key::Char('v'));
        assert_eq!(map_vk(0x31), Key::Digit(1));
        assert_eq!(map_vk(0x39), Key::Digit(9));
        assert_eq!(map_vk(0x69), Key::Digit(9));
        assert_eq!(map_vk(0xA3), Key::Ctrl);
        assert_eq!(char_vk('v'), 0x56);
    }
}
