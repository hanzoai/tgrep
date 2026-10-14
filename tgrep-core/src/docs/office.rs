//! Office documents: OOXML (docx, pptx, xlsx) and OpenDocument (odt, ods,
//! odp), each a zip package of XML parts read in memory.
//!
//! | Format | Parts |
//! |---|---|
//! | docx | every `w:p` paragraph of `word/document.xml` with text, `#para=N`, `N` counting every paragraph in document order; `w:t` runs, `w:tab` a tab, `w:br`/`w:cr` a new line |
//! | pptx | every slide in presentation order, `#slide=N`; each `a:p` paragraph a line |
//! | xlsx | every cell with a value, `#sheet=NAME!REF`; shared and inline strings, numbers, booleans as `TRUE`/`FALSE` |
//! | odt | every `text:p`/`text:h` with text, `#para=N` |
//! | odp | every `draw:page`, `#slide=N` |
//! | ods | every cell with text, `#sheet=NAME!REF`, repeated cells and rows expanded up to [`REPEAT`] |

use std::collections::HashMap;
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};

use super::archive::Package;
use super::{Budget, Loc, Out, Reason, Unit};

/// Events between two deadline checks.
const CHECK_EVERY: usize = 4096;

/// Copies of a repeated spreadsheet cell or row that are read.
pub(super) const REPEAT: u64 = 1024;

