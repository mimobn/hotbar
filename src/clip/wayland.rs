//! Wayland clipboard through the wlr data-control protocol.
//!
//! Supported by Hyprland, Sway, KDE Plasma, niri, river, Wayfire, labwc and
//! most wlroots compositors. GNOME does not offer it.

use std::collections::HashMap;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{
    self, ZwlrDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{
    self, ZwlrDataControlSourceV1,
};

use super::{choose_mime, ClipEvent, Clipboard, SECRET_MIME, TEXT_MIMES};
use crate::content::Content;

const MAX_BYTES: usize = 64 * 1024 * 1024;

struct Payload {
    content: Content,
    transient: bool,
}

struct WriteReq {
    payload: Arc<Payload>,
    done: Sender<()>,
}

struct State {
    offers: HashMap<ObjectId, Vec<String>>,
    current: Option<ZwlrDataControlOfferV1>,
    tx: Sender<ClipEvent>,
    finished: bool,
    /// Last thing copied, so the clipboard survives the app that owned it closing.
    last: Arc<Mutex<Option<Content>>>,
    handles: Option<(ZwlrDataControlManagerV1, ZwlrDataControlDeviceV1)>,
}

fn offer_content(
    manager: &ZwlrDataControlManagerV1,
    device: &ZwlrDataControlDeviceV1,
    qh: &QueueHandle<State>,
    payload: Arc<Payload>,
) {
    let mimes: Vec<&str> = match payload.content {
        Content::Text(_) => TEXT_MIMES.to_vec(),
        Content::Image(_) => vec!["image/png"],
    };
    let transient = payload.transient;
    let source = manager.create_data_source(qh, payload);
    for m in mimes {
        source.offer(m.to_string());
    }
    if transient {
        source.offer(SECRET_MIME.to_string());
    }
    device.set_selection(Some(&source));
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if let Some(old) = state.current.take() {
                    state.offers.remove(&old.id());
                    old.destroy();
                }
                match id {
                    Some(offer) => {
                        state.read_offer(&offer, conn);
                        state.current = Some(offer);
                    }
                    None => {
                        // The owner went away and took the clipboard with it: put it back.
                        let last = state.last.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if let (Some(content), Some((m, d))) = (last, &state.handles) {
                            offer_content(
                                m,
                                d,
                                qh,
                                Arc::new(Payload {
                                    content,
                                    transient: false,
                                }),
                            );
                        }
                    }
                }
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(o) } => {
                state.offers.remove(&o.id());
                o.destroy();
            }
            zwlr_data_control_device_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, Arc<Payload>> for State {
    fn event(
        _: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        data: &Arc<Payload>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                let bytes = if mime_type == SECRET_MIME {
                    b"secret".to_vec()
                } else {
                    data.content.bytes().to_vec()
                };
                std::thread::spawn(move || {
                    let mut f = std::fs::File::from(fd);
                    let _ = f.write_all(&bytes);
                });
            }
            zwlr_data_control_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

/// Read a pipe to the end, giving up if the other side stalls.
fn read_all(mut r: PipeReader) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let mut pfd = libc::pollfd {
            fd: r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, 3000) };
        if n <= 0 {
            return None;
        }
        match r.read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(k) => {
                out.extend_from_slice(&buf[..k]);
                if out.len() > MAX_BYTES {
                    return None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

impl State {
    fn read_offer(&self, offer: &ZwlrDataControlOfferV1, conn: &Connection) {
        let mimes = self.offers.get(&offer.id()).cloned().unwrap_or_default();
        let sensitive = mimes.iter().any(|m| m == SECRET_MIME);
        let Some((mime, kind)) = choose_mime(&mimes) else {
            return;
        };
        let Ok((reader, writer)) = std::io::pipe() else {
            return;
        };
        offer.receive(mime.to_string(), writer.as_fd());
        let _ = conn.flush();
        drop(writer);
        let tx = self.tx.clone();
        let last = self.last.clone();
        std::thread::spawn(move || {
            if let Some(bytes) = read_all(reader) {
                let content = Content::from_parts(kind, bytes);
                // Never bring back a secret after a password manager cleared it.
                if !content.is_empty() && !sensitive {
                    *last.lock().unwrap_or_else(|e| e.into_inner()) = Some(content.clone());
                }
                let _ = tx.send(ClipEvent { content, sensitive });
            }
        });
    }
}

struct WaylandClipboard {
    tx: Mutex<Sender<WriteReq>>,
    wake: Mutex<PipeWriter>,
}

impl Clipboard for WaylandClipboard {
    fn write(&self, c: &Content, transient: bool) -> Result<()> {
        let (done_tx, done_rx) = channel();
        let req = WriteReq {
            payload: Arc::new(Payload {
                content: c.clone(),
                transient,
            }),
            done: done_tx,
        };
        self.tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(req)
            .context("wayland clipboard thread is gone")?;
        let _ = self
            .wake
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(&[1]);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .context("wayland clipboard did not answer")?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "wayland"
    }
}

pub fn available() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<State>(&conn) else {
        return false;
    };
    globals.contents().with_list(|l| {
        l.iter()
            .any(|g| g.interface == "zwlr_data_control_manager_v1")
    })
}

pub fn start(tx: Sender<ClipEvent>) -> Result<Arc<dyn Clipboard>> {
    let conn = Connection::connect_to_env().context("cannot connect to the Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).context("no wl_seat")?;
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .context("the compositor does not support wlr data-control")?;
    let device = manager.get_data_device(&seat, &qh, ());
    let (wtx, wrx): (Sender<WriteReq>, Receiver<WriteReq>) = channel();
    let (mut wake_r, wake_w) = std::io::pipe()?;
    let mut state = State {
        offers: HashMap::new(),
        current: None,
        tx,
        finished: false,
        last: Arc::new(Mutex::new(None)),
        handles: Some((manager.clone(), device.clone())),
    };
    std::thread::Builder::new()
        .name("wayland-clipboard".into())
        .spawn(move || {
            let _seat = seat;
            loop {
                if let Err(e) = queue.dispatch_pending(&mut state) {
                    crate::log!("wayland dispatch failed: {e}");
                    break;
                }
                while let Ok(req) = wrx.try_recv() {
                    offer_content(&manager, &device, &qh, req.payload.clone());
                    let _ = conn.flush();
                    let _ = req.done.send(());
                }
                if state.finished {
                    crate::log!("wayland data device finished");
                    break;
                }
                if queue.flush().is_err() {
                    break;
                }
                let Some(guard) = queue.prepare_read() else {
                    continue;
                };
                let mut fds = [
                    libc::pollfd {
                        fd: guard.connection_fd().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: wake_r.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: two valid pollfds.
                let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if n < 0 {
                    drop(guard);
                    continue;
                }
                if fds[0].revents & libc::POLLIN != 0 {
                    if let Err(e) = guard.read() {
                        crate::log!("wayland read failed: {e}");
                        break;
                    }
                } else {
                    drop(guard);
                }
                if fds[1].revents & libc::POLLIN != 0 {
                    let mut b = [0u8; 64];
                    let _ = wake_r.read(&mut b);
                }
            }
        })?;
    Ok(Arc::new(WaylandClipboard {
        tx: Mutex::new(wtx),
        wake: Mutex::new(wake_w),
    }))
}
