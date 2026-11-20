//! X11 clipboard: XFixes tells us when the CLIPBOARD owner changes, we read
//! it with ConvertSelection, and we own the selection to write.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Property, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use super::{ClipEvent, Clipboard};
use crate::content::{Content, Kind};

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    string: Atom,
    text: Atom,
    text_plain: Atom,
    text_plain_utf8: Atom,
    png: Atom,
    incr: Atom,
    secret: Atom,
    prop: Atom,
    wake: Atom,
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Atoms> {
        Ok(Atoms {
            clipboard: intern(conn, "CLIPBOARD")?,
            targets: intern(conn, "TARGETS")?,
            utf8: intern(conn, "UTF8_STRING")?,
            string: AtomEnum::STRING.into(),
            text: intern(conn, "TEXT")?,
            text_plain: intern(conn, "text/plain")?,
            text_plain_utf8: intern(conn, "text/plain;charset=utf-8")?,
            png: intern(conn, "image/png")?,
            incr: intern(conn, "INCR")?,
            secret: intern(conn, super::SECRET_MIME)?,
            prop: intern(conn, "HOTBAR_SELECTION")?,
            wake: intern(conn, "HOTBAR_WAKE")?,
        })
    }

    fn text_targets(&self) -> [Atom; 5] {
        [
            self.utf8,
            self.text_plain_utf8,
            self.text_plain,
            self.string,
            self.text,
        ]
    }
}

enum Reading {
    Idle,
    Targets,
    Data {
        kind: Kind,
        sensitive: bool,
    },
    Incr {
        kind: Kind,
        sensitive: bool,
        buf: Vec<u8>,
    },
}

struct WriteReq {
    content: Content,
    transient: bool,
    done: Sender<()>,
}

struct Worker {
    conn: RustConnection,
    win: Window,
    a: Atoms,
    owned: Option<(Content, bool)>,
    last: Option<Content>,
    reading: Reading,
    tx: Sender<ClipEvent>,
    pending: Arc<Mutex<Vec<WriteReq>>>,
}

impl Worker {
    fn request_targets(&mut self, time: u32) -> Result<()> {
        self.conn.convert_selection(
            self.win,
            self.a.clipboard,
            self.a.targets,
            self.a.prop,
            time,
        )?;
        self.conn.flush()?;
        self.reading = Reading::Targets;
        Ok(())
    }

    fn deliver(&mut self, kind: Kind, sensitive: bool, bytes: Vec<u8>) {
        let content = Content::from_parts(kind, bytes);
        if !sensitive && !content.is_empty() {
            self.last = Some(content.clone());
        }
        let _ = self.tx.send(ClipEvent { content, sensitive });
    }

    fn on_selection_notify(&mut self, e: SelectionNotifyEvent) -> Result<()> {
        if e.property == NONE {
            self.reading = Reading::Idle;
            return Ok(());
        }
        match std::mem::replace(&mut self.reading, Reading::Idle) {
            Reading::Targets => {
                let r = self
                    .conn
                    .get_property(true, self.win, self.a.prop, AtomEnum::ATOM, 0, u32::MAX / 4)?
                    .reply()?;
                let atoms: Vec<Atom> = r.value32().map(|v| v.collect()).unwrap_or_default();
                let sensitive = atoms.contains(&self.a.secret);
                let text = self
                    .a
                    .text_targets()
                    .into_iter()
                    .find(|t| atoms.contains(t));
                let (target, kind) = match text {
                    Some(t) => (t, Kind::Text),
                    None if atoms.contains(&self.a.png) => (self.a.png, Kind::Image),
                    None => return Ok(()),
                };
                self.conn.convert_selection(
                    self.win,
                    self.a.clipboard,
                    target,
                    self.a.prop,
                    CURRENT_TIME,
                )?;
                self.conn.flush()?;
                self.reading = Reading::Data { kind, sensitive };
            }
            Reading::Data { kind, sensitive } => {
                let r = self
                    .conn
                    .get_property(true, self.win, self.a.prop, AtomEnum::ANY, 0, u32::MAX / 4)?
                    .reply()?;
                if r.type_ == self.a.incr {
                    // Deleting the property (done above) starts the incremental transfer.
                    self.reading = Reading::Incr {
                        kind,
                        sensitive,
                        buf: Vec::new(),
                    };
                } else {
                    self.deliver(kind, sensitive, r.value);
                }
                self.conn.flush()?;
            }
            other => self.reading = other,
        }
        Ok(())
    }

    fn on_property(&mut self, atom: Atom, state: Property) -> Result<()> {
        if atom != self.a.prop || state != Property::NEW_VALUE {
            return Ok(());
        }
        if let Reading::Incr { .. } = self.reading {
            let r = self
                .conn
                .get_property(true, self.win, self.a.prop, AtomEnum::ANY, 0, u32::MAX / 4)?
                .reply()?;
            self.conn.flush()?;
            if r.value.is_empty() {
                if let Reading::Incr {
                    kind,
                    sensitive,
                    buf,
                } = std::mem::replace(&mut self.reading, Reading::Idle)
                {
                    self.deliver(kind, sensitive, buf);
                }
            } else if let Reading::Incr { buf, .. } = &mut self.reading {
                buf.extend_from_slice(&r.value);
            }
        }
        Ok(())
    }

