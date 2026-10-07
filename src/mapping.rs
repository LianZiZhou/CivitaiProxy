//! Bidirectional mapping between upstream hosts (civitai.com, image.civitai.com,
//! storage hosts, ...) and the public URLs served by this proxy.

use std::collections::BTreeMap;

use crate::config::{Config, Mode};

/// Path prefix carrying an explicit upstream host in single-domain mode.
pub const HOST_PREFIX: &str = "/__h/";

#[derive(Debug, Clone)]
pub struct Mapper {
    pub mode: Mode,
    pub public_domain: String,
    pub public_scheme: String,
    pub root: String,
    allow: Vec<AllowRule>,
    ext_host: String,
    /// upstream host -> public label
    aliases: BTreeMap<String, String>,
    /// public label -> upstream host
    aliases_rev: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
enum AllowRule {
    Exact(String),
    /// Matches the domain itself and any subdomain.
    Suffix(String),
}

/// Where the public part of an upstream host lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBase {
    /// Public host (with port, if `public_domain` has one).
    pub host: String,
    /// Path prefix that must precede the upstream path (empty or `/__h/<host>`).
    pub prefix: String,
}

impl Mapper {
    pub fn new(cfg: &Config) -> Self {
        let allow = cfg
            .upstream_allow
            .iter()
            .map(|a| match a.strip_prefix("*.") {
                Some(d) => AllowRule::Suffix(d.to_string()),
                None => AllowRule::Exact(a.clone()),
            })
            .collect();
        let aliases: BTreeMap<String, String> = cfg
            .aliases
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_ascii_lowercase()))
            .collect();
        let aliases_rev = aliases
            .iter()
            .map(|(k, v)| (v.clone(), k.clone()))
            .collect();
        Self {
            mode: cfg.mode,
            public_domain: cfg.public_domain.clone(),
            public_scheme: cfg.public_scheme.clone(),
            root: cfg.upstream_root.clone(),
            allow,
            ext_host: cfg.ext_host.to_ascii_lowercase(),
            aliases,
            aliases_rev,
        }
    }

    /// Domains whose occurrences should be searched for when rewriting bodies.
    pub fn rewrite_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .allow
            .iter()
            .map(|r| match r {
                AllowRule::Exact(d) | AllowRule::Suffix(d) => d.clone(),
            })
            .collect();
        roots.push(self.root.clone());
        roots.sort();
        roots.dedup();
        roots
    }

    pub fn is_allowed(&self, host: &str) -> bool {
        if host == self.root || host.ends_with(&format!(".{}", self.root)) {
            return valid_host(host);
        }
        valid_host(host)
            && self.allow.iter().any(|r| match r {
                AllowRule::Exact(d) => host == d,
                AllowRule::Suffix(d) => {
                    host == d
                        || (host.len() > d.len() + 1
                            && host.ends_with(d.as_str())
                            && host.as_bytes()[host.len() - d.len() - 1] == b'.')
                }
            })
    }

    /// Hosts belonging to the site itself: cookies and credentials are only forwarded to these.
    pub fn is_first_party(&self, host: &str) -> bool {
        host == self.root
            || host.ends_with(&format!(".{}", self.root))
            || self
                .aliases
                .keys()
                .any(|a| host == a || host.ends_with(&format!(".{a}")))
    }

    fn sub_of_root<'a>(&self, host: &'a str) -> Option<&'a str> {
        host.strip_suffix(self.root.as_str())
            .and_then(|s| s.strip_suffix('.'))
            .filter(|s| !s.is_empty())
    }

    /// Public location of an upstream host, or `None` when the host is not proxied.
    pub fn public_base(&self, host: &str) -> Option<PublicBase> {
        let host = host.to_ascii_lowercase();
        if !self.is_allowed(&host) {
            return None;
        }
        let direct = |h: String| {
            Some(PublicBase {
                host: h,
                prefix: String::new(),
            })
        };
        if host == self.root || host == format!("www.{}", self.root) {
            return direct(self.public_domain.clone());
        }
        match self.mode {
            Mode::Single => Some(PublicBase {
                host: self.public_domain.clone(),
                prefix: format!("{HOST_PREFIX}{host}"),
            }),
            Mode::Wildcard => {
                if let Some(sub) = self.sub_of_root(&host) {
                    let first = sub.rsplit('.').next().unwrap_or(sub);
                    let reserved = first == self.ext_host
                        || first == "www"
                        || self.aliases_rev.contains_key(first);
                    if !reserved {
                        return direct(format!("{sub}.{}", self.public_domain));
                    }
                }
                if let Some(label) = self.aliases.get(&host) {
                    return direct(format!("{label}.{}", self.public_domain));
                }
                Some(PublicBase {
                    host: format!("{}.{}", self.ext_host, self.public_domain),
                    prefix: format!("/{host}"),
                })
            }
        }
    }

    /// Public host name to use for a bare (scheme-less) upstream host occurrence.
    /// Hosts that can only be reached through a path prefix have no bare form.
    pub fn public_bare_host(&self, host: &str) -> Option<String> {
        self.public_base(host)
            .filter(|b| b.prefix.is_empty())
            .map(|b| b.host)
    }

    /// Public URL prefix (`scheme://host/prefix`) for an upstream host.
    pub fn public_origin(&self, host: &str) -> Option<String> {
        self.public_base(host)
            .map(|b| format!("{}://{}{}", self.public_scheme, b.host, b.prefix))
    }

    /// Maps an inbound request (Host header + path-and-query) to `(upstream host, upstream path-and-query)`.
    pub fn route(&self, req_host: &str, path_and_query: &str) -> Option<(String, String)> {
        let req_host = req_host.trim_end_matches('.').to_ascii_lowercase();
        match self.mode {
            Mode::Single => {
                if let Some(rest) = path_and_query.strip_prefix(HOST_PREFIX) {
                    return self.split_host_path(rest);
                }
                Some((self.root.clone(), path_and_query.to_string()))
            }
            Mode::Wildcard => {
                if req_host == self.public_domain
                    || req_host == format!("www.{}", self.public_domain)
                {
                    return Some((self.root.clone(), path_and_query.to_string()));
                }
                let label = req_host
                    .strip_suffix(self.public_domain.as_str())?
                    .strip_suffix('.')?;
                if label == self.ext_host {
                    return self.split_host_path(path_and_query.strip_prefix('/')?);
                }
                if let Some(up) = self.aliases_rev.get(label) {
                    return Some((up.clone(), path_and_query.to_string()));
                }
                let host = format!("{label}.{}", self.root);
                self.is_allowed(&host)
                    .then(|| (host, path_and_query.to_string()))
            }
        }
    }

    /// `host/rest?query` -> (host, `/rest?query`)
    fn split_host_path(&self, s: &str) -> Option<(String, String)> {
        let end = s.find(['/', '?']).unwrap_or(s.len());
        let host = s[..end].to_ascii_lowercase();
        if !self.is_allowed(&host) {
            return None;
        }
        let rest = &s[end..];
        let pq = if rest.starts_with('/') {
            rest.to_string()
        } else {
            format!("/{rest}")
        };
        Some((host, pq))
    }

    /// Maps a public host (+ the text following it) back to an upstream host.
    /// Returns the upstream host and how many bytes of `after` (a path-encoded host) were consumed.
    pub fn upstream_for_public(
        &self,
        public_host: &str,
        after: &str,
        sep: &str,
    ) -> Option<(String, usize)> {
        let public_host = public_host.to_ascii_lowercase();
        let take_host = |after: &str, lead: &str| -> Option<(String, usize)> {
            let rest = after.strip_prefix(lead)?;
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
                .unwrap_or(rest.len());
            let host = rest[..end].to_ascii_lowercase();
            self.is_allowed(&host).then(|| (host, lead.len() + end))
        };
        match self.mode {
            Mode::Single => {
                if public_host != self.public_domain {
                    return None;
                }
                let lead = format!("{sep}__h{sep}");
                take_host(after, &lead).or_else(|| Some((self.root.clone(), 0)))
            }
            Mode::Wildcard => {
                if public_host == self.public_domain
                    || public_host == format!("www.{}", self.public_domain)
                {
                    return Some((self.root.clone(), 0));
                }
                let label = public_host
                    .strip_suffix(self.public_domain.as_str())?
                    .strip_suffix('.')?;
                if label == self.ext_host {
                    return take_host(after, sep);
                }
                if let Some(up) = self.aliases_rev.get(label) {
                    return Some((up.clone(), 0));
                }
                let host = format!("{label}.{}", self.root);
                self.is_allowed(&host).then_some((host, 0))
            }
        }
    }
}

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !h.starts_with(['.', '-'])
        && !h.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper(mode: Mode, domain: &str) -> Mapper {
        let cfg = Config {
            mode,
            public_domain: domain.into(),
            ..Config::default()
        };
        Mapper::new(&cfg)
    }

    const R2: &str =
        "civitai-delivery-worker-prod.5ac0637cfd0766c97916cefa3764fbdf.r2.cloudflarestorage.com";

    #[test]
    fn single_routes() {
        let m = mapper(Mode::Single, "cv.example.com");
        assert_eq!(
            m.route("cv.example.com", "/api/v1/models?limit=1"),
            Some(("civitai.com".into(), "/api/v1/models?limit=1".into()))
        );
        assert_eq!(
            m.route("cv.example.com", "/__h/image.civitai.com/x/y.jpeg"),
            Some(("image.civitai.com".into(), "/x/y.jpeg".into()))
        );
        assert_eq!(
            m.route("cv.example.com", &format!("/__h/{R2}?a=1")),
            Some((R2.into(), "/?a=1".into()))
        );
        assert_eq!(m.route("cv.example.com", "/__h/evil.com/x"), None);
        assert_eq!(
            m.public_origin("image.civitai.com").unwrap(),
            "https://cv.example.com/__h/image.civitai.com"
        );
        assert_eq!(
            m.public_origin("civitai.com").unwrap(),
            "https://cv.example.com"
        );
        assert_eq!(m.public_origin("evil.com"), None);
        assert_eq!(m.public_bare_host("image.civitai.com"), None);
        assert_eq!(
            m.public_bare_host("civitai.com").as_deref(),
            Some("cv.example.com")
        );
    }

    #[test]
    fn wildcard_routes() {
        let m = mapper(Mode::Wildcard, "example.com");
        assert_eq!(
            m.route("example.com", "/"),
            Some(("civitai.com".into(), "/".into()))
        );
        assert_eq!(
            m.route("www.example.com", "/"),
            Some(("civitai.com".into(), "/".into()))
        );
        assert_eq!(
            m.route("image.example.com", "/a"),
            Some(("image.civitai.com".into(), "/a".into()))
        );
        assert_eq!(
            m.route("civitai-red.example.com", "/a"),
            Some(("civitai.red".into(), "/a".into()))
        );
        assert_eq!(
            m.route("ext.example.com", &format!("/{R2}/53515/m.safetensors?X=1")),
            Some((R2.into(), "/53515/m.safetensors?X=1".into()))
        );
        assert_eq!(m.route("ext.example.com", "/evil.com/x"), None);
        assert_eq!(m.route("other.org", "/"), None);
        assert_eq!(
            m.public_origin("image.civitai.com").unwrap(),
            "https://image.example.com"
        );
        assert_eq!(
            m.public_origin(R2).unwrap(),
            format!("https://ext.example.com/{R2}")
        );
        assert_eq!(
            m.public_origin("civitai.red").unwrap(),
            "https://civitai-red.example.com"
        );
        assert_eq!(
            m.public_origin("image.civitai.red").unwrap(),
            "https://ext.example.com/image.civitai.red"
        );
        // a civitai subdomain colliding with the ext label falls back to path encoding
        assert_eq!(
            m.public_origin("ext.civitai.com").unwrap(),
            "https://ext.example.com/ext.civitai.com"
        );
    }

    #[test]
    fn reverse() {
        let m = mapper(Mode::Wildcard, "example.com");
        assert_eq!(
            m.upstream_for_public("image.example.com", "/x", "/"),
            Some(("image.civitai.com".into(), 0))
        );
        assert_eq!(
            m.upstream_for_public("ext.example.com", &format!("/{R2}/x"), "/"),
            Some((R2.into(), R2.len() + 1))
        );
        let s = mapper(Mode::Single, "cv.example.com");
        assert_eq!(
            s.upstream_for_public("cv.example.com", "%2F__h%2Fimage.civitai.com%2Fx", "%2F"),
            Some((
                "image.civitai.com".into(),
                "%2F__h%2Fimage.civitai.com".len()
            ))
        );
        assert_eq!(
            s.upstream_for_public("cv.example.com", "/api", "/"),
            Some(("civitai.com".into(), 0))
        );
    }

    #[test]
    fn allow_rules() {
        let m = mapper(Mode::Single, "x");
        assert!(m.is_allowed("civitai.com"));
        assert!(m.is_allowed("a.b.civitai.com"));
        assert!(!m.is_allowed("notcivitai.com"));
        assert!(!m.is_allowed("civitai.com.evil.com"));
        assert!(m.is_allowed(R2));
        assert!(!m.is_allowed("r2.cloudflarestorage.com.evil"));
    }
}
