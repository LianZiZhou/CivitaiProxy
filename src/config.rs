use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One public host; non-primary upstream hosts are encoded in the path (`/__h/<host>/...`).
    #[default]
    Single,
    /// `*.public_domain`; `<sub>.public_domain` maps to `<sub>.<sub_root>`.
    Wildcard,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Local address to listen on (plain HTTP, TLS is terminated by Caddy/CDN).
    pub listen: SocketAddr,
    /// Scheme seen by the end user (the front proxy terminates TLS).
    pub public_scheme: String,
    /// Proxied sites. When empty, a single Civitai site is built from the legacy
    /// top-level fields below (`mode`, `public_domain`, `upstream_*`, `ext_host`, `aliases`).
    pub sites: Vec<SiteConfig>,

    // --- legacy single-site (Civitai) settings ---
    pub mode: Mode,
    pub public_domain: String,
    pub upstream_root: String,
    pub upstream_allow: Vec<String>,
    pub ext_host: String,
    pub aliases: BTreeMap<String, String>,

    /// Outbound proxy for upstream HTTP requests (e.g. `http://127.0.0.1:7890`).
    pub upstream_proxy: Option<String>,
    pub connect_timeout_secs: u64,
    /// Idle read timeout for upstream responses.
    pub read_timeout_secs: u64,
    /// Responses with rewritable content types larger than this are passed through untouched.
    pub max_rewrite_bytes: usize,
    /// Request bodies (json/form) up to this size get public URLs mapped back to upstream.
    pub max_request_rewrite_bytes: usize,
    /// Rewrite bare host names (e.g. `civitai.com` without scheme) in bodies.
    pub rewrite_bare_hosts: bool,
    /// Debug/testing only: send requests for a host to another origin,
    /// e.g. `"civitai.com" = "http://127.0.0.1:9000"`. The Host header stays the upstream host.
    pub upstream_resolve: BTreeMap<String, String>,
    pub access: AccessConfig,
}

