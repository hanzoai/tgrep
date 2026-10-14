//! Archives: zip, tar and gzip, read in memory.
//!
//! Members are read into memory one at a time, never written anywhere, and
//! their names are only labels. A member whose name leaves the archive (an
//! absolute path, a drive prefix or a `..` component) or that is a link is
//! refused; one that declares or decompresses past the budget is skipped
//! without being held whole.

use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use super::{Budget, Out, Reason};

/// Bytes between two deadline checks while decompressing.
const CHECK_EVERY: u64 = 1 << 20;

/// A reader that stops at `limit` bytes and at a deadline.
struct Counted<R> {
    inner: R,
    read: u64,
    limit: u64,
    deadline: Instant,
    next_check: u64,
    tripped: Option<Reason>,
}

impl<R: Read> Counted<R> {
    fn new(inner: R, limit: u64, deadline: Instant) -> Self {
        Self {
            inner,
            read: 0,
            limit,
            deadline,
            next_check: CHECK_EVERY,
            tripped: None,
        }
    }

    fn trip(&mut self, reason: Reason) -> std::io::Error {
        self.tripped = Some(reason);
        std::io::Error::other(reason.label())
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.read >= self.next_check {
            if Instant::now() > self.deadline {
                return Err(self.trip(Reason::Time));
            }
            self.next_check = self.read + CHECK_EVERY;
        }
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.read > self.limit {
            return Err(self.trip(Reason::Total));
        }
        Ok(n)
    }
}

/// Read at most `room` bytes of `reader`: `Ok(None)` when it holds more.
fn read_bounded(reader: impl Read, room: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    reader.take(room.saturating_add(1)).read_to_end(&mut data)?;
    Ok((data.len() as u64 <= room).then_some(data))
}

/// The reason a member that did not fit was skipped: past the member bound,
/// or past what the document has left.
fn overflow(budget: &Budget) -> Reason {
    if budget.room() < budget.limits.entry {
        Reason::Total
    } else {
        Reason::Large
    }
}

/// A member's name, `/`-separated, or `None` when it leaves the archive.
fn member_name(raw: &[u8]) -> Option<String> {
    let name = String::from_utf8_lossy(raw).replace('\\', "/");
    let absolute = name.starts_with('/')
        || name
            .as_bytes()
            .get(1)
            .is_some_and(|&b| b == b':' && name.as_bytes()[0].is_ascii_alphabetic());
    if absolute || name.split('/').any(|part| part == "..") {
        return None;
    }
    let name = name.trim_start_matches("./").to_string();
    Some(name)
}

fn nested(chain: &Arc<[String]>, name: String) -> Arc<[String]> {
    let mut members = chain.to_vec();
    members.push(name);
    members.into()
}

/// Decompress a whole gzip stream (every member of a multi-member file).
pub(super) fn gunzip(bytes: &[u8], budget: &mut Budget) -> Result<Vec<u8>, Reason> {
    let room = budget.room();
    let mut reader = Counted::new(
        flate2::read::MultiGzDecoder::new(bytes),
        u64::MAX,
        budget.deadline,
    );
    match read_bounded(&mut reader, room) {
        Ok(Some(data)) => {
            budget.read += data.len() as u64;
            Ok(data)
        }
        Ok(None) => Err(overflow(budget)),
        Err(_) => Err(reader.tripped.unwrap_or(Reason::Corrupt)),
    }
}

pub(super) fn zip(
    bytes: &[u8],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|_| Reason::Corrupt)?;
    let count = archive.len();
    if count as u64 > budget.limits.members {
        budget.skipped.add(Reason::Members);
    }
    for index in 0..count.min(budget.limits.members as usize) {
        budget.check()?;
        let file = match archive.by_index(index) {
            Ok(file) => file,
            Err(zip::result::ZipError::UnsupportedArchive(_)) => {
                budget.skipped.add(Reason::Unsupported);
                continue;
            }
            Err(_) => {
                budget.skipped.add(Reason::Corrupt);
                continue;
            }
        };
        if file.is_dir() {
            continue;
        }
        let Some(name) = member_name(file.name_raw()) else {
            budget.skipped.add(Reason::Refused);
            continue;
        };
        if file.is_symlink() {
            budget.skipped.add(Reason::Refused);
            continue;
        }
        if file.encrypted() {
            budget.skipped.add(Reason::Encrypted);
            continue;
        }
        let room = budget.room();
        if file.size() > room {
            budget.skipped.add(overflow(budget));
            continue;
        }
        let data = match read_bounded(file, room) {
            Ok(Some(data)) => data,
            Ok(None) => {
                budget.skipped.add(overflow(budget));
                continue;
            }
            Err(_) => {
                budget.skipped.add(Reason::Corrupt);
                continue;
            }
        };
        budget.read += data.len() as u64;
        super::member(&name.clone(), &data, nested(chain, name), budget, out)?;
    }
    Ok(())
}

/// Read a tar stream from `bytes`, decompressing it first when `gzip`.
pub(super) fn tar(
    bytes: &[u8],
    gzip: bool,
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let stream: Box<dyn Read + '_> = if gzip {
        Box::new(flate2::read::MultiGzDecoder::new(bytes))
    } else {
        Box::new(bytes)
    };
    let counted = Counted::new(stream, budget.limits.total, budget.deadline);
    let mut archive = tar::Archive::new(counted);
    let result = read_tar(&mut archive, chain, budget, out);
    match (result, archive.into_inner().tripped) {
        (Err(Reason::Corrupt), Some(reason)) => Err(reason),
        (result, _) => result,
    }
}

fn read_tar<R: Read>(
    archive: &mut tar::Archive<R>,
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let entries = archive.entries().map_err(|_| Reason::Corrupt)?;
    let mut seen = 0u64;
    for entry in entries {
        budget.check()?;
        let entry = entry.map_err(|_| Reason::Corrupt)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() || kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        seen += 1;
        if seen > budget.limits.members {
            budget.skipped.add(Reason::Members);
            break;
        }
        let Some(name) = member_name(&entry.path_bytes()) else {
            budget.skipped.add(Reason::Refused);
            continue;
        };
        if !(kind.is_file() || kind.is_contiguous()) {
            budget.skipped.add(Reason::Refused);
            continue;
        }
        let room = budget.room();
        if entry.size() > room {
            budget.skipped.add(overflow(budget));
            continue;
        }
        let data = match read_bounded(entry, room) {
            Ok(Some(data)) => data,
            Ok(None) => {
                budget.skipped.add(overflow(budget));
                continue;
            }
            Err(_) => return Err(Reason::Corrupt),
        };
        budget.read += data.len() as u64;
        super::member(&name.clone(), &data, nested(chain, name), budget, out)?;
    }
    Ok(())
}
