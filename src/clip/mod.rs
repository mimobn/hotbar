//! Clipboard backends: watch for changes and write content.

use std::sync::mpsc::Sender;
use std::sync::Arc;

use anyhow::Result;

use crate::content::Content;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

pub struct ClipEvent {
    pub content: Content,
    /// A password manager marked this copy as secret.
    pub sensitive: bool,
}

pub trait Clipboard: Send + Sync {
    /// Put content on the clipboard. `transient` marks it so clipboard
    /// managers (including this one) do not record it.
    fn write(&self, c: &Content, transient: bool) -> Result<()>;
    fn name(&self) -> &'static str;
}

/// Start the platform watcher. Every clipboard change is sent on `tx`.
pub fn start(tx: Sender<ClipEvent>) -> Result<Arc<dyn Clipboard>> {
    #[cfg(windows)]
    {
        windows::start(tx)
    }
    #[cfg(target_os = "macos")]
    {
        macos::start(tx)
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match wayland::start(tx.clone()) {
                Ok(c) => return Ok(c),
                Err(e) => {
                    crate::log!("wayland clipboard unavailable: {e:#}");
                    if std::env::var_os("DISPLAY").is_none() {
                        return Err(e);
                    }
                    crate::log!("falling back to X11 (XWayland)");
                }
            }
        }
        x11::start(tx)
    }
}

#[cfg(target_os = "linux")]
pub fn wayland_data_control_available() -> bool {
    wayland::available()
}

/// Text mime types in the order we prefer them.
#[cfg(any(target_os = "linux", test))]
pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

#[cfg(any(target_os = "linux", test))]
pub const SECRET_MIME: &str = "x-kde-passwordManagerHint";

/// Pick what to read from a list of offered mime types.
#[cfg(any(target_os = "linux", test))]
pub fn choose_mime(offered: &[String]) -> Option<(&'static str, crate::content::Kind)> {
    use crate::content::Kind;
    for m in TEXT_MIMES {
        if offered.iter().any(|o| o == m) {
            return Some((m, Kind::Text));
        }
    }
    if offered.iter().any(|o| o == "image/png") {
        return Some(("image/png", Kind::Image));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::Kind;

    #[test]
    fn prefers_utf8_text() {
        let o: Vec<String> = ["image/png", "text/plain", "text/plain;charset=utf-8"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            choose_mime(&o),
            Some(("text/plain;charset=utf-8", Kind::Text))
        );
        let o = vec!["image/png".to_string(), "text/html".to_string()];
        assert_eq!(choose_mime(&o), Some(("image/png", Kind::Image)));
        assert_eq!(choose_mime(&["text/html".to_string()]), None);
    }
}
