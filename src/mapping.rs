//! Bidirectional mapping between upstream hosts (civitai.com, huggingface.co, github.com,
//! registry-1.docker.io, storage/CDN hosts, ...) and the public URLs served by this proxy.

use std::collections::BTreeMap;

use regex::Regex;

use crate::config::{Config, Mode, SiteConfig};

/// Path prefix carrying an explicit upstream host (`/__h/<host>/...`).
pub const HOST_PREFIX: &str = "/__h/";

/// Host pattern: `*.x` matches `x` and its subdomains, other `*` match any characters.
#[derive(Debug, Clone)]
pub enum HostPat {
    Exact(String),
    Suffix(String),
    Glob(String),
}

impl HostPat {
    pub fn parse(s: &str) -> Self {
        let s = s.trim().to_ascii_lowercase();
        match s.strip_prefix("*.") {
            Some(d) if !d.contains('*') => HostPat::Suffix(d.to_string()),
            _ if s.contains('*') => HostPat::Glob(s),
            _ => HostPat::Exact(s),
        }
    }

    pub fn matches(&self, host: &str) -> bool {
        match self {
            HostPat::Exact(d) => host == d,
            HostPat::Suffix(d) => host == d || is_subdomain(host, d),
            HostPat::Glob(g) => glob(g.as_bytes(), host.as_bytes()),
        }
    }

    /// Registrable part used to find occurrences in text.
    fn search_root(&self) -> String {
        match self {
            HostPat::Exact(d) | HostPat::Suffix(d) => d.clone(),
            HostPat::Glob(g) => g
                .rsplit('*')
                .next()
                .unwrap_or("")
                .trim_start_matches(['.', '-'])
                .to_string(),
        }
    }
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| glob(rest, &s[i..])),
        Some((c, rest)) => s.first() == Some(c) && glob(rest, &s[1..]),
    }
}

fn is_subdomain(host: &str, domain: &str) -> bool {
    host.len() > domain.len() + 1
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

fn any(pats: &[HostPat], host: &str) -> bool {
    pats.iter().any(|p| p.matches(host))
}

/// How traffic to a set of upstream hosts is handled.
#[derive(Debug)]
pub struct HostPolicy {
    pub name: String,
    hosts: Vec<HostPat>,
    credential: Vec<HostPat>,
    passthrough_hosts: Vec<HostPat>,
    passthrough_paths: Vec<Regex>,
    passthrough_accept: Vec<String>,
    rewrite_bodies: bool,
    rewrite_paths: Option<Vec<Regex>>,
    rewrite_requests: bool,
    /// Upstream idle read timeout override (seconds).
    pub read_timeout: Option<u64>,
    /// gRPC requests are forwarded over HTTP/2 with trailers.
    pub grpc: bool,
}

fn regexes(v: &Option<Vec<String>>) -> anyhow::Result<Vec<Regex>> {
    v.iter()
        .flatten()
        .map(|r| Regex::new(r).map_err(|e| anyhow::anyhow!("bad regex `{r}`: {e}")))
        .collect()
}

fn pats(v: &Option<Vec<String>>) -> Vec<HostPat> {
    v.iter().flatten().map(|s| HostPat::parse(s)).collect()
}

impl HostPolicy {
    fn new(c: &SiteConfig) -> anyhow::Result<Self> {
        let root = c.root.clone().unwrap_or_default().to_ascii_lowercase();
        let mut hosts = pats(&c.allow);
        if !root.is_empty() {
            hosts.push(HostPat::Exact(root.clone()));
        }
        for r in c.routes.iter().flatten() {
            hosts.push(HostPat::Exact(r.host.to_ascii_lowercase()));
        }
        let credential = match &c.credential_hosts {
            Some(_) => pats(&c.credential_hosts),
            None if root.is_empty() => Vec::new(),
            None => vec![HostPat::Suffix(root.clone())],
        };
        Ok(Self {
            name: c.name.clone().unwrap_or(root),
            hosts,
            credential,
            passthrough_hosts: pats(&c.passthrough_hosts),
            passthrough_paths: regexes(&c.passthrough_paths)?,
            passthrough_accept: c.passthrough_accept.clone().unwrap_or_default(),
            rewrite_bodies: c.rewrite_bodies.unwrap_or(true),
            rewrite_paths: match &c.rewrite_paths {
                Some(_) => Some(regexes(&c.rewrite_paths)?),
                None => None,
            },
            rewrite_requests: c.rewrite_requests.unwrap_or(true),
            read_timeout: c.read_timeout_secs,
            grpc: c.grpc.unwrap_or(false),
        })
    }

    pub fn matches(&self, host: &str) -> bool {
        any(&self.hosts, host)
    }

    pub fn is_credential_host(&self, host: &str) -> bool {
        any(&self.credential, host)
    }

    /// Whether the response body for this request must be passed through untouched.
    pub fn is_passthrough(&self, host: &str, path: &str, accept: &str) -> bool {
        !self.rewrite_bodies
            || any(&self.passthrough_hosts, host)
            || self.passthrough_paths.iter().any(|r| r.is_match(path))
            || self
                .passthrough_accept
                .iter()
                .any(|a| accept.contains(a.as_str()))
    }

    /// Whether a (rewritable) response body may be rewritten.
    pub fn rewrites_response(&self, host: &str, path: &str, accept: &str) -> bool {
        !self.is_passthrough(host, path, accept)
            && self
                .rewrite_paths
                .as_ref()
                .is_none_or(|v| v.iter().any(|r| r.is_match(path)))
    }

    /// Whether query strings, request bodies and WebSocket frames may be rewritten.
    pub fn rewrites_requests(&self, host: &str, path: &str) -> bool {
        self.rewrite_requests && !self.is_passthrough(host, path, "")
    }
}

/// A path prefix on a site's root host: `/<segment>/...` -> `https://<host>/...`.
/// Segment and host may share one `{x}` placeholder (a DNS label).
#[derive(Debug)]
struct Prefix {
    seg: (String, String),
    host: (String, String),
    templated: bool,
}

impl Prefix {
    fn parse(seg: &str, host: &str) -> anyhow::Result<Self> {
        let (seg, host) = (seg.to_ascii_lowercase(), host.to_ascii_lowercase());
        let split = |s: &str| -> (String, String) {
            match s.split_once("{x}") {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (s.to_string(), String::new()),
            }
        };
        let templated = seg.contains("{x}");
        anyhow::ensure!(
            templated == host.contains("{x}"),
            "prefix `{seg}` and host `{host}` must both or neither contain {{x}}"
        );
        anyhow::ensure!(!seg.is_empty() && !seg.contains('/'), "bad prefix `{seg}`");
        Ok(Self {
            seg: split(&seg),
            host: split(&host),
            templated,
        })
    }

    fn host_for(&self, seg: &str) -> Option<String> {
        if !self.templated {
            return (seg == self.seg.0).then(|| self.host.0.clone());
        }
        let x = seg.strip_prefix(&self.seg.0)?.strip_suffix(&self.seg.1)?;
        is_label(x).then(|| format!("{}{x}{}", self.host.0, self.host.1))
    }

    fn seg_for(&self, host: &str) -> Option<String> {
        if !self.templated {
            return (host == self.host.0).then(|| self.seg.0.clone());
        }
        let x = host
            .strip_prefix(&self.host.0)?
            .strip_suffix(&self.host.1)?;
        is_label(x).then(|| format!("{}{x}{}", self.seg.0, self.seg.1))
    }

    fn display(&self) -> String {
        if self.templated {
            format!("{}<x>{}", self.seg.0, self.seg.1)
        } else {
            self.seg.0.clone()
        }
    }
}

/// One provider listed on a gateway's index page.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub name: String,
    pub description: String,
    /// `(prefix as displayed, upstream host as displayed)`
    pub prefixes: Vec<(String, String)>,
    pub base_path: String,
}

