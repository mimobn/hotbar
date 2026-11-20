//! Config file (TOML), paths and theme colors.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::chord::{EngineConfig, Hotkey, Mode, Primary};
use crate::store::Limits;

pub const DEFAULT_CONFIG: &str = include_str!("../packaging/config.toml");

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub history: History,
    pub keys: Keys,
    pub paste: Paste,
    pub picker: Picker,
    pub theme: Theme,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct History {
    pub max_items: usize,
    pub max_age_days: u64,
    pub min_length: usize,
    pub max_length: usize,
    pub max_image_mb: usize,
    pub ignore_sensitive: bool,
    pub ignore_blank: bool,
}

impl Default for History {
    fn default() -> Self {
        History {
            max_items: 500,
            max_age_days: 0,
            min_length: 1,
            max_length: 0,
            max_image_mb: 20,
            ignore_sensitive: true,
            ignore_blank: true,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct Keys {
    /// "ctrl-held", "release" or "off".
    pub chord: String,
    pub timeout_ms: u64,
    /// "auto" (Cmd on macOS, Ctrl elsewhere), "ctrl" or "cmd".
    pub primary: String,
    /// Hotkey that opens the history picker, for example "super+v". Empty to disable.
    pub picker: String,
    pub copy_key: char,
    pub paste_key: char,
    pub numpad: bool,
}

/// Picker hotkey per platform. Windows keeps Win+V for its own clipboard.
pub fn default_picker() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd+shift+v"
    } else if cfg!(windows) {
        "ctrl+alt+v"
    } else {
        "super+v"
    }
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            chord: "ctrl-held".into(),
            timeout_ms: 300,
            primary: "auto".into(),
            picker: default_picker().into(),
            copy_key: 'c',
            paste_key: 'v',
            numpad: true,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct Paste {
    pub restore_clipboard: bool,
    pub restore_delay_ms: u64,
    pub notify: bool,
}

impl Default for Paste {
    fn default() -> Self {
        Paste {
            restore_clipboard: true,
            restore_delay_ms: 300,
            notify: true,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct Picker {
    /// "builtin", or a dmenu style command such as "fuzzel --dmenu" or "rofi -dmenu".
    pub command: String,
    pub width: f32,
    pub height: f32,
    pub close_on_unfocus: bool,
}

impl Default for Picker {
    fn default() -> Self {
        Picker {
            command: "builtin".into(),
            width: 640.0,
            height: 460.0,
            close_on_unfocus: true,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct Theme {
    /// Take colors from ~/.cache/wal/colors.json when it exists.
    pub pywal: bool,
    pub background: String,
    pub surface: String,
    pub foreground: String,
    pub muted: String,
    pub accent: String,
    pub radius: f32,
    pub font_size: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            pywal: false,
            background: "#1e1e2ef5".into(),
            surface: "#313244".into(),
            foreground: "#cdd6f4".into(),
            muted: "#7f849c".into(),
            accent: "#89b4fa".into(),
            radius: 12.0,
            font_size: 15.0,
        }
    }
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(_) => Ok(Config::default()),
        }
    }

    pub fn limits(&self) -> Limits {
        Limits {
            max_items: self.history.max_items,
            max_age_days: self.history.max_age_days,
            min_length: self.history.min_length,
            max_length: self.history.max_length,
            max_image_bytes: self.history.max_image_mb * 1024 * 1024,
            ignore_blank: self.history.ignore_blank,
        }
    }

    pub fn engine(&self) -> EngineConfig {
        let mode = match self.keys.chord.as_str() {
            "release" => Mode::Release,
            "off" | "none" | "false" => Mode::Off,
            _ => Mode::CtrlHeld,
        };
        let primary = match self.keys.primary.as_str() {
            "ctrl" | "control" => Primary::Ctrl,
            "cmd" | "command" | "meta" | "super" => Primary::Meta,
            _ => {
                if cfg!(target_os = "macos") {
                    Primary::Meta
                } else {
                    Primary::Ctrl
                }
            }
        };
        let picker = if self.keys.picker.trim().is_empty() {
            None
        } else {
            match Hotkey::parse(&self.keys.picker) {
                Some(h) => Some(h),
                None => {
                    crate::log!(
                        "could not read picker hotkey {:?}, using none",
                        self.keys.picker
                    );
                    None
                }
            }
        };
        EngineConfig {
            primary,
            mode,
            timeout: Duration::from_millis(self.keys.timeout_ms.max(50)),
            copy_key: self.keys.copy_key.to_ascii_lowercase(),
            paste_key: self.keys.paste_key.to_ascii_lowercase(),
            picker,
        }
    }
}

fn dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "hotbar")
}

/// Config file location. `HOTBAR_CONFIG` overrides it. On Linux and macOS
/// `$XDG_CONFIG_HOME/hotbar/config.toml` (default `~/.config`) is used, since
/// that is where people keep their dotfiles.
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("HOTBAR_CONFIG") {
        return PathBuf::from(p);
    }
    #[cfg(unix)]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        if let Some(b) = base {
            return b.join("hotbar").join("config.toml");
        }
    }
    dirs()
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("hotbar.toml"))
}

