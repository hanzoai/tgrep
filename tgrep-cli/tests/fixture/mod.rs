//! Documents built byte by byte at test time: PDF, zip (stored), tar and gzip
//! (stored deflate blocks). Nothing here depends on an archive library, so the
//! bytes the adapters read are exactly the formats' specifications.
#![allow(dead_code)]

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A PDF whose page `i` shows the lines of `pages[i]` in Helvetica.
pub fn pdf(pages: &[&[&str]]) -> Vec<u8> {
    let count = pages.len();
    let font = 3 + 2 * count;
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {count} >>",
            (0..count)
                .map(|i| format!("{} 0 R", 3 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        )
        .into_bytes(),
    ];
    for (i, lines) in pages.iter().enumerate() {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
                 /Resources << /Font << /F1 {font} 0 R >> >> >>",
                4 + 2 * i
            )
            .into_bytes(),
        );
        let mut stream = String::from("BT /F1 12 Tf 72 712 Td");
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                stream.push_str(" 0 -14 Td");
            }
            stream.push_str(&format!(
                " ({}) Tj",
                line.replace('(', "\\(").replace(')', "\\)")
            ));
        }
        stream.push_str(" ET");
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            )
            .into_bytes(),
        );
    }
    objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A zip of stored members; a name ending in `/` is a directory.
pub fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    const DATE: u16 = (40 << 9) | (1 << 5) | 1;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        let name_len = name.len() as u16;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for field in [20u16, 0, 0, 0, DATE] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        for field in [crc, size, size] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        let external: u32 = if name.ends_with('/') { 0x10 } else { 0 };
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for field in [20u16, 20, 0, 0, 0, DATE] {
            central.extend_from_slice(&field.to_le_bytes());
        }
        for field in [crc, size, size] {
            central.extend_from_slice(&field.to_le_bytes());
        }
        for field in [name_len, 0, 0, 0, 0] {
            central.extend_from_slice(&field.to_le_bytes());
        }
        central.extend_from_slice(&external.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    let count = entries.len() as u16;
    for field in [0u16, 0, count, count] {
        out.extend_from_slice(&field.to_le_bytes());
    }
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A ustar archive of regular files.
pub fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// A gzip stream of stored deflate blocks.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65_535).collect()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn relationships(targets: &[(String, String)]) -> String {
    let mut xml = format!(r#"{XML}<Relationships xmlns="{REL}">"#);
    for (id, target) in targets {
        xml.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{R}/x" Target="{target}"/>"#
        ));
    }
    xml.push_str("</Relationships>");
    xml
}

/// A docx whose body holds `paragraphs`; `""` is an empty paragraph, a tab
/// is `w:tab` and a newline `w:br`.
pub fn docx(paragraphs: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for paragraph in paragraphs {
        if paragraph.is_empty() {
            body.push_str("<w:p/>");
            continue;
        }
        body.push_str("<w:p><w:r>");
        for (i, line) in paragraph.split('\n').enumerate() {
            if i > 0 {
                body.push_str("<w:br/>");
            }
            for (j, piece) in line.split('\t').enumerate() {
                if j > 0 {
                    body.push_str("<w:tab/>");
                }
                body.push_str(&format!(
                    r#"<w:t xml:space="preserve">{}</w:t>"#,
                    xml_escape(piece)
                ));
            }
        }
        body.push_str("</w:r></w:p>");
    }
    let document =
        format!(r#"{XML}<w:document xmlns:w="{W}"><w:body>{body}</w:body></w:document>"#);
    zip(&[
        ("[Content_Types].xml", b"<Types/>"),
        ("word/document.xml", document.as_bytes()),
    ])
}

/// A pptx of `slides`, each a list of paragraphs. Slide files are numbered
/// in reverse, so only the presentation's own list gives the order.
pub fn pptx(slides: &[&[&str]]) -> Vec<u8> {
    let count = slides.len();
    let mut ids = String::new();
    let mut targets = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for (i, paragraphs) in slides.iter().enumerate() {
        let file = count - i;
        ids.push_str(&format!(
            r#"<p:sldId id="{}" r:id="rId{}"/>"#,
            256 + i,
            i + 2
        ));
        targets.push((format!("rId{}", i + 2), format!("slides/slide{file}.xml")));
        let mut body = String::new();
        for paragraph in *paragraphs {
            body.push_str(&format!(
                "<a:p><a:r><a:t>{}</a:t></a:r></a:p>",
                xml_escape(paragraph)
            ));
        }
        files.push((
            format!("ppt/slides/slide{file}.xml"),
            format!(
                r#"{XML}<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree><p:sp><p:txBody>{body}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            ),
        ));
    }
    let presentation = format!(
        r#"{XML}<p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#
    );
    files.push(("ppt/presentation.xml".into(), presentation));
    files.push((
        "ppt/_rels/presentation.xml.rels".into(),
        relationships(&targets),
    ));
    let entries: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_bytes()))
        .collect();
    zip(&entries)
}