/// One proxied site. Unset optional fields come from the `preset`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SiteConfig {
    /// `civitai`, `huggingface`, `github`, `ghcr` or `dockerhub`; empty = fully custom.
    pub preset: Option<String>,
    /// Display name (defaults to the preset name).
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub mode: Mode,
    /// Public domain of this site, e.g. `hf.example.com`. In wildcard mode `*.<public_domain>`
    /// must also resolve to the proxy.
    pub public_domain: String,
    /// Upstream host served at the root of `public_domain`.
    pub root: Option<String>,
    /// Wildcard mode: `<label>.<public_domain>` maps to `<label>.<sub_root>` (defaults to `root`).
    pub sub_root: Option<String>,
    /// Upstream hosts this site may reach. `*.x` = `x` and its subdomains; `*` elsewhere is a glob.
    pub allow: Option<Vec<String>>,
    /// Wildcard mode: label of the host carrying path-encoded hosts (`ext.<public_domain>/<host>/...`).
    pub ext_host: Option<String>,
    /// Wildcard mode: `"civitai.red" = "civitai-red"` maps a host to `civitai-red.<public_domain>`;
    /// `"*.hf.space" = "space"` maps `x.hf.space` to `x--space.<public_domain>`.
    pub aliases: Option<BTreeMap<String, String>>,
    /// Requests on the root public host whose path matches `path` (regex) go to `host`.
    pub routes: Option<Vec<RouteConfig>>,
    /// Hosts that receive the client's `Cookie` and `Authorization` headers.
    pub credential_hosts: Option<Vec<String>>,
    /// Responses from these hosts are never rewritten.
    pub passthrough_hosts: Option<Vec<String>>,
    /// Responses for paths matching these regexes are never rewritten (file downloads etc).
    pub passthrough_paths: Option<Vec<String>>,
    /// Responses are not rewritten when the request `Accept` header contains one of these.
    pub passthrough_accept: Option<Vec<String>>,
    /// Set to false to never rewrite bodies of this site (headers are still rewritten).
    pub rewrite_bodies: Option<bool>,
    /// Docker registry: `/v2/<name>/...` -> `/v2/library/<name>/...` for single-segment names.
    pub docker_library: Option<bool>,
    /// Cookie name hint shown on the `/__cp/login` import page.
    pub session_cookie: Option<String>,
    /// Response headers of a redirect that are re-attached to the response of its (proxied)
    /// target. Clients that follow same-host redirects (single mode) still see them.
    pub carry_headers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteConfig {
    pub path: String,
    pub host: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AccessConfig {
    /// When enabled, only whitelisted client IPs may use the proxy.
    pub enabled: bool,
    /// Token for `/__cp/*` admin endpoints and the CLI. Admin endpoints are disabled when empty.
    pub admin_token: String,
    pub whitelist_file: PathBuf,
    /// Default lifetime (seconds) of entries added via `/__cp/allow`. 0 = never expire.
    pub default_ttl_secs: u64,
    /// Only peers in these networks are trusted to supply the client IP header.
    pub trusted_proxies: Vec<IpNet>,
    /// Header carrying the real client IP (`X-Forwarded-For`, `CF-Connecting-IP`, `X-Real-IP`).
    pub client_ip_header: String,
}

impl Default for Config {
    fn default() -> Self {
        let civitai = preset("civitai").expect("civitai preset");
        Self {
            listen: "127.0.0.1:8787".parse().unwrap(),
            public_scheme: "https".into(),
            sites: Vec::new(),
            mode: Mode::Single,
            public_domain: "localhost:8787".into(),
            upstream_root: civitai.root.clone().unwrap(),
            upstream_allow: civitai.allow.clone().unwrap(),
            ext_host: "ext".into(),
            aliases: civitai.aliases.clone().unwrap(),
            upstream_proxy: None,
            connect_timeout_secs: 15,
            read_timeout_secs: 300,
            max_rewrite_bytes: 32 * 1024 * 1024,
            max_request_rewrite_bytes: 4 * 1024 * 1024,
            rewrite_bare_hosts: true,
            upstream_resolve: BTreeMap::new(),
            access: AccessConfig::default(),
        }
    }
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            admin_token: String::new(),
            whitelist_file: "whitelist.json".into(),
            default_ttl_secs: 0,
            trusted_proxies: vec!["127.0.0.1/32".parse().unwrap(), "::1/128".parse().unwrap()],
            client_ip_header: "X-Forwarded-For".into(),
        }
    }
}

fn strs(v: &[&str]) -> Option<Vec<String>> {
    Some(v.iter().map(|s| s.to_string()).collect())
}

