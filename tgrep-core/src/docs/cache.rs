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

/// A directory of extracted documents.
pub struct Cache {
    dir: PathBuf,
    used: Mutex<HashSet<Key>>,
}

impl Cache {
    /// Open, creating it if missing, the cache in `dir`.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            used: Mutex::new(HashSet::new()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, key: &Key) -> PathBuf {
        let hex = blake3::Hash::from_bytes(*key).to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    pub(crate) fn get(&self, key: &Key) -> Option<Result<Doc, Reason>> {
        let bytes = std::fs::read(self.file(key)).ok()?;
        let entry = decode(&bytes)?;
        self.used.lock().unwrap().insert(*key);
        Some(entry)
    }

    pub(crate) fn put(&self, key: &Key, entry: &Result<Doc, Reason>) -> std::io::Result<()> {
        let path = self.file(key);
        let dir = path
            .parent()
            .expect("an entry is inside a fan-out directory");
        std::fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(&encode(entry))?;
        file.persist(&path).map_err(|error| error.error)?;
        self.used.lock().unwrap().insert(*key);
        Ok(())
    }

    /// Remove every entry this process neither read nor wrote. Call after a
    /// full build, when the entries in use are exactly the indexed documents'.
    /// Returns how many entries it removed.
    pub fn sweep(&self) -> std::io::Result<u64> {
        let used = self.used.lock().unwrap();
        let mut removed = 0;
        for fan in std::fs::read_dir(&self.dir)? {
            let fan = fan?;
            if !fan.file_type()?.is_dir() {
                continue;
            }
            let prefix = fan.file_name().to_string_lossy().into_owned();
            for entry in std::fs::read_dir(fan.path())? {
                let entry = entry?;
                let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
                let keep =
                    blake3::Hash::from_hex(&name).is_ok_and(|hash| used.contains(hash.as_bytes()));
                if !keep {
                    std::fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Entries and their bytes on disk.
    pub fn usage(&self) -> std::io::Result<(u64, u64)> {
        let mut entries = 0;
        let mut bytes = 0;
        for fan in std::fs::read_dir(&self.dir)? {
            let fan = fan?;
            if !fan.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(fan.path())? {
                entries += 1;
                bytes += entry?.metadata()?.len();
            }
        }
        Ok((entries, bytes))
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
}
