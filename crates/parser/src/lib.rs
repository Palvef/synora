//! Directory-listing parsers (spec §14/§15/§60): turn an upstream index
//! page — nginx/apache autoindex HTML, Caddy browse JSON, S3 ListObjectsV2
//! XML — into a flat [`Entry`] list. Parsers are decoupled from any sync
//! logic (the nginx crate's parser-crate pattern) and are *total*: they
//! never fail, malformed input just yields whatever could be read.

use time::PrimitiveDateTime;

/// One entry of a directory listing (spec §14).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: String,
    pub size: Option<u64>,
    pub modified: Option<PrimitiveDateTime>,
    pub kind: EntryKind,
    /// Link target when the listing format carries one; `None` = unknown
    /// (tsumugu semantics: mirror the link under its own name).
    pub symlink_target: Option<String>,
}

/// Whether the entry is a file, a subdirectory (dirs recurse), or a
/// symlink (mirrored as a local symlink, never downloaded/recurse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// A directory-listing parser (spec §14/§15).
pub trait IndexParser: Send + Sync {
    fn name(&self) -> &'static str;
    fn parse(&self, body: &[u8]) -> Vec<Entry>;
}

/// nginx autoindex / fancyindex HTML — the nginx crate's parser.
pub struct NginxParser;

impl IndexParser for NginxParser {
    fn name(&self) -> &'static str {
        "nginx"
    }
    fn parse(&self, body: &[u8]) -> Vec<Entry> {
        nginx::NginxParser::parse(body)
            .into_iter()
            .map(Entry::from)
            .collect()
    }
}

impl From<nginx::RemoteEntry> for Entry {
    fn from(e: nginx::RemoteEntry) -> Self {
        Entry {
            path: e.path,
            size: e.size,
            modified: e.modified,
            symlink_target: e.symlink_target,
            kind: match e.kind {
                nginx::EntryKind::File => EntryKind::File,
                nginx::EntryKind::Dir => EntryKind::Dir,
                nginx::EntryKind::Symlink => EntryKind::Symlink,
            },
        }
    }
}

/// Apache mod_autoindex produces the same HTML shape as nginx autoindex —
/// same parser, different name.
pub struct ApacheParser;

impl IndexParser for ApacheParser {
    fn name(&self) -> &'static str {
        "apache"
    }
    fn parse(&self, body: &[u8]) -> Vec<Entry> {
        NginxParser.parse(body)
    }
}

/// Caddy v2 `file_server browse` JSON: an array of `{name, size, mod_time,
/// is_dir, url}` objects. Parsed with serde_json; `mod_time` is RFC3339-ish.
pub struct CaddyParser;

#[derive(serde::Deserialize)]
struct CaddyItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mod_time: Option<String>,
    #[serde(default)]
    is_dir: bool,
}

impl IndexParser for CaddyParser {
    fn name(&self) -> &'static str {
        "caddy"
    }
    fn parse(&self, body: &[u8]) -> Vec<Entry> {
        let Ok(items) = serde_json::from_slice::<Vec<CaddyItem>>(body) else {
            return Vec::new();
        };
        items
            .into_iter()
            // The parent `..` entry is a browse artifact, not a mirror entry.
            .filter(|i| !i.name.is_empty() && i.name != "." && i.name != "..")
            .map(|i| Entry {
                path: i.name,
                size: i.size,
                modified: i.mod_time.as_deref().and_then(parse_mod_time),
                symlink_target: None,
                kind: if i.is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
            })
            .collect()
    }
}

/// AWS S3 ListObjectsV2 XML: `<Contents>` blocks (Key/Size/LastModified) for
/// objects plus `<CommonPrefixes>`/`<Prefix>` for delimiter-truncated
/// directories. Parsed with a small byte-scan over the tags — no xml dep.
pub struct S3Parser;

impl IndexParser for S3Parser {
    fn name(&self) -> &'static str {
        "s3"
    }
    fn parse(&self, body: &[u8]) -> Vec<Entry> {
        let text = String::from_utf8_lossy(body);
        let mut entries = Vec::new();

        let mut rest: &str = text.as_ref();
        while let Some(contents) = take_block(&mut rest, "Contents") {
            let Some(key) = inner_tag(contents, "Key") else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            entries.push(Entry {
                size: inner_tag(contents, "Size").and_then(|s| s.parse::<u64>().ok()),
                modified: inner_tag(contents, "LastModified").and_then(|s| parse_mod_time(&s)),
                kind: if key.ends_with('/') {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                path: key,
                symlink_target: None,
            });
        }

        let mut rest: &str = text.as_ref();
        while let Some(prefixes) = take_block(&mut rest, "CommonPrefixes") {
            let Some(prefix) = inner_tag(prefixes, "Prefix") else {
                continue;
            };
            if !prefix.is_empty() {
                entries.push(Entry {
                    path: prefix,
                    size: None,
                    modified: None,
                    kind: EntryKind::Dir,
                    symlink_target: None,
                });
            }
        }
        entries
    }
}

