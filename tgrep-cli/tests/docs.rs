//! Documents (feature `docs`): extracted text is indexed by the trigram index
//! and every hit names the file plus where inside it.
#![cfg(feature = "docs")]

mod fixture;

use std::collections::BTreeSet;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn tgrep_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("tgrep")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn lines(&self) -> BTreeSet<String> {
        self.stdout.lines().map(str::to_string).collect()
    }
}

/// Run tgrep in `root` with `args`.
fn run(root: &Path, args: &[&str]) -> Run {
    let output = Command::new(tgrep_bin())
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The documents and source files every test searches. Returns the relative
/// paths written.
fn corpus(root: &Path) -> Vec<&'static str> {
    let files: Vec<(&'static str, Vec<u8>)> = vec![
        (
            "docs/report.pdf",
            fixture::pdf(&[
                &["Quarterly report", "revenue grew needle one"],
                &["second page needle two", "closing words"],
            ]),
        ),
        (
            "docs/bundle.zip",
            fixture::zip(&[
                ("notes/", b""),
                ("notes/todo.txt", b"call the bank\nneedle in zip\n"),
                ("inner/deck.pdf", &fixture::pdf(&[&["nested needle pdf"]])),
                ("logo.png", b"\x89PNG\r\n\x1a\n\0\0"),
            ]),
        ),
        (
            "docs/logs.tar.gz",
            fixture::gzip(&fixture::tar(&[
                ("2026/app.log", b"started\nneedle in tar\nstopped\n"),
                (
                    "deep.zip",
                    &fixture::zip(&[("deep.txt", b"x\ndeep needle\n")]),
                ),
            ])),
        ),
        (
            "docs/app.log.gz",
            fixture::gzip(b"line one\nneedle in gz\n"),
        ),
        (
            "docs/plain.tar",
            fixture::tar(&[("readme.md", b"# Title\nneedle in plain tar\n")]),
        ),
        ("src/main.rs", b"fn needle() {}\nfn main() {}\n".to_vec()),
        ("notes.txt", b"no match here\nneedle in notes\n".to_vec()),
    ];
    for (path, data) in &files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data).unwrap();
    }
    files.into_iter().map(|(path, _)| path).collect()
}

fn repo() -> (TempDir, PathBuf, Vec<&'static str>) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join(".git")).unwrap();
    let files = corpus(&root);
    (temp, root, files)
}

fn index(root: &Path) -> Run {
    let run = run(root, &["index", "."]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    run
}

/// Every `path[location]:line:text` a regex scan of each file's extracted text
/// finds: the oracle indexed and scanned searches must equal.
fn scan(root: &Path, files: &[&str], pattern: &str) -> BTreeSet<String> {
    let regex = regex::Regex::new(pattern).unwrap();
    let mut hits = BTreeSet::new();
    let mut add = |shown: &str, text: &str| {
        for (n, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                hits.insert(format!("{shown}:{}:{line}", n + 1));
            }
        }
    };
    for file in files {
        let bytes = fs::read(root.join(file)).unwrap();
        if tgrep_core::docs::handles(Path::new(file)) {
            let doc = tgrep_core::docs::extract(file, &bytes).unwrap();
            for part in &doc.parts {
                add(&format!("{file}{}", part.loc), doc.part_text(part));
            }
        } else {
            add(file, &String::from_utf8(bytes).unwrap());
        }
    }
    hits
}

