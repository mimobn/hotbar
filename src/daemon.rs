//! The background process: watches the clipboard, runs the key chords and
//! answers requests from the CLI and the picker.

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};

use crate::chord::{Act, Engine, Mode, Mods, Primary};
use crate::clip::{self, ClipEvent, Clipboard};
use crate::config::{self, Config};
use crate::content::Content;
use crate::input::{self, Injector, SharedEngine};
use crate::ipc::{self, Request, Response, SlotInfo};
use crate::store::{Limits, Store};
use crate::system;

struct Live {
    content: Option<Content>,
    sensitive: bool,
    seq: u64,
    /// Hashes of our own writes that the watcher will report back to us.
    suppress: Vec<(u64, Instant)>,
}

pub struct Daemon {
    cfg: RwLock<Config>,
    store: Mutex<Store>,
    clip: Arc<dyn Clipboard>,
    inj: Arc<dyn Injector>,
    engine: SharedEngine,
    live: Mutex<Live>,
    live_cv: Condvar,
    paste_lock: Mutex<()>,
    armed_seq: AtomicU64,
    picker_open: AtomicBool,
    own_writes_echo: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Requests that only need the store. Used by the daemon and by the CLI
/// when no daemon is running.
pub fn handle_store(store: &mut Store, limits: &Limits, req: &Request) -> Option<Response> {
    let r = (|| -> Result<Response> {
        Ok(match req {
            Request::List { limit } => {
                let all = store.list();
                let n = if *limit == 0 {
                    all.len()
                } else {
                    (*limit).min(all.len())
                };
                Response::Entries {
                    entries: all[..n].to_vec(),
                }
            }
            Request::Slots => Response::Slots {
                slots: store
                    .slots()
                    .into_iter()
                    .map(|(n, entry)| SlotInfo { n, entry })
                    .collect(),
            },
            Request::Get { id } => match store.get(*id)? {
                Some(c) => Response::data(&c),
                None => bail!("no entry with id {id}"),
            },
            Request::GetSlot { n } => match store
                .slot(*n)
                .map(|id| store.get(id))
                .transpose()?
                .flatten()
            {
                Some(c) => Response::data(&c),
                None => bail!("slot {n} is empty"),
            },
            Request::SetSlotData { n, kind, data } => {
                let c = Content::from_parts(*kind, ipc::decode(data)?);
                store.set_slot_content(*n, &c, limits)?;
                Response::Ok
            }
            Request::SetSlotEntry { n, id } => {
                store.set_slot(*n, *id)?;
                Response::Ok
            }
            Request::ClearSlot { n } => {
                store.clear_slot(*n)?;
                Response::Ok
            }
            Request::Delete { id } => {
                if !store.delete(*id)? {
                    bail!("no entry with id {id}");
                }
                Response::Ok
            }
            Request::Wipe { slots } => {
                store.wipe(*slots)?;
                Response::Ok
            }
            _ => {
                return Ok(Response::Error {
                    message: String::new(),
                })
            }
        })
    })();
    match r {
        Ok(Response::Error { message }) if message.is_empty() => None,
        Ok(resp) => Some(resp),
        Err(e) => Some(Response::err(format!("{e:#}"))),
    }
}

impl Daemon {
    fn cfg(&self) -> Config {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn primary_mods(&self) -> Mods {
        match input::lock(&self.engine).config().primary {
            Primary::Ctrl => Mods {
                ctrl: true,
                ..Mods::default()
            },
            Primary::Meta => Mods {
                meta: true,
                ..Mods::default()
            },
        }
    }

    fn notify(&self, title: &str, body: &str) {
        if self.cfg().paste.notify {
            system::notify(title, body);
        }
    }

    fn on_clip(&self, ev: ClipEvent) {
        let hash = ev.content.hash();
        {
            let mut live = lock(&self.live);
            live.suppress
                .retain(|(_, t)| t.elapsed() < Duration::from_secs(3));
            if let Some(pos) = live.suppress.iter().position(|(h, _)| *h == hash) {
                live.suppress.remove(pos);
                return;
            }
            live.content = Some(ev.content.clone());
            live.sensitive = ev.sensitive;
            live.seq += 1;
        }
        self.live_cv.notify_all();
        let cfg = self.cfg();
        if ev.sensitive && cfg.history.ignore_sensitive {
            return;
        }
        let limits = cfg.limits();
        if !limits.accepts(&ev.content) {
            return;
        }
        if let Err(e) = lock(&self.store).add(&ev.content, &limits) {
            crate::log!("could not save history entry: {e:#}");
        }
    }

    fn write_clip(&self, c: &Content, transient: bool) -> Result<()> {
        if self.own_writes_echo {
            lock(&self.live).suppress.push((c.hash(), Instant::now()));
        }
        self.clip.write(c, transient)
    }

    fn save_slot_now(&self, n: u8) -> Result<String> {
        let (content, sensitive) = {
            let live = lock(&self.live);
            (live.content.clone(), live.sensitive)
        };
        let Some(c) = content else {
            bail!("the clipboard is empty");
        };
        if sensitive && self.cfg().history.ignore_sensitive {
            bail!("not saved: a password manager marked this copy as secret");
        }
        let limits = self.cfg().limits();
        lock(&self.store).set_slot_content(n, &c, &limits)?;
        Ok(c.preview(60))
    }

    /// Ctrl+C then a digit: wait for the copy to land, then save it.
    fn save_slot_after_copy(self: &Arc<Self>, n: u8) {
        let me = self.clone();
        std::thread::spawn(move || {
            let armed = me.armed_seq.load(Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_millis(600);
            let mut live = lock(&me.live);
            while live.seq <= armed {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                live = me
                    .live_cv
                    .wait_timeout(live, left)
                    .map(|r| r.0)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
            drop(live);
            match me.save_slot_now(n) {
                Ok(preview) => me.notify(&format!("Slot {n}"), &preview),
                Err(e) => me.notify(&format!("Slot {n}"), &e.to_string()),
            }
        });
    }

    fn paste_slot(&self, n: u8, mods: Mods) -> Result<()> {
        let _g = lock(&self.paste_lock);
        let content = {
            let store = lock(&self.store);
            match store.slot(n) {
                Some(id) => store.get(id)?,
                None => None,
            }
        };
        let Some(content) = content else {
            self.notify(&format!("Slot {n}"), "empty");
            bail!("slot {n} is empty");
        };
        let cfg = self.cfg();
        let prev = lock(&self.live).content.clone();
        self.write_clip(&content, true)?;
        std::thread::sleep(Duration::from_millis(40));
        self.inj.tap(cfg.keys.paste_key, mods);
        if cfg.paste.restore_clipboard {
            if let Some(prev) = prev.filter(|p| *p != content) {
                std::thread::sleep(Duration::from_millis(cfg.paste.restore_delay_ms));
                self.write_clip(&prev, true)?;
            }
        }
        Ok(())
    }

    fn use_entry(&self, id: u64, paste: bool) -> Result<()> {
        let _g = lock(&self.paste_lock);
        let content = lock(&self.store).get(id)?.context("entry not found")?;
        self.write_clip(&content, false)?;
        {
            let mut live = lock(&self.live);
            live.content = Some(content.clone());
            live.sensitive = false;
            live.seq += 1;
        }
        lock(&self.store).touch(id)?;
        if paste {
            std::thread::sleep(Duration::from_millis(40));
            let key = self.cfg().keys.paste_key;
            self.inj.tap(key, self.primary_mods());
        }
        Ok(())
    }

    fn open_picker(self: &Arc<Self>) {
        if self.picker_open.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let return_to = system::foreground_window();
        std::thread::spawn(move || {
            let cmd = me.cfg().picker.command;
            let r = if cmd.trim() == "builtin" {
                me.run_builtin_picker(return_to)
            } else {
                crate::picker::run_dmenu(&cmd)
            };
            if let Err(e) = r {
                crate::log!("picker failed: {e:#}");
            }
            me.picker_open.store(false, Ordering::SeqCst);
        });
    }

    fn run_builtin_picker(&self, return_to: isize) -> Result<()> {
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(exe);
        cmd.arg("pick").arg("--builtin");
        if return_to != 0 {
            cmd.arg("--return-to").arg(return_to.to_string());
        }
        system::no_console(&mut cmd);
        cmd.status().context("could not start the picker")?;
        Ok(())
    }

    fn on_act(self: &Arc<Self>, act: Act) {
        match act {
            Act::CopyArmed => {
                let seq = lock(&self.live).seq;
                self.armed_seq.store(seq, Ordering::SeqCst);
            }
            Act::SaveSlot(n) => self.save_slot_after_copy(n),
            Act::PasteSlot { n, mods } => {
                let me = self.clone();
                std::thread::spawn(move || {
                    if let Err(e) = me.paste_slot(n, mods) {
                        crate::log!("paste slot {n}: {e:#}");
                    }
                });
            }
            Act::OpenPicker => self.open_picker(),
            Act::Replay(_) | Act::Tap { .. } => {}
        }
    }

    fn handle(self: &Arc<Self>, req: Request) -> Response {
        let limits = self.cfg().limits();
        if let Some(r) = handle_store(&mut lock(&self.store), &limits, &req) {
            return r;
        }
        let r = (|| -> Result<Response> {
            Ok(match req {
                Request::Ping => Response::Pong {
                    pid: std::process::id(),
                    version: crate::VERSION.into(),
                },
                Request::SaveSlot { n } => {
                    self.save_slot_now(n)?;
                    Response::Ok
                }
                Request::PasteSlot { n, delay_ms } => {
                    let mods = self.primary_mods();
                    let me = self.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(delay_ms));
                        if let Err(e) = me.paste_slot(n, mods) {
                            crate::log!("paste slot {n}: {e:#}");
                        }
                    });
                    Response::Ok
                }
                Request::PasteEntry { id, delay_ms } => {
                    let me = self.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(delay_ms));
                        if let Err(e) = me.use_entry(id, true) {
                            crate::log!("paste entry {id}: {e:#}");
                        }
                    });
                    Response::Ok
                }
                Request::CopyEntry { id } => {
                    self.use_entry(id, false)?;
                    Response::Ok
                }
                Request::OpenPicker => {
                    self.open_picker();
                    Response::Ok
                }
                Request::Stop => {
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_millis(100));
                        std::process::exit(0);
                    });
                    Response::Ok
                }
                other => bail!("unsupported request {other:?}"),
            })
        })();
        r.unwrap_or_else(|e| Response::err(format!("{e:#}")))
    }

    fn watch_config(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::spawn(move || {
            let path = config::config_path();
            let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            let mut last: Option<SystemTime> = mtime(&path);
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let now = mtime(&path);
                if now == last {
                    continue;
                }
                last = now;
                match Config::load() {
                    Ok(c) => {
                        let was_hooked = me.inj.hooked();
                        let wants_hook = c.engine().mode != Mode::Off;
                        if was_hooked != wants_hook {
                            crate::log!("turning key chords on or off needs a restart: `hotbar stop`, then `hotbar daemon`");
                        }
                        input::NUMPAD.store(c.keys.numpad, Ordering::Relaxed);
                        input::lock(&me.engine).set_config(c.engine());
                        lock(&me.store).prune_and_save(&c.limits()).ok();
                        *me.cfg.write().unwrap_or_else(|e| e.into_inner()) = c;
                        crate::log!("config reloaded");
                    }
                    Err(e) => crate::log!("config not reloaded: {e:#}"),
                }
            }
        });
    }
}

