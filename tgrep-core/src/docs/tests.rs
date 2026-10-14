use std::io::Write;
use std::time::Duration;

use super::*;

/// A PDF whose pages show `pages[i]`, one line per entry, in Helvetica.
/// `deflate` compresses the content streams.
pub(crate) fn pdf(pages: &[&[&str]], deflate: bool) -> Vec<u8> {
    let mut objects: Vec<Vec<u8>> = Vec::new();
    let page_count = pages.len();
    let font = 3 + 2 * page_count;
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    let kids: Vec<String> = (0..page_count)
        .map(|i| format!("{} 0 R", 3 + 2 * i))
        .collect();
    objects.push(
        format!(
            "<< /Type /Pages /Kids [{}] /Count {page_count} >>",
            kids.join(" ")
        )
        .into_bytes(),
    );
    for (i, lines) in pages.iter().enumerate() {
        let content_id = 4 + 2 * i;
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {content_id} 0 R \
                 /Resources << /Font << /F1 {font} 0 R >> >> >>"
            )
            .into_bytes(),
        );
        let mut stream = String::from("BT /F1 12 Tf 72 712 Td");
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                stream.push_str(" 0 -14 Td");
            }
            let escaped = line
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)");
            stream.push_str(&format!(" ({escaped}) Tj"));
        }
        stream.push_str(" ET");
        let (data, filter) = if deflate {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(stream.as_bytes()).unwrap();
            (encoder.finish().unwrap(), " /Filter /FlateDecode")
        } else {
            (stream.into_bytes(), "")
        };
        let mut object = format!("<< /Length {}{filter} >>\nstream\n", data.len()).into_bytes();
        object.extend_from_slice(&data);
        object.extend_from_slice(b"\nendstream");
        objects.push(object);
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

pub(crate) fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        if name.ends_with('/') {
            writer.add_directory(*name, options).unwrap();
        } else {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
    }
    writer.finish().unwrap().into_inner()
}

pub(crate) fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, *data).unwrap();
    }
    builder.into_inner().unwrap()
}

pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn shown(doc: &Doc) -> Vec<(String, String)> {
    doc.parts
        .iter()
        .map(|part| (part.loc.to_string(), doc.part_text(part).to_string()))
        .collect()
}

#[test]
fn pdf_pages_are_parts_with_their_lines() {
    for deflate in [false, true] {
        let bytes = pdf(
            &[&["Hello page one", "second line"], &["Other (page) two"]],
            deflate,
        );
        let doc = extract("report.pdf", &bytes).unwrap();
        assert_eq!(
            shown(&doc),
            vec![
                ("#page=1".into(), "Hello page one\nsecond line\n".into()),
                ("#page=2".into(), "Other (page) two\n".into()),
            ]
        );
        assert_eq!(doc.text, "Hello page one\nsecond line\nOther (page) two\n");
    }
}

#[test]
fn pdf_page_text_matches_pdftotext() {
    let Ok(output) = std::process::Command::new("pdftotext").arg("-v").output() else {
        eprintln!("pdftotext not installed; skipping the cross-check");
        return;
    };
    assert!(output.status.success() || !output.stderr.is_empty());
    let dir = tempfile::tempdir().unwrap();
    let pages: &[&[&str]] = &[&["alpha beta gamma", "delta"], &["epsilon zeta"]];
    let path = dir.path().join("x.pdf");
    std::fs::write(&path, pdf(pages, true)).unwrap();
    let doc = extract("x.pdf", &std::fs::read(&path).unwrap()).unwrap();
    for (i, part) in doc.parts.iter().enumerate() {
        let page = (i + 1).to_string();
        let output = std::process::Command::new("pdftotext")
            .args(["-f", &page, "-l", &page, "-layout"])
            .arg(&path)
            .arg("-")
            .output()
            .unwrap();
        let theirs: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let ours: Vec<String> = doc
            .part_text(part)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert_eq!(ours, theirs, "page {page}");
    }
}

