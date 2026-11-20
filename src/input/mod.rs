//! Keyboard hooks and key injection.
//!
//! Each platform module turns native key events into [`KeyEvent`]s, passes
//! them through [`on_key`], and provides an [`Injector`] to send keys.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::chord::{Act, Engine, KeyEvent, Mods};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub type SharedEngine = Arc<Mutex<Engine>>;

/// Accept keypad digits as slot numbers.
pub static NUMPAD: AtomicBool = AtomicBool::new(true);

pub fn numpad() -> bool {
    NUMPAD.load(Ordering::Relaxed)
}

pub trait Injector: Send + Sync {
    /// Press and release `key` with exactly `mods` held, then put the
    /// modifiers back the way they were.
    fn tap(&self, key: char, mods: Mods);
    /// Send an event the engine swallowed earlier.
    fn replay(&self, ev: &KeyEvent);
    /// Windows: keep the Start menu from opening after a swallowed Win+key.
    fn mask_menu(&self) {}
    /// True when keys are being intercepted (chords work).
    fn hooked(&self) -> bool;
}

pub fn lock(e: &SharedEngine) -> MutexGuard<'_, Engine> {
    e.lock().unwrap_or_else(|p| p.into_inner())
}

fn run_acts(paste_key: char, acts: Vec<Act>, inj: &dyn Injector, tx: &Sender<Act>) {
    for a in acts {
        match a {
            Act::Replay(ev) => inj.replay(&ev),
            Act::Tap { mods } => inj.tap(paste_key, mods),
            Act::OpenPicker => {
                inj.mask_menu();
                let _ = tx.send(Act::OpenPicker);
            }
            other => {
                let _ = tx.send(other);
            }
        }
    }
}

/// Run one key event through the engine. Returns true to swallow it.
/// The engine lock is held while injecting so events never interleave.
pub fn on_key(engine: &SharedEngine, inj: &dyn Injector, tx: &Sender<Act>, ev: KeyEvent) -> bool {
    let mut e = lock(engine);
    let out = e.handle(ev, Instant::now());
    let key = e.config().paste_key;
    run_acts(key, out.acts, inj, tx);
    out.swallow
}

/// Release mode needs a clock: fire engine deadlines.
fn spawn_ticker(engine: SharedEngine, inj: Arc<dyn Injector>, tx: Sender<Act>) {
    std::thread::Builder::new()
        .name("chord-timer".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(10));
            let mut e = lock(&engine);
            if let Some(d) = e.deadline() {
                let now = Instant::now();
                if now >= d {
                    let acts = e.tick(now);
                    let key = e.config().paste_key;
                    run_acts(key, acts, inj.as_ref(), &tx);
                }
            }
        })
        .ok();
}

/// Start intercepting keys (unless `hook` is false) and return an injector.
pub fn start(engine: SharedEngine, tx: Sender<Act>, hook: bool) -> Result<Arc<dyn Injector>> {
    #[cfg(windows)]
    let inj = windows::start(engine.clone(), tx.clone(), hook)?;
    #[cfg(target_os = "linux")]
    let inj = linux::start(engine.clone(), tx.clone(), hook)?;
    #[cfg(target_os = "macos")]
    let inj = macos::start(engine.clone(), tx.clone(), hook)?;
    if hook {
        spawn_ticker(engine, inj.clone(), tx);
    }
    Ok(inj)
}

/// Letter or symbol for a key, shared by the platform key tables.
pub fn symbol_key(c: char) -> crate::chord::Key {
    crate::chord::Key::Char(c.to_ascii_lowercase())
}
