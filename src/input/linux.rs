//! Linux: grab keyboards with evdev and pass keys on through a uinput
//! virtual keyboard. Works the same on X11 and every Wayland compositor.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, Device, EventType, InputEvent, Key as EvKey};

use super::{numpad, on_key, Injector, SharedEngine};
use crate::chord::{Act, Key, KeyEvent, Mods};

const VIRT_NAME: &str = "hotbar virtual keyboard";

const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;

/// Physical key positions (named after the US layout, as evdev does).
const SYMBOLS: &[(u16, char)] = &[
    (16, 'q'),
    (17, 'w'),
    (18, 'e'),
    (19, 'r'),
    (20, 't'),
    (21, 'y'),
    (22, 'u'),
    (23, 'i'),
    (24, 'o'),
    (25, 'p'),
    (30, 'a'),
    (31, 's'),
    (32, 'd'),
    (33, 'f'),
    (34, 'g'),
    (35, 'h'),
    (36, 'j'),
    (37, 'k'),
    (38, 'l'),
    (39, ';'),
    (40, '\''),
    (44, 'z'),
    (45, 'x'),
    (46, 'c'),
    (47, 'v'),
    (48, 'b'),
    (49, 'n'),
    (50, 'm'),
    (51, ','),
    (52, '.'),
    (53, '/'),
];

fn map(code: u16) -> Key {
    match code {
        KEY_LEFTCTRL | KEY_RIGHTCTRL => Key::Ctrl,
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => Key::Shift,
        KEY_LEFTALT | KEY_RIGHTALT => Key::Alt,
        KEY_LEFTMETA | KEY_RIGHTMETA => Key::Meta,
        2..=10 => Key::Digit((code - 1) as u8),
        79 | 80 | 81 | 75 | 76 | 77 | 71 | 72 | 73 if numpad() => Key::Digit(match code {
            79 => 1,
            80 => 2,
            81 => 3,
            75 => 4,
            76 => 5,
            77 => 6,
            71 => 7,
            72 => 8,
            _ => 9,
        }),
        _ => SYMBOLS
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, ch)| Key::Char(*ch))
            .unwrap_or(Key::Other),
    }
}

fn char_code(c: char) -> u16 {
    SYMBOLS
        .iter()
        .find(|(_, ch)| *ch == c.to_ascii_lowercase())
        .map(|(code, _)| *code)
        .unwrap_or(47)
}

fn is_keyboard(d: &Device) -> bool {
    if d.name() == Some(VIRT_NAME) {
        return false;
    }
    let Some(keys) = d.supported_keys() else {
        return false;
    };
    let typing = keys.contains(EvKey::KEY_A)
        && keys.contains(EvKey::KEY_V)
        && keys.contains(EvKey::KEY_LEFTCTRL);
    // A device that also moves a pointer would lose its mouse events when grabbed.
    typing && d.supported_relative_axes().is_none() && d.supported_absolute_axes().is_none()
}

/// Keyboards we can read (needs the `input` group).
pub fn keyboards() -> Vec<(PathBuf, Device)> {
    evdev::enumerate().filter(|(_, d)| is_keyboard(d)).collect()
}

struct LinuxInjector {
    dev: Mutex<VirtualDevice>,
    /// Keys currently held on the virtual keyboard.
    held: Mutex<HashSet<u16>>,
    hooked: bool,
}

impl LinuxInjector {
    fn emit(&self, code: u16, value: i32) {
        {
            let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
            match value {
                0 => {
                    held.remove(&code);
                }
                1 => {
                    held.insert(code);
                }
                _ => {}
            }
        }
        let mut dev = self.dev.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = dev.emit(&[InputEvent::new(EventType::KEY, code, value)]) {
            crate::log!("uinput write failed: {e}");
        }
    }