fn map(v: &[(&str, &str)]) -> Option<BTreeMap<String, String>> {
    Some(
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

/// Built-in site definitions.
pub fn preset(name: &str) -> Option<SiteConfig> {
    let s = match name {
        "civitai" => SiteConfig {
            root: Some("civitai.com".into()),
            allow: strs(&[
                "*.civitai.com",
                "*.civitai.green",
                "*.civitai.red",
                "*.r2.cloudflarestorage.com",
                "*.backblazeb2.com",
            ]),
            aliases: map(&[
                ("civitai.green", "civitai-green"),
                ("civitai.red", "civitai-red"),
            ]),
            credential_hosts: strs(&["*.civitai.com", "*.civitai.green", "*.civitai.red"]),
            passthrough_hosts: strs(&["*.r2.cloudflarestorage.com", "*.backblazeb2.com"]),
            session_cookie: Some("__Secure-civitai-token".into()),
            ..Default::default()
        },
        "huggingface" => SiteConfig {
            root: Some("huggingface.co".into()),
            allow: strs(&["*.huggingface.co", "*.hf.co", "*.hf.space"]),
            aliases: map(&[("*.hf.space", "space")]),
            credential_hosts: strs(&[
                "huggingface.co",
                "*.huggingface.co",
                "hf.co",
                "cas-server.xethub.hf.co",
            ]),
            passthrough_hosts: strs(&[
                "*.cdn.hf.co",
                "cdn-lfs*.hf.co",
                "cdn-lfs*.huggingface.co",
                "cas-bridge.xethub.hf.co",
                "transfer.xethub.hf.co",
            ]),
            // file contents: /<repo>/resolve/<rev>/<file>, /<repo>/raw/..., resolve-cache redirects
            passthrough_paths: strs(&[
                r"/(resolve|raw)/",
                r"^/api/resolve-cache/",
                r"\.git/info/lfs/objects/[0-9a-f]{64}",
            ]),
            session_cookie: Some("token".into()),
            // huggingface_hub follows same-host redirects and reads file metadata from the final response
            carry_headers: strs(&[
                "x-repo-commit",
                "x-linked-etag",
                "x-linked-size",
                "x-xet-hash",
                "link",
            ]),
            ..Default::default()
        },
        "github" => SiteConfig {
            root: Some("github.com".into()),
            allow: strs(&[
                "*.github.com",
                "*.githubusercontent.com",
                "*.githubassets.com",
                "*.github.io",
                "*.githubapp.com",
            ]),
            aliases: map(&[("*.github.io", "io")]),
            credential_hosts: strs(&[
                "*.github.com",
                "raw.githubusercontent.com",
                "gist.githubusercontent.com",
            ]),
            // raw content, archives, release assets; static assets keep their SRI hashes valid
            passthrough_hosts: strs(&[
                "*.githubusercontent.com",
                "codeload.github.com",
                "*.githubassets.com",
            ]),
            passthrough_paths: strs(&[r"\.(patch|diff)$", r"/(raw|archive)/"]),
            passthrough_accept: strs(&[".raw", ".diff", ".patch"]),
            session_cookie: Some("user_session; __Host-user_session_same_site".into()),
            ..Default::default()
        },
        "ghcr" => SiteConfig {
            root: Some("ghcr.io".into()),
            allow: strs(&["ghcr.io", "pkg-containers.githubusercontent.com"]),
            credential_hosts: strs(&["ghcr.io"]),
            rewrite_bodies: Some(false),
            docker_library: Some(false),
            ..Default::default()
        },
        "dockerhub" => SiteConfig {
            root: Some("hub.docker.com".into()),
            sub_root: Some("docker.com".into()),
            allow: strs(&["*.docker.com", "*.docker.io", "*.dockerstatic.com"]),
            // the registry API lives on the same public host as the Hub web UI
            routes: Some(vec![RouteConfig {
                path: r"^/v2/?$|^/v2/_catalog|^/v2/.+/(manifests|blobs|tags/list|referrers)(/|$)"
                    .into(),
                host: "registry-1.docker.io".into(),
            }]),
            credential_hosts: strs(&["*.docker.io", "*.docker.com"]),
            passthrough_hosts: strs(&[
                "registry-1.docker.io",
                "*.cloudflare.docker.com",
                "*.docker.io",
            ]),
            docker_library: Some(true),
            ..Default::default()
        },
        _ => return None,
    };
    Some(s)
}

impl SiteConfig {
    /// Fills unset fields from the preset.
    pub fn resolved(&self) -> anyhow::Result<SiteConfig> {
        let base = match &self.preset {
            Some(p) => preset(p).ok_or_else(|| anyhow::anyhow!("unknown site preset `{p}`"))?,
            None => SiteConfig::default(),
        };
        macro_rules! pick {
            ($($f:ident),*) => { SiteConfig { $($f: self.$f.clone().or(base.$f),)* mode: self.mode, public_domain: self.public_domain.clone() } };
        }
        let mut s = pick!(
            preset,
            name,
            enabled,
            root,
            sub_root,
            allow,
            ext_host,
            aliases,
            routes,
            credential_hosts,
            passthrough_hosts,
            passthrough_paths,
            passthrough_accept,
            rewrite_bodies,
            docker_library,
            session_cookie,
            carry_headers
        );
        if s.root.is_none() {
            anyhow::bail!("site `{}` needs `root` (or a preset)", self.public_domain);
        }
        if s.public_domain.is_empty() {
            anyhow::bail!(
                "site `{}` needs `public_domain`",
                s.root.as_deref().unwrap_or("")
            );
        }
        s.name = s.name.or(s.preset.clone()).or(s.root.clone());
        Ok(s)
    }
}

impl Config {
    /// Loads the config file (if it exists) and applies `CP_*` environment overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg: Config = if path.exists() {
            let text = std::fs::read_to_string(path)?;
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?
        } else {
            Config::default()
        };
        cfg.apply_env()?;
        cfg.normalize();
        cfg.site_configs()?;
        Ok(cfg)
    }

    /// Effective, preset-resolved site list.
    pub fn site_configs(&self) -> anyhow::Result<Vec<SiteConfig>> {
        if self.sites.is_empty() {
            let legacy = SiteConfig {
                preset: Some("civitai".into()),
                mode: self.mode,
                public_domain: self.public_domain.clone(),
                root: Some(self.upstream_root.clone()),
                allow: Some(self.upstream_allow.clone()),
                ext_host: Some(self.ext_host.clone()),
                aliases: Some(self.aliases.clone()),
                ..Default::default()
            };
            return Ok(vec![legacy.resolved()?]);
        }
        let mut out = Vec::new();
        for s in &self.sites {
            let s = s.resolved()?;
            if s.enabled != Some(false) {
                out.push(s);
            }
        }
        anyhow::ensure!(!out.is_empty(), "no enabled sites");
        Ok(out)
    }

    fn apply_env(&mut self) -> anyhow::Result<()> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("CP_LISTEN") {
            self.listen = v.parse()?;
        }
        if let Some(v) = env("CP_MODE") {
            self.mode = match v.as_str() {
                "single" => Mode::Single,
                "wildcard" => Mode::Wildcard,
                _ => anyhow::bail!("CP_MODE must be `single` or `wildcard`"),
            };
        }
        if let Some(v) = env("CP_PUBLIC_DOMAIN") {
            self.public_domain = v;
        }
        if let Some(v) = env("CP_PUBLIC_SCHEME") {
            self.public_scheme = v;
        }
        if let Some(v) = env("CP_UPSTREAM_PROXY") {
            self.upstream_proxy = Some(v);
        }
        if let Some(v) = env("CP_ACCESS_ENABLED") {
            self.access.enabled = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = env("CP_ADMIN_TOKEN") {
            self.access.admin_token = v;
        }
        if let Some(v) = env("CP_WHITELIST_FILE") {
            self.access.whitelist_file = v.into();
        }
        // CP_SITES="civitai=civitai.example.com,huggingface=hf.example.com,dockerhub=docker.example.com"
        // (append `:wildcard` to a domain for wildcard mode)
        if let Some(v) = env("CP_SITES") {
            self.sites.clear();
            for item in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let (p, d) = item.split_once('=').ok_or_else(|| {
                    anyhow::anyhow!("CP_SITES item `{item}` must be preset=domain")
                })?;
                let (d, mode) = match d.strip_suffix(":wildcard") {
                    Some(d) => (d, Mode::Wildcard),
                    None => (d, Mode::Single),
                };
                self.sites.push(SiteConfig {
                    preset: Some(p.into()),
                    public_domain: d.into(),
                    mode,
                    ..Default::default()
                });
            }
        }
        Ok(())
    }

    fn normalize(&mut self) {
        let norm = |s: &str| s.trim().trim_end_matches('.').to_ascii_lowercase();
        self.public_domain = norm(&self.public_domain);
        self.upstream_root = norm(&self.upstream_root);
        self.public_scheme = self.public_scheme.trim().to_ascii_lowercase();
        for a in &mut self.upstream_allow {
            *a = norm(a);
        }
        for s in &mut self.sites {
            s.public_domain = norm(&s.public_domain);
        }
    }
}
