//! Document adapters (feature `docs`).
//!
//! A document is a file whose bytes are not searchable text but contain some:
//! a PDF, an archive, an office file. An adapter turns its bytes into a [`Doc`]:
//! one UTF-8 text made of [`Part`]s, each tagged with the [`Loc`] it came from
//! (a page, a slide, a paragraph, a spreadsheet cell, an archive member). That
//! text is what the trigram index holds for the file, so a query over
//! documents costs what a query over code costs: the same postings, the same
//! planner, the same verification. Verification searches each part on its own,
//! so a hit names the file plus where inside it, and its line number counts
//! from the start of that part.
//!
//! # Formats
//!
//! | Extension | Parts |
//! |---|---|
//! | `.pdf` | one per page with text: `#page=N` |
//! | `.zip` | one per member: `!member` |
//! | `.tar` | one per member: `!member` |
//! | `.tar.gz`, `.tgz` | one per member: `!member` |
//! | `.gz` | the decompressed stream, adapted by the name without `.gz` |
//!
//! Members are adapted by their own names, so archives nest (`a.zip!b.tar.gz!c.pdf#page=2`)
//! up to [`Limits::depth`] containers. A member with no adapter is text if it decodes
//! to text, and is skipped if it is binary.
//!
//! # Location syntax
//!
//! A hit prints as `PATH[!MEMBER]...[#UNIT]:LINE:TEXT`:
//!
//! ```text
//! report.pdf#page=2:14:quarterly revenue
//! bundle.zip!notes/todo.txt:3:call the bank
//! logs.tar.gz!2026/app.log:120:connection reset
//! nested.zip!inner.tar.gz!slides.pdf#page=1:1:Agenda
//! ```
//!
//! `MEMBER` is a member's path inside its archive and `UNIT` is `page=N` (1-based).
//! In member names `%`, `!`, `#`, `:` and control characters are written as `%XX`,
//! so the location parses back exactly. `LINE` counts from the start of the part.
//! [`Loc::json`] gives the same fields as a JSON object.
//!
//! # Cache
//!
//! Extraction is memoized on disk by a hash of the document's bytes, its adapter
//! and [`VERSION`] ([`Cache`]): an unchanged document is extracted once, a changed
//! one is extracted again, and a failure is remembered as such. A cache never
//! changes a result, only how long it takes.
//!
//! # Limits
//!
//! Extraction runs in memory and never writes a member to disk. [`Limits`] bound
//! nesting, one member's and one document's decompressed bytes, members per
//! archive, extracted text and wall time; members that exceed a bound, members
//! whose names escape the archive (`..`, absolute paths) and link members are
//! skipped and counted in [`Doc::skipped`]. A document whose adapter fails,
//! panics or runs out of time is not indexed and is counted in [`stats`].

use std::borrow::Cow;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

mod archive;
mod cache;
mod pdf;

pub use cache::Cache;

/// Version of every adapter's output. Part of each cache key, so changing what
/// an adapter emits re-extracts every document.
pub const VERSION: u32 = 1;

/// Separates a file's path from its [`Loc`] in a virtual path ([`join`]).
///
/// No path contains a NUL, so the split is exact.
pub const MARK: char = '\0';

/// The adapter a file name selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Pdf,
    Zip,
    Tar,
    TarGz,
    Gz,
}

impl Kind {
    pub(crate) fn of(name: &str) -> Option<Self> {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let lower = base.to_ascii_lowercase();
        let kind = if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Self::TarGz
        } else {
            match lower.rsplit_once('.')?.1 {
                "pdf" => Self::Pdf,
                "zip" => Self::Zip,
                "tar" => Self::Tar,
                "gz" => Self::Gz,
                _ => return None,
            }
        };
        Some(kind)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::Gz => "gz",
        }
    }
}

/// Whether `path` names a document an adapter reads.
pub fn handles(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| Kind::of(name).is_some())
}

/// Where a part of a document's text came from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Loc {
    /// Archive members from the outermost archive inward.
    pub members: Arc<[String]>,
    /// The unit inside the innermost document, if it has units.
    pub unit: Option<Unit>,
}

/// A numbered unit inside a document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    /// A PDF page, 1-based.
    Page(u32),
}

