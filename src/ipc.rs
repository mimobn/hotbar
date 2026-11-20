//! Requests between the CLI (or picker) and the daemon.
//!
//! One JSON line per request, one JSON line back, over a local socket
//! (a named pipe on Windows, a unix socket in a private directory elsewhere).

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;
use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use serde::{Deserialize, Serialize};

use crate::content::{Content, Kind};
use crate::store::EntryMeta;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    List {
        limit: usize,
    },
    Slots,
    Get {
        id: u64,
    },
    GetSlot {
        n: u8,
    },
    SetSlotData {
        n: u8,
        kind: Kind,
        data: String,
    },
    SetSlotEntry {
        n: u8,
        id: u64,
    },
    ClearSlot {
        n: u8,
    },
    Delete {
        id: u64,
    },
    Wipe {
        slots: bool,
    },
    /// Current clipboard into slot n.
    SaveSlot {
        n: u8,
    },
    /// Paste slot n into the focused window.
    PasteSlot {
        n: u8,
        #[serde(default)]
        delay_ms: u64,
    },
    /// Put an entry on the clipboard and paste it (used by pickers).
    PasteEntry {
        id: u64,
        delay_ms: u64,
    },
    /// Put an entry on the clipboard without pasting.
    CopyEntry {
        id: u64,
    },
    OpenPicker,
    Stop,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SlotInfo {
    pub n: u8,
    pub entry: Option<EntryMeta>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong { pid: u32, version: String },
    Error { message: String },
    Entries { entries: Vec<EntryMeta> },
    Slots { slots: Vec<SlotInfo> },
    Data { kind: Kind, data: String },
}

impl Response {
    pub fn err(e: impl std::fmt::Display) -> Response {
        Response::Error {
            message: e.to_string(),
        }
    }

    pub fn data(c: &Content) -> Response {
        Response::Data {
            kind: c.kind(),
            data: encode(c.bytes()),
        }
    }

    pub fn into_result(self) -> Result<Response> {
        match self {
            Response::Error { message } => Err(anyhow::anyhow!(message)),
            r => Ok(r),
        }
    }
}

pub fn encode(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

pub fn decode(s: &str) -> Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

fn user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

#[cfg(windows)]
fn name() -> Result<Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    Ok(format!("hotbar-{}", user()).to_ns_name::<GenericNamespaced>()?)
}

#[cfg(unix)]
pub fn socket_path() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(dir).join("hotbar.sock");
    }
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("hotbar-{uid}-{}.sock", user()))
}

#[cfg(unix)]
fn name() -> Result<Name<'static>> {
    use interprocess::local_socket::GenericFilePath;
    Ok(socket_path().to_fs_name::<GenericFilePath>()?)
}

/// Send one request to the running daemon. Errors if none is running.
pub fn send(req: &Request) -> Result<Response> {
    let conn = Stream::connect(name()?)
        .context("hotbar daemon is not running (start it with `hotbar daemon`)")?;
    let mut conn = BufReader::new(conn);
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    conn.get_mut().write_all(line.as_bytes())?;
    conn.get_mut().flush()?;
    let mut reply = String::new();
    conn.read_line(&mut reply)?;
    anyhow::ensure!(!reply.is_empty(), "daemon closed the connection");
    Ok(serde_json::from_str(&reply)?)
}

pub fn daemon_running() -> bool {
    matches!(send(&Request::Ping), Ok(Response::Pong { .. }))
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Accept connections forever, one thread per connection.
pub fn serve(handler: Handler) -> Result<()> {
    #[cfg(unix)]
    {
        let p = socket_path();
        // A stale socket from a crashed daemon would block the bind.
        let _ = std::fs::remove_file(&p);
    }
    let listener = ListenerOptions::new()
        .name(name()?)
        .create_sync()
        .context("could not open the hotbar socket")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_path(), std::fs::Permissions::from_mode(0o600));
    }
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                crate::log!("socket accept failed: {e}");
                continue;
            }
        };
        let handler = handler.clone();
        std::thread::spawn(move || {
            let mut conn = BufReader::new(conn);
            let mut line = String::new();
            if conn.read_line(&mut line).is_err() || line.trim().is_empty() {
                return;
            }
            let resp = match serde_json::from_str::<Request>(&line) {
                Ok(req) => handler(req),
                Err(e) => Response::err(format!("bad request: {e}")),
            };
            if let Ok(mut out) = serde_json::to_string(&resp) {
                out.push('\n');
                let _ = conn.get_mut().write_all(out.as_bytes());
                let _ = conn.get_mut().flush();
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape() {
        let j = serde_json::to_string(&Request::Ping).unwrap();
        assert_eq!(j, r#"{"op":"ping"}"#);
        let r: Request = serde_json::from_str(r#"{"op":"paste_slot","n":3}"#).unwrap();
        assert!(matches!(r, Request::PasteSlot { n: 3, delay_ms: 0 }));
        let r: Request = serde_json::from_str(r#"{"op":"list","limit":5}"#).unwrap();
        assert!(matches!(r, Request::List { limit: 5 }));
    }

    #[test]
    fn base64_roundtrip() {
        let b = vec![0u8, 1, 2, 250, 255];
        assert_eq!(decode(&encode(&b)).unwrap(), b);
    }
}