#[test]
fn archives_nest_and_name_every_member() {
    let inner_pdf = pdf(&[&["inside the pdf"]], true);
    let inner_zip = zip(&[("deep/note.txt", b"deep note\nsecond\n")]);
    let outer = zip(&[
        ("dir/", b""),
        ("dir/a.txt", b"first line\nneedle here\n"),
        ("doc.pdf", &inner_pdf),
        ("inner.zip", &inner_zip),
        ("image.png", b"\x89PNG not text"),
        ("raw.bin", b"bytes\0with nul"),
    ]);
    let doc = extract("bundle.zip", &outer).unwrap();
    assert_eq!(
        shown(&doc),
        vec![
            ("!dir/a.txt".into(), "first line\nneedle here\n".into()),
            ("!doc.pdf#page=1".into(), "inside the pdf\n".into()),
            (
                "!inner.zip!deep/note.txt".into(),
                "deep note\nsecond\n".into()
            ),
        ]
    );
    assert_eq!(doc.skipped.get(Reason::Binary), 2);

    let tarball = gzip(&tar(&[
        ("logs/app.log", b"started\nfailed: timeout\n"),
        ("b.zip", &outer),
    ]));
    let doc = extract("logs.tar.gz", &tarball).unwrap();
    let names: Vec<String> = doc.parts.iter().map(|part| part.loc.to_string()).collect();
    assert_eq!(
        names,
        [
            "!logs/app.log",
            "!b.zip!dir/a.txt",
            "!b.zip!doc.pdf#page=1",
            "!b.zip!inner.zip!deep/note.txt"
        ]
    );
    assert_eq!(extract("logs.tgz", &tarball).unwrap().text, doc.text);
    assert_eq!(
        extract("logs.tar", &tar(&[("x.txt", b"plain\n")]))
            .unwrap()
            .text,
        "plain\n"
    );
}

#[test]
fn gzip_is_read_by_the_name_inside_it() {
    let doc = extract("app.log.gz", &gzip(b"one\ntwo\n")).unwrap();
    assert_eq!(shown(&doc), vec![(String::new(), "one\ntwo\n".into())]);
    let doc = extract("report.pdf.gz", &gzip(&pdf(&[&["gz page"]], false))).unwrap();
    assert_eq!(shown(&doc), vec![("#page=1".into(), "gz page\n".into())]);
}

