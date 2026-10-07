use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One public host; non-primary upstream hosts are encoded in the path (`/__h/<host>/...`).
    Single,
    /// `*.public_domain`; `<sub>.public_domain` maps to `<sub>.civitai.com`.
    Wildcard,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Local address to listen on (plain HTTP, TLS is terminated by Caddy/CDN).
    pub listen: SocketAddr,
    pub mode: Mode,
    /// Public domain, e.g. `civitai.example.com` (single) or `example.com` (wildcard).
    pub public_domain: String,
    /// Scheme seen by the end user (the front proxy terminates TLS).
    pub public_scheme: String,
    /// The primary upstream site.
    pub upstream_root: String,
    /// Upstream hosts the proxy is allowed to reach. `*.x` matches `x` and any subdomain of it.
    pub upstream_allow: Vec<String>,
    /// Wildcard mode: label of the host that carries path-encoded external hosts
    /// (`<ext_host>.<public_domain>/<upstream-host>/...`).
    pub ext_host: String,
    /// Wildcard mode: extra upstream hosts mapped to a dedicated public label,
    /// e.g. `"civitai.red" = "civitai-red"` -> `civitai-red.<public_domain>`.
    pub aliases: BTreeMap<String, String>,
    /// Outbound proxy for upstream HTTP requests (e.g. `http://127.0.0.1:7890`).
    pub upstream_proxy: Option<String>,
    pub connect_timeout_secs: u64,
    /// Idle read timeout for upstream responses.
    pub read_timeout_secs: u64,
    /// Responses with rewritable content types larger than this are passed through untouched.
    pub max_rewrite_bytes: usize,
    /// Request bodies (json/form/text) up to this size get public URLs mapped back to upstream.
    pub max_request_rewrite_bytes: usize,
    /// Rewrite bare host names (e.g. `civitai.com` without scheme) in bodies.
    pub rewrite_bare_hosts: bool,
    /// Debug/testing only: send requests for a host to another origin,
    /// e.g. `"civitai.com" = "http://127.0.0.1:9000"`. The Host header stays the upstream host.
    pub upstream_resolve: BTreeMap<String, String>,
    pub access: AccessConfig,
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
        Self {
            listen: "127.0.0.1:8787".parse().unwrap(),
            mode: Mode::Single,
            public_domain: "localhost:8787".into(),
            public_scheme: "https".into(),
            upstream_root: "civitai.com".into(),
            upstream_allow: vec![
                "*.civitai.com".into(),
                "*.civitai.green".into(),
                "*.civitai.red".into(),
                "*.r2.cloudflarestorage.com".into(),
                "*.backblazeb2.com".into(),
            ],
            ext_host: "ext".into(),
            aliases: BTreeMap::from([
                ("civitai.green".to_string(), "civitai-green".to_string()),
                ("civitai.red".to_string(), "civitai-red".to_string()),
            ]),
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
        Ok(cfg)
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
        Ok(())
    }

    fn normalize(&mut self) {
        self.public_domain = self
            .public_domain
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        self.upstream_root = self.upstream_root.trim().to_ascii_lowercase();
        self.public_scheme = self.public_scheme.trim().to_ascii_lowercase();
        for a in &mut self.upstream_allow {
            *a = a.trim().to_ascii_lowercase();
        }
    }
}
