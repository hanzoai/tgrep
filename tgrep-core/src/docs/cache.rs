//! On-disk memo of extracted documents.
//!
//! An entry is named by the BLAKE3 hash of [`super::VERSION`], the adapter and
//! the document's bytes, so it is valid for exactly those bytes read by exactly
//! that adapter: a changed file hashes to a new entry and an unchanged one is
//! never extracted again. Entries are written to a temporary file and renamed
//! into place, so readers never see a partial entry.
//!
//! An entry is `tgrep-doc\n`, one line of JSON, then the text. The JSON is
//! `{"parts":[[start,end,"loc"],...],"skipped":{"reason":n}}` for a document
//! and `{"failed":"reason"}` for one no adapter could read.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::{Doc, Kind, Loc, Part, Reason, Skips};

const MAGIC: &[u8] = b"tgrep-doc\n";

pub(crate) type Key = [u8; 32];

pub(crate) fn key(kind: Kind, bytes: &[u8]) -> Key {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tgrep-docs\0");
    hasher.update(&super::VERSION.to_le_bytes());
    hasher.update(kind.label().as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// Bytes a cache keeps before it evicts the entries read least recently.
pub const CAP: u64 = 2 << 30;

/// A directory of extracted documents, at most [`Cache::open_with`]'s `cap`
/// bytes: past it, the entries read least recently are removed until it is
/// under nine tenths of the cap. Eviction only costs a later re-extraction.
pub struct Cache {
    dir: PathBuf,
    cap: u64,
    used: Mutex<HashSet<Key>>,
    bytes: AtomicU64,
}

/// An entry on disk: its path, size and the last time it was read or written.
struct Entry {
    path: PathBuf,
    name: String,
    len: u64,
    touched: SystemTime,
}

impl Cache {
    /// Open, creating it if missing, the cache in `dir`, holding at most [`CAP`].
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        Self::open_with(dir, CAP)
    }

    /// Open the cache in `dir`, holding at most `cap` bytes.
    pub fn open_with(dir: &Path, cap: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let cache = Self {
            dir: dir.to_path_buf(),
            cap,
            used: Mutex::new(HashSet::new()),
            bytes: AtomicU64::new(0),
        };
        let bytes = cache.entries()?.iter().map(|entry| entry.len).sum();
        cache.bytes.store(bytes, Ordering::Relaxed);
        Ok(cache)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, key: &Key) -> PathBuf {
        let hex = blake3::Hash::from_bytes(*key).to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    fn entries(&self) -> std::io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for fan in std::fs::read_dir(&self.dir)? {
            let fan = fan?;
            if !fan.file_type()?.is_dir() {
                continue;
            }
            let prefix = fan.file_name().to_string_lossy().into_owned();
            for entry in std::fs::read_dir(fan.path())? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                entries.push(Entry {
                    path: entry.path(),
                    name: format!("{prefix}{}", entry.file_name().to_string_lossy()),
                    len: metadata.len(),
                    touched: metadata.modified()?,
                });
            }
        }
        Ok(entries)
    }

    pub(crate) fn get(&self, key: &Key) -> Option<Result<Doc, Reason>> {
        let path = self.file(key);
        let bytes = std::fs::read(&path).ok()?;
        let entry = decode(&bytes)?;
        // Reading an entry makes it recent; a failed touch only ages it.
        if let Ok(file) = std::fs::File::open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
        self.used.lock().unwrap().insert(*key);
        Some(entry)
    }

    pub(crate) fn put(&self, key: &Key, entry: &Result<Doc, Reason>) -> std::io::Result<()> {
        let path = self.file(key);
        let dir = path
            .parent()
            .expect("an entry is inside a fan-out directory");
        std::fs::create_dir_all(dir)?;
        let data = encode(entry);
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(&data)?;
        file.persist(&path).map_err(|error| error.error)?;
        self.used.lock().unwrap().insert(*key);
        let bytes = self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;
        if bytes > self.cap {
            self.evict()?;
        }
        Ok(())
    }

    /// Remove the entries read least recently until the cache is under nine
    /// tenths of its cap.
    fn evict(&self) -> std::io::Result<()> {
        let mut entries = self.entries()?;
        let mut bytes: u64 = entries.iter().map(|entry| entry.len).sum();
        entries.sort_by_key(|entry| entry.touched);
        let goal = self.cap / 10 * 9;
        for entry in entries {
            if bytes <= goal {
                break;
            }
            std::fs::remove_file(&entry.path)?;
            bytes -= entry.len;
        }
        self.bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Remove every entry this process neither read nor wrote. Call after a
    /// full build, when the entries in use are exactly the indexed documents'.
    /// Returns how many entries it removed.
    pub fn sweep(&self) -> std::io::Result<u64> {
        let used = self.used.lock().unwrap();
        let mut removed = 0;
        let mut bytes = 0;
        for entry in self.entries()? {
            let keep = blake3::Hash::from_hex(&entry.name)
                .is_ok_and(|hash| used.contains(hash.as_bytes()));
            if keep {
                bytes += entry.len;
            } else {
                std::fs::remove_file(&entry.path)?;
                removed += 1;
            }
        }
        self.bytes.store(bytes, Ordering::Relaxed);
        Ok(removed)
    }

    /// Entries and their bytes on disk.
    pub fn usage(&self) -> std::io::Result<(u64, u64)> {
        let entries = self.entries()?;
        Ok((
            entries.len() as u64,
            entries.iter().map(|entry| entry.len).sum(),
        ))
    }
}

fn encode(entry: &Result<Doc, Reason>) -> Vec<u8> {
    let (header, text) = match entry {
        Ok(doc) => {
            let parts: Vec<_> = doc
                .parts
                .iter()
                .map(|part| serde_json::json!([part.start, part.end, part.loc.to_string()]))
                .collect();
            let skipped: serde_json::Map<_, _> = doc
                .skipped
                .iter()
                .map(|(reason, count)| (reason.label().to_string(), count.into()))
                .collect();
            (
                serde_json::json!({"parts": parts, "skipped": skipped}),
                doc.text.as_str(),
            )
        }
        Err(reason) => (serde_json::json!({"failed": reason.label()}), ""),
    };
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(header.to_string().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(text.as_bytes());
    out
}

fn decode(bytes: &[u8]) -> Option<Result<Doc, Reason>> {
    let rest = bytes.strip_prefix(MAGIC)?;
    let newline = memchr::memchr(b'\n', rest)?;
    let header: serde_json::Value = serde_json::from_slice(&rest[..newline]).ok()?;
    if let Some(failed) = header.get("failed") {
        return Some(Err(Reason::from_label(failed.as_str()?)?));
    }
    let text = String::from_utf8(rest[newline + 1..].to_vec()).ok()?;
    let mut parts = Vec::new();
    for part in header.get("parts")?.as_array()? {
        let start = usize::try_from(part.get(0)?.as_u64()?).ok()?;
        let end = usize::try_from(part.get(1)?.as_u64()?).ok()?;
        if start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return None;
        }
        text.get(start..end)?;
        let loc = Loc::parse(part.get(2)?.as_str()?)?;
        parts.push(Part { start, end, loc });
    }
    let mut skipped = Skips::default();
    for (label, count) in header.get("skipped")?.as_object()? {
        skipped.0[Reason::from_label(label)?.index()] = count.as_u64()?;
    }
    Some(Ok(Doc {
        text,
        parts,
        skipped,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let loc = Loc {
            members: vec!["a!b.txt".to_string()].into(),
            unit: None,
        };
        let doc = Doc {
            text: "one\ntwo\n".into(),
            parts: vec![Part {
                start: 4,
                end: 8,
                loc,
            }],
            skipped: {
                let mut skips = Skips::default();
                skips.add(Reason::Large);
                skips
            },
        };
        assert_eq!(decode(&encode(&Ok(doc.clone()))), Some(Ok(doc)));
        assert_eq!(decode(&encode(&Err(Reason::Time))), Some(Err(Reason::Time)));
        let mut damaged = encode(&Err(Reason::Corrupt));
        damaged[0] = b'x';
        assert_eq!(decode(&damaged), None);
        assert_eq!(
            decode(b"tgrep-doc\n{\"parts\":[[0,99,\"\"]],\"skipped\":{}}\nab"),
            None
        );
    }

    #[test]
    fn a_full_cache_evicts_what_was_read_least_recently() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open_with(dir.path(), 4000).unwrap();
        let doc = |n: u8| Doc {
            text: format!("{}\n", (n as char).to_string().repeat(1000)),
            parts: Vec::new(),
            skipped: Skips::default(),
        };
        let keys: Vec<Key> = (b'a'..=b'e').map(|n| key(Kind::Zip, &[n])).collect();
        for (n, key) in (b'a'..).zip(&keys[..3]) {
            cache.put(key, &Ok(doc(n))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(cache.get(&keys[0]).is_some());
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.put(&keys[3], &Ok(doc(b'd'))).unwrap();
        assert!(
            cache.get(&keys[1]).is_none(),
            "the entry read least recently goes first"
        );
        assert!(cache.get(&keys[0]).is_some() && cache.get(&keys[3]).is_some());
        assert!(cache.usage().unwrap().1 <= 4000);
    }
}