/// History and slots location. `HOTBAR_DATA` overrides it.
pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("HOTBAR_DATA") {
        return PathBuf::from(p);
    }
    dirs()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("hotbar"))
}

pub fn write_default_config(force: bool) -> Result<PathBuf> {
    let path = config_path();
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, DEFAULT_CONFIG)?;
    Ok(path)
}

/// Parse "#rgb", "#rrggbb" or "#rrggbbaa".
pub fn parse_color(s: &str) -> Option<[u8; 4]> {
    let h = s.trim().trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    match h.len() {
        3 => {
            let d = |i: usize| {
                u8::from_str_radix(h.get(i..i + 1)?, 16)
                    .ok()
                    .map(|v| v * 17)
            };
            Some([d(0)?, d(1)?, d(2)?, 255])
        }
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

impl Theme {
    /// Apply pywal colors if enabled and available.
    pub fn resolved(&self) -> Theme {
        let mut t = self.clone();
        if !self.pywal {
            return t;
        }
        let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        else {
            return t;
        };
        let path = PathBuf::from(home)
            .join(".cache")
            .join("wal")
            .join("colors.json");
        let Ok(raw) = fs::read_to_string(path) else {
            return t;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return t;
        };
        let get = |a: &str, b: &str| {
            v.get(a)
                .and_then(|x| x.get(b))
                .and_then(|x| x.as_str())
                .map(String::from)
        };
        if let Some(bg) = get("special", "background") {
            t.background = format!("{bg}f5");
        }
        if let Some(fg) = get("special", "foreground") {
            t.foreground = fg;
        }
        if let Some(c) = get("colors", "color0") {
            t.surface = c;
        }
        if let Some(c) = get("colors", "color8") {
            t.muted = c;
        }
        if let Some(c) = get("colors", "color4") {
            t.accent = c;
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_parses_and_matches_defaults() {
        let c: Config = toml::from_str(DEFAULT_CONFIG).expect("packaging/config.toml must parse");
        let d = Config::default();
        assert_eq!(c.history.max_items, d.history.max_items);
        assert_eq!(c.keys.chord, d.keys.chord);
        assert_eq!(c.paste.restore_delay_ms, d.paste.restore_delay_ms);
        assert_eq!(c.picker.command, d.picker.command);
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#fff"), Some([255, 255, 255, 255]));
        assert_eq!(parse_color("#102030"), Some([16, 32, 48, 255]));
        assert_eq!(parse_color("10203040"), Some([16, 32, 48, 64]));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn partial_config_keeps_defaults() {
        let c: Config = toml::from_str("[keys]\nchord = \"release\"\n").unwrap();
        assert_eq!(c.keys.chord, "release");
        assert_eq!(c.keys.paste_key, 'v');
        assert_eq!(c.history.max_items, 500);
    }
}