/// An xlsx of `sheets`, each a name and its `(reference, value)` cells. A
/// value that parses as a number is a number cell; anything else a shared
/// string.
pub fn xlsx(sheets: &[(&str, &[(&str, &str)])]) -> Vec<u8> {
    let mut strings: Vec<String> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut entries = String::new();
    let mut targets = Vec::new();
    for (i, (name, cells)) in sheets.iter().enumerate() {
        entries.push_str(&format!(
            r#"<sheet name="{}" sheetId="{}" r:id="rId{}"/>"#,
            xml_escape(name),
            i + 1,
            i + 1
        ));
        targets.push((
            format!("rId{}", i + 1),
            format!("worksheets/sheet{}.xml", i + 1),
        ));
        let mut rows: std::collections::BTreeMap<u32, String> = std::collections::BTreeMap::new();
        for (reference, value) in *cells {
            let row: u32 = reference
                .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                .parse()
                .unwrap();
            let cell = if value.parse::<f64>().is_ok() {
                format!(r#"<c r="{reference}"><v>{value}</v></c>"#)
            } else {
                strings.push(value.to_string());
                format!(
                    r#"<c r="{reference}" t="s"><v>{}</v></c>"#,
                    strings.len() - 1
                )
            };
            rows.entry(row).or_default().push_str(&cell);
        }
        let data: String = rows
            .iter()
            .map(|(row, cells)| format!(r#"<row r="{row}">{cells}</row>"#))
            .collect();
        files.push((
            format!("xl/worksheets/sheet{}.xml", i + 1),
            format!(r#"{XML}<worksheet xmlns="{S}"><sheetData>{data}</sheetData></worksheet>"#),
        ));
    }
    let shared: String = strings
        .iter()
        .map(|s| format!("<si><t>{}</t></si>", xml_escape(s)))
        .collect();
    files.push((
        "xl/workbook.xml".into(),
        format!(
            r#"{XML}<workbook xmlns="{S}" xmlns:r="{R}"><sheets>{entries}</sheets></workbook>"#
        ),
    ));
    files.push(("xl/_rels/workbook.xml.rels".into(), relationships(&targets)));
    files.push((
        "xl/sharedStrings.xml".into(),
        format!(r#"{XML}<sst xmlns="{S}">{shared}</sst>"#),
    ));
    let entries: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_bytes()))
        .collect();
    zip(&entries)
}

const ODF: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0""#;

fn odf(mimetype: &str, body: &str) -> Vec<u8> {
    let content = format!(
        r#"{XML}<office:document-content {ODF}><office:body>{body}</office:body></office:document-content>"#
    );
    zip(&[
        ("mimetype", mimetype.as_bytes()),
        ("content.xml", content.as_bytes()),
    ])
}

/// An odt whose first paragraph is a heading.
pub fn odt(paragraphs: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for (i, paragraph) in paragraphs.iter().enumerate() {
        let tag = if i == 0 { "text:h" } else { "text:p" };
        body.push_str(&format!("<{tag}>{}</{tag}>", xml_escape(paragraph)));
    }
    odf(
        "application/vnd.oasis.opendocument.text",
        &format!("<office:text>{body}</office:text>"),
    )
}

/// An ods of `sheets`, each a name and its rows of cell texts; `""` is an
/// empty cell.
pub fn ods(sheets: &[(&str, &[&[&str]])]) -> Vec<u8> {
    let mut body = String::new();
    for (name, rows) in sheets {
        body.push_str(&format!(
            r#"<table:table table:name="{}">"#,
            xml_escape(name)
        ));
        for row in *rows {
            body.push_str("<table:table-row>");
            for cell in *row {
                if cell.is_empty() {
                    body.push_str("<table:table-cell/>");
                } else {
                    body.push_str(&format!(
                        r#"<table:table-cell office:value-type="string"><text:p>{}</text:p></table:table-cell>"#,
                        xml_escape(cell)
                    ));
                }
            }
            body.push_str("</table:table-row>");
        }
        body.push_str("</table:table>");
    }
    odf(
        "application/vnd.oasis.opendocument.spreadsheet",
        &format!("<office:spreadsheet>{body}</office:spreadsheet>"),
    )
}

/// An odp of `slides`, each a list of paragraphs.
pub fn odp(slides: &[&[&str]]) -> Vec<u8> {
    let mut body = String::new();
    for (i, paragraphs) in slides.iter().enumerate() {
        body.push_str(&format!(
            r#"<draw:page draw:name="p{}"><draw:frame><draw:text-box>"#,
            i + 1
        ));
        for paragraph in *paragraphs {
            body.push_str(&format!("<text:p>{}</text:p>", xml_escape(paragraph)));
        }
        body.push_str("</draw:text-box></draw:frame></draw:page>");
    }
    odf(
        "application/vnd.oasis.opendocument.presentation",
        &format!("<office:presentation>{body}</office:presentation>"),
    )
}
