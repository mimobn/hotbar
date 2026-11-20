//! hotbar: numbered clipboard slots and clipboard history.

pub mod chord;
pub mod cli;
pub mod clip;
pub mod config;
pub mod content;
pub mod daemon;
pub mod input;
pub mod ipc;
pub mod logging;
pub mod picker;
pub mod system;
pub mod store;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