impl Loc {
    /// True for the whole file: no member and no unit.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.unit.is_none()
    }

    /// Parse the text [`fmt::Display`] writes.
    pub fn parse(text: &str) -> Option<Self> {
        let (chain, unit) = match text.split_once('#') {
            Some((chain, unit)) => (chain, Some(unit)),
            None => (text, None),
        };
        let members = if chain.is_empty() {
            Vec::new()
        } else {
            chain
                .strip_prefix('!')?
                .split('!')
                .map(unescape)
                .collect::<Option<Vec<_>>>()?
        };
        let unit = match unit {
            None => None,
            Some(unit) => {
                let (key, value) = unit.split_once('=')?;
                match key {
                    "page" => Some(Unit::Page(value.parse().ok()?)),
                    _ => return None,
                }
            }
        };
        Some(Self {
            members: members.into(),
            unit,
        })
    }

    /// The location as a JSON object: `members` (when inside an archive) and
    /// the unit's key (`page`).
    pub fn json(&self) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        if !self.members.is_empty() {
            object.insert("members".into(), serde_json::json!(self.members.as_ref()));
        }
        match &self.unit {
            Some(Unit::Page(n)) => {
                object.insert("page".into(), serde_json::json!(n));
            }
            None => {}
        }
        serde_json::Value::Object(object)
    }
}

impl fmt::Display for Loc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for member in self.members.iter() {
            write!(f, "!{}", escape(member))?;
        }
        match &self.unit {
            Some(Unit::Page(n)) => write!(f, "#page={n}"),
            None => Ok(()),
        }
    }
}

fn escape(text: &str) -> Cow<'_, str> {
    let special = |c: char| matches!(c, '%' | '!' | '#' | ':') || c.is_control();
    if !text.contains(special) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if special(c) {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn unescape(text: &str) -> Option<String> {
    if !text.contains('%') {
        return Some(text.to_string());
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A file path joined with a location: `path`, then [`MARK`], then the
/// location's text. A whole-file location leaves the path unchanged.
pub fn join(path: &str, loc: &Loc) -> String {
    if loc.is_empty() {
        path.to_string()
    } else {
        format!("{path}{MARK}{loc}")
    }
}

/// Split a [`join`]ed path into the file path and the location's text.
pub fn split(file: &str) -> (&str, Option<&str>) {
    match file.split_once(MARK) {
        Some((path, loc)) => (path, Some(loc)),
        None => (file, None),
    }
}

/// One part of a document's text: `text[start..end]` came from `loc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub start: usize,
    pub end: usize,
    pub loc: Loc,
}

/// A document's extracted text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    /// Every part's text, in document order. Each part ends with a newline, so
    /// no line spans two parts; the text holds no NUL byte.
    pub text: String,
    pub parts: Vec<Part>,
    /// Members left out, by reason.
    pub skipped: Skips,
}

impl Doc {
    pub fn part_text(&self, part: &Part) -> &str {
        &self.text[part.start..part.end]
    }
}

/// Why a document or a member was not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The bytes do not parse as the format their name claims.
    Corrupt,
    /// Encrypted, with no empty password.
    Encrypted,
    /// A member larger than [`Limits::entry`].
    Large,
    /// The document's decompressed bytes reached [`Limits::total`].
    Total,
    /// An archive with more than [`Limits::members`] members.
    Members,
    /// A container nested deeper than [`Limits::depth`].
    Depth,
    /// A member whose name leaves the archive, or a link.
    Refused,
    /// A member that is neither a document nor text.
    Binary,
    /// A compression method or format variant no adapter reads.
    Unsupported,
    /// The document took longer than [`Limits::time`].
    Time,
    /// The adapter panicked.
    Panic,
    /// Text past [`Limits::text`].
    Text,
}

impl Reason {
    pub const ALL: [Reason; 12] = [
        Reason::Corrupt,
        Reason::Encrypted,
        Reason::Large,
        Reason::Total,
        Reason::Members,
        Reason::Depth,
        Reason::Refused,
        Reason::Binary,
        Reason::Unsupported,
        Reason::Time,
        Reason::Panic,
        Reason::Text,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Reason::Corrupt => "corrupt",
            Reason::Encrypted => "encrypted",
            Reason::Large => "large",
            Reason::Total => "total",
            Reason::Members => "members",
            Reason::Depth => "depth",
            Reason::Refused => "refused",
            Reason::Binary => "binary",
            Reason::Unsupported => "unsupported",
            Reason::Time => "time",
            Reason::Panic => "panic",
            Reason::Text => "text",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.label() == label)
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Counts by [`Reason`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Skips([u64; Reason::ALL.len()]);

impl Skips {
    pub fn get(&self, reason: Reason) -> u64 {
        self.0[reason.index()]
    }

    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }

    /// Non-zero counts.
    pub fn iter(&self) -> impl Iterator<Item = (Reason, u64)> + '_ {
        Reason::ALL
            .into_iter()
            .map(|reason| (reason, self.get(reason)))
            .filter(|(_, count)| *count > 0)
    }