/// Generic HTML fallback: same parser as nginx autoindex — it degrades
/// gracefully on malformed markup, which is all a fallback can promise.
pub struct DirectoryListingParser;

impl IndexParser for DirectoryListingParser {
    fn name(&self) -> &'static str {
        "directory-listing"
    }
    fn parse(&self, body: &[u8]) -> Vec<Entry> {
        NginxParser.parse(body)
    }
}

/// Total no-op: never yields entries, never fails. A sync planned with this
/// parser downloads nothing (and, with delete, removes local files).
pub struct FallbackParser;

impl IndexParser for FallbackParser {
    fn name(&self) -> &'static str {
        "fallback"
    }
    fn parse(&self, _body: &[u8]) -> Vec<Entry> {
        Vec::new()
    }
}

/// Look up a parser by config name: `nginx`, `apache`, `caddy`, `s3`,
/// `directory-listing`, `fallback`.
pub fn parser_for(name: &str) -> Option<Box<dyn IndexParser>> {
    Some(match name {
        "nginx" => Box::new(NginxParser),
        "apache" => Box::new(ApacheParser),
        "caddy" => Box::new(CaddyParser),
        "s3" => Box::new(S3Parser),
        "directory-listing" => Box::new(DirectoryListingParser),
        "fallback" => Box::new(FallbackParser),
        _ => return None,
    })
}

/// Lenient RFC3339-ish timestamp: "2026-08-16T10:00:00Z", with fractional
/// seconds, with an offset, or space-separated ("2026-08-16 10:00:00") as
/// Caddy browse can emit. Timestamps are treated as naive listing-local
/// time — that is how the listing reported them.
fn parse_mod_time(s: &str) -> Option<PrimitiveDateTime> {
    if let Ok(odt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
    {
        // Truncate sub-second precision: listings and local mtime compares
        // are whole-second granularity everywhere else.
        return Some(PrimitiveDateTime::new(
            odt.date(),
            time::Time::from_hms(odt.hour(), odt.minute(), odt.second()).ok()?,
        ));
    }
    let t = s.trim().replace(' ', "T");
    let t = t.split(['Z', '+', '.']).next().unwrap_or_default();
    let t = &t[..t.len().min(19)];
    PrimitiveDateTime::parse(
        t,
        &time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]"),
    )
    .ok()
}

/// Pop the first `open..close` tag block, advancing `rest` past it.
fn take_block<'a>(rest: &mut &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = rest.find(&open)? + open.len();
    let end = rest[start..].find(&close)? + start;
    let block = &rest[start..end];
    *rest = &rest[end + close.len()..];
    Some(block)
}