#[derive(Debug)]
pub struct Site {
    pub name: String,
    pub mode: Mode,
    /// Public domain (may carry a port).
    pub public: String,
    /// Upstream host served at the root; empty for a pure gateway (prefixes only).
    pub root: String,
    pub sub_root: String,
    pub ext_host: String,
    /// upstream host -> label
    alias_exact: BTreeMap<String, String>,
    /// (upstream suffix, label): `x.<suffix>` <-> `x--<label>.<public>`
    alias_suffix: Vec<(String, String)>,
    routes: Vec<(Regex, String)>,
    /// First match wins; the site's own policy comes first.
    policies: Vec<HostPolicy>,
    prefixes: Vec<Prefix>,
    pub index: Vec<IndexEntry>,
    pub docker_library: bool,
    pub session_cookie: String,
    pub carry_headers: Vec<String>,
}

impl Site {
    pub fn new(c: &SiteConfig) -> anyhow::Result<Self> {
        let root = c.root.clone().unwrap_or_default().to_ascii_lowercase();
        let mut alias_exact = BTreeMap::new();
        let mut alias_suffix = Vec::new();
        for (k, v) in c.aliases.iter().flatten() {
            let (k, v) = (k.to_ascii_lowercase(), v.to_ascii_lowercase());
            match k.strip_prefix("*.") {
                Some(s) => alias_suffix.push((s.to_string(), v)),
                None => {
                    alias_exact.insert(k, v);
                }
            }
        }
        let mut routes = Vec::new();
        for r in c.routes.iter().flatten() {
            let re = Regex::new(&r.path)
                .map_err(|e| anyhow::anyhow!("bad route regex `{}`: {e}", r.path))?;
            routes.push((re, r.host.to_ascii_lowercase()));
        }
        let mut policies = Vec::new();
        let mut prefixes = Vec::new();
        let mut index = Vec::new();
        if !root.is_empty() || c.allow.as_ref().is_some_and(|a| !a.is_empty()) {
            policies.push(HostPolicy::new(c)?);
        }
        for (k, v) in c.prefixes.iter().flatten() {
            prefixes.push(Prefix::parse(k, v)?);
        }
        for name in c.providers.iter().flatten() {
            let p = crate::providers::provider(name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider `{name}`"))?;
            policies.push(HostPolicy::new(&p)?);
            let mut shown = Vec::new();
            for (k, v) in p.prefixes.iter().flatten() {
                let pre = Prefix::parse(k, v)?;
                shown.push((pre.display(), v.replace("{x}", "<x>")));
                prefixes.push(pre);
            }
            // the provider's own name first, then the simplest prefix
            shown.sort_by_key(|(seg, _)| (seg != name, seg.len()));
            index.push(IndexEntry {
                name: name.clone(),
                description: p.description.unwrap_or_default(),
                prefixes: shown,
                base_path: p.base_path.unwrap_or_default(),
            });
        }
        // literal prefixes first, then the most specific templates
        prefixes.sort_by_key(|p| {
            (
                p.templated,
                std::cmp::Reverse(p.seg.0.len() + p.seg.1.len()),
            )
        });
        Ok(Self {
            name: c.name.clone().unwrap_or_else(|| root.clone()),
            mode: c.mode,
            public: c.public_domain.clone(),
            sub_root: c
                .sub_root
                .clone()
                .unwrap_or_else(|| root.clone())
                .to_ascii_lowercase(),
            root,
            ext_host: c
                .ext_host
                .clone()
                .unwrap_or_else(|| "ext".into())
                .to_ascii_lowercase(),
            alias_exact,
            alias_suffix,
            routes,
            policies,
            prefixes,
            index,
            docker_library: c.docker_library.unwrap_or(false),
            session_cookie: c.session_cookie.clone().unwrap_or_default(),
            carry_headers: c
                .carry_headers
                .iter()
                .flatten()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
        })
    }

    /// Distinct read timeouts used by this site's policies.
    pub fn read_timeouts(&self) -> Vec<u64> {
        self.policies
            .iter()
            .filter_map(|p| p.read_timeout)
            .collect()
    }

    /// A pure gateway has no upstream at its root, only prefixes.
    pub fn is_gateway(&self) -> bool {
        self.root.is_empty()
    }

    pub fn allows(&self, host: &str) -> bool {
        valid_host(host) && self.policies.iter().any(|p| p.matches(host))
    }

    pub fn policy_for(&self, host: &str) -> Option<&HostPolicy> {
        self.policies.iter().find(|p| p.matches(host))
    }

    fn is_route_host(&self, host: &str) -> bool {
        self.routes.iter().any(|(_, h)| h == host)
    }

    /// Upstream host for a first path segment, if it is one of this site's prefixes.
    fn prefix_host(&self, seg: &str) -> Option<String> {
        let seg = seg.to_ascii_lowercase();
        self.prefixes
            .iter()
            .find_map(|p| p.host_for(&seg))
            .filter(|h| self.allows(h))
    }

    /// Path segment under which an upstream host is reachable, if any.
    fn prefix_seg(&self, host: &str) -> Option<String> {
        self.prefixes.iter().find_map(|p| p.seg_for(host))
    }

    fn ext_base(&self, host: &str) -> PublicBase {
        match self.mode {
            Mode::Single => PublicBase {
                host: self.public.clone(),
                prefix: format!("{HOST_PREFIX}{host}"),
            },
            Mode::Wildcard => PublicBase {
                host: format!("{}.{}", self.ext_host, self.public),
                prefix: format!("/{host}"),
            },
        }
    }

    /// Preferred public location of `host` (may still collide with another site; see `Mapper::public_base`).
    fn candidate(&self, host: &str) -> PublicBase {
        let direct = |h: String| PublicBase {
            host: h,
            prefix: String::new(),
        };
        if host == self.root || host == format!("www.{}", self.root) || self.is_route_host(host) {
            return direct(self.public.clone());
        }
        if let Some(seg) = self.prefix_seg(host) {
            return PublicBase {
                host: self.public.clone(),
                prefix: format!("/{seg}"),
            };
        }
        if self.mode == Mode::Single {
            return self.ext_base(host);
        }
        if let Some(label) = self.alias_exact.get(host) {
            return direct(format!("{label}.{}", self.public));
        }
        for (suffix, label) in &self.alias_suffix {
            if let Some(x) = host
                .strip_suffix(suffix.as_str())
                .and_then(|x| x.strip_suffix('.'))
                && is_label(x)
                && !x.contains("--")
            {
                return direct(format!("{x}--{label}.{}", self.public));
            }
        }
        if let Some(sub) = host
            .strip_suffix(self.sub_root.as_str())
            .and_then(|s| s.strip_suffix('.'))
            && is_label(sub)
            && !sub.contains("--")
            && sub != self.ext_host
            && sub != "www"
            && !self.alias_exact.values().any(|l| l == sub)
        {
            return direct(format!("{sub}.{}", self.public));
        }
        self.ext_base(host)
    }

    /// What a public label (`<label>.<public>`) of this wildcard site maps to.
    fn label_target(&self, label: &str) -> Option<PublicTarget> {
        if label == self.ext_host {
            return Some(PublicTarget::Ext);
        }
        if let Some((up, _)) = self.alias_exact.iter().find(|(_, l)| l.as_str() == label) {
            return Some(PublicTarget::Host(up.clone()));
        }
        if let Some((x, l)) = label.rsplit_once("--") {
            let (suffix, _) = self.alias_suffix.iter().find(|(_, al)| al == l)?;
            let host = format!("{x}.{suffix}");
            return self.allows(&host).then_some(PublicTarget::Host(host));
        }
        let host = format!("{label}.{}", self.sub_root);
        self.allows(&host).then_some(PublicTarget::Host(host))
    }

    /// Rewrites the request path for docker registry quirks (`nginx` -> `library/nginx`).
    fn fix_path(&self, host: &str, pq: &str) -> String {
        if self.docker_library
            && self.is_route_host(host)
            && let Some(rest) = pq.strip_prefix("/v2/")
        {
            let name_end = rest.find('/').unwrap_or(rest.len());
            let after = &rest[name_end..];
            let is_api = ["/manifests/", "/blobs/", "/tags/", "/referrers/"]
                .iter()
                .any(|p| after.starts_with(p));
            if name_end > 0 && is_api && &rest[..name_end] != "library" {
                return format!("/v2/library/{rest}");
            }
        }
        pq.to_string()
    }

    pub fn is_credential_host(&self, host: &str) -> bool {
        self.policy_for(host)
            .is_some_and(|p| p.is_credential_host(host))
    }

    pub fn is_passthrough(&self, host: &str, path: &str, accept: &str) -> bool {
        self.policy_for(host)
            .is_none_or(|p| p.is_passthrough(host, path, accept))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PublicTarget {
    /// The site's root host (routes and `/__h/` apply).
    Root,
    /// Path-encoded host (`ext.<public>/<host>/...`).
    Ext,
    Host(String),
}

/// Where the public part of an upstream host lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBase {
    /// Public host (with port, if the site's public domain has one).
    pub host: String,
    /// Path prefix that must precede the upstream path (empty, `/__h/<host>` or `/<host>`).
    pub prefix: String,
}

#[derive(Debug)]
pub struct Mapper {
    pub sites: Vec<Site>,
    pub public_scheme: String,
}

impl Mapper {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let sites = cfg
            .site_configs()?
            .iter()
            .map(Site::new)
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            sites,
            public_scheme: cfg.public_scheme.clone(),
        })
    }

    /// Domains whose occurrences are searched for when rewriting bodies.
    pub fn rewrite_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .sites
            .iter()
            .flat_map(|s| s.policies.iter().flat_map(|p| p.hosts.iter()))
            .map(|p| p.search_root())
            .filter(|r| r.contains('.'))
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// All public domains (for reverse rewriting).
    pub fn public_domains(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sites.iter().map(|s| s.public.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn site_for_upstream(&self, host: &str) -> Option<&Site> {
        let host = host.to_ascii_lowercase();
        self.sites
            .iter()
            .find(|s| s.root == host || s.is_route_host(&host))
            .or_else(|| self.sites.iter().find(|s| s.allows(&host)))
    }

    pub fn is_allowed(&self, host: &str) -> bool {
        self.site_for_upstream(host).is_some()
    }

    pub fn is_credential_host(&self, host: &str) -> bool {
        self.site_for_upstream(host)
            .is_some_and(|s| s.is_credential_host(host))
    }

    pub fn is_passthrough(&self, host: &str, path: &str, accept: &str) -> bool {
        self.site_for_upstream(host)
            .is_none_or(|s| s.is_passthrough(host, path, accept))
    }

    /// Policy for a request to `up_host` that arrived on public host `req_host`: the routing
    /// site's policy wins, so a host reachable through several sites follows the one used.
    pub fn policy(&self, req_host: &str, up_host: &str) -> Option<&HostPolicy> {
        self.site_for_public(req_host)
            .and_then(|s| s.policy_for(up_host))
            .or_else(|| self.site_for_upstream(up_host)?.policy_for(up_host))
    }

    /// Resolves a public host to its site and target.
    fn resolve_public(&self, public_host: &str) -> Option<(&Site, PublicTarget)> {
        let h = public_host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(s) = self
            .sites
            .iter()
            .find(|s| s.public == h || format!("www.{}", s.public) == h)
        {
            return Some((s, PublicTarget::Root));
        }
        // longest wildcard suffix wins
        let mut best: Option<(&Site, &str)> = None;
        for s in self.sites.iter().filter(|s| s.mode == Mode::Wildcard) {
            if let Some(label) = h
                .strip_suffix(s.public.as_str())
                .and_then(|l| l.strip_suffix('.'))
                && is_label(label)
                && best.is_none_or(|(b, _)| s.public.len() > b.public.len())
            {
                best = Some((s, label));
            }
        }
        match best {
            Some((s, label)) => s.label_target(label).map(|t| (s, t)),
            // With a single single-domain site, any Host header (IP, localhost, ...) reaches it.
            None if self.sites.len() == 1 && self.sites[0].mode == Mode::Single => {
                Some((&self.sites[0], PublicTarget::Root))
            }
            None => None,
        }
    }

    /// Public location of an upstream host, or `None` when the host is not proxied.
    pub fn public_base(&self, host: &str) -> Option<PublicBase> {
        let host = host.to_ascii_lowercase();
        let site = self.site_for_upstream(&host)?;
        let cand = site.candidate(&host);
        if cand.prefix.is_empty() {
            // make sure the public host maps back to the same upstream (no clash with other sites)
            let back = match self.resolve_public(&cand.host) {
                Some((s, PublicTarget::Root)) => {
                    s.root == host || format!("www.{}", s.root) == host || s.is_route_host(&host)
                }
                Some((_, PublicTarget::Host(h))) => h == host,
                _ => false,
            };
            if !back {
                return Some(site.ext_base(&host));
            }
        }
        Some(cand)
    }

    /// Public host name for a bare (scheme-less) upstream host occurrence.
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

    /// The site a public request host belongs to.
    pub fn site_for_public(&self, public_host: &str) -> Option<&Site> {
        self.resolve_public(public_host).map(|(s, _)| s)
    }

    /// Maps an inbound request (Host header + path-and-query) to `(upstream host, upstream path-and-query)`.
    pub fn route(&self, req_host: &str, pq: &str) -> Option<(String, String)> {
        let (site, target) = self.resolve_public(req_host)?;
        // ghproxy style: /https://github.com/owner/repo/...
        if let Some(r) = self.full_url_path(pq) {
            // a non-allowed full URL is refused rather than sent to the root host
            return r;
        }
        match target {
            PublicTarget::Root => {
                if let Some(rest) = pq.strip_prefix(HOST_PREFIX) {
                    return self.split_host_path(rest);
                }
                let path = pq.split('?').next().unwrap_or(pq);
                let seg = path[1.min(path.len())..].split('/').next().unwrap_or("");
                if !seg.is_empty()
                    && let Some(host) = site.prefix_host(seg)
                {
                    let rest = &pq[1 + seg.len()..];
                    let rest = if rest.starts_with('/') {
                        rest.to_string()
                    } else {
                        format!("/{rest}")
                    };
                    return Some((host, rest));
                }
                if let Some((_, host)) = site.routes.iter().find(|(re, _)| re.is_match(path)) {
                    return Some((host.clone(), site.fix_path(host, pq)));
                }
                (!site.is_gateway()).then(|| (site.root.clone(), pq.to_string()))
            }
            PublicTarget::Ext => self.split_host_path(pq.strip_prefix('/')?),
            PublicTarget::Host(h) => Some((h, pq.to_string())),
        }
    }

    /// `Some(route)` when the path is a full URL (`/https://host/...`), `None` otherwise.
    fn full_url_path(&self, pq: &str) -> Option<Option<(String, String)>> {
        let rest = pq.strip_prefix('/')?;
        let lower = rest.get(..7)?.to_ascii_lowercase();
        let rest = if lower.starts_with("https:/") {
            &rest[7..]
        } else if lower.starts_with("http:/") {
            &rest[6..]
        } else {
            return None;
        };
        Some(self.split_host_path(rest.trim_start_matches('/')))
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
        let take_host = |after: &str, lead: &str| -> Option<(String, usize)> {
            let rest = after.strip_prefix(lead)?;
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
                .unwrap_or(rest.len());
            let host = rest[..end].to_ascii_lowercase();
            self.is_allowed(&host).then(|| (host, lead.len() + end))
        };
        let (site, target) = self.resolve_public(public_host)?;
        match target {
            PublicTarget::Root => {
                if let Some(r) = take_host(after, &format!("{sep}__h{sep}")) {
                    return Some(r);
                }
                if let Some(rest) = after.strip_prefix(sep) {
                    let end = rest
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                        .unwrap_or(rest.len());
                    if let Some(h) = site.prefix_host(&rest[..end]) {
                        return Some((h, sep.len() + end));
                    }
                }
                (!site.is_gateway()).then(|| (site.root.clone(), 0))
            }
            PublicTarget::Ext => take_host(after, sep),
            PublicTarget::Host(h) => Some((h, 0)),
        }
    }
}

fn is_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
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
    use crate::config::SiteConfig;

    fn legacy(mode: Mode, domain: &str) -> Mapper {
        let cfg = Config {
            mode,
            public_domain: domain.into(),
            ..Config::default()
        };
        Mapper::new(&cfg).unwrap()
    }

    fn multi(sites: &[(&str, Mode, &str)]) -> Mapper {
        let sites = sites
            .iter()
            .map(|(p, m, d)| SiteConfig {
                preset: Some(p.to_string()),
                mode: *m,
                public_domain: d.to_string(),
                ..Default::default()
            })
            .collect();
        Mapper::new(&Config {
            sites,
            ..Config::default()
        })
        .unwrap()
    }

    const R2: &str =
        "civitai-delivery-worker-prod.5ac0637cfd0766c97916cefa3764fbdf.r2.cloudflarestorage.com";

    fn r(m: &Mapper, h: &str, pq: &str) -> Option<(String, String)> {
        m.route(h, pq)
    }

    fn some(h: &str, pq: &str) -> Option<(String, String)> {
        Some((h.to_string(), pq.to_string()))
    }

    #[test]
    fn single_routes() {
        let m = legacy(Mode::Single, "cv.example.com");
        assert_eq!(
            r(&m, "cv.example.com", "/api/v1/models?limit=1"),
            some("civitai.com", "/api/v1/models?limit=1")
        );
        assert_eq!(
            r(&m, "cv.example.com", "/__h/image.civitai.com/x/y.jpeg"),
            some("image.civitai.com", "/x/y.jpeg")
        );
        assert_eq!(
            r(&m, "cv.example.com", &format!("/__h/{R2}?a=1")),
            some(R2, "/?a=1")
        );
        assert_eq!(r(&m, "cv.example.com", "/__h/evil.com/x"), None);
        assert_eq!(r(&m, "127.0.0.1:8787", "/"), some("civitai.com", "/"));
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
        let m = legacy(Mode::Wildcard, "example.com");
        assert_eq!(r(&m, "example.com", "/"), some("civitai.com", "/"));
        assert_eq!(r(&m, "www.example.com", "/"), some("civitai.com", "/"));
        assert_eq!(
            r(&m, "image.example.com", "/a"),
            some("image.civitai.com", "/a")
        );
        assert_eq!(
            r(&m, "civitai-red.example.com", "/a"),
            some("civitai.red", "/a")
        );
        assert_eq!(
            r(
                &m,
                "ext.example.com",
                &format!("/{R2}/53515/m.safetensors?X=1")
            ),
            some(R2, "/53515/m.safetensors?X=1")
        );
        assert_eq!(r(&m, "ext.example.com", "/evil.com/x"), None);
        assert_eq!(r(&m, "other.org", "/"), None);
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
        assert_eq!(
            m.public_origin("ext.civitai.com").unwrap(),
            "https://ext.example.com/ext.civitai.com"
        );
        // multi-label subdomains do not fit a single-level wildcard certificate
        assert_eq!(
            m.public_origin("a.b.civitai.com").unwrap(),
            "https://ext.example.com/a.b.civitai.com"
        );
    }

    #[test]
    fn reverse() {
        let m = legacy(Mode::Wildcard, "example.com");
        assert_eq!(
            m.upstream_for_public("image.example.com", "/x", "/"),
            Some(("image.civitai.com".into(), 0))
        );
        assert_eq!(
            m.upstream_for_public("ext.example.com", &format!("/{R2}/x"), "/"),
            Some((R2.into(), R2.len() + 1))
        );
        let s = legacy(Mode::Single, "cv.example.com");
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
        let m = legacy(Mode::Single, "x");
        assert!(m.is_allowed("civitai.com"));
        assert!(m.is_allowed("a.b.civitai.com"));
        assert!(!m.is_allowed("notcivitai.com"));
        assert!(!m.is_allowed("civitai.com.evil.com"));
        assert!(m.is_allowed(R2));
        assert!(!m.is_allowed("r2.cloudflarestorage.com.evil"));
        assert!(HostPat::parse("cdn-lfs*.hf.co").matches("cdn-lfs-us-1.hf.co"));
        assert!(!HostPat::parse("cdn-lfs*.hf.co").matches("cdn.hf.co"));
    }

    #[test]
    fn multi_site() {
        let m = multi(&[
            ("civitai", Mode::Wildcard, "example.com"),
            ("huggingface", Mode::Wildcard, "hf.example.com"),
            ("github", Mode::Single, "gh.example.com"),
            ("dockerhub", Mode::Single, "docker.example.com"),
        ]);
        // exact single-site hosts win over the civitai wildcard
        assert_eq!(
            r(&m, "gh.example.com", "/cli/cli"),
            some("github.com", "/cli/cli")
        );
        assert_eq!(
            r(&m, "hf.example.com", "/gpt2"),
            some("huggingface.co", "/gpt2")
        );
        assert_eq!(
            r(&m, "cdn-lfs.hf.example.com", "/x"),
            some("cdn-lfs.huggingface.co", "/x")
        );
        assert_eq!(
            r(&m, "image.example.com", "/x"),
            some("image.civitai.com", "/x")
        );
        // hf.space suffix alias
        assert_eq!(
            m.public_origin("user-app.hf.space").unwrap(),
            "https://user-app--space.hf.example.com"
        );
        assert_eq!(
            r(&m, "user-app--space.hf.example.com", "/"),
            some("user-app.hf.space", "/")
        );
        // gh.civitai.com would clash with the github site -> path encoding
        assert_eq!(
            m.public_origin("gh.civitai.com").unwrap(),
            "https://ext.example.com/gh.civitai.com"
        );
        // cross-site rewriting targets
        assert_eq!(
            m.public_origin("github.com").unwrap(),
            "https://gh.example.com"
        );
        assert_eq!(
            m.public_origin("release-assets.githubusercontent.com")
                .unwrap(),
            "https://gh.example.com/__h/release-assets.githubusercontent.com"
        );
        assert_eq!(
            m.public_origin("us.aws.cdn.hf.co").unwrap(),
            "https://ext.hf.example.com/us.aws.cdn.hf.co"
        );
        // ghproxy-style full URLs
        assert_eq!(
            r(
                &m,
                "gh.example.com",
                "/https://github.com/a/b.git/info/refs?service=git-upload-pack"
            ),
            some("github.com", "/a/b.git/info/refs?service=git-upload-pack")
        );
        assert_eq!(
            r(
                &m,
                "gh.example.com",
                "/https:/raw.githubusercontent.com/a/b/main/x"
            ),
            some("raw.githubusercontent.com", "/a/b/main/x")
        );
        assert_eq!(r(&m, "gh.example.com", "/https://evil.com/x"), None);
    }

    #[test]
    fn docker_routes() {
        let m = multi(&[("dockerhub", Mode::Single, "docker.example.com")]);
        let d = "docker.example.com";
        assert_eq!(r(&m, d, "/v2/"), some("registry-1.docker.io", "/v2/"));
        assert_eq!(
            r(&m, d, "/v2/nginx/manifests/latest"),
            some("registry-1.docker.io", "/v2/library/nginx/manifests/latest")
        );
        assert_eq!(
            r(&m, d, "/v2/library/nginx/blobs/sha256:ab"),
            some("registry-1.docker.io", "/v2/library/nginx/blobs/sha256:ab")
        );
        assert_eq!(
            r(&m, d, "/v2/bitnami/redis/tags/list"),
            some("registry-1.docker.io", "/v2/bitnami/redis/tags/list")
        );
        // Hub web API stays on hub.docker.com
        assert_eq!(
            r(&m, d, "/v2/repositories/library/nginx/tags/?page_size=10"),
            some(
                "hub.docker.com",
                "/v2/repositories/library/nginx/tags/?page_size=10"
            )
        );
        assert_eq!(r(&m, d, "/_/nginx"), some("hub.docker.com", "/_/nginx"));
        assert_eq!(
            m.public_origin("registry-1.docker.io").unwrap(),
            "https://docker.example.com"
        );
        assert_eq!(
            m.public_origin("auth.docker.io").unwrap(),
            "https://docker.example.com/__h/auth.docker.io"
        );
        assert!(m.is_passthrough(
            "registry-1.docker.io",
            "/v2/library/nginx/manifests/latest",
            ""
        ));
        assert!(!m.is_passthrough("hub.docker.com", "/v2/repositories/library/nginx/", ""));
    }

    #[test]
    fn hf_rules() {
        let m = multi(&[("huggingface", Mode::Single, "hf.example.com")]);
        assert!(m.is_passthrough("huggingface.co", "/gpt2/resolve/main/config.json", ""));
        assert!(m.is_passthrough(
            "huggingface.co",
            "/api/resolve-cache/models/gpt2/abc/config.json",
            ""
        ));
        assert!(m.is_passthrough("us.aws.cdn.hf.co", "/xet-bridge-us/x", ""));
        assert!(!m.is_passthrough("huggingface.co", "/api/models/gpt2", ""));
        assert!(!m.is_passthrough("cas-server.xethub.hf.co", "/v1/reconstructions/x", ""));
        assert!(m.is_credential_host("cas-server.xethub.hf.co"));
        assert!(!m.is_credential_host("user-app.hf.space"));
    }

    #[test]
    fn ai_gateway_prefixes() {
        let m = multi(&[
            ("ai", Mode::Single, "ai.example.com"),
            ("github", Mode::Single, "gh.example.com"),
        ]);
        let a = "ai.example.com";
        assert_eq!(
            r(&m, a, "/openai/v1/chat/completions"),
            some("api.openai.com", "/v1/chat/completions")
        );
        assert_eq!(
            r(&m, a, "/anthropic/v1/messages?beta=true"),
            some("api.anthropic.com", "/v1/messages?beta=true")
        );
        assert_eq!(
            r(&m, a, "/gemini?x=1"),
            some("generativelanguage.googleapis.com", "/?x=1")
        );
        assert_eq!(r(&m, a, "/openai"), some("api.openai.com", "/"));
        // templated prefixes; the most specific template wins
        assert_eq!(
            r(
                &m,
                a,
                "/vertex-us-central1/v1/projects/p/locations/us-central1/x"
            ),
            some(
                "us-central1-aiplatform.googleapis.com",
                "/v1/projects/p/locations/us-central1/x"
            )
        );
        assert_eq!(
            r(&m, a, "/vertex/v1/x"),
            some("aiplatform.googleapis.com", "/v1/x")
        );
        assert_eq!(
            r(&m, a, "/azure-myres/openai/deployments"),
            some("myres.openai.azure.com", "/openai/deployments")
        );
        assert_eq!(
            r(&m, a, "/azure-cog-myres/x"),
            some("myres.cognitiveservices.azure.com", "/x")
        );
        assert_eq!(
            r(&m, a, "/bedrock-us-east-1/model/x/converse"),
            some(
                "bedrock-runtime.us-east-1.amazonaws.com",
                "/model/x/converse"
            )
        );
        assert_eq!(
            r(&m, a, "/bedrock-mantle-us-east-1/openai/v1"),
            some("bedrock-mantle.us-east-1.api.aws", "/openai/v1")
        );
        // a pure gateway has nothing at its root or under unknown prefixes
        assert_eq!(r(&m, a, "/"), None);
        assert_eq!(r(&m, a, "/nope/x"), None);
        assert_eq!(r(&m, a, "/vertex-bad.label/x"), None);
        // /__h/ still works for allowed hosts
        assert_eq!(r(&m, a, "/__h/api.x.ai/v1"), some("api.x.ai", "/v1"));
        // forward mapping uses the prefixes
        assert_eq!(
            m.public_origin("api.openai.com").unwrap(),
            "https://ai.example.com/openai"
        );
        assert_eq!(
            m.public_origin("europe-west4-aiplatform.googleapis.com")
                .unwrap(),
            "https://ai.example.com/vertex-europe-west4"
        );
        assert_eq!(
            m.public_origin("delivery-eu1.bfl.ai").unwrap(),
            "https://ai.example.com/__h/delivery-eu1.bfl.ai"
        );
        assert_eq!(m.public_bare_host("api.openai.com"), None);
        // reverse mapping
        assert_eq!(
            m.upstream_for_public(a, "/anthropic/v1", "/"),
            Some(("api.anthropic.com".into(), "/anthropic".len()))
        );
        assert_eq!(
            m.upstream_for_public(a, "%2Fopenai%2Fv1", "%2F"),
            Some(("api.openai.com".into(), "%2Fopenai".len()))
        );
        assert_eq!(m.upstream_for_public(a, "/nope", "/"), None);
    }

    #[test]
    fn ai_policies() {
        let m = multi(&[
            ("ai", Mode::Single, "ai.example.com"),
            ("huggingface", Mode::Single, "hf.example.com"),
        ]);
        let p = m.policy("ai.example.com", "api.openai.com").unwrap();
        assert_eq!(p.name, "openai");
        assert_eq!(p.read_timeout, Some(3600));
        assert!(p.is_credential_host("api.openai.com"));
        assert!(!p.rewrites_requests("api.openai.com", "/v1/chat/completions"));
        assert!(!p.rewrites_response("api.openai.com", "/v1/chat/completions", ""));
        assert!(p.rewrites_response("api.openai.com", "/v1/images/generations", ""));
        let blob = m
            .policy(
                "ai.example.com",
                "oaidalleapiprodscus.blob.core.windows.net",
            )
            .unwrap();
        assert!(!blob.is_credential_host("oaidalleapiprodscus.blob.core.windows.net"));
        let a = m.policy("ai.example.com", "api.anthropic.com").unwrap();
        assert!(a.rewrites_response("api.anthropic.com", "/v1/messages/batches/msgbatch_1", ""));
        assert!(!a.rewrites_response(
            "api.anthropic.com",
            "/v1/messages/batches/msgbatch_1/results",
            ""
        ));
        assert!(
            m.policy("ai.example.com", "us-central1-aiplatform.googleapis.com")
                .unwrap()
                .grpc
        );
        // router.huggingface.co follows the site the request came through
        assert_eq!(
            m.policy("ai.example.com", "router.huggingface.co")
                .unwrap()
                .name,
            "hf-router"
        );
        assert_eq!(
            m.policy("hf.example.com", "router.huggingface.co")
                .unwrap()
                .name,
            "huggingface"
        );
        // existing sites keep their behaviour
        let hf = m.policy("hf.example.com", "huggingface.co").unwrap();
        assert!(hf.rewrites_requests("huggingface.co", "/api/models"));
        assert!(hf.rewrites_response("huggingface.co", "/api/models", ""));
        assert_eq!(hf.read_timeout, None);
    }

    #[test]
    fn standalone_provider_site() {
        let m = multi(&[
            ("openai", Mode::Single, "openai.example.com"),
            ("vertex", Mode::Wildcard, "vertex.example.com"),
        ]);
        assert_eq!(
            r(&m, "openai.example.com", "/v1/models"),
            some("api.openai.com", "/v1/models")
        );
        assert_eq!(
            m.public_origin("api.openai.com").unwrap(),
            "https://openai.example.com"
        );
        // wildcard vertex: regional hosts get their own subdomain (needed for gRPC)
        assert_eq!(
            r(
                &m,
                "us-central1-aiplatform.vertex.example.com",
                "/google.cloud.aiplatform.v1.PredictionService/GenerateContent"
            ),
            some(
                "us-central1-aiplatform.googleapis.com",
                "/google.cloud.aiplatform.v1.PredictionService/GenerateContent"
            )
        );
        // URLs in responses use the path prefix form, which works for REST clients
        assert_eq!(
            m.public_origin("us-central1-aiplatform.googleapis.com")
                .unwrap(),
            "https://vertex.example.com/vertex-us-central1"
        );
    }
}