    fn is_held(&self, code: u16) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&code)
    }

    /// Without the grab we cannot see the real keyboard state. Wait a moment
    /// for modifiers from the hotkey that launched us to be released.
    fn wait_for_release(&self) {
        if self.hooked {
            return;
        }
        let mods = [
            KEY_LEFTCTRL,
            KEY_RIGHTCTRL,
            KEY_LEFTSHIFT,
            KEY_RIGHTSHIFT,
            KEY_LEFTALT,
            KEY_RIGHTALT,
            KEY_LEFTMETA,
            KEY_RIGHTMETA,
        ];
        let deadline = Instant::now() + Duration::from_millis(600);
        let kbds = keyboards();
        while Instant::now() < deadline {
            let any = kbds.iter().any(|(_, d)| {
                d.get_key_state()
                    .map(|s| mods.iter().any(|m| s.contains(EvKey::new(*m))))
                    .unwrap_or(false)
            });
            if !any {
                return;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

impl Injector for LinuxInjector {
    fn tap(&self, key: char, want: Mods) {
        self.wait_for_release();
        let groups = [
            (want.ctrl, [KEY_LEFTCTRL, KEY_RIGHTCTRL]),
            (want.shift, [KEY_LEFTSHIFT, KEY_RIGHTSHIFT]),
            (want.alt, [KEY_LEFTALT, KEY_RIGHTALT]),
            (want.meta, [KEY_LEFTMETA, KEY_RIGHTMETA]),
        ];
        let mut pre: Vec<(u16, i32)> = Vec::new();
        let mut post: Vec<(u16, i32)> = Vec::new();
        for (wanted, codes) in groups {
            let down: Vec<u16> = codes.iter().copied().filter(|c| self.is_held(*c)).collect();
            if wanted && down.is_empty() {
                pre.push((codes[0], 1));
                post.push((codes[0], 0));
            } else if !wanted && !down.is_empty() {
                for c in down {
                    pre.push((c, 0));
                    post.push((c, 1));
                }
            }
        }
        let code = char_code(key);
        for (c, v) in pre {
            self.emit(c, v);
        }
        self.emit(code, 1);
        self.emit(code, 0);
        for (c, v) in post.into_iter().rev() {
            self.emit(c, v);
        }
    }

    fn replay(&self, ev: &KeyEvent) {
        let value = if ev.down { (ev.extra as i32).max(1) } else { 0 };
        self.emit(ev.raw as u16, value);
    }

    fn hooked(&self) -> bool {
        self.hooked
    }
}

fn virtual_keyboard() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<EvKey>::new();
    // Real keys only: button codes (0x100 to 0x15f) would make this look like a mouse.
    for code in (1u16..=0xff).chain(0x160..=0x2ff) {
        keys.insert(EvKey::new(code));
    }
    let dev = VirtualDeviceBuilder::new()
        .context("cannot open /dev/uinput (run `hotbar setup`)")?
        .name(VIRT_NAME)
        .with_keys(&keys)?
        .build()
        .context("cannot create the virtual keyboard")?;
    Ok(dev)
}

fn run_device(
    path: PathBuf,
    mut dev: Device,
    engine: SharedEngine,
    inj: Arc<LinuxInjector>,
    tx: Sender<Act>,
) {
    // Grabbing while a key is down leaves it stuck in the app that saw the press.
    for _ in 0..300 {
        let idle = dev
            .get_key_state()
            .map(|s| s.iter().next().is_none())
            .unwrap_or(true);
        if idle {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Err(e) = dev.grab() {
        crate::log!("cannot grab {}: {e}", path.display());
        return;
    }
    crate::log!(
        "keyboard: {} ({})",
        dev.name().unwrap_or("?"),
        path.display()
    );
    loop {
        let events: Vec<InputEvent> = match dev.fetch_events() {
            Ok(evs) => evs.collect(),
            Err(e) => {
                crate::log!("keyboard {} gone: {e}", path.display());
                return;
            }
        };
        for ev in events {
            if ev.event_type() != EventType::KEY {
                continue;
            }
            let code = ev.code();
            let value = ev.value();
            let kev = KeyEvent {
                key: map(code),
                down: value != 0,
                raw: code as u32,
                extra: value as u64,
            };
            if !on_key(&engine, inj.as_ref(), &tx, kev) {
                inj.emit(code, value);
            }
        }
    }
}

pub fn start(engine: SharedEngine, tx: Sender<Act>, hook: bool) -> Result<Arc<dyn Injector>> {
    let inj = Arc::new(LinuxInjector {
        dev: Mutex::new(virtual_keyboard()?),
        held: Mutex::new(HashSet::new()),
        hooked: hook,
    });
    // Give the compositor a moment to notice the new device.
    std::thread::sleep(Duration::from_millis(200));
    if !hook {
        return Ok(inj);
    }
    if keyboards().is_empty() {
        anyhow::bail!("no readable keyboards in /dev/input (run `hotbar setup`)");
    }
    let grabbed: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    let watch_inj = inj.clone();
    std::thread::Builder::new()
        .name("keyboards".into())
        .spawn(move || loop {
            for (path, dev) in keyboards() {
                let mut g = grabbed.lock().unwrap_or_else(|e| e.into_inner());
                if g.contains(&path) {
                    continue;
                }
                g.insert(path.clone());
                drop(g);
                let (engine, inj, tx, grabbed) = (
                    engine.clone(),
                    watch_inj.clone(),
                    tx.clone(),
                    grabbed.clone(),
                );
                std::thread::spawn(move || {
                    run_device(path.clone(), dev, engine, inj, tx);
                    grabbed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&path);
                });
            }
            std::thread::sleep(Duration::from_secs(2));
        })?;
    Ok(inj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_table() {
        assert_eq!(map(47), Key::Char('v'));
        assert_eq!(map(46), Key::Char('c'));
        assert_eq!(map(2), Key::Digit(1));
        assert_eq!(map(10), Key::Digit(9));
        assert_eq!(map(73), Key::Digit(9));
        assert_eq!(map(97), Key::Ctrl);
        assert_eq!(char_code('.'), 52);
    }
}
