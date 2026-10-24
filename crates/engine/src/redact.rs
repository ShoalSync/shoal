//! Redaction for diagnostics bundles (ADR 0030): names that could say what is in a folder
//! become keyed hashes, IDs are shortened, and addresses are reduced to their kind.

use std::net::IpAddr;
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, MatchKind};
use regex::Regex;
use serde_json::Value;

/// Hex digits kept of a path component's hash: enough that names in a large folder do not
/// collide.
const HASH_DIGITS: usize = 12;

/// JSON fields that hold a path, hashed whole.
pub(crate) const PATH_FIELDS: &[&str] = &[
    "path", "dir", "file", "root", "old", "new", "from", "to", "mount", "data_dir",
];

/// Characters that may sit next to a known name for it to count as that name, not as part of
/// a word: `state` in "engine state" is not a file named `state`.
fn boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace() || "\"'`/:=(),;[]<>".contains(c))
}

/// True if `name` is already a hash this redactor makes, so hashing is idempotent.
fn is_hash(name: &str) -> bool {
    let (hash, ext) = name.split_at(name.len().min(HASH_DIGITS));
    hash.len() == HASH_DIGITS
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && (ext.is_empty()
            || (ext.starts_with('.')
                && (2..=9).contains(&ext.len())
                && ext[1..].chars().all(|c| c.is_ascii_alphanumeric())))
}

fn patterns() -> &'static [Regex; 5] {
    static PATTERNS: OnceLock<[Regex; 5]> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // URLs: kept to their scheme and host.
            Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s"'<>]+"#).unwrap(),
            // Full-length IDs: devices, users, folders.
            Regex::new(r"\b[0-9a-fA-F]{64}\b").unwrap(),
            // Addresses, IPv6 in brackets or bare, with an optional port.
            Regex::new(
                r"\[[0-9a-fA-F:.]+\](?::\d+)?|\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b|\b[0-9a-fA-F]{0,4}(?::[0-9a-fA-F]{0,4}){2,7}\b",
            )
            .unwrap(),
            // Anything path-like left: a run without spaces or quotes that holds a `/`.
            Regex::new(r#"[^\s"'`<>()\[\]{},;=]*/[^\s"'`<>()\[\]{},;=]*"#).unwrap(),
            // A quoted path, spaces and all.
            Regex::new(r#""([^"\n]*/[^"\n]*)"|'([^'\n]*/[^'\n]*)'"#).unwrap(),
        ]
    })
}

/// Turns names, IDs and addresses into what a bundle may carry. One per bundle: its key makes
/// the same name hash the same everywhere in it, and differently in any other bundle.
pub(crate) struct Redactor {
    key: [u8; 32],
    include_paths: bool,
    /// Paths known to be names, longest first, and their replacements.
    known: Option<(AhoCorasick, Vec<String>)>,
}