#[test]
fn indexed_hits_name_the_page_member_and_line() {
    let (_temp, root, _) = repo();
    index(&root);
    let found = run(&root, &["-n", "--", "needle"]);
    assert_eq!(found.code, 0, "{}", found.stderr);
    let expected: BTreeSet<String> = [
        "docs/report.pdf#page=1:2:revenue grew needle one",
        "docs/report.pdf#page=2:1:second page needle two",
        "docs/bundle.zip!notes/todo.txt:2:needle in zip",
        "docs/bundle.zip!inner/deck.pdf#page=1:1:nested needle pdf",
        "docs/logs.tar.gz!2026/app.log:2:needle in tar",
        "docs/logs.tar.gz!deep.zip!deep.txt:2:deep needle",
        "docs/app.log.gz:2:needle in gz",
        "docs/plain.tar!readme.md:2:needle in plain tar",
        "src/main.rs:1:fn needle() {}",
        "notes.txt:2:needle in notes",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(found.lines(), expected);
}

#[test]
fn hits_equal_a_regex_scan_of_the_extracted_text() {
    let (_temp, root, files) = repo();
    index(&root);
    for pattern in [
        "needle",
        r"\bpage\b",
        "(?i)NEEDLE (in|one)",
        "^[a-z]+$",
        "zip$",
    ] {
        let expected = scan(&root, &files, pattern);
        assert!(!expected.is_empty(), "{pattern} finds nothing to compare");
        for mode in [&[][..], &["--no-index"][..]] {
            let mut args = vec!["-n"];
            args.extend_from_slice(mode);
            args.extend_from_slice(&["--", pattern]);
            let found = run(&root, &args);
            assert_eq!(found.code, 0, "{pattern} {mode:?}: {}", found.stderr);
            assert_eq!(found.lines(), expected, "{pattern} {mode:?}");
        }
    }
}

#[test]
fn exit_codes_follow_ripgrep() {
    let (_temp, root, _) = repo();
    index(&root);
    assert_eq!(run(&root, &["-q", "--", "nested needle"]).code, 0);
    let none = run(&root, &["--", "absent-in-every-document"]);
    assert_eq!((none.code, none.stdout.as_str()), (1, ""));
    assert_eq!(run(&root, &["--", "unclosed("]).code, 2);
}

#[test]
fn files_with_matches_lists_a_document_once() {
    let (_temp, root, _) = repo();
    index(&root);
    let found = run(&root, &["-l", "--", "needle"]);
    let expected: BTreeSet<String> = [
        "docs/report.pdf",
        "docs/bundle.zip",
        "docs/logs.tar.gz",
        "docs/app.log.gz",
        "docs/plain.tar",
        "src/main.rs",
        "notes.txt",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(found.lines(), expected);
    let without = run(&root, &["--files-without-match", "--", "Quarterly"]);
    assert!(!without.lines().contains("docs/report.pdf"));
    assert!(without.lines().contains("docs/bundle.zip"));
    let counts = run(&root, &["-c", "--", "needle"]);
    assert!(
        counts.lines().contains("docs/report.pdf#page=2:1"),
        "{}",
        counts.stdout
    );
}

#[test]
fn json_carries_the_location() {
    let (_temp, root, _) = repo();
    index(&root);
    let found = run(&root, &["--json", "--", "deep needle|second page"]);
    let matches: Vec<serde_json::Value> = found
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["type"] == "match")
        .collect();
    let mut seen: Vec<(String, serde_json::Value, u64)> = matches
        .iter()
        .map(|event| {
            let data = &event["data"];
            (
                data["path"]["text"].as_str().unwrap().to_string(),
                data["location"].clone(),
                data["line_number"].as_u64().unwrap(),
            )
        })
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        seen,
        vec![
            (
                "docs/logs.tar.gz!deep.zip!deep.txt".to_string(),
                serde_json::json!({"path": "docs/logs.tar.gz", "members": ["deep.zip", "deep.txt"]}),
                2
            ),
            (
                "docs/report.pdf#page=2".to_string(),
                serde_json::json!({"path": "docs/report.pdf", "page": 2}),
                1
            ),
        ]
    );
}

