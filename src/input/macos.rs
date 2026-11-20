//! macOS: a CGEventTap sees and swallows keys, CGEventPost sends them.
//! Needs the Accessibility and Input Monitoring permissions.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Result};

use super::{numpad, on_key, Injector, SharedEngine};
use crate::chord::{Act, Key, KeyEvent, Mods};

type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type TapCallback = unsafe extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventCreateKeyboardEvent(source: *mut c_void, keycode: u16, down: bool) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn AXIsProcessTrusted() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        alloc: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
}

const HID_TAP: u32 = 0;
const SESSION_TAP: u32 = 1;
const HEAD_INSERT: u32 = 0;
const OPTION_DEFAULT: u32 = 0;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const TAP_DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_USER: u32 = 0xFFFF_FFFF;
const FIELD_KEYCODE: u32 = 9;
const FIELD_USER_DATA: u32 = 42;
const MARK: i64 = 0x484f_5442;

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CTRL: u64 = 0x0004_0000;
const FLAG_ALT: u64 = 0x0008_0000;
const FLAG_CMD: u64 = 0x0010_0000;

/// ANSI layout key codes.
const SYMBOLS: &[(u16, char)] = &[
    (0, 'a'),
    (1, 's'),
    (2, 'd'),
    (3, 'f'),
    (4, 'h'),
    (5, 'g'),
    (6, 'z'),
    (7, 'x'),
    (8, 'c'),
    (9, 'v'),
    (11, 'b'),
    (12, 'q'),
    (13, 'w'),
    (14, 'e'),
    (15, 'r'),
    (16, 'y'),
    (17, 't'),
    (31, 'o'),
    (32, 'u'),
    (34, 'i'),
    (35, 'p'),
    (37, 'l'),
    (38, 'j'),
    (39, '\''),
    (40, 'k'),
    (41, ';'),
    (43, ','),
    (44, '/'),
    (45, 'n'),
    (46, 'm'),
    (47, '.'),
];
const DIGITS: &[(u16, u8)] = &[
    (18, 1),
    (19, 2),
    (20, 3),
    (21, 4),
    (23, 5),
    (22, 6),
    (26, 7),
    (28, 8),
    (25, 9),
];
const KEYPAD: &[(u16, u8)] = &[
    (83, 1),
    (84, 2),
    (85, 3),
    (86, 4),
    (87, 5),
    (88, 6),
    (89, 7),
    (91, 8),
    (92, 9),
];

fn modifier_flag(code: u16) -> Option<(Key, u64)> {
    match code {
        54 | 55 => Some((Key::Meta, FLAG_CMD)),
        56 | 60 => Some((Key::Shift, FLAG_SHIFT)),
        58 | 61 => Some((Key::Alt, FLAG_ALT)),
        59 | 62 => Some((Key::Ctrl, FLAG_CTRL)),
        _ => None,
    }
}

fn map(code: u16) -> Key {
    if let Some((k, _)) = modifier_flag(code) {
        return k;
    }
    if let Some((_, d)) = DIGITS.iter().find(|(c, _)| *c == code) {
        return Key::Digit(*d);
    }
    if numpad() {
        if let Some((_, d)) = KEYPAD.iter().find(|(c, _)| *c == code) {
            return Key::Digit(*d);
        }
    }
    SYMBOLS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, ch)| Key::Char(*ch))
        .unwrap_or(Key::Other)
}

fn char_code(c: char) -> u16 {
    SYMBOLS
        .iter()
        .find(|(_, ch)| *ch == c.to_ascii_lowercase())
        .map(|(code, _)| *code)
        .unwrap_or(9)
}

fn flags_for(m: Mods) -> u64 {
    let mut f = 0;
    if m.shift {
        f |= FLAG_SHIFT;
    }
    if m.ctrl {
        f |= FLAG_CTRL;
    }
    if m.alt {
        f |= FLAG_ALT;
    }
    if m.meta {
        f |= FLAG_CMD;
    }
    f
}