impl Redactor {
    /// A redactor with a random key. `known` are paths to replace wherever they appear, such as
    /// folder roots, the data directory and every file path; with `include_paths` names are
    /// left as they are.
    pub(crate) fn new(
        include_paths: bool,
        known: impl IntoIterator<Item = String>,
    ) -> std::io::Result<Self> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("the OS provides randomness");
        let mut redactor = Self {
            key,
            include_paths,
            known: None,
        };
        if !include_paths {
            let mut known: Vec<String> = known.into_iter().filter(|k| k.len() > 1).collect();
            known.sort_by_key(|k| std::cmp::Reverse(k.len()));
            known.dedup();
            let replacements = known.iter().map(|k| redactor.path(k)).collect();
            // Without it names would go out as they are: no bundle is better.
            let matcher = AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(&known)
                .map_err(std::io::Error::other)?;
            redactor.known = Some((matcher, replacements));
        }
        Ok(redactor)
    }

    /// Known names replaced where they stand as names. A known name followed by `/` starts a
    /// path, which runs to `": "`, a quote, a line's end or the text's end, spaces and all, and
    /// is hashed whole: `/srv/root/Taxes 2025/a b.pdf: denied`.
    fn known_names(&self, text: &str) -> String {
        let Some((matcher, replacements)) = &self.known else {
            return text.to_owned();
        };
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for m in matcher.find_iter(text) {
            let (start, end) = (m.start(), m.end());
            let after = text[end..].chars().next();
            if start < last || !boundary(text[..start].chars().next_back()) || !boundary(after) {
                continue;
            }
            out.push_str(&text[last..start]);
            if after == Some('/') {
                let rest = &text[end..];
                let stop = [": ", "\"", "'", "`", "\n"]
                    .iter()
                    .filter_map(|p| rest.find(p))
                    .min()
                    .unwrap_or(rest.len());
                out.push_str(&self.path(&text[start..end + stop]));
                last = end + stop;
            } else {
                out.push_str(&replacements[m.pattern().as_usize()]);
                last = end;
            }
        }
        out.push_str(&text[last..]);
        out
    }

    /// A path, absolute or relative, with each component hashed: `a/b.jpg` becomes
    /// `3f2a…/9c1e….jpg`.
    pub(crate) fn path(&self, path: &str) -> String {
        if self.include_paths {
            return path.to_owned();
        }
        path.split('/')
            .map(|c| {
                if c.is_empty() {
                    String::new()
                } else {
                    self.component(c)
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// One name: its hash, and its extension if it looks like one (1 to 8 letters or digits
    /// after the last dot, which is not the first character).
    fn component(&self, name: &str) -> String {
        if name == "." || name == ".." || name == "~" || is_hash(name) {
            return name.to_owned();
        }
        let hash = blake3::keyed_hash(&self.key, name.as_bytes()).to_hex();
        let hash = &hash[..HASH_DIGITS];
        match name.rfind('.') {
            Some(dot)
                if dot > 0
                    && (1..=8).contains(&(name.len() - dot - 1))
                    && name[dot + 1..].chars().all(|c| c.is_ascii_alphanumeric()) =>
            {
                format!("{hash}{}", &name[dot..])
            }
            _ => hash.to_owned(),
        }
    }

    /// Free text, such as a log message or an error.
    pub(crate) fn text(&self, text: &str) -> String {
        let [urls, ids, addresses, paths, quoted] = patterns();
        let mut out = urls
            .replace_all(text, |c: &regex::Captures<'_>| url(&c[0]))
            .into_owned();
        if !self.include_paths {
            out = self.known_names(&out);
            out = quoted
                .replace_all(&out, |c: &regex::Captures<'_>| {
                    let inner = c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str());
                    let quote = &c[0][..1];
                    if inner.contains("://") || is_source(inner) {
                        c[0].to_owned()
                    } else {
                        format!("{quote}{}{quote}", self.path(inner))
                    }
                })
                .into_owned();
        }
        out = ids
            .replace_all(&out, |c: &regex::Captures<'_>| format!("{}…", &c[0][..8]))
            .into_owned();
        out = addresses
            .replace_all(&out, |c: &regex::Captures<'_>| {
                address(&c[0]).unwrap_or_else(|| c[0].to_owned())
            })
            .into_owned();
        if !self.include_paths {
            out = paths
                .replace_all(&out, |c: &regex::Captures<'_>| {
                    // Punctuation after a path, as in `a/b.pdf: denied`, is not part of it.
                    let token = c[0].trim_end_matches([':', '.', '!', '?']);
                    let rest = &c[0][token.len()..];
                    // A URL already cut to its host, a hash already made, and source locations
                    // in backtraces are left alone. Files in folders were hashed above.
                    if token.contains("://") || token.contains('…') || is_source(token) {
                        c[0].to_owned()
                    } else {
                        format!("{}{rest}", self.path(token))
                    }
                })
                .into_owned();
        }
        out
    }

    /// A JSON value: path fields hashed whole, every other string treated as text.
    pub(crate) fn json(&self, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.text(s),
            Value::Array(items) => items.iter_mut().for_each(|v| self.json(v)),
            Value::Object(fields) => {
                for (key, v) in fields.iter_mut() {
                    match v {
                        Value::String(s) if PATH_FIELDS.contains(&key.as_str()) => {
                            *s = self.text(&self.path(s));
                        }
                        Value::Array(items) if PATH_FIELDS.contains(&key.as_str()) => {
                            for item in items.iter_mut() {
                                if let Value::String(s) = item {
                                    *s = self.text(&self.path(s));
                                }
                            }
                        }
                        _ => self.json(v),
                    }
                }
            }
            _ => {}
        }
    }

    /// A log file: JSON lines redacted field by field, other lines as text.
    pub(crate) fn log(&self, log: &str) -> String {
        log.lines()
            .map(|line| match serde_json::from_str::<Value>(line) {
                Ok(mut value) => {
                    self.json(&mut value);
                    value.to_string()
                }
                Err(_) => self.text(line),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A place in Shoal's or a library's source, as backtraces name them, or `i/o`.
fn is_source(token: &str) -> bool {
    let file = token.split(':').next().unwrap_or(token);
    token == "i/o"
        || file.ends_with(".rs")
        || token.contains("/rustc/")
        || token.contains(".cargo/registry")
}

/// A URL cut to its scheme and host: no user, password, path or query.
fn url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = address(host).unwrap_or_else(|| host.to_owned());
    format!("{scheme}://{host}")
}

/// What kind of address `text` is, such as `local-ipv4`, if it is one.
fn address(text: &str) -> Option<String> {
    let bare = text.trim_start_matches('[');
    let bare = match bare.split_once(']') {
        Some((ip, _)) => ip,
        None if bare.matches(':').count() == 1 => bare.split(':').next().unwrap_or(bare),
        // A bare IPv6 address needs some digits: `e::` in `engine::materialize` parses as one.
        None if bare.chars().filter(char::is_ascii_hexdigit).count() < 4 => return None,
        None => bare,
    };
    let ip: IpAddr = bare.parse().ok()?;
    let local = match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    };
    let family = if ip.is_ipv4() { "ipv4" } else { "ipv6" };
    Some(format!(
        "{}-{family}",
        if local { "local" } else { "public" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor(known: &[&str]) -> Redactor {
        Redactor::new(false, known.iter().map(|s| (*s).to_owned())).unwrap()
    }

    #[test]
    fn paths_keep_only_real_extensions() {
        let r = redactor(&[]);
        let hashed = r.path("Photos/Trip to Rome/IMG_0001.JPG");
        let parts: Vec<&str> = hashed.split('/').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[2].ends_with(".JPG") && parts[2].len() == HASH_DIGITS + 4);
        assert!(!hashed.contains("Rome"));
        assert_eq!(
            r.path("a/b.txt"),
            r.path("a/b.txt"),
            "the same within a bundle"
        );
        assert_ne!(
            r.path("a/b.txt"),
            redactor(&[]).path("a/b.txt"),
            "not across bundles"
        );
        assert_eq!(r.component(".bashrc").len(), HASH_DIGITS);
        assert_eq!(r.component("photo.of-our-trip").len(), HASH_DIGITS);
        assert!(r.path("/home/me/x").starts_with('/'));
    }

    #[test]
    fn text_hides_names_ids_and_addresses() {
        let r = redactor(&["/srv/Our Family", "notes (Laptop, 2026-09-27).txt"]);
        let id = "ab".repeat(32);
        let line = format!(
            "2026-09-28T02:23:10.123Z WARN update failed folder={id} error=i/o error on \
             /srv/Our Family/Tax/2025.pdf: permission denied; see notes (Laptop, 2026-09-27).txt \
             from 192.168.1.20:4433 and [2001:db8::1]:7777 via https://user:pw@relay.example.com/path?x=1"
        );
        let out = r.text(&line);
        for leak in [
            "Family", "Tax", "2025", "Laptop", "192.168", "2001:db8", "user:pw", "path?x", &id,
        ] {
            assert!(!out.contains(leak), "{leak} leaked: {out}");
        }
        assert!(out.contains("02:23:10.123"), "timestamps stay: {out}");
        assert!(out.contains("local-ipv4") && out.contains("public-ipv6"));
        assert!(out.contains("https://relay.example.com"));
        assert!(out.contains(&format!("{}…", &id[..8])));
        assert!(out.contains(".pdf"));
    }

    #[test]
    fn names_with_spaces_do_not_leak() {
        let r = redactor(&["/srv/root"]);
        let out = r.text(
            "cannot open /srv/root/Taxes 2025/Divorce Settlement.pdf: permission denied; \
             saving \"/home/me/My Stuff/a b.txt\" failed",
        );
        for leak in ["Taxes", "Divorce", "Settlement", "Stuff", "a b"] {
            assert!(!out.contains(leak), "{leak} leaked: {out}");
        }
        assert!(out.contains(".pdf: permission denied"), "{out}");
    }

    #[test]
    fn a_name_hashes_the_same_everywhere() {
        let r = redactor(&["x/known file.txt"]);
        for path in ["x/known file.txt", "a/b/unknown.txt"] {
            let hashed = r.path(path);
            assert_eq!(r.path(&hashed), hashed, "hashing is idempotent");
            assert!(r.text(&format!("saving {path} failed")).contains(&hashed));
            let mut field = serde_json::json!({ "path": path });
            r.json(&mut field);
            assert_eq!(field["path"], hashed.as_str());
        }
    }

    #[test]
    fn known_names_match_whole_words_only() {
        // Files named `date` and `load` do not turn words that contain them into hashes.
        let r = redactor(&["date", "load"]);
        let out = r.text("updated states; loaded 1 folder");
        assert_eq!(out, "updated states; loaded 1 folder");
        assert!(
            !r.text("wrote date").contains(" date"),
            "the name itself is hashed"
        );
    }

    #[test]
    fn source_locations_stay() {
        let r = redactor(&["notes/secret.rs"]);
        let out = r.text("at crates/engine/src/materialize.rs:12:5 on i/o, see notes/secret.rs");
        assert!(out.contains("crates/engine/src/materialize.rs:12:5") && out.contains("i/o"));
        assert!(!out.contains("secret"), "{out}");
    }

    #[test]
    fn json_logs_hash_path_fields() {
        let r = redactor(&[]);
        let line = r#"{"timestamp":"2026-09-28T02:23:10Z","level":"WARN","fields":{"message":"update failed","path":"My Taxes/2025 return.pdf","error":"denied"},"target":"shoal_engine::materialize"}"#;
        let out = r.log(line);
        assert!(!out.contains("Taxes") && !out.contains("return"), "{out}");
        assert!(
            out.contains("shoal_engine::materialize") && out.contains("update failed"),
            "{out}"
        );
    }

    #[test]
    fn including_paths_still_shortens_ids_and_addresses() {
        let r = Redactor::new(true, Vec::new()).unwrap();
        let out = r.text(&format!("x/notes.txt {} 10.0.0.1", "cd".repeat(32)));
        assert!(out.contains("x/notes.txt") && out.contains("local-ipv4"));
        assert!(!out.contains(&"cd".repeat(32)));
    }
}
