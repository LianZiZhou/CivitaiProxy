//! Text rewriting of URLs/hosts between upstream and public form.
//!
//! Handles plain URLs (`https://host`), protocol-relative (`//host`), JSON-escaped
//! (`https:\/\/host`), URL-encoded (`https%3A%2F%2Fhost`) and bare host occurrences.

use std::sync::Arc;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};

use crate::mapping::Mapper;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Enc {
    Plain,
    Json,
    Url,
}

impl Enc {
    fn sep(self) -> &'static str {
        match self {
            Enc::Plain => "/",
            Enc::Json => "\\/",
            Enc::Url => "%2F",
        }
    }

    fn encode(self, s: &str) -> String {
        match self {
            Enc::Plain => s.to_string(),
            Enc::Json => s.replace('/', "\\/"),
            Enc::Url => s.replace(':', "%3A").replace('/', "%2F"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Http,
    Ws,
}

/// A recognised scheme prefix directly in front of a host.
#[derive(Clone, Copy, Debug)]
struct Prefix {
    len: usize,
    enc: Enc,
    scheme: Option<Scheme>,
}

const PREFIXES: &[(&str, Enc, Option<Scheme>)] = &[
    ("https%3a%2f%2f", Enc::Url, Some(Scheme::Http)),
    ("http%3a%2f%2f", Enc::Url, Some(Scheme::Http)),
    ("wss%3a%2f%2f", Enc::Url, Some(Scheme::Ws)),
    ("ws%3a%2f%2f", Enc::Url, Some(Scheme::Ws)),
    ("https:\\/\\/", Enc::Json, Some(Scheme::Http)),
    ("http:\\/\\/", Enc::Json, Some(Scheme::Http)),
    ("wss:\\/\\/", Enc::Json, Some(Scheme::Ws)),
    ("ws:\\/\\/", Enc::Json, Some(Scheme::Ws)),
    ("https://", Enc::Plain, Some(Scheme::Http)),
    ("http://", Enc::Plain, Some(Scheme::Http)),
    ("wss://", Enc::Plain, Some(Scheme::Ws)),
    ("ws://", Enc::Plain, Some(Scheme::Ws)),
    ("%2f%2f", Enc::Url, None),
    ("\\/\\/", Enc::Json, None),
    ("//", Enc::Plain, None),
];

fn detect_prefix(input: &[u8], host_start: usize) -> Option<Prefix> {
    let before = &input[..host_start];
    for (p, enc, scheme) in PREFIXES {
        let p = p.as_bytes();
        if before.len() >= p.len() && before[before.len() - p.len()..].eq_ignore_ascii_case(p) {
            // A scheme-less `//` must not be the tail of a scheme we do not handle (e.g. `ftp://`).
            if scheme.is_none() {
                let i = before.len() - p.len();
                if i > 0 && before[i - 1] == b':' {
                    return None;
                }
            }
            return Some(Prefix {
                len: p.len(),
                enc: *enc,
                scheme: *scheme,
            });
        }
    }
    None
}

fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'.'
}

/// A located host occurrence inside a text.
struct Hit<'a> {
    /// Start of the replaced span (scheme prefix included).
    start: usize,
    /// Start of the host itself.
    host_start: usize,
    host: &'a str,
    prefix: Option<Prefix>,
}

/// Finds host occurrences ending with one of the automaton's patterns and lets `f` replace them.
/// `f` returns the replacement and how many bytes after the host it consumed.
fn scan<F>(input: &[u8], ac: &AhoCorasick, mut f: F) -> Option<Vec<u8>>
where
    F: FnMut(&Hit, &[u8]) -> Option<(String, usize)>,
{
    let mut out: Option<Vec<u8>> = None;
    let mut last = 0usize;
    for m in ac.find_iter(input) {
        let end = m.end();
        // right boundary: the host must end here
        if let Some(&b) = input.get(end) {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                continue;
            }
            if b == b'.'
                && input
                    .get(end + 1)
                    .is_some_and(|c| c.is_ascii_alphanumeric())
            {
                continue;
            }
        }
        let mut hs = m.start();
        while hs > 0 && is_host_byte(input[hs - 1]) {
            // stop at a percent-escape such as the `%2F` in `https%3A%2F%2Fhost`
            if hs >= 3
                && input[hs - 3] == b'%'
                && input[hs - 2].is_ascii_hexdigit()
                && input[hs - 1].is_ascii_hexdigit()
            {
                break;
            }
            hs -= 1;
        }
        while hs < m.start() && (input[hs] == b'.' || input[hs] == b'-') {
            hs += 1;
        }
        if hs < last {
            continue;
        }
        let Ok(host) = std::str::from_utf8(&input[hs..end]) else {
            continue;
        };
        let prefix = if hs > 0 && input[hs - 1] == b'.' {
            None
        } else {
            detect_prefix(input, hs)
        };
        let start = hs - prefix.map_or(0, |p| p.len);
        if start < last {
            continue;
        }
        let hit = Hit {
            start,
            host_start: hs,
            host,
            prefix,
        };
        if let Some((rep, consumed)) = f(&hit, &input[end..]) {
            let buf = out.get_or_insert_with(|| Vec::with_capacity(input.len() + 1024));
            buf.extend_from_slice(&input[last..hit.start]);
            buf.extend_from_slice(rep.as_bytes());
            last = end + consumed;
        }
    }
    let mut buf = out?;
    buf.extend_from_slice(&input[last..]);
    Some(buf)
}

