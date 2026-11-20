//! The key chord engine.
//!
//! Pure logic with no platform code, so every rule here is unit tested.
//! Platform hooks turn native key events into [`KeyEvent`]s, ask the engine
//! what to do, and carry out the returned [`Act`]s in order.
//!
//! Default behavior ("ctrl-held"):
//!   * Ctrl+V is held back while Ctrl stays down.
//!       - Ctrl+digit next: paste that slot.
//!       - Ctrl released or any other key: the normal Ctrl+V is sent right away.
//!   * Ctrl+C passes through. A Ctrl+digit before Ctrl is released saves the
//!     copy to that slot. The digit is swallowed.
//!   * Ctrl+digit without a V or C before it passes through untouched.

use std::collections::HashSet;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Ctrl,
    Shift,
    Alt,
    Meta,
    Digit(u8),
    Char(char),
    Other,
}

impl Key {
    pub fn is_modifier(self) -> bool {
        matches!(self, Key::Ctrl | Key::Shift | Key::Alt | Key::Meta)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub down: bool,
    /// Platform key code. Used to match a key up with its key down.
    pub raw: u32,
    /// Extra platform data needed to replay the event (scan code, flags).
    pub extra: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Mods {
    pub fn none(&self) -> bool {
        !(self.ctrl || self.shift || self.alt || self.meta)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primary {
    Ctrl,
    Meta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    CtrlHeld,
    Release,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: Mods,
    pub key: Key,
}

impl Hotkey {
    /// Parse "super+v", "ctrl+shift+v", "cmd+shift+v" and so on.
    pub fn parse(s: &str) -> Option<Hotkey> {
        let mut mods = Mods::default();
        let mut key = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => mods.ctrl = true,
                "shift" => mods.shift = true,
                "alt" | "option" | "opt" => mods.alt = true,
                "super" | "win" | "meta" | "cmd" | "command" | "logo" | "mod4" => mods.meta = true,
                p if p.chars().count() == 1 => {
                    let c = p.chars().next()?;
                    key = Some(match c.to_digit(10) {
                        Some(d) if d > 0 => Key::Digit(d as u8),
                        _ => Key::Char(c),
                    });
                }
                _ => return None,
            }
        }
        Some(Hotkey { mods, key: key? })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    pub primary: Primary,
    pub mode: Mode,
    pub timeout: Duration,
    pub copy_key: char,
    pub paste_key: char,
    pub picker: Option<Hotkey>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            primary: Primary::Ctrl,
            mode: Mode::CtrlHeld,
            timeout: Duration::from_millis(300),
            copy_key: 'c',
            paste_key: 'v',
            picker: Hotkey::parse("super+v"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    /// Send this (previously swallowed) event to the system now.
    Replay(KeyEvent),
    /// Press and release the paste key with exactly these modifiers held.
    Tap {
        mods: Mods,
    },
    /// A copy shortcut went through. The daemon notes the clipboard state.
    CopyArmed,
    SaveSlot(u8),
    PasteSlot {
        n: u8,
        mods: Mods,
    },
    OpenPicker,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Outcome {
    /// Drop the current event instead of letting it through.
    pub swallow: bool,
    pub acts: Vec<Act>,
}

impl Outcome {
    fn pass() -> Self {
        Outcome::default()
    }
    fn pass_with(acts: Vec<Act>) -> Self {
        Outcome {
            swallow: false,
            acts,
        }
    }
    fn swallow(acts: Vec<Act>) -> Self {
        Outcome {
            swallow: true,
            acts,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Paste key pressed with the primary modifier; waiting to see if a digit follows.
    Pending {
        mods: Mods,
    },
    /// Copy shortcut went through; a digit now saves it to a slot.
    Armed,
    /// Release mode: everything let go after a paste, waiting for a digit.
    WaitPaste {
        mods: Mods,
        deadline: Instant,
    },
    /// Release mode: everything let go after a copy, waiting for a digit.
    WaitCopy {
        deadline: Instant,
    },
}

pub struct Engine {
    cfg: EngineConfig,
    state: State,
    held: HashSet<(Key, u32)>,
    swallowed: HashSet<u32>,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Engine {
        Engine {
            cfg,
            state: State::Idle,
            held: HashSet::new(),
            swallowed: HashSet::new(),
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: EngineConfig) {
        self.cfg = cfg;
        self.state = State::Idle;
    }

    pub fn mods(&self) -> Mods {
        let mut m = Mods::default();
        for (k, _) in &self.held {
            match k {
                Key::Ctrl => m.ctrl = true,
                Key::Shift => m.shift = true,
                Key::Alt => m.alt = true,
                Key::Meta => m.meta = true,
                _ => {}
            }
        }
        m
    }

    /// When the engine wants [`Engine::tick`] called.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::WaitPaste { deadline, .. } | State::WaitCopy { deadline } => Some(deadline),
            _ => None,
        }
    }

    /// Call when the deadline passes. Returns acts to carry out.
    pub fn tick(&mut self, now: Instant) -> Vec<Act> {
        match self.state {
            State::WaitPaste { mods, deadline } if now >= deadline => {
                self.state = State::Idle;
                vec![Act::Tap { mods }]
            }
            State::WaitCopy { deadline } if now >= deadline => {
                self.state = State::Idle;
                vec![]
            }
            _ => vec![],
        }
    }

    fn track(&mut self, ev: &KeyEvent) {
        if ev.key.is_modifier() {
            if ev.down {
                self.held.insert((ev.key, ev.raw));
            } else {
                self.held.remove(&(ev.key, ev.raw));
            }
        }
    }

    fn primary_down(&self, m: Mods) -> bool {
        match self.cfg.primary {
            Primary::Ctrl => m.ctrl,
            Primary::Meta => m.meta,
        }
    }

    fn is_primary(&self, k: Key) -> bool {
        match self.cfg.primary {
            Primary::Ctrl => k == Key::Ctrl,
            Primary::Meta => k == Key::Meta,
        }
    }

    /// Alt, or the "other" big modifier, rules a chord out (AltGr, Ctrl+Alt+V, ...).
    fn blocked(&self, m: Mods) -> bool {
        m.alt
            || match self.cfg.primary {
                Primary::Ctrl => m.meta,
                Primary::Meta => m.ctrl,
            }
    }

    fn is_chord_key(&self, ev: &KeyEvent, m: Mods, c: char) -> bool {
        ev.down && ev.key == Key::Char(c) && self.primary_down(m) && !self.blocked(m)
    }

    fn is_paste(&self, ev: &KeyEvent, m: Mods) -> bool {
        self.is_chord_key(ev, m, self.cfg.paste_key)
    }

    fn is_copy(&self, ev: &KeyEvent, m: Mods) -> bool {
        self.is_chord_key(ev, m, self.cfg.copy_key)
    }

    fn picker_hit(&self, ev: &KeyEvent, m: Mods) -> bool {
        match self.cfg.picker {
            Some(h) => ev.down && ev.key == h.key && m == h.mods,
            None => false,
        }
    }

    fn eat(&mut self, ev: &KeyEvent) {
        self.swallowed.insert(ev.raw);
    }

    pub fn handle(&mut self, ev: KeyEvent, now: Instant) -> Outcome {
        let was_swallowed = self.swallowed.contains(&ev.raw);
        self.track(&ev);
        if self.cfg.mode == Mode::Off {
            self.swallowed.clear();
            return Outcome::pass();
        }
        let m = self.mods();

        // The up of a key whose down we ate is eaten too.
        if !ev.down && was_swallowed {
            self.swallowed.remove(&ev.raw);
            return Outcome::swallow(vec![]);
        }
        // Auto repeat of a key we ate: eat it as well, except the paste key,
        // where each repeat counts as a new press so held Ctrl+V still repeats.
        if ev.down && was_swallowed && !self.is_paste(&ev, m) {
            return Outcome::swallow(vec![]);
        }
        self.step(ev, m, now)
    }

    fn step(&mut self, ev: KeyEvent, m: Mods, now: Instant) -> Outcome {
        match self.state {
            State::Idle => self.idle(ev, m),

            State::Pending { mods: pm } => {
                if let (true, Key::Digit(n)) = (ev.down, ev.key) {
                    if self.primary_down(m) {
                        self.state = State::Idle;
                        self.eat(&ev);
                        return Outcome::swallow(vec![Act::PasteSlot { n, mods: pm }]);
                    }
                }
                if self.is_paste(&ev, m) {
                    // Another V while Ctrl is held: the previous one was a plain paste.
                    self.eat(&ev);
                    self.state = State::Pending { mods: m };
                    return Outcome::swallow(vec![Act::Tap { mods: pm }]);
                }
                if ev.key.is_modifier() {
                    if self.is_primary(ev.key) && !ev.down && !self.primary_down(m) {
                        return match self.cfg.mode {
                            Mode::Release => {
                                self.state = State::WaitPaste {
                                    mods: pm,
                                    deadline: now + self.cfg.timeout,
                                };
                                Outcome::pass()
                            }
                            _ => {
                                self.state = State::Idle;
                                Outcome::swallow(vec![Act::Tap { mods: pm }, Act::Replay(ev)])
                            }
                        };
                    }
                    // Shift or Alt changing does not cancel the chord.
                    return Outcome::pass();
                }
                self.state = State::Idle;
                self.flush_then(pm, ev, m)
            }

            State::Armed => {
                if let (true, Key::Digit(n)) = (ev.down, ev.key) {
                    if self.primary_down(m) {
                        self.state = State::Idle;
                        self.eat(&ev);
                        return Outcome::swallow(vec![Act::SaveSlot(n)]);
                    }
                }
                if self.is_copy(&ev, m) {
                    return Outcome::pass_with(vec![Act::CopyArmed]);
                }
                // Letting go of C keeps the copy armed.
                if !ev.down && ev.key == Key::Char(self.cfg.copy_key) {
                    return Outcome::pass();
                }
                if ev.key.is_modifier() {
                    if self.is_primary(ev.key) && !ev.down && !self.primary_down(m) {
                        self.state = match self.cfg.mode {
                            Mode::Release => State::WaitCopy {
                                deadline: now + self.cfg.timeout,
                            },
                            _ => State::Idle,
                        };
                    }
                    return Outcome::pass();
                }
                self.state = State::Idle;
                self.idle(ev, m)
            }

            State::WaitPaste { mods: pm, deadline } => {
                if now < deadline {
                    if let (true, Key::Digit(n)) = (ev.down, ev.key) {
                        if !self.primary_down(m) && !self.blocked(m) {
                            self.state = State::Idle;
                            self.eat(&ev);
                            return Outcome::swallow(vec![Act::PasteSlot { n, mods: pm }]);
                        }
                    }
                }
                self.state = State::Idle;
                self.flush_then(pm, ev, m)
            }

            State::WaitCopy { deadline } => {
                if now < deadline {
                    if let (true, Key::Digit(n)) = (ev.down, ev.key) {
                        if !self.primary_down(m) && !self.blocked(m) {
                            self.state = State::Idle;
                            self.eat(&ev);
                            return Outcome::swallow(vec![Act::SaveSlot(n)]);
                        }
                    }
                }
                self.state = State::Idle;
                self.idle(ev, m)
            }
        }
    }

    /// Send the held back plain paste, then handle `ev` as if nothing was pending.
    fn flush_then(&mut self, pm: Mods, ev: KeyEvent, m: Mods) -> Outcome {
        let rest = self.idle(ev, m);
        let mut acts = vec![Act::Tap { mods: pm }];
        if !rest.swallow {
            acts.push(Act::Replay(ev));
        }
        acts.extend(rest.acts);
        Outcome::swallow(acts)
    }

    fn idle(&mut self, ev: KeyEvent, m: Mods) -> Outcome {
        if self.picker_hit(&ev, m) {
            self.eat(&ev);
            return Outcome::swallow(vec![Act::OpenPicker]);
        }
        if self.is_paste(&ev, m) {
            self.eat(&ev);
            self.state = State::Pending { mods: m };
            return Outcome::swallow(vec![]);
        }
        if self.is_copy(&ev, m) {
            self.state = State::Armed;
            return Outcome::pass_with(vec![Act::CopyArmed]);
        }
        Outcome::pass()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: u32 = 1;
    const RCTRL: u32 = 2;
    const SHIFT: u32 = 3;
    const ALT: u32 = 4;
    const META: u32 = 5;
    const C: u32 = 10;
    const V: u32 = 11;
    const X: u32 = 12;
    fn digit_raw(n: u8) -> u32 {
        100 + n as u32
    }

    fn ev(key: Key, raw: u32, down: bool) -> KeyEvent {
        KeyEvent {
            key,
            down,
            raw,
            extra: 0,
        }
    }
    fn ctrl(down: bool) -> KeyEvent {
        ev(Key::Ctrl, LCTRL, down)
    }
    fn shift(down: bool) -> KeyEvent {
        ev(Key::Shift, SHIFT, down)
    }
    fn alt(down: bool) -> KeyEvent {
        ev(Key::Alt, ALT, down)
    }
    fn meta(down: bool) -> KeyEvent {
        ev(Key::Meta, META, down)
    }
    fn v(down: bool) -> KeyEvent {
        ev(Key::Char('v'), V, down)
    }
    fn c(down: bool) -> KeyEvent {
        ev(Key::Char('c'), C, down)
    }
    fn x(down: bool) -> KeyEvent {
        ev(Key::Char('x'), X, down)
    }
    fn d(n: u8, down: bool) -> KeyEvent {
        ev(Key::Digit(n), digit_raw(n), down)
    }

    const CTRL: Mods = Mods {
        ctrl: true,
        shift: false,
        alt: false,
        meta: false,
    };
    const CTRL_SHIFT: Mods = Mods {
        ctrl: true,
        shift: true,
        alt: false,
        meta: false,
    };

    struct T {
        e: Engine,
        now: Instant,
    }

    impl T {
        fn new() -> T {
            T::with(EngineConfig::default())
        }
        fn with(cfg: EngineConfig) -> T {
            T {
                e: Engine::new(cfg),
                now: Instant::now(),
            }
        }
        fn release() -> T {
            T::with(EngineConfig {
                mode: Mode::Release,
                ..EngineConfig::default()
            })
        }
        fn send(&mut self, k: KeyEvent) -> Outcome {
            self.now += Duration::from_millis(20);
            self.e.handle(k, self.now)
        }
        fn wait(&mut self, ms: u64) -> Vec<Act> {
            self.now += Duration::from_millis(ms);
            self.e.tick(self.now)
        }
    }

    fn pass() -> Outcome {
        Outcome::pass()
    }
    fn eat() -> Outcome {
        Outcome::swallow(vec![])
    }

    #[test]
    fn plain_paste_goes_out_when_ctrl_is_released() {
        let mut t = T::new();
        assert_eq!(t.send(ctrl(true)), pass());
        assert_eq!(t.send(v(true)), eat());
        assert_eq!(t.send(v(false)), eat());
        assert_eq!(
            t.send(ctrl(false)),
            Outcome::swallow(vec![Act::Tap { mods: CTRL }, Act::Replay(ctrl(false))])
        );
    }

    #[test]
    fn paste_slot_with_ctrl_held() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(v(true));
        t.send(v(false));
        assert_eq!(
            t.send(d(3, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 3, mods: CTRL }])
        );
        assert_eq!(t.send(d(3, false)), eat());
        assert_eq!(t.send(ctrl(false)), pass());
    }

    #[test]
    fn digit_while_v_still_held_also_works() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(v(true));
        assert_eq!(
            t.send(d(1, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 1, mods: CTRL }])
        );
        assert_eq!(t.send(v(false)), eat());
        assert_eq!(t.send(d(1, false)), eat());
        assert_eq!(t.send(ctrl(false)), pass());
    }

    #[test]
    fn other_key_flushes_paste_first() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(v(true));
        t.send(v(false));
        assert_eq!(
            t.send(x(true)),
            Outcome::swallow(vec![Act::Tap { mods: CTRL }, Act::Replay(x(true))])
        );
        assert_eq!(t.send(x(false)), pass());
    }

    #[test]
    fn repeated_v_pastes_each_time() {
        let mut t = T::new();
        t.send(ctrl(true));
        assert_eq!(t.send(v(true)), eat());
        // auto repeat
        assert_eq!(
            t.send(v(true)),
            Outcome::swallow(vec![Act::Tap { mods: CTRL }])
        );
        assert_eq!(
            t.send(v(true)),
            Outcome::swallow(vec![Act::Tap { mods: CTRL }])
        );
        assert_eq!(t.send(v(false)), eat());
        assert_eq!(
            t.send(ctrl(false)),
            Outcome::swallow(vec![Act::Tap { mods: CTRL }, Act::Replay(ctrl(false))])
        );
    }

    #[test]
    fn ctrl_digit_alone_passes_through() {
        let mut t = T::new();
        t.send(ctrl(true));
        assert_eq!(t.send(d(1, true)), pass());
        assert_eq!(t.send(d(1, false)), pass());
        assert_eq!(t.send(ctrl(false)), pass());
    }

    #[test]
    fn copy_then_digit_saves_slot() {
        let mut t = T::new();
        t.send(ctrl(true));
        assert_eq!(t.send(c(true)), Outcome::pass_with(vec![Act::CopyArmed]));
        assert_eq!(t.send(c(false)), pass());
        assert_eq!(t.send(d(4, true)), Outcome::swallow(vec![Act::SaveSlot(4)]));
        assert_eq!(t.send(d(4, false)), eat());
        assert_eq!(t.send(ctrl(false)), pass());
    }

    #[test]
    fn digit_repeat_after_slot_paste_is_eaten() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(v(true));
        t.send(v(false));
        t.send(d(2, true));
        // held digit repeats must not reach the app as Ctrl+2
        assert_eq!(t.send(d(2, true)), eat());
        assert_eq!(t.send(d(2, false)), eat());
    }

    #[test]
    fn copy_then_plain_digit_after_release_is_normal_typing() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(c(true));
        t.send(c(false));
        t.send(ctrl(false));
        assert_eq!(t.send(d(1, true)), pass());
    }

