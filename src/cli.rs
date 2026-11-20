//! Command line interface.

use std::io::{Read, Write};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::{self, Config};
use crate::content::Content;
use crate::ipc::{self, Request, Response};
use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "hotbar",
    version,
    about = "Numbered clipboard slots and clipboard history",
    long_about = "Numbered clipboard slots and clipboard history.\n\n\
        While the daemon runs:\n  \
        Ctrl+C then a digit   copy and also save to that slot\n  \
        Ctrl+V then a digit   paste that slot (keep Ctrl down, tap V, tap the digit)\n  \
        Super+V               open the history picker\n\n\
        On macOS use Cmd instead of Ctrl, and Cmd+Shift+V for the picker."
)]
pub struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

fn slot_arg() -> clap::builder::RangedI64ValueParser<u8> {
    clap::value_parser!(u8).range(1..=9)
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the background daemon (clipboard watcher and key chords).
    Daemon,
    /// Stop the running daemon.
    Stop,
    /// Save what is on the clipboard now into slot N.
    Save {
        #[arg(value_parser = slot_arg())]
        n: u8,
    },
    /// Paste slot N into the focused window.
    Paste {
        #[arg(value_parser = slot_arg())]
        n: u8,
    },
    /// Print the content of slot N.
    Get {
        #[arg(value_parser = slot_arg())]
        n: u8,
    },
    /// Fill slot N from the text argument, or from stdin when none is given.
    Set {
        #[arg(value_parser = slot_arg())]
        n: u8,
        text: Option<String>,
    },
    /// Empty slot N.
    Clear {
        #[arg(value_parser = slot_arg())]
        n: u8,
    },
    /// Show the nine slots.
    Slots,
    /// List history as "id<TAB>preview", newest first (cliphist compatible).
    List {
        #[arg(short, long, default_value_t = 0)]
        limit: usize,
    },
    /// Print an entry. Takes an id, or a "id<TAB>preview" line on stdin.
    Decode { id: Option<String> },
    /// Put a history entry back on the clipboard. Takes an id or a line on stdin.
    Copy { id: Option<String> },
    /// Delete an entry. Takes an id or a line on stdin.
    Delete { id: Option<String> },
    /// Open the history picker.
    Pick {
        /// Use this dmenu style command instead of the configured picker.
        #[arg(long)]
        dmenu: Option<String>,
        /// Force the built-in window.
        #[arg(long)]
        builtin: bool,
        #[arg(long, hide = true, default_value_t = 0)]
        return_to: isize,
    },
    /// Delete history. Slots are kept unless --slots is given.
    Wipe {
        #[arg(long)]
        slots: bool,
    },
    /// Set up permissions for key chords (Linux and macOS).
    Setup {
        /// Do not ask before running commands.
        #[arg(short, long)]
        yes: bool,
    },
    /// Check what works on this machine.
    Doctor,
    /// Start hotbar at login, or stop doing so.
    Autostart {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Show the config path, or write the default config.
    Config {
        /// Write the default config file.
        #[arg(long)]
        init: bool,
        /// Overwrite an existing file with --init.
        #[arg(long)]
        force: bool,
    },
}

/// Send to the daemon if it runs, otherwise act on the store directly.
fn request(req: Request) -> Result<Response> {
    match ipc::send(&req) {
        Ok(r) => r.into_result(),
        Err(e) => {
            let cfg = Config::load().unwrap_or_default();
            let mut store = Store::open(&config::data_dir())?;
            match crate::daemon::handle_store(&mut store, &cfg.limits(), &req) {
                Some(r) => r.into_result(),
                None => Err(e),
            }
        }
    }
}

fn read_id(arg: Option<String>) -> Result<u64> {
    let line = match arg {
        Some(a) => a,
        None => {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s
        }
    };
    crate::daemon::parse_id(&line).with_context(|| format!("no entry id in {:?}", line.trim()))
}

fn print_data(r: Response) -> Result<()> {
    if let Response::Data { data, .. } = r {
        let bytes = ipc::decode(&data)?;
        let mut out = std::io::stdout().lock();
        out.write_all(&bytes)?;
        out.flush()?;
        Ok(())
    } else {
        bail!("unexpected reply")
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon => crate::daemon::run(),
        Cmd::Stop => {
            ipc::send(&Request::Stop)?.into_result()?;
            println!("stopped");
            Ok(())
        }
        Cmd::Save { n } => {
            ipc::send(&Request::SaveSlot { n })?.into_result()?;
            Ok(())
        }
        Cmd::Paste { n } => {
            ipc::send(&Request::PasteSlot { n, delay_ms: 0 })?.into_result()?;
            Ok(())
        }
        Cmd::Get { n } => print_data(request(Request::GetSlot { n })?),
        Cmd::Set { n, text } => {
            let c = match text {
                Some(t) => Content::Text(t),
                None => {
                    let mut b = Vec::new();
                    std::io::stdin().read_to_end(&mut b)?;
                    Content::sniff(b)
                }
            };
            request(Request::SetSlotData {
                n,
                kind: c.kind(),
                data: ipc::encode(c.bytes()),
            })?;
            Ok(())
        }
        Cmd::Clear { n } => {
            request(Request::ClearSlot { n })?;
            Ok(())
        }
        Cmd::Slots => {
            if let Response::Slots { slots } = request(Request::Slots)? {
                for s in slots {
                    match s.entry {
                        Some(m) => println!("{}\t{}", s.n, m.preview),
                        None => println!("{}\t", s.n),
                    }
                }
            }
            Ok(())
        }
        Cmd::List { limit } => {
            if let Response::Entries { entries } = request(Request::List { limit })? {
                let mut out = std::io::stdout().lock();
                for m in entries {
                    if writeln!(out, "{}\t{}", m.id, m.preview).is_err() {
                        break;
                    }
                }
            }
            Ok(())
        }
        Cmd::Decode { id } => print_data(request(Request::Get { id: read_id(id)? })?),
        Cmd::Copy { id } => {
            ipc::send(&Request::CopyEntry { id: read_id(id)? })?.into_result()?;
            Ok(())
        }
        Cmd::Delete { id } => {
            request(Request::Delete { id: read_id(id)? })?;
            Ok(())
        }
        Cmd::Pick { dmenu, builtin, return_to } => {
            let cfg = Config::load().unwrap_or_default();
            let command = dmenu.unwrap_or(cfg.picker.command);
            if builtin || command.trim() == "builtin" {
                crate::picker::run_builtin(return_to)
            } else {
                crate::picker::run_dmenu(&command)
            }
        }
        Cmd::Wipe { slots } => {
            request(Request::Wipe { slots })?;
            Ok(())
        }
        Cmd::Setup { yes } => crate::system::setup(yes),
        Cmd::Doctor => crate::system::doctor(),
        Cmd::Autostart { state } => {
            println!("{}", crate::system::autostart(state == "on")?);
            Ok(())
        }
        Cmd::Config { init, force } => {
            if init {
                let p = config::write_default_config(force)?;
                println!("wrote {}", p.display());
            } else {
                println!("{}", config::config_path().display());
            }
            Ok(())
        }
    }
}