/// The leading number of a "id<TAB>preview" line.
pub fn parse_id(line: &str) -> Option<u64> {
    line.trim().split(['\t', ' ']).next()?.parse().ok()
}

pub fn run() -> Result<()> {
    if ipc::daemon_running() {
        bail!("hotbar daemon is already running");
    }
    crate::logging::to_file();
    crate::log!("hotbar {} starting", crate::VERSION);
    let cfg = Config::load().unwrap_or_else(|e| {
        crate::log!("{e:#}; using the default config");
        Config::default()
    });
    let store = Store::open(&config::data_dir())?;

    let (clip_tx, clip_rx) = channel::<ClipEvent>();
    let clip = clip::start(clip_tx).context("could not start the clipboard watcher")?;
    crate::log!("clipboard backend: {}", clip.name());

    input::NUMPAD.store(cfg.keys.numpad, Ordering::Relaxed);
    let engine: SharedEngine = Arc::new(Mutex::new(Engine::new(cfg.engine())));
    let (act_tx, act_rx) = channel::<Act>();
    let want_hook = cfg.engine().mode != Mode::Off;
    let inj = match input::start(engine.clone(), act_tx.clone(), want_hook) {
        Ok(i) => i,
        Err(e) if want_hook => {
            crate::log!("key chords disabled: {e:#}");
            crate::log!("run `hotbar doctor` to see what is missing");
            input::start(engine.clone(), act_tx.clone(), false)?
        }
        Err(e) => return Err(e),
    };
    crate::log!("key chords: {}", if inj.hooked() { "on" } else { "off" });

    let own_writes_echo = clip.name() == "wayland";
    let d = Arc::new(Daemon {
        cfg: RwLock::new(cfg),
        store: Mutex::new(store),
        clip,
        inj,
        engine,
        live: Mutex::new(Live {
            content: None,
            sensitive: false,
            seq: 0,
            suppress: Vec::new(),
        }),
        live_cv: Condvar::new(),
        paste_lock: Mutex::new(()),
        armed_seq: AtomicU64::new(0),
        picker_open: AtomicBool::new(false),
        own_writes_echo,
    });

    {
        let d = d.clone();
        std::thread::Builder::new()
            .name("clip-events".into())
            .spawn(move || {
                for ev in clip_rx {
                    d.on_clip(ev);
                }
            })?;
    }
    {
        let d = d.clone();
        std::thread::Builder::new()
            .name("acts".into())
            .spawn(move || {
                for a in act_rx {
                    d.on_act(a);
                }
            })?;
    }
    d.watch_config();

    let handler_d = d.clone();
    ipc::serve(Arc::new(move |req| handler_d.handle(req)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_menu_lines() {
        assert_eq!(parse_id("42\thello world\n"), Some(42));
        assert_eq!(parse_id("7\t[3] slot text"), Some(7));
        assert_eq!(parse_id(""), None);
        assert_eq!(parse_id("abc\tx"), None);
    }
}
