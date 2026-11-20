//! History and slots on disk.
//!
//! Layout inside the data dir:
//!   index.json     entry metadata (newest first) and the slot table
//!   blobs/<id>     raw bytes of each entry
//!
//! Plain files on purpose: easy to inspect, back up or wipe by hand.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::content::{Content, Kind};

pub const SLOT_COUNT: u8 = 9;
const PREVIEW_CHARS: usize = 120;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct EntryMeta {
    pub id: u64,
    pub kind: Kind,
    pub hash: u64,
    pub size: usize,
    pub preview: String,
    pub created: u64,
    pub last_used: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct Index {
    next_id: u64,
    /// Newest first.
    entries: Vec<EntryMeta>,
    slots: BTreeMap<u8, u64>,
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_items: usize,
    pub max_age_days: u64,
    pub min_length: usize,
    pub max_length: usize,
    pub max_image_bytes: usize,
    pub ignore_blank: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_items: 500,
            max_age_days: 0,
            min_length: 1,
            max_length: 0,
            max_image_bytes: 20 * 1024 * 1024,
            ignore_blank: true,
        }
    }
}

impl Limits {
    /// Should this item go into history at all?
    pub fn accepts(&self, c: &Content) -> bool {
        if self.ignore_blank && c.is_blank() {
            return false;
        }
        match c {
            Content::Text(s) => {
                let n = s.chars().count();
                n >= self.min_length && (self.max_length == 0 || n <= self.max_length)
            }
            Content::Image(b) => self.max_image_bytes == 0 || b.len() <= self.max_image_bytes,
        }
    }
}