    #[test]
    fn copy_then_paste_still_chords() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(c(true));
        t.send(c(false));
        assert_eq!(t.send(v(true)), eat());
        assert_eq!(
            t.send(d(5, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 5, mods: CTRL }])
        );
    }

    #[test]
    fn shift_variant_for_terminals() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(shift(true));
        assert_eq!(t.send(v(true)), eat());
        // letting go of shift before the digit keeps the chord
        assert_eq!(t.send(shift(false)), pass());
        assert_eq!(
            t.send(d(7, true)),
            Outcome::swallow(vec![Act::PasteSlot {
                n: 7,
                mods: CTRL_SHIFT
            }])
        );
    }

    #[test]
    fn shift_plain_paste_replays_with_shift() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(shift(true));
        t.send(v(true));
        t.send(v(false));
        t.send(shift(false));
        assert_eq!(
            t.send(ctrl(false)),
            Outcome::swallow(vec![
                Act::Tap { mods: CTRL_SHIFT },
                Act::Replay(ctrl(false))
            ])
        );
    }

    #[test]
    fn alt_blocks_chord() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(alt(true));
        assert_eq!(t.send(v(true)), pass());
        assert_eq!(t.send(v(false)), pass());
    }

    #[test]
    fn right_ctrl_counts() {
        let mut t = T::new();
        t.send(ev(Key::Ctrl, RCTRL, true));
        assert_eq!(t.send(v(true)), eat());
        assert_eq!(
            t.send(d(9, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 9, mods: CTRL }])
        );
    }

    #[test]
    fn releasing_one_of_two_ctrls_keeps_pending() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(ev(Key::Ctrl, RCTRL, true));
        t.send(v(true));
        assert_eq!(t.send(ctrl(false)), pass());
        assert_eq!(
            t.send(d(1, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 1, mods: CTRL }])
        );
    }

    #[test]
    fn picker_hotkey() {
        let mut t = T::new();
        t.send(meta(true));
        assert_eq!(t.send(v(true)), Outcome::swallow(vec![Act::OpenPicker]));
        assert_eq!(t.send(v(false)), eat());
        assert_eq!(t.send(meta(false)), pass());
    }

    #[test]
    fn ctrl_alt_v_picker_leaves_win_v_alone() {
        let mut t = T::with(EngineConfig {
            picker: Hotkey::parse("ctrl+alt+v"),
            ..EngineConfig::default()
        });
        t.send(ctrl(true));
        t.send(alt(true));
        assert_eq!(t.send(v(true)), Outcome::swallow(vec![Act::OpenPicker]));
        assert_eq!(t.send(v(false)), eat());
        t.send(alt(false));
        t.send(ctrl(false));
        // Win+V goes to Windows untouched
        t.send(meta(true));
        assert_eq!(t.send(v(true)), pass());
        assert_eq!(t.send(v(false)), pass());
        t.send(meta(false));
        // and plain Ctrl+V chords still work
        t.send(ctrl(true));
        assert_eq!(t.send(v(true)), eat());
        assert_eq!(
            t.send(d(2, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 2, mods: CTRL }])
        );
    }

    #[test]
    fn mac_primary_is_cmd_and_picker_wins() {
        let mut t = T::with(EngineConfig {
            primary: Primary::Meta,
            picker: Hotkey::parse("cmd+shift+v"),
            ..EngineConfig::default()
        });
        t.send(meta(true));
        assert_eq!(t.send(v(true)), eat());
        let cmd = Mods {
            meta: true,
            ..Mods::default()
        };
        assert_eq!(
            t.send(d(2, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 2, mods: cmd }])
        );
        t.send(d(2, false));
        t.send(v(false));
        t.send(shift(true));
        assert_eq!(t.send(v(true)), Outcome::swallow(vec![Act::OpenPicker]));
        // Ctrl+V on a Mac is not a paste
        let mut t2 = T::with(EngineConfig {
            primary: Primary::Meta,
            ..EngineConfig::default()
        });
        t2.send(ctrl(true));
        assert_eq!(t2.send(v(true)), pass());
    }

    #[test]
    fn off_mode_passes_everything() {
        let mut t = T::with(EngineConfig {
            mode: Mode::Off,
            ..EngineConfig::default()
        });
        for k in [
            ctrl(true),
            v(true),
            d(1, true),
            d(1, false),
            v(false),
            ctrl(false),
        ] {
            assert_eq!(t.send(k), pass());
        }
    }

    #[test]
    fn release_mode_digit_after_letting_go() {
        let mut t = T::release();
        t.send(ctrl(true));
        t.send(v(true));
        t.send(v(false));
        assert_eq!(t.send(ctrl(false)), pass());
        assert_eq!(
            t.send(d(2, true)),
            Outcome::swallow(vec![Act::PasteSlot { n: 2, mods: CTRL }])
        );
        assert_eq!(t.send(d(2, false)), eat());
    }

    #[test]
    fn release_mode_timeout_sends_plain_paste() {
        let mut t = T::release();
        t.send(ctrl(true));
        t.send(v(true));
        t.send(v(false));
        t.send(ctrl(false));
        assert!(t.e.deadline().is_some());
        assert_eq!(t.wait(100), vec![]);
        assert_eq!(t.wait(300), vec![Act::Tap { mods: CTRL }]);
        assert!(t.e.deadline().is_none());
        assert_eq!(t.send(d(2, true)), pass());
    }

    #[test]
    fn release_mode_other_key_flushes() {
        let mut t = T::release();
        t.send(ctrl(true));
        t.send(v(true));
        t.send(v(false));
        t.send(ctrl(false));
        assert_eq!(
            t.send(x(true)),
            Outcome::swallow(vec![Act::Tap { mods: CTRL }, Act::Replay(x(true))])
        );
    }

    #[test]
    fn release_mode_ctrl_again_flushes_then_new_chord() {
        let mut t = T::release();
        t.send(ctrl(true));
        t.send(v(true));
        t.send(v(false));
        t.send(ctrl(false));
        assert_eq!(
            t.send(ctrl(true)),
            Outcome::swallow(vec![Act::Tap { mods: CTRL }, Act::Replay(ctrl(true))])
        );
        assert_eq!(t.send(v(true)), eat());
    }

    #[test]
    fn release_mode_copy() {
        let mut t = T::release();
        t.send(ctrl(true));
        t.send(c(true));
        t.send(c(false));
        t.send(ctrl(false));
        assert_eq!(t.send(d(6, true)), Outcome::swallow(vec![Act::SaveSlot(6)]));
        // after the timeout digits are plain typing again
        let mut t = T::release();
        t.send(ctrl(true));
        t.send(c(true));
        t.send(c(false));
        t.send(ctrl(false));
        t.wait(400);
        assert_eq!(t.send(d(6, true)), pass());
    }

    #[test]
    fn hotkey_parse() {
        let h = Hotkey::parse("Super+V").unwrap();
        assert!(h.mods.meta && !h.mods.ctrl);
        assert_eq!(h.key, Key::Char('v'));
        let h = Hotkey::parse("ctrl+shift+1").unwrap();
        assert!(h.mods.ctrl && h.mods.shift);
        assert_eq!(h.key, Key::Digit(1));
        assert!(Hotkey::parse("ctrl+").is_none());
        assert!(Hotkey::parse("hyper+v").is_none());
    }

    #[test]
    fn config_change_resets_state() {
        let mut t = T::new();
        t.send(ctrl(true));
        t.send(v(true));
        t.e.set_config(EngineConfig::default());
        assert_eq!(t.send(d(1, true)), pass());
    }
}
