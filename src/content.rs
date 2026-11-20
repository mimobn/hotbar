//! What a clipboard item is: text or a PNG image.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Image,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// PNG encoded bytes.
    Image(Vec<u8>),
}

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

impl Content {
    pub fn kind(&self) -> Kind {
        match self {
            Content::Text(_) => Kind::Text,
            Content::Image(_) => Kind::Image,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Content::Text(s) => s.as_bytes(),
            Content::Image(b) => b,
        }
    }

    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }

    pub fn from_parts(kind: Kind, bytes: Vec<u8>) -> Content {
        match kind {
            Kind::Text => Content::Text(String::from_utf8_lossy(&bytes).into_owned()),
            Kind::Image => Content::Image(bytes),
        }
    }

    /// Guess the kind from raw bytes (used for `hotbar set N < file`).
    pub fn sniff(bytes: Vec<u8>) -> Content {
        if bytes.starts_with(PNG_MAGIC) {
            Content::Image(bytes)
        } else {
            Content::Text(String::from_utf8_lossy(&bytes).into_owned())
        }
    }

    /// Stable 64 bit FNV-1a hash, used to spot duplicates.
    pub fn hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let tag = [self.kind() as u8];
        for b in tag.iter().chain(self.bytes()) {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    /// One line summary for lists and menus.
    pub fn preview(&self, max_chars: usize) -> String {
        match self {
            Content::Text(s) => {
                let mut out = String::new();
                let mut last_space = false;
                for c in s.trim().chars() {
                    let c = if c.is_whitespace() { ' ' } else { c };
                    if c == ' ' && last_space {
                        continue;
                    }
                    last_space = c == ' ';
                    out.push(c);
                    if out.chars().count() >= max_chars {
                        out.push_str("...");
                        break;
                    }
                }
                out
            }
            Content::Image(b) => match png_size(b) {
                Some((w, h)) => format!("[image {w}x{h}, {}]", human_size(b.len())),
                None => format!("[image, {}]", human_size(b.len())),
            },
        }
    }

    pub fn is_blank(&self) -> bool {
        match self {
            Content::Text(s) => s.trim().is_empty(),
            Content::Image(b) => b.is_empty(),
        }
    }
}

pub fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || !b.starts_with(PNG_MAGIC) {
        return None;
    }
    let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
    Some((w, h))
}

pub fn human_size(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Decode any image the `image` crate understands and return PNG bytes.
pub fn to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_collapses_whitespace() {
        let c = Content::Text("  hello\n\n  world\tagain ".into());
        assert_eq!(c.preview(100), "hello world again");
    }

    #[test]
    fn preview_truncates() {
        let c = Content::Text("abcdefghij".into());
        assert_eq!(c.preview(4), "abcd...");
    }

    #[test]
    fn hash_differs_by_kind() {
        let a = Content::Text("x".into());
        let b = Content::Image(b"x".to_vec());
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash(), Content::Text("x".into()).hash());
    }

    #[test]
    fn sniff_png() {
        let mut b = PNG_MAGIC.to_vec();
        b.extend_from_slice(&[0; 20]);
        assert_eq!(Content::sniff(b).kind(), Kind::Image);
        assert_eq!(Content::sniff(b"hi".to_vec()).kind(), Kind::Text);
    }
}