/// One XML event, reduced to what text extraction needs.
enum Ev<'a> {
    Open(&'a BytesStart<'a>),
    /// An element with no content, `<x/>`.
    Leaf(&'a BytesStart<'a>),
    Close(&'a str),
    Text(&'a str),
}

/// Walk `xml`, handing every event to `f`. A parse error is [`Reason::Corrupt`].
fn walk(
    xml: &str,
    budget: &Budget,
    mut f: impl FnMut(Ev<'_>) -> Result<(), Reason>,
) -> Result<(), Reason> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut count = 0usize;
    loop {
        count += 1;
        if count.is_multiple_of(CHECK_EVERY) {
            budget.check()?;
        }
        match reader.read_event().map_err(|_| Reason::Corrupt)? {
            Event::Start(start) => f(Ev::Open(&start))?,
            Event::Empty(start) => f(Ev::Leaf(&start))?,
            Event::End(end) => f(Ev::Close(end.local_name().into_inner()))?,
            Event::Text(text) => f(Ev::Text(&text.xml10_content()))?,
            Event::CData(data) => f(Ev::Text(&data.into_inner()))?,
            Event::GeneralRef(entity) => {
                let text = match entity.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    Ok(None) => match entity.into_inner().as_ref() {
                        "amp" => "&".to_string(),
                        "lt" => "<".to_string(),
                        "gt" => ">".to_string(),
                        "quot" => "\"".to_string(),
                        "apos" => "'".to_string(),
                        _ => String::new(),
                    },
                    Err(_) => String::new(),
                };
                f(Ev::Text(&text))?;
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn name<'a>(start: &'a BytesStart<'_>) -> &'a str {
    start.local_name().into_inner()
}

/// The value of attribute `key` (its qualified name).
fn attr(start: &BytesStart<'_>, key: &str) -> Option<String> {
    start
        .try_get_attribute(key)
        .ok()
        .flatten()
        .and_then(|attr| {
            attr.normalized_value(quick_xml::XmlVersion::default())
                .ok()
                .map(|value| value.into_owned())
        })
}

fn loc(chain: &Arc<[String]>, unit: Unit) -> Loc {
    Loc {
        members: chain.clone(),
        unit: Some(unit),
    }
}

fn required(part: Option<String>) -> Result<String, Reason> {
    part.ok_or(Reason::Corrupt)
}

/// Paragraphs, numbered in the order they open. A paragraph inside another
/// (a text box, a note) is a paragraph of its own.
struct Paragraphs {
    open: Vec<(u32, String)>,
    count: u32,
}

impl Paragraphs {
    fn new() -> Self {
        Self {
            open: Vec::new(),
            count: 0,
        }
    }

    fn open(&mut self) {
        self.count += 1;
        self.open.push((self.count, String::new()));
    }

    /// An empty paragraph still takes a number.
    fn skip(&mut self) {
        self.count += 1;
    }

    fn push(&mut self, text: &str) {
        if let Some((_, buffer)) = self.open.last_mut() {
            buffer.push_str(text);
        }
    }

    fn close(&mut self) -> Option<(u32, String)> {
        self.open.pop()
    }
}

pub(super) fn docx(
    bytes: &[u8],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let mut package = Package::open(bytes)?;
    let xml = required(package.text("word/document.xml", budget)?)?;
    let mut paragraphs = Paragraphs::new();
    let mut in_text = false;
    let mut done = Vec::new();
    walk(&xml, budget, |ev| {
        match ev {
            Ev::Open(e) => match name(e) {
                "p" => paragraphs.open(),
                "t" => in_text = true,
                _ => {}
            },
            Ev::Leaf(e) => match name(e) {
                "p" => paragraphs.skip(),
                "tab" => paragraphs.push("\t"),
                "br" | "cr" => paragraphs.push("\n"),
                _ => {}
            },
            Ev::Close("t") => in_text = false,
            Ev::Close("p") => done.extend(paragraphs.close()),
            Ev::Close(_) => {}
            Ev::Text(text) if in_text => paragraphs.push(text),
            Ev::Text(_) => {}
        }
        Ok(())
    })?;
    done.sort_by_key(|(n, _)| *n);
    for (n, text) in done {
        out.push(loc(chain, Unit::Para(n)), &text, budget);
    }
    Ok(())
}

/// `Id` to `Target` of a relationships part.
fn relationships(xml: &str, budget: &Budget) -> Result<HashMap<String, String>, Reason> {
    let mut map = HashMap::new();
    walk(xml, budget, |ev| {
        if let Ev::Open(e) | Ev::Leaf(e) = ev
            && name(e) == "Relationship"
            && let (Some(id), Some(target)) = (attr(e, "Id"), attr(e, "Target"))
        {
            map.insert(id, target);
        }
        Ok(())
    })?;
    Ok(map)
}

/// Resolve a relationship target against the directory of the part that
/// names it: `base` is that directory, `/`-terminated.
fn resolve(base: &str, target: &str) -> String {
    let joined = match target.strip_prefix('/') {
        Some(absolute) => absolute.to_string(),
        None => format!("{base}{target}"),
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

pub(super) fn pptx(
    bytes: &[u8],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let mut package = Package::open(bytes)?;
    let presentation = required(package.text("ppt/presentation.xml", budget)?)?;
    let rels = required(package.text("ppt/_rels/presentation.xml.rels", budget)?)?;
    let rels = relationships(&rels, budget)?;
    let mut slides = Vec::new();
    walk(&presentation, budget, |ev| {
        if let Ev::Open(e) | Ev::Leaf(e) = ev
            && name(e) == "sldId"
            && let Some(id) = attr(e, "r:id")
        {
            slides.push(id);
        }
        Ok(())
    })?;
    for (i, id) in slides.iter().enumerate() {
        let Some(target) = rels.get(id) else {
            continue;
        };
        let Some(xml) = package.text(&resolve("ppt/", target), budget)? else {
            continue;
        };
        let mut text = String::new();
        let mut in_text = false;
        walk(&xml, budget, |ev| {
            match ev {
                Ev::Open(e) if name(e) == "t" => in_text = true,
                Ev::Close("t") => in_text = false,
                Ev::Close("p") => text.push('\n'),
                Ev::Leaf(e) if name(e) == "br" => text.push('\n'),
                Ev::Text(t) if in_text => text.push_str(t),
                _ => {}
            }
            Ok(())
        })?;
        out.push(loc(chain, Unit::Slide(i as u32 + 1)), &text, budget);
    }
    Ok(())
}

/// Column letters of a 0-based column: 0 is `A`, 26 is `AA`.
pub(super) fn column(mut index: u64) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.reverse();
    String::from_utf8(letters).expect("column letters are ASCII")
}

/// The 0-based column and 1-based row of a reference such as `B7`.
fn parse_ref(cell: &str) -> Option<(u64, u64)> {
    let split = cell.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = cell.split_at(split);
    if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let col = letters
        .bytes()
        .fold(0u64, |acc, b| acc * 26 + u64::from(b - b'A' + 1))
        - 1;
    Some((col, digits.parse().ok()?))
}

pub(super) fn xlsx(
    bytes: &[u8],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let mut package = Package::open(bytes)?;
    let workbook = required(package.text("xl/workbook.xml", budget)?)?;
    let rels = required(package.text("xl/_rels/workbook.xml.rels", budget)?)?;
    let rels = relationships(&rels, budget)?;
    let mut sheets = Vec::new();
    walk(&workbook, budget, |ev| {
        if let Ev::Open(e) | Ev::Leaf(e) = ev
            && name(e) == "sheet"
            && let (Some(sheet), Some(id)) = (attr(e, "name"), attr(e, "r:id"))
        {
            sheets.push((sheet, id));
        }
        Ok(())
    })?;
    let strings = match package.text("xl/sharedStrings.xml", budget)? {
        Some(xml) => shared_strings(&xml, budget)?,
        None => Vec::new(),
    };
    for (sheet, id) in sheets {
        let Some(target) = rels.get(&id) else {
            continue;
        };
        let Some(xml) = package.text(&resolve("xl/", target), budget)? else {
            continue;
        };
        let sheet: Arc<str> = sheet.into();
        worksheet(&xml, &sheet, &strings, chain, budget, out)?;
    }
    Ok(())
}

fn shared_strings(xml: &str, budget: &Budget) -> Result<Vec<String>, Reason> {
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_text = false;
    let mut phonetic = false;
    walk(xml, budget, |ev| {
        match ev {
            Ev::Open(e) => match name(e) {
                "si" => current.clear(),
                "t" => in_text = true,
                "rPh" => phonetic = true,
                _ => {}
            },
            Ev::Leaf(e) if name(e) == "si" => strings.push(String::new()),
            Ev::Close("si") => strings.push(std::mem::take(&mut current)),
            Ev::Close("t") => in_text = false,
            Ev::Close("rPh") => phonetic = false,
            Ev::Text(t) if in_text && !phonetic => current.push_str(t),
            _ => {}
        }
        Ok(())
    })?;
    Ok(strings)
}

fn worksheet(
    xml: &str,
    sheet: &Arc<str>,
    strings: &[String],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let mut cells: Vec<(String, String)> = Vec::new();
    let mut row = 0u64;
    let mut col = 0u64;
    let mut cell: Option<(String, String)> = None;
    let mut value = String::new();
    let mut reading = false;
    walk(xml, budget, |ev| {
        match ev {
            Ev::Open(e) | Ev::Leaf(e) if name(e) == "row" => {
                row = attr(e, "r").and_then(|r| r.parse().ok()).unwrap_or(row + 1);
                col = 0;
            }
            Ev::Open(e) if name(e) == "c" => {
                let reference = attr(e, "r");
                if let Some((c, r)) = reference.as_deref().and_then(parse_ref) {
                    col = c;
                    row = r;
                }
                cell = Some((
                    format!("{}{row}", column(col)),
                    attr(e, "t").unwrap_or_default(),
                ));
                col += 1;
                value.clear();
            }
            Ev::Leaf(e) if name(e) == "c" => col += 1,
            Ev::Open(e) if matches!(name(e), "v" | "t") => reading = true,
            Ev::Close("v" | "t") => reading = false,
            Ev::Text(t) if reading => value.push_str(t),
            Ev::Close("c") => {
                if let Some((reference, kind)) = cell.take() {
                    let text = match kind.as_str() {
                        "s" => value
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| strings.get(i).cloned())
                            .unwrap_or_default(),
                        "b" => match value.trim() {
                            "1" => "TRUE".to_string(),
                            "0" => "FALSE".to_string(),
                            other => other.to_string(),
                        },
                        _ => std::mem::take(&mut value),
                    };
                    cells.push((reference, text));
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    for (reference, text) in cells {
        out.push(
            loc(
                chain,
                Unit::Cell {
                    sheet: sheet.clone(),
                    cell: reference,
                },
            ),
            &text,
            budget,
        );
    }
    Ok(())
}

/// The OpenDocument flavour, by what its units are.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Odf {
    Text,
    Sheet,
    Slides,
}

pub(super) fn odf(
    flavour: Odf,
    bytes: &[u8],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let mut package = Package::open(bytes)?;
    let xml = required(package.text("content.xml", budget)?)?;
    match flavour {
        Odf::Text => odt(&xml, chain, budget, out),
        Odf::Slides => odp(&xml, chain, budget, out),
        Odf::Sheet => ods(&xml, chain, budget, out),
    }
}

/// Text run events shared by every OpenDocument flavour: `text:s` is `c`
/// spaces, `text:tab` a tab, `text:line-break` a new line.
fn odf_inline(e: &BytesStart<'_>) -> Option<String> {
    match name(e) {
        "s" => {
            let count = attr(e, "text:c")
                .and_then(|c| c.parse::<usize>().ok())
                .unwrap_or(1)
                .min(1024);
            Some(" ".repeat(count))
        }
        "tab" => Some("\t".to_string()),
        "line-break" => Some("\n".to_string()),
        _ => None,
    }
}

fn odt(xml: &str, chain: &Arc<[String]>, budget: &mut Budget, out: &mut Out) -> Result<(), Reason> {
    let mut paragraphs = Paragraphs::new();
    let mut done = Vec::new();
    walk(xml, budget, |ev| {
        match ev {
            Ev::Open(e) if matches!(name(e), "p" | "h") => paragraphs.open(),
            Ev::Leaf(e) if matches!(name(e), "p" | "h") => paragraphs.skip(),
            Ev::Leaf(e) => {
                if let Some(text) = odf_inline(e) {
                    paragraphs.push(&text);
                }
            }
            Ev::Close("p" | "h") => done.extend(paragraphs.close()),
            Ev::Text(t) => paragraphs.push(t),
            _ => {}
        }
        Ok(())
    })?;
    done.sort_by_key(|(n, _)| *n);
    for (n, text) in done {
        out.push(loc(chain, Unit::Para(n)), &text, budget);
    }
    Ok(())
}

fn odp(xml: &str, chain: &Arc<[String]>, budget: &mut Budget, out: &mut Out) -> Result<(), Reason> {
    let mut slides: Vec<String> = Vec::new();
    let mut in_page = false;
    let mut depth = 0usize;
    walk(xml, budget, |ev| {
        match ev {
            Ev::Open(e) if name(e) == "page" => {
                in_page = true;
                slides.push(String::new());
            }
            Ev::Leaf(e) if name(e) == "page" => slides.push(String::new()),
            Ev::Close("page") => in_page = false,
            Ev::Open(e) if in_page && name(e) == "p" => depth += 1,
            Ev::Close("p") if in_page => {
                depth = depth.saturating_sub(1);
                if let Some(slide) = slides.last_mut() {
                    slide.push('\n');
                }
            }
            Ev::Leaf(e) if in_page => {
                if let (Some(text), Some(slide)) = (odf_inline(e), slides.last_mut()) {
                    slide.push_str(&text);
                }
            }
            Ev::Text(t) if in_page && depth > 0 => {
                if let Some(slide) = slides.last_mut() {
                    slide.push_str(t);
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    for (i, text) in slides.iter().enumerate() {
        out.push(loc(chain, Unit::Slide(i as u32 + 1)), text, budget);
    }
    Ok(())
}

fn repeated(e: &BytesStart<'_>, key: &str) -> u64 {
    attr(e, key)
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(1)
        .max(1)
}

fn ods(xml: &str, chain: &Arc<[String]>, budget: &mut Budget, out: &mut Out) -> Result<(), Reason> {
    struct Cell {
        col: u64,
        repeat: u64,
        text: String,
    }
    let mut sheet: Arc<str> = Arc::from("");
    let mut row = 0u64;
    let mut row_repeat = 1u64;
    let mut col = 0u64;
    let mut cells: Vec<Cell> = Vec::new();
    let mut cell: Option<Cell> = None;
    let mut paragraphs = 0usize;
    let mut found: Vec<(Arc<str>, String, String)> = Vec::new();
    walk(xml, budget, |ev| {
        match ev {
            Ev::Open(e) if name(e) == "table" => {
                sheet = attr(e, "table:name").unwrap_or_default().into();
                row = 0;
            }
            Ev::Open(e) if name(e) == "table-row" => {
                row_repeat = repeated(e, "table:number-rows-repeated");
                col = 0;
                cells.clear();
            }
            Ev::Leaf(e) if name(e) == "table-row" => {
                row += repeated(e, "table:number-rows-repeated");
            }
            Ev::Close("table-row") => {
                for copy in 0..row_repeat.min(REPEAT) {
                    for cell in &cells {
                        for offset in 0..cell.repeat.min(REPEAT) {
                            found.push((
                                sheet.clone(),
                                format!("{}{}", column(cell.col + offset), row + 1 + copy),
                                cell.text.clone(),
                            ));
                        }
                    }
                }
                row += row_repeat;
            }
            Ev::Open(e) if matches!(name(e), "table-cell" | "covered-table-cell") => {
                let repeat = repeated(e, "table:number-columns-repeated");
                cell = Some(Cell {
                    col,
                    repeat,
                    text: String::new(),
                });
                col += repeat;
            }
            Ev::Leaf(e) if matches!(name(e), "table-cell" | "covered-table-cell") => {
                col += repeated(e, "table:number-columns-repeated");
            }
            Ev::Close("table-cell" | "covered-table-cell") => {
                if let Some(cell) = cell.take()
                    && !cell.text.trim().is_empty()
                {
                    cells.push(cell);
                }
            }
            Ev::Open(e) if name(e) == "p" => {
                if let Some(cell) = cell.as_mut()
                    && paragraphs == 0
                    && !cell.text.is_empty()
                {
                    cell.text.push('\n');
                }
                paragraphs += 1;
            }
            Ev::Close("p") => paragraphs = paragraphs.saturating_sub(1),
            Ev::Leaf(e) => {
                if let (Some(text), Some(cell)) = (odf_inline(e), cell.as_mut()) {
                    cell.text.push_str(&text);
                }
            }
            Ev::Text(t) if paragraphs > 0 => {
                if let Some(cell) = cell.as_mut() {
                    cell.text.push_str(t);
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    for (sheet, cell, text) in found {
        out.push(loc(chain, Unit::Cell { sheet, cell }), &text, budget);
    }
    Ok(())
}