#[test]
fn a_named_document_is_searched_directly() {
    let (_temp, root, _) = repo();
    let found = run(&root, &["-n", "--", "needle", "docs/report.pdf"]);
    assert_eq!(
        found.lines(),
        [
            "docs/report.pdf#page=1:2:revenue grew needle one",
            "docs/report.pdf#page=2:1:second page needle two"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
    // `-a` searches the bytes on disk, as ripgrep would.
    let raw = run(&root, &["-a", "-n", "--", "needle", "docs/report.pdf"]);
    assert_eq!(raw.code, 0, "{}", raw.stderr);
    assert!(
        raw.stdout.contains("(revenue grew needle one) Tj"),
        "{}",
        raw.stdout
    );
    assert!(!raw.stdout.contains("#page="), "{}", raw.stdout);
}

#[test]
fn no_index_sees_a_document_written_after_the_index() {
    let (_temp, root, _) = repo();
    index(&root);
    fs::write(
        root.join("docs/late.zip"),
        fixture::zip(&[("late.txt", b"written after indexing\n")]),
    )
    .unwrap();
    assert_eq!(run(&root, &["--", "written after"]).code, 1);
    let found = run(&root, &["-n", "--no-index", "--", "written after"]);
    assert_eq!(
        found.stdout,
        "docs/late.zip!late.txt:1:written after indexing\n"
    );
}

#[test]
fn a_second_index_reads_the_cache() {
    let (_temp, root, _) = repo();
    let first = index(&root);
    assert!(
        first.stderr.contains("Documents: 5 extracted, 0 cached"),
        "{}",
        first.stderr
    );
    let second = index(&root);
    assert!(
        second.stderr.contains("Documents: 0 extracted, 5 cached"),
        "{}",
        second.stderr
    );
    fs::write(
        root.join("docs/app.log.gz"),
        fixture::gzip(b"changed needle\n"),
    )
    .unwrap();
    let third = index(&root);
    assert!(
        third.stderr.contains("Documents: 1 extracted, 4 cached"),
        "{}",
        third.stderr
    );
    assert!(third.stderr.contains("(1 removed)"), "{}", third.stderr);
    assert_eq!(
        run(&root, &["-n", "--", "changed needle"]).stdout,
        "docs/app.log.gz:1:changed needle\n"
    );
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Serve `root` and wait until it answers indexed queries.
fn serve(root: &Path, args: &[&str]) -> Server {
    let child = Command::new(tgrep_bin())
        .arg("serve")
        .args(args)
        .arg(".")
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server(child);
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "server did not start"
        );
        let status = run(root, &["status", "."]);
        if let Ok(data) = fs::read_to_string(root.join(".tgrep/serve.json"))
            && let Ok(info) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(port) = info["port"].as_u64()
            && TcpStream::connect(("127.0.0.1", port as u16)).is_ok()
            && status.stdout.contains("Indexing:   complete")
            && status.stdout.contains("Hidden coverage: complete")
        {
            return server;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_server_answers_documents_like_the_index() {
    let (_temp, root, files) = repo();
    let _server = serve(&root, &["--no-watch"]);
    for pattern in ["needle", "(?i)PAGE", "^stopped$"] {
        let found = run(&root, &["-n", "--stats", "--", pattern]);
        assert!(found.stderr.contains("(via "), "{}", found.stderr);
        assert_eq!(found.lines(), scan(&root, &files, pattern), "{pattern}");
    }
    let listed = run(&root, &["-l", "--", "needle"]);
    assert_eq!(listed.lines().len(), 7, "{}", listed.stdout);
}

/// Office documents and an exported site, beside one HTML file on disk.
fn office(root: &Path) -> Vec<&'static str> {
    let files: Vec<(&'static str, Vec<u8>)> = vec![
        (
            "office/memo.docx",
            fixture::docx(&[
                "Memo title",
                "",
                "first needle paragraph",
                "tab\there needle\nsecond line",
            ]),
        ),
        (
            "office/deck.pptx",
            fixture::pptx(&[&["Agenda", "needle on slide one"], &["Closing needle"]]),
        ),
        (
            "office/book.xlsx",
            fixture::xlsx(&[
                (
                    "Q1",
                    &[("A1", "Revenue"), ("B7", "needle total"), ("C2", "42")],
                ),
                ("Q2!x", &[("A1", "needle odd sheet")]),
            ]),
        ),
        (
            "office/notes.odt",
            fixture::odt(&["Heading needle", "body text", "needle body"]),
        ),
        (
            "office/sheet.ods",
            fixture::ods(&[("Data", &[&["a", "", "needle cell"], &["", "needle b2"]])]),
        ),
        (
            "office/talk.odp",
            fixture::odp(&[&["intro"], &["needle slide two", "more"]]),
        ),
        (
            "office/site.zip",
            fixture::zip(&[(
                "index.html",
                b"<html><body>\n<p>Hello <b>nee</b>dle &amp; co</p>\n</body></html>\n",
            )]),
        ),
        (
            "office/page.html",
            b"<p>needle <b>raw</b> html</p>\n".to_vec(),
        ),
    ];
    for (path, data) in &files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data).unwrap();
    }
    files.into_iter().map(|(path, _)| path).collect()
}

#[test]
fn office_hits_name_the_paragraph_slide_and_cell() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join(".git")).unwrap();
    let files = office(&root);
    index(&root);
    let found = run(&root, &["-n", "--", "needle"]);
    let expected: BTreeSet<String> = [
        "office/memo.docx#para=3:1:first needle paragraph",
        "office/memo.docx#para=4:1:tab\there needle",
        "office/deck.pptx#slide=1:2:needle on slide one",
        "office/deck.pptx#slide=2:1:Closing needle",
        "office/book.xlsx#sheet=Q1!B7:1:needle total",
        "office/book.xlsx#sheet=Q2%21x!A1:1:needle odd sheet",
        "office/notes.odt#para=1:1:Heading needle",
        "office/notes.odt#para=3:1:needle body",
        "office/sheet.ods#sheet=Data!C1:1:needle cell",
        "office/sheet.ods#sheet=Data!B2:1:needle b2",
        "office/talk.odp#slide=2:1:needle slide two",
        "office/site.zip!index.html:2: Hello needle & co",
        "office/page.html:1:<p>needle <b>raw</b> html</p>",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(found.lines(), expected, "{}", found.stderr);
    for pattern in [
        "needle",
        "(?i)^(agenda|revenue|intro)$",
        "42",
        "second line",
    ] {
        let expected = scan(&root, &files, pattern);
        assert!(!expected.is_empty(), "{pattern}");
        for mode in [&[][..], &["--no-index"][..]] {
            let mut args = vec!["-n"];
            args.extend_from_slice(mode);
            args.extend_from_slice(&["--", pattern]);
            assert_eq!(run(&root, &args).lines(), expected, "{pattern} {mode:?}");
        }
    }
    let json = run(&root, &["--json", "--", "odd sheet"]);
    let event: serde_json::Value = json
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["type"] == "match")
        .unwrap();
    assert_eq!(
        event["data"]["location"],
        serde_json::json!({"path": "office/book.xlsx", "sheet": "Q2!x", "cell": "A1"})
    );
}

