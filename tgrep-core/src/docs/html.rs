//! HTML archive members: the visible text, line for line with the source.
//!
//! Inline tags (`b`, `span`, `a`, ...) vanish and every other tag becomes a
//! space, comments and the contents of `script`, `style` and `template` are
//! dropped, character references are decoded, and every line
//! break of the source is kept, so a hit's line number is the source line.
//! An HTML file on disk is source and is indexed as written; only a member of
//! an archive (a saved site, a wiki export) is read this way.

/// Elements whose content is not text.
const HIDDEN: [&str; 3] = ["script", "style", "template"];

/// Elements inside a run of text: their tags separate nothing.
const INLINE: [&str; 26] = [
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "dfn", "em", "font", "i", "kbd",
    "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "time", "u", "var", "wbr",
];

pub(super) fn text(source: &str) -> String {
    let mut out = String::with_capacity(source.len() / 2);
    let mut rest = source;
    while let Some(lt) = rest.find('<') {
        decode(&rest[..lt], &mut out);
        rest = &rest[lt..];
        if let Some(body) = rest.strip_prefix("<!--") {
            let end = body.find("-->").map_or(body.len(), |i| i + 3);
            newlines(&body[..end], &mut out);
            rest = &body[end..];
            continue;
        }
        let end = tag_end(rest);
        let tag = &rest[..end];
        newlines(tag, &mut out);
        rest = &rest[end..];
        let name: String = tag[1..]
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if !INLINE.contains(&name.as_str()) {
            out.push(' ');
        }
        if HIDDEN.contains(&name.as_str()) && !tag.starts_with("</") && !tag.ends_with("/>") {
            let close = find_close(rest, &name);
            newlines(&rest[..close], &mut out);
            rest = &rest[close..];
        }
    }
    decode(rest, &mut out);
    out
}

/// Length of the tag at the start of `s`, `>` included; quoted attribute
/// values may hold `>`.
fn tag_end(s: &str) -> usize {
    let mut quote = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return i + 1,
            _ => {}
        }
    }
    s.len()
}

/// Offset of `</name` in `s`, case-insensitively, or its end.
fn find_close(s: &str, name: &str) -> usize {
    let lower = s.to_ascii_lowercase();
    lower.find(&format!("</{name}")).unwrap_or(s.len())
}

fn newlines(s: &str, out: &mut String) {
    for _ in s.matches('\n') {
        out.push('\n');
    }
}

fn decode(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map_or(rest.len(), |i| i + 1);
        let entity = &rest[1..end];
        let terminated = rest[end..].starts_with(';');
        match named(entity).or_else(|| numeric(entity)) {
            Some(c) => {
                out.push(c);
                rest = &rest[end + usize::from(terminated)..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

fn named(entity: &str) -> Option<char> {
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "deg" => '°',
        "times" => '×',
        "divide" => '÷',
        _ => return None,
    })
}

fn numeric(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    char::from_u32(code).filter(|&c| c != '\0')
}

#[cfg(test)]
mod tests {
    #[test]
    fn visible_text_keeps_source_lines() {
        let source = "<html><head><title>T &amp; C</title>\n<style>p {\n color: red }</style></head>\n\
                      <body><p>Hello <b>wo</b>rld&nbsp;&#x41;&#66;</p><!-- hidden\n note -->\n\
                      <script>var x = \"<p>\";\n</script><a href=\"x>y\">link</a></body></html>";
        let text = super::text(source);
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        assert_eq!(lines.len(), source.lines().count());
        assert_eq!(lines, ["T & C", "", "", "Hello world AB", "", "", "link"]);
        assert!(!text.contains("color") && !text.contains("var x") && !text.contains("hidden"));
        assert_eq!(super::text("a &bogus; b & c"), "a &bogus; b & c");
    }
}