    fn serve(&mut self, e: SelectionRequestEvent) -> Result<()> {
        let mut prop = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        let max = self.conn.maximum_request_bytes().saturating_sub(64);
        let ok = match &self.owned {
            Some((c, transient)) => {
                if e.target == self.a.targets {
                    let mut t = vec![self.a.targets];
                    match c {
                        Content::Text(_) => t.extend(self.a.text_targets()),
                        Content::Image(_) => t.push(self.a.png),
                    }
                    if *transient {
                        t.push(self.a.secret);
                    }
                    self.conn
                        .change_property32(PropMode::REPLACE, e.requestor, prop, AtomEnum::ATOM, &t)
                        .is_ok()
                } else if e.target == self.a.secret && *transient {
                    self.conn
                        .change_property8(PropMode::REPLACE, e.requestor, prop, e.target, b"secret")
                        .is_ok()
                } else {
                    let fits = c.len() <= max;
                    let matches = match c {
                        Content::Text(_) => self.a.text_targets().contains(&e.target),
                        Content::Image(_) => e.target == self.a.png,
                    };
                    fits && matches
                        && self
                            .conn
                            .change_property8(
                                PropMode::REPLACE,
                                e.requestor,
                                prop,
                                e.target,
                                c.bytes(),
                            )
                            .is_ok()
                }
            }
            None => false,
        };
        if !ok {
            prop = NONE;
        }
        let reply = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: prop,
        };
        self.conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, reply)?;
        self.conn.flush()?;
        Ok(())
    }

    fn take_ownership(&mut self, content: Content, transient: bool) -> Result<()> {
        self.owned = Some((content, transient));
        self.conn
            .set_selection_owner(self.win, self.a.clipboard, CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(())
    }

    fn run(mut self) -> Result<()> {
        self.request_targets(CURRENT_TIME)?;
        loop {
            match self.conn.wait_for_event()? {
                Event::XfixesSelectionNotify(e) => {
                    if e.owner == self.win {
                        continue;
                    }
                    if e.owner == NONE {
                        // The owner quit and the clipboard went with it: keep it alive.
                        if let Some(c) = self.last.clone() {
                            self.take_ownership(c, false)?;
                        }
                        continue;
                    }
                    self.request_targets(e.timestamp)?;
                }
                Event::SelectionNotify(e) => self.on_selection_notify(e)?,
                Event::PropertyNotify(e) => self.on_property(e.atom, e.state)?,
                Event::SelectionRequest(e) => self.serve(e)?,
                Event::SelectionClear(e) => {
                    if e.selection == self.a.clipboard {
                        self.owned = None;
                    }
                }
                Event::ClientMessage(e) if e.type_ == self.a.wake => {
                    let reqs: Vec<WriteReq> = std::mem::take(
                        &mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()),
                    );
                    for r in reqs {
                        self.take_ownership(r.content, r.transient)?;
                        let _ = r.done.send(());
                    }
                }
                _ => {}
            }
        }
    }
}

struct X11Clipboard {
    waker: Mutex<RustConnection>,
    win: Window,
    wake: Atom,
    pending: Arc<Mutex<Vec<WriteReq>>>,
}

impl Clipboard for X11Clipboard {
    fn write(&self, c: &Content, transient: bool) -> Result<()> {
        let (done_tx, done_rx) = channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(WriteReq {
                content: c.clone(),
                transient,
                done: done_tx,
            });
        {
            let conn = self.waker.lock().unwrap_or_else(|e| e.into_inner());
            let ev = ClientMessageEvent::new(32, self.win, self.wake, [0u32; 5]);
            conn.send_event(false, self.win, EventMask::NO_EVENT, ev)?;
            conn.flush()?;
        }
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .context("X11 clipboard did not answer")?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "x11"
    }
}

pub fn start(tx: Sender<ClipEvent>) -> Result<Arc<dyn Clipboard>> {
    let (conn, screen_num) =
        RustConnection::connect(None).context("cannot connect to the X server")?;
    let screen = &conn.setup().roots[screen_num];
    let win = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.xfixes_query_version(5, 0)?
        .reply()
        .context("the X server has no XFIXES")?;
    let a = Atoms::new(&conn)?;
    conn.xfixes_select_selection_input(
        win,
        a.clipboard,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;
    conn.flush()?;
    let (waker, _) = RustConnection::connect(None)?;
    let pending = Arc::new(Mutex::new(Vec::new()));
    let wake = a.wake;
    let worker = Worker {
        conn,
        win,
        a,
        owned: None,
        last: None,
        reading: Reading::Idle,
        tx,
        pending: pending.clone(),
    };
    std::thread::Builder::new()
        .name("x11-clipboard".into())
        .spawn(move || {
            if let Err(e) = worker.run() {
                crate::log!("X11 clipboard stopped: {e:#}");
            }
        })?;
    Ok(Arc::new(X11Clipboard {
        waker: Mutex::new(waker),
        win,
        wake,
        pending,
    }))
}