fn inner_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(decode_entities(&block[start..end]))
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Month};

    const NGINX_SAMPLE: &str = r#"<html>
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

    fn modified(d: u8, h: u8, min: u8) -> PrimitiveDateTime {
        PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::August, d).unwrap(),
            time::Time::from_hms(h, min, 0).unwrap(),
        )
    }

    #[test]
    fn nginx_sample() {
        let entries = NginxParser.parse(NGINX_SAMPLE.as_bytes());
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].path, "dists/");
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].size, None);
        assert_eq!(entries[0].modified, Some(modified(16, 10, 0)));
        let gz = &entries[2];
        assert_eq!(gz.path, "ls-lR.gz");
        assert_eq!(gz.kind, EntryKind::File);
        assert_eq!(gz.size, Some(12 * 1024));
        assert_eq!(
            entries[3].size,
            Some((4.2f64 * 1024.0 * 1024.0 * 1024.0).round() as u64)
        );
        assert_eq!(entries[4].size, Some(3204));
    }

    #[test]
    fn fancyindex_symlink_entries() {
        // fancyindex marks symlinks with a trailing `@` in the displayed
        // name (`@/` = link to a directory); the target is not in the page.
        let html = r#"<html><body><table class="fancy">
<tr><td class="n"><a href="../">Parent Directory</a>/</td><td></td></tr>
<tr><td class="n"><a href="latest">latest@/</a></td><td class="m">2026-08-13 12:53</td><td class="s">-</td></tr>
<tr><td class="n"><a href="current.tar.gz">current.tar.gz@</a></td><td class="m">2026-08-13 12:53</td><td class="s">123</td></tr>
</table></body></html>"#;
        let entries = NginxParser.parse(html.as_bytes());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "latest");
        assert_eq!(entries[0].kind, EntryKind::Symlink);
        assert_eq!(entries[0].symlink_target, None);
        assert_eq!(entries[1].path, "current.tar.gz");
        assert_eq!(entries[1].kind, EntryKind::Symlink);
    }

    #[test]
    fn apache_same_shape_as_nginx() {
        // mod_autoindex output has the same HTML shape; parse must agree.
        assert_eq!(
            ApacheParser.parse(NGINX_SAMPLE.as_bytes()),
            NginxParser.parse(NGINX_SAMPLE.as_bytes())
        );
    }

    #[test]
    fn caddy_json() {
        let body = br#"[
          {"name":"..","size":0,"mod_time":"2026-08-16T10:00:00Z","is_dir":true,"url":"/repo/"},
          {"name":"docs","size":0,"mod_time":"2026-08-16T10:00:00.123Z","is_dir":true,"url":"/repo/docs/"},
          {"name":"synora.tar.gz","size":12345678,"mod_time":"2026-08-16T09:30:00+08:00","is_dir":false,"url":"/repo/synora.tar.gz"},
          {"name":"readme.md","size":42,"mod_time":"2026-08-16 10:00:00","is_dir":false,"url":"/repo/readme.md"}
        ]"#;
        let entries = CaddyParser.parse(body);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "docs");
        assert_eq!(entries[0].kind, EntryKind::Dir);
        // Caddy reports a zero size for dirs.
        assert_eq!(entries[0].size, Some(0));
        assert_eq!(entries[0].modified, Some(modified(16, 10, 0)));
        assert_eq!(entries[1].path, "synora.tar.gz");
        assert_eq!(entries[1].kind, EntryKind::File);
        assert_eq!(entries[1].size, Some(12345678));
        assert_eq!(entries[1].modified, Some(modified(16, 9, 30)));
        assert_eq!(entries[2].path, "readme.md");
        assert_eq!(entries[2].size, Some(42));
    }

    #[test]
    fn s3_list_objects_v2() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>mirror</Name>
  <KeyCount>3</KeyCount>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>ubuntu/ls-lR.gz</Key>
    <LastModified>2026-08-16T10:00:00.000Z</LastModified>
    <ETag>"abc123"</ETag>
    <Size>12345</Size>
  </Contents>
  <Contents>
    <Key>ubuntu/pool/</Key>
    <LastModified>2026-08-16T10:00:00.000Z</LastModified>
    <Size>0</Size>
  </Contents>
  <CommonPrefixes>
    <Prefix>ubuntu/dists/</Prefix>
  </CommonPrefixes>
</ListBucketResult>"#;
        let entries = S3Parser.parse(body);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "ubuntu/ls-lR.gz");
        assert_eq!(entries[0].kind, EntryKind::File);
        assert_eq!(entries[0].size, Some(12345));
        assert_eq!(entries[0].modified, Some(modified(16, 10, 0)));
        assert_eq!(entries[1].path, "ubuntu/pool/");
        assert_eq!(entries[1].kind, EntryKind::Dir);
        assert_eq!(entries[2].path, "ubuntu/dists/");
        assert_eq!(entries[2].kind, EntryKind::Dir);
        assert_eq!(entries[2].size, None);
    }

    #[test]
    fn directory_listing_fallback_reuses_nginx() {
        assert_eq!(
            DirectoryListingParser.parse(NGINX_SAMPLE.as_bytes()),
            NginxParser.parse(NGINX_SAMPLE.as_bytes())
        );
        assert!(FallbackParser.parse(NGINX_SAMPLE.as_bytes()).is_empty());
        assert!(FallbackParser.parse(b"").is_empty());
    }

    #[test]
    fn empty_and_malformed_inputs_are_empty_lists() {
        let parsers: Vec<Box<dyn IndexParser>> = vec![
            Box::new(NginxParser),
            Box::new(ApacheParser),
            Box::new(CaddyParser),
            Box::new(S3Parser),
            Box::new(DirectoryListingParser),
            Box::new(FallbackParser),
        ];
        for p in parsers {
            assert!(p.parse(b"").is_empty(), "{} on empty input", p.name());
            assert!(
                p.parse(b"<html><body>this is not a listing at all")
                    .is_empty(),
                "{} on garbage input",
                p.name()
            );
            let _ = p.parse(b"\xff\xfe\x00 broken \x80 bytes");
        }
    }

    #[test]
    fn s3_empty_and_malformed_xml() {
        assert!(S3Parser
            .parse(b"<ListBucketResult></ListBucketResult>")
            .is_empty());
        assert!(S3Parser.parse(b"<Contents>no closing tag").is_empty());
    }

    #[test]
    fn parser_for_names() {
        for name in [
            "nginx",
            "apache",
            "caddy",
            "s3",
            "directory-listing",
            "fallback",
        ] {
            let p = parser_for(name).unwrap_or_else(|| panic!("parser_for({name})"));
            assert_eq!(p.name(), name);
        }
        assert!(parser_for("fancyindex").is_none()); // fancyindex HTML is nginx-parsed
        assert!(parser_for("").is_none());
    }
}