unsafe fn post(code: u16, down: bool, flags: u64) {
    let ev = CGEventCreateKeyboardEvent(null_mut(), code, down);
    if ev.is_null() {
        return;
    }
    CGEventSetFlags(ev, flags);
    CGEventSetIntegerValueField(ev, FIELD_USER_DATA, MARK);
    CGEventPost(HID_TAP, ev);
    CFRelease(ev as *const c_void);
}

struct MacInjector {
    hooked: bool,
}

impl Injector for MacInjector {
    fn tap(&self, key: char, mods: Mods) {
        let code = char_code(key);
        let flags = flags_for(mods);
        unsafe {
            post(code, true, flags);
            post(code, false, flags);
        }
    }

    fn replay(&self, ev: &KeyEvent) {
        unsafe {
            post(ev.raw as u16, ev.down, ev.extra);
        }
    }

    fn hooked(&self) -> bool {
        self.hooked
    }
}

struct Ctx {
    engine: SharedEngine,
    inj: Arc<MacInjector>,
    tx: Mutex<Sender<Act>>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static TAP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

unsafe extern "C" fn callback(
    _proxy: *mut c_void,
    etype: u32,
    event: CGEventRef,
    _user: *mut c_void,
) -> CGEventRef {
    if etype == TAP_DISABLED_TIMEOUT || etype == TAP_DISABLED_USER {
        let tap = TAP.load(Ordering::SeqCst);
        if !tap.is_null() {
            CGEventTapEnable(tap, true);
        }
        return event;
    }
    if event.is_null() || CGEventGetIntegerValueField(event, FIELD_USER_DATA) == MARK {
        return event;
    }
    let Some(ctx) = CTX.get() else {
        return event;
    };
    let code = CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16;
    let flags = CGEventGetFlags(event);
    let down = match etype {
        KEY_DOWN => true,
        KEY_UP => false,
        FLAGS_CHANGED => match modifier_flag(code) {
            Some((_, bit)) => flags & bit != 0,
            None => return event,
        },
        _ => return event,
    };
    let kev = KeyEvent {
        key: map(code),
        down,
        raw: code as u32,
        extra: flags,
    };
    let tx = ctx.tx.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if on_key(&ctx.engine, ctx.inj.as_ref(), &tx, kev) {
        return null_mut();
    }
    event
}

pub fn permissions() -> (bool, bool) {
    unsafe { (AXIsProcessTrusted(), CGPreflightListenEventAccess()) }
}

pub fn start(engine: SharedEngine, tx: Sender<Act>, hook: bool) -> Result<Arc<dyn Injector>> {
    let inj = Arc::new(MacInjector { hooked: hook });
    if !hook {
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
        bail!("event tap already started");
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
    std::thread::Builder::new()
        .name("event-tap".into())
        .spawn(move || unsafe {
            if !CGPreflightListenEventAccess() {
                CGRequestListenEventAccess();
            }
            let mask = (1u64 << KEY_DOWN) | (1u64 << KEY_UP) | (1u64 << FLAGS_CHANGED);
            let tap = CGEventTapCreate(
                SESSION_TAP,
                HEAD_INSERT,
                OPTION_DEFAULT,
                mask,
                callback,
                null_mut(),
            );
            if tap.is_null() {
                let _ = ready_tx.send(false);
                return;
            }
            TAP.store(tap, Ordering::SeqCst);
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            let _ = ready_tx.send(true);
            CFRunLoopRun();
        })?;
    if !ready_rx.recv().unwrap_or(false) {
        bail!("macOS refused the event tap: allow hotbar in Privacy & Security > Input Monitoring and Accessibility (run `hotbar setup`)");
    }
    Ok(inj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_table() {
        assert_eq!(map(9), Key::Char('v'));
        assert_eq!(map(18), Key::Digit(1));
        assert_eq!(map(25), Key::Digit(9));
        assert_eq!(map(55), Key::Meta);
        assert_eq!(
            flags_for(Mods {
                meta: true,
                ..Mods::default()
            }),
            FLAG_CMD
        );
    }
}