/// Retry `check` until it holds or 30 seconds pass.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let started = Instant::now();
    while !check() {
        assert!(started.elapsed() < Duration::from_secs(30), "{what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_server_reextracts_changed_documents_and_drops_removed_ones() {
    let (temp, root, _) = repo();
    office(&root);
    let _server = serve(&root, &[]);
    let hit = |pattern: &str| run(&root, &["-n", "--", pattern]);

    fs::write(
        root.join("docs/app.log.gz"),
        fixture::gzip(b"rotated needle\n"),
    )
    .unwrap();
    eventually("a changed gzip is re-read", || {
        hit("rotated needle").stdout == "docs/app.log.gz:1:rotated needle\n"
    });
    assert_eq!(hit("needle in gz").code, 1);

    fs::write(
        root.join("office/memo.docx"),
        fixture::docx(&["Memo title", "revised memo text"]),
    )
    .unwrap();
    eventually("a changed docx is re-extracted", || {
        hit("revised memo").stdout == "office/memo.docx#para=2:1:revised memo text\n"
    });
    assert_eq!(hit("first needle paragraph").code, 1);

    fs::remove_file(root.join("docs/report.pdf")).unwrap();
    eventually("a deleted pdf leaves no hit", || {
        hit("revenue grew").code == 1
    });

    fs::rename(root.join("docs/bundle.zip"), root.join("docs/moved.zip")).unwrap();
    eventually("a renamed zip is found under its new name only", || {
        hit("call the bank").stdout == "docs/moved.zip!notes/todo.txt:1:call the bank\n"
    });

    let status = run(&root, &["status", "."]);
    assert!(
        status.stdout.contains("  Documents:  ") && status.stdout.contains("extracted"),
        "{}",
        status.stdout
    );
    drop(temp);
}

/// Set the uncompressed size a zip's local and central headers declare for
/// its first member.
fn declare_size(zip: &mut [u8], size: u32) {
    zip[22..26].copy_from_slice(&size.to_le_bytes());
    let central = (0..zip.len() - 4)
        .find(|&at| zip[at..at + 4] == 0x0201_4b50u32.to_le_bytes())
        .unwrap();
    zip[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
}

#[test]
fn a_member_declaring_more_than_the_bound_is_skipped_unread() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join(".git")).unwrap();
    let mut bomb = fixture::zip(&[("big.txt", b"needle tiny"), ("ok.txt", b"needle ok\n")]);
    declare_size(&mut bomb, 200 << 20);
    fs::write(root.join("bomb.zip"), bomb).unwrap();
    let built = index(&root);
    assert!(
        built.stderr.contains("members skipped: 1 large"),
        "{}",
        built.stderr
    );
    assert_eq!(
        run(&root, &["-n", "--", "needle"]).stdout,
        "bomb.zip!ok.txt:1:needle ok\n"
    );
}