pub struct Store {
    dir: PathBuf,
    index: Index,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        fs::create_dir_all(dir.join("blobs"))
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("index.json");
        let index = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                crate::log!("index.json is damaged ({e}), starting a fresh history");
                Index::default()
            }),
            Err(_) => Index::default(),
        };
        Ok(Store {
            dir: dir.to_path_buf(),
            index,
        })
    }

    pub fn reload(&mut self) -> Result<()> {
        *self = Store::open(&self.dir)?;
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let tmp = self.dir.join("index.json.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(&self.index)?)?;
        f.sync_all().ok();
        drop(f);
        fs::rename(&tmp, self.dir.join("index.json"))?;
        Ok(())
    }

    fn blob(&self, id: u64) -> PathBuf {
        self.dir.join("blobs").join(id.to_string())
    }

    pub fn list(&self) -> &[EntryMeta] {
        &self.index.entries
    }

    pub fn meta(&self, id: u64) -> Option<&EntryMeta> {
        self.index.entries.iter().find(|e| e.id == id)
    }

    pub fn get(&self, id: u64) -> Result<Option<Content>> {
        let Some(meta) = self.meta(id) else {
            return Ok(None);
        };
        let bytes = fs::read(self.blob(id)).with_context(|| format!("reading entry {id}"))?;
        Ok(Some(Content::from_parts(meta.kind, bytes)))
    }

    /// Add an item to history (or move an identical one to the top).
    /// Returns the entry id.
    pub fn add(&mut self, c: &Content, limits: &Limits) -> Result<u64> {
        let hash = c.hash();
        let now = now_secs();
        if let Some(pos) = self
            .index
            .entries
            .iter()
            .position(|e| e.hash == hash && e.size == c.len())
        {
            let id = self.index.entries[pos].id;
            let same = fs::read(self.blob(id))
                .map(|b| b == c.bytes())
                .unwrap_or(false);
            if same {
                let mut e = self.index.entries.remove(pos);
                e.last_used = now;
                self.index.entries.insert(0, e);
                self.prune(limits);
                self.save()?;
                return Ok(id);
            }
        }
        self.index.next_id += 1;
        let id = self.index.next_id;
        fs::write(self.blob(id), c.bytes())?;
        self.index.entries.insert(
            0,
            EntryMeta {
                id,
                kind: c.kind(),
                hash,
                size: c.len(),
                preview: c.preview(PREVIEW_CHARS),
                created: now,
                last_used: now,
            },
        );
        self.prune(limits);
        self.save()?;
        Ok(id)
    }

    /// Move an entry to the top of history.
    pub fn touch(&mut self, id: u64) -> Result<()> {
        if let Some(pos) = self.index.entries.iter().position(|e| e.id == id) {
            let mut e = self.index.entries.remove(pos);
            e.last_used = now_secs();
            self.index.entries.insert(0, e);
            self.save()?;
        }
        Ok(())
    }

    pub fn delete(&mut self, id: u64) -> Result<bool> {
        let before = self.index.entries.len();
        self.index.entries.retain(|e| e.id != id);
        self.index.slots.retain(|_, v| *v != id);
        let removed = self.index.entries.len() != before;
        if removed {
            fs::remove_file(self.blob(id)).ok();
            self.save()?;
        }
        Ok(removed)
    }

    pub fn slot(&self, n: u8) -> Option<u64> {
        self.index.slots.get(&n).copied()
    }

    pub fn slots(&self) -> Vec<(u8, Option<EntryMeta>)> {
        (1..=SLOT_COUNT)
            .map(|n| (n, self.slot(n).and_then(|id| self.meta(id).cloned())))
            .collect()
    }

    pub fn set_slot(&mut self, n: u8, id: u64) -> Result<()> {
        anyhow::ensure!((1..=SLOT_COUNT).contains(&n), "slot must be 1 to 9");
        anyhow::ensure!(self.meta(id).is_some(), "no entry with id {id}");
        self.index.slots.insert(n, id);
        self.save()
    }

    /// Store content and put it in slot n. Ignores history limits on purpose:
    /// if you asked for it, you get it.
    pub fn set_slot_content(&mut self, n: u8, c: &Content, limits: &Limits) -> Result<u64> {
        let mut relaxed = limits.clone();
        relaxed.min_length = 0;
        relaxed.max_length = 0;
        relaxed.max_image_bytes = 0;
        relaxed.ignore_blank = false;
        let id = self.add(c, &relaxed)?;
        self.set_slot(n, id)?;
        Ok(id)
    }

    pub fn clear_slot(&mut self, n: u8) -> Result<()> {
        self.index.slots.remove(&n);
        self.save()
    }

    pub fn wipe(&mut self, include_slots: bool) -> Result<()> {
        let keep: Vec<u64> = if include_slots {
            self.index.slots.clear();
            Vec::new()
        } else {
            self.index.slots.values().copied().collect()
        };
        let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.index.entries)
            .into_iter()
            .partition(|e| keep.contains(&e.id));
        for e in gone {
            fs::remove_file(self.blob(e.id)).ok();
        }
        self.index.entries = kept;
        self.save()
    }

    /// Drop old or extra entries. Entries held in a slot are never dropped.
    pub fn prune(&mut self, limits: &Limits) {
        let slotted: Vec<u64> = self.index.slots.values().copied().collect();
        let cutoff = if limits.max_age_days > 0 {
            now_secs().saturating_sub(limits.max_age_days * 86_400)
        } else {
            0
        };
        let mut kept = 0usize;
        let mut drop_ids = Vec::new();
        for e in &self.index.entries {
            let pinned = slotted.contains(&e.id);
            if pinned {
                continue;
            }
            let too_old = cutoff > 0 && e.last_used < cutoff;
            let too_many = limits.max_items > 0 && kept >= limits.max_items;
            if too_old || too_many {
                drop_ids.push(e.id);
            } else {
                kept += 1;
            }
        }
        if drop_ids.is_empty() {
            return;
        }
        self.index.entries.retain(|e| !drop_ids.contains(&e.id));
        for id in drop_ids {
            fs::remove_file(self.blob(id)).ok();
        }
    }

    pub fn prune_and_save(&mut self, limits: &Limits) -> Result<()> {
        self.prune(limits);
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hotbar-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn text(s: &str) -> Content {
        Content::Text(s.into())
    }

    #[test]
    fn dedupe_moves_to_top() {
        let dir = tmp();
        let mut s = Store::open(&dir).unwrap();
        let l = Limits::default();
        let a = s.add(&text("a"), &l).unwrap();
        let _b = s.add(&text("b"), &l).unwrap();
        let a2 = s.add(&text("a"), &l).unwrap();
        assert_eq!(a, a2);
        assert_eq!(s.list().len(), 2);
        assert_eq!(s.list()[0].id, a);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn slots_survive_pruning_and_reopen() {
        let dir = tmp();
        let l = Limits {
            max_items: 2,
            ..Limits::default()
        };
        {
            let mut s = Store::open(&dir).unwrap();
            s.set_slot_content(3, &text("pinned"), &l).unwrap();
            for i in 0..5 {
                s.add(&text(&format!("item {i}")), &l).unwrap();
            }
            assert_eq!(s.list().len(), 3, "2 history items plus the pinned one");
        }
        let s = Store::open(&dir).unwrap();
        let id = s.slot(3).unwrap();
        assert_eq!(s.get(id).unwrap(), Some(text("pinned")));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn limits_filter() {
        let l = Limits {
            min_length: 3,
            max_length: 5,
            ..Limits::default()
        };
        assert!(!l.accepts(&text("ab")));
        assert!(l.accepts(&text("abc")));
        assert!(!l.accepts(&text("abcdef")));
        assert!(!Limits::default().accepts(&text("   \n")));
    }

    #[test]
    fn wipe_keeps_slots_unless_asked() {
        let dir = tmp();
        let l = Limits::default();
        let mut s = Store::open(&dir).unwrap();
        s.set_slot_content(1, &text("keep"), &l).unwrap();
        s.add(&text("gone"), &l).unwrap();
        s.wipe(false).unwrap();
        assert_eq!(s.list().len(), 1);
        s.wipe(true).unwrap();
        assert!(s.list().is_empty());
        assert_eq!(s.slot(1), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn delete_clears_slot() {
        let dir = tmp();
        let l = Limits::default();
        let mut s = Store::open(&dir).unwrap();
        let id = s.set_slot_content(2, &text("x"), &l).unwrap();
        assert!(s.delete(id).unwrap());
        assert_eq!(s.slot(2), None);
        fs::remove_dir_all(dir).ok();
    }
}