pub struct Rewriter {
    mapper: Arc<Mapper>,
    rewrite_bare: bool,
    fwd: AhoCorasick,
    rev: AhoCorasick,
}

impl Rewriter {
    pub fn new(mapper: Arc<Mapper>, rewrite_bare: bool) -> Self {
        let fwd = AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::LeftmostLongest)
            .build(mapper.rewrite_roots())
            .expect("build automaton");
        let rev = AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::LeftmostLongest)
            .build(mapper.public_domains())
            .expect("build automaton");
        Self {
            mapper,
            rewrite_bare,
            fwd,
            rev,
        }
    }

    pub fn mapper(&self) -> &Mapper {
        &self.mapper
    }

    /// Upstream -> public. Returns `None` when nothing changed.
    pub fn to_public(&self, input: &[u8]) -> Option<Vec<u8>> {
        let m = &self.mapper;
        scan(input, &self.fwd, |hit, _| {
            let host = hit.host.to_ascii_lowercase();
            match hit.prefix {
                Some(p) => {
                    let base = m.public_base(&host)?;
                    let scheme = match p.scheme {
                        None => String::new(),
                        Some(Scheme::Http) => format!("{}://", m.public_scheme),
                        Some(Scheme::Ws) => {
                            if m.public_scheme == "https" {
                                "wss://".into()
                            } else {
                                "ws://".into()
                            }
                        }
                    };
                    let scheme = if p.scheme.is_none() {
                        "//".to_string()
                    } else {
                        scheme
                    };
                    Some((
                        p.enc
                            .encode(&format!("{scheme}{}{}", base.host, base.prefix)),
                        0,
                    ))
                }
                None => {
                    if !self.rewrite_bare {
                        return None;
                    }
                    // leave e-mail addresses and path segments of other URLs alone
                    if hit.host_start > 0 && matches!(input[hit.host_start - 1], b'@' | b'/') {
                        return None;
                    }
                    m.public_bare_host(&host).map(|h| (h, 0))
                }
            }
        })
    }

    pub fn to_public_string(&self, s: &str) -> String {
        match self.to_public(s.as_bytes()) {
            Some(v) => String::from_utf8(v).unwrap_or_else(|_| s.to_string()),
            None => s.to_string(),
        }
    }

    /// Public -> upstream (for request headers, query strings and bodies).
    pub fn to_upstream(&self, input: &[u8]) -> Option<Vec<u8>> {
        let m = &self.mapper;
        // `public_domain` may carry a port; let the port be part of the matched host.
        scan(input, &self.rev, |hit, after| {
            let enc = hit.prefix.map_or(Enc::Plain, |p| p.enc);
            let after = std::str::from_utf8(&after[..after.len().min(300)]).unwrap_or("");
            let (up, consumed) = m.upstream_for_public(hit.host, after, enc.sep())?;
            match hit.prefix {
                Some(p) => {
                    let scheme = match p.scheme {
                        None => "//",
                        Some(Scheme::Http) => "https://",
                        Some(Scheme::Ws) => "wss://",
                    };
                    Some((p.enc.encode(&format!("{scheme}{up}")), consumed))
                }
                None if self.rewrite_bare && consumed == 0 => Some((up, 0)),
                None => None,
            }
        })
    }

    pub fn to_upstream_string(&self, s: &str) -> String {
        match self.to_upstream(s.as_bytes()) {
            Some(v) => String::from_utf8(v).unwrap_or_else(|_| s.to_string()),
            None => s.to_string(),
        }
    }
}

