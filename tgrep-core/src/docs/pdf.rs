//! PDF: the text of each page, from its content stream's text operators.
//!
//! Strings shown by `Tj`, `TJ`, `'` and `"` are decoded through the current
//! font's encoding (its `ToUnicode` map when it has one). A new line starts at
//! `T*`, `'`, `"`, a `Td`/`TD`/`Tm` that moves vertically and the end of a text
//! object; a horizontal move or a wide `TJ` gap becomes a space. Every stream
//! is decompressed within the member bound, so a compressed page cannot
//! inflate without limit.

use std::collections::BTreeMap;
use std::sync::Arc;

use lopdf::{Document, Encoding, Object, content::Content};

use super::{Budget, Loc, Out, Reason, Unit};

/// A `TJ` adjustment more negative than this, in thousandths of an em, is a
/// gap between words rather than kerning.
const WORD_GAP: f32 = -200.0;

/// Operators between two deadline checks.
const CHECK_EVERY: usize = 4096;

pub(super) fn extract(
    bytes: &[u8],
    chain: &Arc<[String]>,
    budget: &mut Budget,
    out: &mut Out,
) -> Result<(), Reason> {
    let limit = usize::try_from(budget.room()).unwrap_or(usize::MAX);
    let options = lopdf::LoadOptions {
        max_decompressed_size: Some(limit),
        ..Default::default()
    };
    let doc = Document::load_mem_with_options(bytes, options).map_err(|error| match error {
        lopdf::Error::Decryption(_)
        | lopdf::Error::InvalidPassword
        | lopdf::Error::UnsupportedSecurityHandler(_) => Reason::Encrypted,
        _ => Reason::Corrupt,
    })?;
    for (number, page) in doc.get_pages() {
        budget.check()?;
        let text = page_text(&doc, page, limit, budget)?;
        out.push(
            Loc {
                members: chain.clone(),
                unit: Some(Unit::Page(number)),
            },
            &text,
            budget,
        );
    }
    Ok(())
}

fn page_text(
    doc: &Document,
    page: lopdf::ObjectId,
    limit: usize,
    budget: &Budget,
) -> Result<String, Reason> {
    let encodings: BTreeMap<Vec<u8>, Encoding> = match doc.get_page_fonts(page) {
        Ok(fonts) => fonts
            .into_iter()
            .filter_map(|(name, font)| {
                font.get_font_encoding_with_limit(doc, limit)
                    .ok()
                    .map(|encoding| (name, encoding))
            })
            .collect(),
        Err(_) => BTreeMap::new(),
    };
    let Ok(data) = doc.get_page_content_with_limit(page, limit) else {
        return Ok(String::new());
    };
    let Ok(content) = Content::decode(&data) else {
        return Ok(String::new());
    };
    let mut text = String::new();
    let mut font: Option<&Encoding> = None;
    let mut line_y: Option<f32> = None;
    for (i, op) in content.operations.iter().enumerate() {
        if i.is_multiple_of(CHECK_EVERY) {
            budget.check()?;
        }
        let operands = &op.operands;
        match op.operator.as_str() {
            "Tf" => {
                font = operands
                    .first()
                    .and_then(|name| name.as_name().ok())
                    .and_then(|name| encodings.get(name));
            }
            "Tj" => show(&mut text, font, operands.first()),
            "TJ" => {
                if let Some(Object::Array(items)) = operands.first() {
                    for item in items {
                        match item {
                            Object::String(..) => show(&mut text, font, Some(item)),
                            _ => {
                                if item.as_float().is_ok_and(|gap| gap < WORD_GAP) {
                                    space(&mut text);
                                }
                            }
                        }
                    }
                }
            }
            "'" => {
                newline(&mut text);
                show(&mut text, font, operands.first());
            }
            "\"" => {
                newline(&mut text);
                show(&mut text, font, operands.get(2));
            }
            "T*" | "ET" => newline(&mut text),
            "Td" | "TD" => {
                let dy = operands
                    .get(1)
                    .and_then(|y| y.as_float().ok())
                    .unwrap_or(0.0);
                if dy != 0.0 {
                    newline(&mut text);
                } else {
                    space(&mut text);
                }
            }
            "Tm" => {
                let y = operands.get(5).and_then(|y| y.as_float().ok());
                if y.is_some() && y != line_y {
                    newline(&mut text);
                } else {
                    space(&mut text);
                }
                line_y = y;
            }
            _ => {}
        }
    }
    Ok(text)
}

fn show(text: &mut String, font: Option<&Encoding>, operand: Option<&Object>) {
    let (Some(encoding), Some(Object::String(bytes, _))) = (font, operand) else {
        return;
    };
    if let Ok(decoded) = Document::decode_text(encoding, bytes) {
        text.push_str(&decoded);
    }
}

fn newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

fn space(text: &mut String) {
    if !text.is_empty() && !text.ends_with([' ', '\n']) {
        text.push(' ');
    }
}
