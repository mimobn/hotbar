//! History pickers: the built-in window, or any dmenu style program.

use std::io::{BufRead, Write};
use std::process::Stdio;

use anyhow::{Context, Result};

use crate::ipc::{self, Request, Response, SlotInfo};
use crate::store::EntryMeta;

mod builtin;

pub use builtin::run as run_builtin;

pub fn fetch() -> Result<(Vec<SlotInfo>, Vec<EntryMeta>)> {
    let slots = match ipc::send(&Request::Slots)?.into_result()? {
        Response::Slots { slots } => slots,
        _ => Vec::new(),
    };
    let entries = match ipc::send(&Request::List { limit: 0 })?.into_result()? {
        Response::Entries { entries } => entries,
        _ => Vec::new(),
    };
    Ok((slots, entries))
}

/// Lines in cliphist style: "id<TAB>preview". Slots come first, tagged [n].
pub fn menu_lines(slots: &[SlotInfo], entries: &[EntryMeta]) -> String {
    let mut out = String::new();
    for s in slots {
        if let Some(m) = &s.entry {
            out.push_str(&format!("{}\t[{}] {}\n", m.id, s.n, m.preview));
        }
    }
    for m in entries {
        out.push_str(&format!("{}\t{}\n", m.id, m.preview));
    }
    out
}

/// Feed history to a dmenu style program and paste what it returns.
pub fn run_dmenu(command: &str) -> Result<()> {
    let (slots, entries) = fetch()?;
    let lines = menu_lines(&slots, &entries);
    let mut child = crate::system::shell(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {command:?}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(lines.as_bytes());
    }
    let mut choice = String::new();
    if let Some(out) = child.stdout.take() {
        let _ = std::io::BufReader::new(out).read_line(&mut choice);
    }
    let _ = child.wait();
    if let Some(id) = crate::daemon::parse_id(&choice) {
        ipc::send(&Request::PasteEntry { id, delay_ms: 80 })?.into_result()?;
    }
    Ok(())
}