/// Content types whose bodies are rewritten.
/// Request bodies whose public URLs are mapped back to upstream form.
pub fn is_rewritable_request(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ct == "application/x-www-form-urlencoded" || ct == "application/json" || ct.ends_with("+json")
}

/// Removes `integrity="..."` (Subresource Integrity) attributes from HTML: rewritten
/// assets would no longer match their hashes.
pub fn strip_integrity(html: &[u8]) -> Option<Vec<u8>> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::bytes::Regex::new(r#"(?i)\sintegrity\s*=\s*("[^"]*"|'[^']*')"#).unwrap()
    });
    if !re.is_match(html) {
        return None;
    }
    Some(re.replace_all(html, &b""[..]).into_owned())
}

pub fn is_rewritable(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ct == "text/event-stream" {
        return false;
    }
    ct.starts_with("text/")
        || ct.contains("javascript")
        || ct.contains("json")
        || ct.contains("xml")
        || ct == "application/x-www-form-urlencoded"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Mode};

    fn rw(mode: Mode, domain: &str) -> Rewriter {
        let cfg = Config {
            mode,
            public_domain: domain.into(),
            ..Config::default()
        };
        Rewriter::new(Arc::new(Mapper::new(&cfg).unwrap()), true)
    }

    fn fwd(r: &Rewriter, s: &str) -> String {
        r.to_public_string(s)
    }

    const R2: &str =
        "civitai-delivery-worker-prod.5ac0637cfd0766c97916cefa3764fbdf.r2.cloudflarestorage.com";

    #[test]
    fn wildcard_forward() {
        let r = rw(Mode::Wildcard, "example.com");
        assert_eq!(
            fwd(&r, r#"<a href="https://civitai.com/models/1">"#),
            r#"<a href="https://example.com/models/1">"#
        );
        assert_eq!(
            fwd(
                &r,
                r#"{"url":"https://image.civitai.com/xG1/abc/width=450/a.jpeg"}"#
            ),
            r#"{"url":"https://image.example.com/xG1/abc/width=450/a.jpeg"}"#
        );
        assert_eq!(
            fwd(&r, r#""https:\/\/image.civitai.com\/x""#),
            r#""https:\/\/image.example.com\/x""#
        );
        assert_eq!(
            fwd(&r, "cb=https%3A%2F%2Fcivitai.com%2Fapi"),
            "cb=https%3A%2F%2Fexample.com%2Fapi"
        );
        assert_eq!(
            fwd(&r, "src=//image.civitai.com/a"),
            "src=//image.example.com/a"
        );
        assert_eq!(
            fwd(&r, "wss://signals-new.civitai.com/hub"),
            "wss://signals-new.example.com/hub"
        );
        assert_eq!(fwd(&r, "domain=.civitai.com;"), "domain=.example.com;");
        assert_eq!(fwd(&r, "host===\"civitai.com\""), "host===\"example.com\"");
        assert_eq!(
            fwd(&r, "mail support@civitai.com"),
            "mail support@civitai.com"
        );
        assert_eq!(
            fwd(&r, "https://cdn.ads.net/engine/civitai.com/loader.js"),
            "https://cdn.ads.net/engine/civitai.com/loader.js"
        );
        assert_eq!(
            fwd(&r, "https://notcivitai.com/x"),
            "https://notcivitai.com/x"
        );
        assert_eq!(
            fwd(&r, "https://civitai.com.evil.org/x"),
            "https://civitai.com.evil.org/x"
        );
        assert_eq!(
            fwd(
                &r,
                &format!("Location: https://{R2}/53515/model/a.safetensors?X-Amz-Signature=abc")
            ),
            format!(
                "Location: https://ext.example.com/{R2}/53515/model/a.safetensors?X-Amz-Signature=abc"
            )
        );
        // bare storage host has no public bare form
        assert_eq!(fwd(&r, R2), R2);
    }

    #[test]
    fn single_forward() {
        let r = rw(Mode::Single, "cv.example.com");
        assert_eq!(
            fwd(
                &r,
                r#"{"downloadUrl":"https://civitai.com/api/download/models/128713","img":"https://image.civitai.com/a.jpg"}"#
            ),
            r#"{"downloadUrl":"https://cv.example.com/api/download/models/128713","img":"https://cv.example.com/__h/image.civitai.com/a.jpg"}"#
        );
        assert_eq!(
            fwd(&r, r#""https:\/\/image.civitai.com\/x""#),
            r#""https:\/\/cv.example.com\/__h\/image.civitai.com\/x""#
        );
        assert_eq!(
            fwd(&r, "u=https%3A%2F%2Fimage.civitai.com%2Fx"),
            "u=https%3A%2F%2Fcv.example.com%2F__h%2Fimage.civitai.com%2Fx"
        );
        // bare subdomains are left alone in single mode, the root is mapped
        assert_eq!(
            fwd(&r, "a image.civitai.com b civitai.com"),
            "a image.civitai.com b cv.example.com"
        );
    }

    #[test]
    fn reverse() {
        let r = rw(Mode::Wildcard, "example.com");
        assert_eq!(
            r.to_upstream_string("https://example.com/models/1"),
            "https://civitai.com/models/1"
        );
        assert_eq!(
            r.to_upstream_string("https://image.example.com/a"),
            "https://image.civitai.com/a"
        );
        assert_eq!(
            r.to_upstream_string(&format!("https://ext.example.com/{R2}/a?b")),
            format!("https://{R2}/a?b")
        );
        assert_eq!(
            r.to_upstream_string("callbackUrl=https%3A%2F%2Fexample.com%2Fuser"),
            "callbackUrl=https%3A%2F%2Fcivitai.com%2Fuser"
        );
        assert_eq!(
            r.to_upstream_string("https://myexample.com/a"),
            "https://myexample.com/a"
        );

        let s = rw(Mode::Single, "localhost:8787");
        assert_eq!(
            s.to_upstream_string("http://localhost:8787/models"),
            "https://civitai.com/models"
        );
        assert_eq!(
            s.to_upstream_string("http://localhost:8787/__h/image.civitai.com/a"),
            "https://image.civitai.com/a"
        );
    }

    #[test]
    fn roundtrip_single_with_port() {
        let r = rw(Mode::Single, "localhost:8787");
        let p = fwd(&r, "https://image.civitai.com/a.jpg");
        assert_eq!(p, "https://localhost:8787/__h/image.civitai.com/a.jpg");
        assert_eq!(r.to_upstream_string(&p), "https://image.civitai.com/a.jpg");
    }

    #[test]
    fn integrity() {
        let h = br#"<script src="/a.js" integrity="sha512-abc" crossorigin="anonymous"></script>"#;
        assert_eq!(
            strip_integrity(h).unwrap(),
            br#"<script src="/a.js" crossorigin="anonymous"></script>"#.to_vec()
        );
        assert!(strip_integrity(b"<p>integrity</p>").is_none());
    }

    #[test]
    fn multi_site_bodies() {
        use crate::config::SiteConfig;
        let site = |p: &str, m: Mode, d: &str| SiteConfig {
            preset: Some(p.into()),
            mode: m,
            public_domain: d.into(),
            ..Default::default()
        };
        let cfg = Config {
            sites: vec![
                site("civitai", Mode::Single, "cv.example.com"),
                site("huggingface", Mode::Single, "hf.example.com"),
                site("github", Mode::Single, "gh.example.com"),
            ],
            ..Config::default()
        };
        let r = Rewriter::new(Arc::new(Mapper::new(&cfg).unwrap()), true);
        assert_eq!(
            fwd(
                &r,
                "see https://huggingface.co/gpt2 and https://github.com/a/b and https://civitai.com/models/1"
            ),
            "see https://hf.example.com/gpt2 and https://gh.example.com/a/b and https://cv.example.com/models/1"
        );
        assert_eq!(
            fwd(
                &r,
                "<https://cas-server.xethub.hf.co/v1/reconstructions/ab>; rel=\"xet-reconstruction-info\""
            ),
            "<https://hf.example.com/__h/cas-server.xethub.hf.co/v1/reconstructions/ab>; rel=\"xet-reconstruction-info\""
        );
        assert_eq!(
            r.to_upstream_string("https://gh.example.com/a"),
            "https://github.com/a"
        );
        assert_eq!(
            r.to_upstream_string("https://hf.example.com/__h/cdn-lfs.hf.co/x"),
            "https://cdn-lfs.hf.co/x"
        );
    }

    #[test]
    fn content_types() {
        assert!(is_rewritable("text/html; charset=utf-8"));
        assert!(is_rewritable("application/javascript"));
        assert!(is_rewritable("application/json"));
        assert!(is_rewritable("text/x-component"));
        assert!(!is_rewritable("text/event-stream"));
        assert!(!is_rewritable("image/jpeg"));
        assert!(!is_rewritable("application/octet-stream"));
    }
}