    fn add(&mut self, reason: Reason) {
        self.0[reason.index()] += 1;
    }
}

/// Bounds on one document's extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Containers nested inside one another, the document itself counted.
    pub depth: u32,
    /// Decompressed bytes of one member. A member declaring more is skipped
    /// unread; one that decompresses past it is dropped at the bound.
    pub entry: u64,
    /// Decompressed bytes of all members of one document, at every depth.
    pub total: u64,
    /// Members of one archive; members past it are not read.
    pub members: u64,
    /// Bytes of text kept from one document; parts past it are dropped.
    pub text: u64,
    /// Wall time one document may take, checked between members, pages and
    /// blocks of decompressed bytes.
    pub time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            depth: 4,
            entry: 64 << 20,
            total: 512 << 20,
            members: 65_536,
            text: 64 << 20,
            time: Duration::from_secs(30),
        }
    }
}

/// Mutable state of one extraction: what it may still spend and what it left
/// out.
struct Budget {
    limits: Limits,
    deadline: Instant,
    read: u64,
    skipped: Skips,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self {
            limits,
            deadline: Instant::now() + limits.time,
            read: 0,
            skipped: Skips::default(),
        }
    }

    fn check(&self) -> Result<(), Reason> {
        if Instant::now() > self.deadline {
            Err(Reason::Time)
        } else {
            Ok(())
        }
    }

    /// Bytes one member may still decompress to.
    fn room(&self) -> u64 {
        self.limits
            .entry
            .min(self.limits.total.saturating_sub(self.read))
    }
}

/// The text being built and its parts.
struct Out {
    text: String,
    parts: Vec<Part>,
    cap: u64,
}

impl Out {
    fn push(&mut self, loc: Loc, text: &str, budget: &mut Budget) {
        let text = text.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            return;
        }
        if (self.text.len() + text.len() + 1) as u64 > self.cap {
            budget.skipped.add(Reason::Text);
            return;
        }
        let start = self.text.len();
        if text.contains('\0') {
            self.text.push_str(&text.replace('\0', " "));
        } else {
            self.text.push_str(text);
        }
        self.text.push('\n');
        self.parts.push(Part {
            start,
            end: self.text.len(),
            loc,
        });
    }
}

/// Read a document, or member, by the adapter its name selects.
fn adapt(
    kind: Kind,
    name: &str,
    bytes: &[u8],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    budget.check()?;
    match kind {
        Kind::Pdf => pdf::extract(bytes, chain, budget, out),
        Kind::Zip => archive::zip(bytes, chain, budget, out),
        Kind::Tar => archive::tar(bytes, false, chain, budget, out),
        Kind::TarGz => archive::tar(bytes, true, chain, budget, out),
        Kind::Gz => {
            let data = archive::gunzip(bytes, budget)?;
            let inner = name.rsplit(['/', '\\']).next().unwrap_or(name);
            let inner = &inner[..inner.len() - 3];
            member(inner, &data, chain.clone(), budget, out)
        }
    }
}

/// Read one member: a nested document by its adapter, anything else as text.
/// A member's failure is counted and does not fail its archive, except running
/// out of time, which ends the whole document.
fn member(
    name: &str,
    bytes: &[u8],
    chain: Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    match Kind::of(name) {
        Some(kind) => {
            let depth = chain.len() as u32 + 1;
            if depth > budget.limits.depth {
                budget.skipped.add(Reason::Depth);
                return Ok(());
            }
            match adapt(kind, name, bytes, &chain, budget, out) {
                Ok(()) => Ok(()),
                Err(Reason::Time) => Err(Reason::Time),
                Err(reason) => {
                    budget.skipped.add(reason);
                    Ok(())
                }
            }
        }
        None => {
            if crate::walker::is_binary_extension(Path::new(name)) {
                budget.skipped.add(Reason::Binary);
                return Ok(());
            }
            let text = crate::encoding::decode(bytes, crate::encoding::EncodingMode::Auto);
            if crate::trigram::is_binary(text.as_bytes()) {
                budget.skipped.add(Reason::Binary);
                return Ok(());
            }
            out.push(
                Loc {
                    members: chain,
                    unit: None,
                },
                &text,
                budget,
            );
            Ok(())
        }
    }
}

