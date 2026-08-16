//! Nginx directory-index parser (spec §15): autoindex and fancyindex HTML →
//! `RemoteEntry` list. Parser is decoupled from any sync logic (tsumugu's
//! parser-crate pattern, alignment decision).

use serde::{Deserialize, Serialize};
use time::{Date, Month, PrimitiveDateTime, Time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
}

/// One entry of a directory listing (spec §14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub path: String,
    pub size: Option<u64>,
    pub modified: Option<PrimitiveDateTime>,
    pub kind: EntryKind,
}

pub struct NginxParser;

impl NginxParser {
    /// Parse nginx autoindex / fancyindex HTML. Malformed HTML degrades
    /// gracefully: every link that can be read is returned; nothing panics.
    pub fn parse(html: &[u8]) -> Vec<RemoteEntry> {
        let text = String::from_utf8_lossy(html);
        let mut entries = Vec::new();
        let mut rest = text.as_ref();
        while let Some(pos) = rest.find("<a ") {
            let after_a = &rest[pos + 3..];
            let Some(tag_end) = after_a.find('>') else { break };
            let tag = &after_a[..tag_end];
            let Some(href) = extract_href(tag) else {
                rest = &after_a[tag_end + 1..];
                continue;
            };
            let after_tag = &after_a[tag_end + 1..];
            let Some(link_end) = after_tag.find("</a>") else { break };
            let label = strip_html(&after_tag[..link_end]);
            let name = if label.is_empty() { href.clone() } else { label };
            let trailing = &after_tag[link_end + 4..];
            rest = trailing;

            if !is_listable(&href) {
                continue;
            }
            let (size, modified) = scan_trailing(trailing);
            entries.push(RemoteEntry {
                path: name.trim().to_string(),
                size,
                modified,
                kind: if href.ends_with('/') {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
            });
        }
        entries
    }
}

fn is_listable(href: &str) -> bool {
    if href == "../"
        || href == "/"
        || href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("mailto:")
        || href.contains("?")
    {
        return false;
    }
    let name = href.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    !name.is_empty() && name != "."
}

/// href attribute value from the anchor tag, without quotes and HTML entities.
fn extract_href(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pos = lower.find("href")?;
    let rest = &tag[pos + 4..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let (value, _) = match rest.chars().next()? {
        '"' => rest[1..].split_once('"')?,
        '\'' => rest[1..].split_once('\'')?,
        _ => rest.split_once([' ', '>'])?,
    };
    Some(decode_entities(value))
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                // Tag boundary acts as a separator so adjacent table cells
                // don't fuse into one token ("10:00" + "456" → "10:00456").
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(out.trim())
}

/// Look at the text following an entry (up to ~400 chars, tags stripped) for
/// an autoindex-style date (`16-Aug-2026 10:00`) and size (`123`, `12K`,
/// `1.5M`, `-`). Fancyindex puts both in nearby table cells, so the same
/// scan works for either format.
fn scan_trailing(trailing: &str) -> (Option<u64>, Option<PrimitiveDateTime>) {
    let window: String = trailing.chars().take(400).collect();
    let text = strip_html(&window);
    let text = text.replace('\n', " ");
    let modified = find_date(&text);
    let size = find_size(&text);
    (size, modified)
}

/// dd-Mon-yyyy hh:mm, e.g. "16-Aug-2026 10:00". Byte-level scan: all fields
/// are ASCII, so this is multibyte-safe.
fn find_date(text: &str) -> Option<PrimitiveDateTime> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let b = text.as_bytes();
    // "dd-Mon-yyyy hh:mm" is 17 bytes.
    for i in 0..b.len().saturating_sub(17) {
        let w = &b[i..i + 17];
        if w[2] != b'-' || w[6] != b'-' || w[11] != b' ' {
            continue;
        }
        let day = two_digits(w[0], w[1])?;
        let mon = &text[i + 3..i + 6].to_ascii_lowercase();
        let m = MONTHS.iter().position(|m| *m == mon)?;
        let year = two_digits(w[7], w[8])? as i32 * 100
            + two_digits(w[9], w[10])? as i32;
        let hour = two_digits(w[12], w[13])?;
        let minute = two_digits(w[15], w[16])?;
        let date = Date::from_calendar_date(year, Month::try_from(m as u8 + 1).ok()?, day).ok()?;
        let time = Time::from_hms(hour, minute, 0).ok()?;
        return Some(PrimitiveDateTime::new(date, time));
    }
    None
}

fn two_digits(a: u8, b: u8) -> Option<u8> {
    if !a.is_ascii_digit() || !b.is_ascii_digit() {
        return None;
    }
    Some((a - b'0') * 10 + (b - b'0'))
}

fn find_size(text: &str) -> Option<u64> {
    for tok in text.split_whitespace() {
        if tok == "-" || tok == "—" {
            return None;
        }
        if let Some(size) = parse_size_token(tok) {
            return Some(size);
        }
    }
    None
}

fn parse_size_token(tok: &str) -> Option<u64> {
    let (num, mult) = match tok.chars().last()? {
        'K' => (&tok[..tok.len() - 1], 1024f64),
        'M' => (&tok[..tok.len() - 1], 1024f64 * 1024.0),
        'G' => (&tok[..tok.len() - 1], 1024f64 * 1024.0 * 1024.0),
        'T' => (&tok[..tok.len() - 1], 1024f64 * 1024.0 * 1024.0 * 1024.0),
        _ => (tok, 1.0),
    };
    let v: f64 = num.parse().ok()?;
    Some((v * mult).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTOINDEX: &str = r#"<html>
<head><title>Index of /ubuntu/</title></head>
<body>
<h1>Index of /ubuntu/</h1><hr><pre><a href="../">../</a>
<a href="dists/">dists/</a>                                            16-Aug-2026 10:00                   -
<a href="pool/">pool/</a>                                            16-Aug-2026 10:00                   -
<a href="ls-lR.gz">ls-lR.gz</a>                                       16-Aug-2026 10:00                12K
<a href="ubuntu-24.04.iso">ubuntu-24.04.iso</a>                               16-Aug-2026 10:00               4.2G
<a href="README.html">README.html</a>                                    16-Aug-2026 10:00                3204
</pre><hr></body>
</html>"#;

    #[test]
    fn autoindex_basic() {
        let entries = NginxParser::parse(AUTOINDEX.as_bytes());
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].path, "dists/");
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].size, None);
        let gz = &entries[2];
        assert_eq!(gz.path, "ls-lR.gz");
        assert_eq!(gz.kind, EntryKind::File);
        assert_eq!(gz.size, Some(12 * 1024));
        let iso = &entries[3];
        assert_eq!(iso.size, Some((4.2f64 * 1024.0 * 1024.0 * 1024.0).round() as u64));
        assert_eq!(entries[4].size, Some(3204));
        let date = entries[0].modified.unwrap();
        assert_eq!(date.year(), 2026);
        assert_eq!(date.month(), Month::August);
        assert_eq!(date.day(), 16);
        assert_eq!((date.hour(), date.minute()), (10, 0));
    }

    #[test]
    fn fancyindex_rows() {
        let html = r#"<html><head><title>Index of /repo/</title></head><body>
<table class="fancy">
<tr><td class="n"><a href="../">Parent Directory</a>/</td><td></td></tr>
<tr><td class="n"><a href="packages/">packages/</a>/</td><td class="m">2026-08-16 10:00</td><td class="s">-</td></tr>
<tr><td class="n"><a href="repomd.xml">repomd.xml</a></td><td class="m">2026-08-16 10:00</td><td class="s">456</td></tr>
</table></body></html>"#;
        let entries = NginxParser::parse(html.as_bytes());
        // "Parent Directory" maps to href "../" which is skipped.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "packages/");
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[1].path, "repomd.xml");
        assert_eq!(entries[1].size, Some(456));
    }

    #[test]
    fn empty_listing() {
        let html = r#"<html><head><title>Index of /x/</title></head><body>
<h1>Index of /x/</h1><hr><pre><a href="../">../</a>
</pre><hr></body></html>"#;
        assert!(NginxParser::parse(html.as_bytes()).is_empty());
    }

    #[test]
    fn unicode_names_preserved() {
        let html = r#"<pre><a href="../">../</a>
<a href="测试目录/">测试目录/</a>    16-Aug-2026 10:00    -
<a href="résumé.pdf">résumé.pdf</a>    16-Aug-2026 10:00    100
</pre>"#;
        let entries = NginxParser::parse(html.as_bytes());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "测试目录/");
        assert_eq!(entries[1].path, "résumé.pdf");
    }

    #[test]
    fn malformed_html_degrades_gracefully() {
        let html = b"<html><body><a href=\"ok.txt\">ok.txt</a> broken <a href unclosed ";
        let entries = NginxParser::parse(html);
        assert!(!entries.is_empty(), "still parses the readable link");
        assert_eq!(entries[0].path, "ok.txt");
        // No panic even on fully broken input.
        let _ = NginxParser::parse(b"\xff\xfe <a <a <a");
    }

    #[test]
    fn absolute_and_query_links_skipped() {
        let html = r#"<pre>
<a href="https://example.org/x.iso">x.iso</a>    16-Aug-2026 10:00    100
<a href="download?id=1">d</a>    16-Aug-2026 10:00    100
<a href="ok.txt">ok.txt</a>    16-Aug-2026 10:00    100
</pre>"#;
        let entries = NginxParser::parse(html.as_bytes());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "ok.txt");
    }

    #[test]
    fn large_size_token() {
        assert_eq!(parse_size_token("10T").unwrap(), 10 * 1024u64.pow(4));
        assert_eq!(parse_size_token("1.5M").unwrap(), (1.5 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size_token("42").unwrap(), 42);
        assert!(parse_size_token("abc").is_none());
    }
}