#[test]
fn members_leaving_the_archive_and_links_are_refused() {
    let doc = extract(
        "evil.zip",
        &zip(&[
            ("../escape.txt", b"no"),
            ("/abs.txt", b"no"),
            ("C:/win.txt", b"no"),
            ("ok.txt", b"yes\n"),
        ]),
    )
    .unwrap();
    assert_eq!(shown(&doc), vec![("!ok.txt".into(), "yes\n".into())]);
    assert_eq!(doc.skipped.get(Reason::Refused), 3);

    let mut builder = tar::Builder::new(Vec::new());
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    builder
        .append_link(&mut link, "passwd", "/etc/passwd")
        .unwrap();
    let mut traversal = tar::Header::new_old();
    traversal.as_old_mut().name[..9].copy_from_slice(b"../x.txt\0");
    traversal.set_size(3);
    traversal.set_cksum();
    builder.append(&traversal, &b"bad"[..]).unwrap();
    let doc = extract("links.tar", &builder.into_inner().unwrap()).unwrap();
    assert!(doc.parts.is_empty());
    assert_eq!(doc.skipped.get(Reason::Refused), 2);

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .add_symlink(
            "link",
            "/etc/passwd",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    let doc = extract("link.zip", &writer.finish().unwrap().into_inner()).unwrap();
    assert_eq!(doc.skipped.get(Reason::Refused), 1);
}

#[test]
fn a_member_past_the_entry_bound_is_skipped_unread() {
    let limits = Limits {
        entry: 1024,
        ..Limits::default()
    };
    let bomb = vec![b'a'; 1 << 20];
    let doc = extract_with(
        "bomb.zip",
        &zip(&[("big.txt", &bomb), ("small.txt", b"fits\n")]),
        limits,
    )
    .unwrap();
    assert_eq!(shown(&doc), vec![("!small.txt".into(), "fits\n".into())]);
    assert_eq!(doc.skipped.get(Reason::Large), 1);
    // A member that lies about its size is cut at the bound.
    let mut lying = zip(&[("big.txt", &bomb)]);
    let size = (bomb.len() as u32).to_le_bytes();
    let tiny = 10u32.to_le_bytes();
    for at in (0..lying.len() - 4).rev() {
        if lying[at..at + 4] == size {
            lying[at..at + 4].copy_from_slice(&tiny);
        }
    }
    let doc = extract_with("lying.zip", &lying, limits);
    assert!(
        doc.as_ref().map_or(true, |doc| doc.text.len() <= 1024),
        "{doc:?}"
    );
}

#[test]
fn the_document_total_bounds_every_member_together() {
    let limits = Limits {
        entry: 1 << 20,
        total: 3000,
        ..Limits::default()
    };
    let block = vec![b'x'; 1000];
    let entries: Vec<(String, Vec<u8>)> = (0..6)
        .map(|i| (format!("{i}.txt"), block.clone()))
        .collect();
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    let doc = extract_with("many.zip", &zip(&refs), limits).unwrap();
    assert_eq!(doc.parts.len(), 3);
    assert_eq!(doc.skipped.get(Reason::Total), 3);
    // A gzip stream that inflates past the total stops at it.
    let doc = extract_with(
        "bomb.tar.gz",
        &gzip(&tar(&[("a.txt", &vec![b'y'; 1 << 20])])),
        limits,
    );
    assert_eq!(doc, Err(Reason::Total));
}

#[test]
fn nesting_deeper_than_the_bound_is_skipped() {
    let mut data = zip(&[("leaf.txt", b"leaf\n")]);
    for _ in 0..5 {
        data = zip(&[("n.zip", &data)]);
    }
    let doc = extract("deep.zip", &data).unwrap();
    assert!(doc.parts.is_empty());
    assert_eq!(doc.skipped.get(Reason::Depth), 1);
    let limits = Limits {
        depth: 7,
        ..Limits::default()
    };
    let doc = extract_with("deep.zip", &data, limits).unwrap();
    assert_eq!(
        doc.parts[0].loc.to_string(),
        "!n.zip!n.zip!n.zip!n.zip!n.zip!leaf.txt"
    );
}

#[test]
fn members_past_the_member_bound_are_not_read() {
    let limits = Limits {
        members: 2,
        ..Limits::default()
    };
    let data = zip(&[("a.txt", b"a\n"), ("b.txt", b"b\n"), ("c.txt", b"c\n")]);
    let doc = extract_with("m.zip", &data, limits).unwrap();
    assert_eq!(doc.parts.len(), 2);
    assert_eq!(doc.skipped.get(Reason::Members), 1);
    let doc = extract_with(
        "m.tar",
        &tar(&[("a.txt", b"a\n"), ("b.txt", b"b\n"), ("c.txt", b"c\n")]),
        limits,
    )
    .unwrap();
    assert_eq!(doc.parts.len(), 2);
    assert_eq!(doc.skipped.get(Reason::Members), 1);
}

#[test]
fn text_past_its_bound_is_dropped_by_part() {
    let limits = Limits {
        text: 10,
        ..Limits::default()
    };
    let doc = extract_with(
        "t.zip",
        &zip(&[("a.txt", b"12345\n"), ("b.txt", b"67890\n")]),
        limits,
    )
    .unwrap();
    assert_eq!(doc.text, "12345\n");
    assert_eq!(doc.skipped.get(Reason::Text), 1);
}

#[test]
fn a_document_out_of_time_fails() {
    let limits = Limits {
        time: Duration::ZERO,
        ..Limits::default()
    };
    std::thread::sleep(Duration::from_millis(2));
    assert_eq!(
        extract_with("t.zip", &zip(&[("a.txt", b"a\n")]), limits),
        Err(Reason::Time)
    );
    assert_eq!(
        extract_with("t.pdf", &pdf(&[&["x"]], false), limits),
        Err(Reason::Time)
    );
}

#[test]
fn damaged_documents_fail_without_panicking() {
    assert_eq!(extract("x.pdf", b"%PDF-1.4 garbage"), Err(Reason::Corrupt));
    assert_eq!(
        extract("x.zip", b"PK\x03\x04 garbage"),
        Err(Reason::Corrupt)
    );
    assert_eq!(extract("x.gz", b"\x1f\x8b garbage"), Err(Reason::Corrupt));
    assert_eq!(extract("x.txt", b"text"), Err(Reason::Unsupported));
    let samples = [
        ("a.pdf", pdf(&[&["one"], &["two"]], true)),
        (
            "a.zip",
            zip(&[("a.txt", b"hello\n"), ("b.pdf", &pdf(&[&["p"]], true))]),
        ),
        ("a.tar.gz", gzip(&tar(&[("a.txt", b"hello\n")]))),
        ("a.gz", gzip(b"hello\n")),
    ];
    for (name, bytes) in &samples {
        let step = (bytes.len() / 97).max(1);
        for len in (0..bytes.len()).step_by(step) {
            let result = extract(name, &bytes[..len]);
            assert_ne!(result, Err(Reason::Panic), "{name} cut at {len}");
        }
        let mut flipped = bytes.clone();
        for at in (0..flipped.len()).step_by(step) {
            flipped[at] ^= 0x5a;
            assert_ne!(
                extract(name, &flipped),
                Err(Reason::Panic),
                "{name} flipped at {at}"
            );
        }
    }
}

#[test]
fn locations_print_and_parse_back() {
    let loc = Loc {
        members: vec!["a!b#c:d%e.zip".to_string(), "x/y z.pdf".to_string()].into(),
        unit: Some(Unit::Page(7)),
    };
    let text = loc.to_string();
    assert_eq!(text, "!a%21b%23c%3Ad%25e.zip!x/y z.pdf#page=7");
    assert_eq!(Loc::parse(&text), Some(loc.clone()));
    assert_eq!(
        loc.json(),
        serde_json::json!({"members": ["a!b#c:d%e.zip", "x/y z.pdf"], "page": 7})
    );
    let file = join("docs/r.zip", &loc);
    assert_eq!(split(&file), ("docs/r.zip", Some(text.as_str())));
    assert_eq!(join("a.txt", &Loc::default()), "a.txt");
    assert_eq!(split("a.txt"), ("a.txt", None));
    assert_eq!(Loc::parse("no-bang"), None);
}

#[test]
fn the_cache_returns_what_was_extracted_and_remembers_failures() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(dir.path()).unwrap();
    let bytes = zip(&[("a.txt", b"cached text\n")]);
    let first = load_with(Some(&cache), "c.zip", &bytes).unwrap();
    assert_eq!(cache.usage().unwrap().0, 1);
    let key = cache::key(Kind::Zip, &bytes);
    assert_eq!(cache.get(&key), Some(Ok((*first).clone())));
    assert_eq!(*load_with(Some(&cache), "c.zip", &bytes).unwrap(), *first);
    assert_eq!(
        load_with(Some(&cache), "bad.pdf", b"nope"),
        Err(Reason::Corrupt)
    );
    assert_eq!(
        cache.get(&cache::key(Kind::Pdf, b"nope")),
        Some(Err(Reason::Corrupt))
    );
    // The same bytes under another adapter are another entry.
    assert_eq!(cache.get(&cache::key(Kind::Tar, &bytes)), None);

    let fresh = Cache::open(dir.path()).unwrap();
    load_with(Some(&fresh), "c.zip", &bytes).unwrap();
    assert_eq!(fresh.sweep().unwrap(), 1);
    assert_eq!(fresh.usage().unwrap().0, 1);
}

#[test]
fn index_text_marks_unreadable_documents_binary() {
    assert_eq!(
        index_text("a.rs", b"fn main() {}\n").as_ref(),
        b"fn main() {}\n"
    );
    assert!(crate::trigram::is_binary(&index_text("a.pdf", b"broken")));
    let bytes = zip(&[("a.txt", b"zipped text\n"), ("b.txt", b"nul\0member\n")]);
    assert_eq!(index_text("a.zip", &bytes).as_ref(), b"zipped text\n");
}