/// Extract `bytes`, read by the adapter `name` selects, within `limits`.
/// Never panics: an adapter's panic is [`Reason::Panic`].
pub fn extract_with(name: &str, bytes: &[u8], limits: Limits) -> Result<Doc, Reason> {
    let kind = Kind::of(name).ok_or(Reason::Unsupported)?;
    let run = || {
        let mut budget = Budget::new(limits);
        let mut out = Out {
            text: String::new(),
            parts: Vec::new(),
            cap: limits.text,
        };
        adapt(kind, name, bytes, &Arc::from([]), &mut budget, &mut out)?;
        Ok(Doc {
            text: out.text,
            parts: out.parts,
            skipped: budget.skipped,
        })
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or(Err(Reason::Panic))
}

/// Extract with the default [`Limits`], uncached.
pub fn extract(name: &str, bytes: &[u8]) -> Result<Doc, Reason> {
    extract_with(name, bytes, Limits::default())
}

static INSTALLED: RwLock<Option<Arc<Cache>>> = RwLock::new(None);

/// Use `cache` for every later [`load`] in this process; `None` stops caching.
pub fn install(cache: Option<Cache>) {
    *INSTALLED.write().unwrap() = cache.map(Arc::new);
}

/// The cache [`install`] set.
pub fn installed() -> Option<Arc<Cache>> {
    INSTALLED.read().unwrap().clone()
}

/// A document's [`Doc`], from the installed cache or by extracting it (and
/// caching the result). `name` selects the adapter.
pub fn load(name: &str, bytes: &[u8]) -> Result<Arc<Doc>, Reason> {
    load_with(installed().as_deref(), name, bytes)
}

/// [`load`] with an explicit cache.
pub fn load_with(cache: Option<&Cache>, name: &str, bytes: &[u8]) -> Result<Arc<Doc>, Reason> {
    let kind = Kind::of(name).ok_or(Reason::Unsupported)?;
    let key = cache::key(kind, bytes);
    if let Some(found) = cache.and_then(|cache| cache.get(&key)) {
        STATS.cached.fetch_add(1, Ordering::Relaxed);
        return found.map(Arc::new);
    }
    let result = extract(name, bytes);
    match &result {
        Ok(doc) => {
            STATS.extracted.fetch_add(1, Ordering::Relaxed);
            for (reason, count) in doc.skipped.iter() {
                STATS.members[reason.index()].fetch_add(count, Ordering::Relaxed);
            }
        }
        Err(reason) => {
            STATS.failed[reason.index()].fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(cache) = cache
        && let Err(error) = cache.put(&key, &result)
    {
        eprintln!("warning: cannot cache extracted text of {name}: {error}");
    }
    result.map(Arc::new)
}

/// The bytes the index holds for a file: a document's extracted text, or the
/// decoded file for anything else. A document that cannot be read yields a
/// NUL, so the index treats it as binary and keeps only its name.
pub fn index_text<'a>(name: &str, bytes: &'a [u8]) -> Cow<'a, [u8]> {
    if Kind::of(name).is_none() {
        return crate::encoding::decode_for_index(bytes);
    }
    match load(name, bytes) {
        Ok(doc) => Cow::Owned(doc.text.as_bytes().to_vec()),
        Err(_) => Cow::Borrowed(b"\0"),
    }
}

struct Counters {
    extracted: AtomicU64,
    cached: AtomicU64,
    failed: [AtomicU64; Reason::ALL.len()],
    members: [AtomicU64; Reason::ALL.len()],
}

static STATS: Counters = Counters {
    extracted: AtomicU64::new(0),
    cached: AtomicU64::new(0),
    failed: [const { AtomicU64::new(0) }; Reason::ALL.len()],
    members: [const { AtomicU64::new(0) }; Reason::ALL.len()],
};

/// What [`load`] did in this process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Documents extracted.
    pub extracted: u64,
    /// Documents read from the cache instead.
    pub cached: u64,
    /// Documents not indexed, by reason.
    pub failed: Skips,
    /// Members left out of extracted documents, by reason.
    pub members: Skips,
}

impl Stats {
    pub fn json(&self) -> serde_json::Value {
        let skips = |skips: &Skips| {
            serde_json::Value::Object(
                skips
                    .iter()
                    .map(|(reason, count)| (reason.label().to_string(), count.into()))
                    .collect(),
            )
        };
        serde_json::json!({
            "extracted": self.extracted,
            "cached": self.cached,
            "failed": skips(&self.failed),
            "members_skipped": skips(&self.members),
        })
    }
}

pub fn stats() -> Stats {
    let read = |counters: &[AtomicU64; Reason::ALL.len()]| {
        Skips(std::array::from_fn(|i| counters[i].load(Ordering::Relaxed)))
    };
    Stats {
        extracted: STATS.extracted.load(Ordering::Relaxed),
        cached: STATS.cached.load(Ordering::Relaxed),
        failed: read(&STATS.failed),
        members: read(&STATS.members),
    }
}

#[cfg(test)]
mod tests;
