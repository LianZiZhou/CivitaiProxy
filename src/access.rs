//! Optional client IP whitelist, persisted to a JSON file.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use http::HeaderMap;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::config::AccessConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub net: IpNet,
    /// Unix timestamp (seconds) after which the entry is ignored. `None` = permanent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub added_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Entry {
    fn alive(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|t| t > now)
    }
}

pub struct Access {
    pub cfg: AccessConfig,
    entries: RwLock<Vec<Entry>>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parses `1.2.3.4`, `1.2.3.0/24`, `::1` or `2001:db8::/32`.
pub fn parse_net(s: &str) -> Option<IpNet> {
    let s = s.trim();
    s.parse::<IpNet>()
        .ok()
        .or_else(|| {
            s.parse::<IpAddr>()
                .ok()
                .map(|ip| IpNet::from(normalize(ip)))
        })
        .map(|n| n.trunc())
}

/// Treats IPv4-mapped IPv6 addresses as IPv4.
pub fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

pub fn load_entries(path: &Path) -> Vec<Entry> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable whitelist {}: {e}", path.display());
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

pub fn save_entries(path: &Path, entries: &[Entry]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp: PathBuf = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(entries).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

/// Inserts or replaces the entry for `net` in `entries`.
pub fn upsert(entries: &mut Vec<Entry>, net: IpNet, ttl_secs: u64, note: Option<String>) -> Entry {
    let t = now();
    let entry = Entry {
        net,
        expires_at: (ttl_secs > 0).then(|| t + ttl_secs),
        added_at: t,
        note: note.filter(|n| !n.is_empty()),
    };
    entries.retain(|e| e.net != net && e.alive(t));
    entries.push(entry.clone());
    entry
}

impl Access {
    pub fn new(cfg: AccessConfig) -> Self {
        let entries = load_entries(&cfg.whitelist_file);
        Self {
            cfg,
            entries: RwLock::new(entries),
        }
    }

    pub fn admin_enabled(&self) -> bool {
        !self.cfg.admin_token.is_empty()
    }

    pub fn check_token(&self, token: &str) -> bool {
        let a = self.cfg.admin_token.as_bytes();
        let b = token.as_bytes();
        // constant-time comparison
        !a.is_empty()
            && a.len() == b.len()
            && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    pub fn is_allowed(&self, ip: IpAddr) -> bool {
        if !self.cfg.enabled {
            return true;
        }
        let ip = normalize(ip);
        let t = now();
        self.entries
            .read()
            .unwrap()
            .iter()
            .any(|e| e.alive(t) && e.net.contains(&ip))
    }

    /// Determines the real client IP. The configured header is trusted only when the TCP peer
    /// is one of `trusted_proxies`; for `X-Forwarded-For` the right-most untrusted hop is used.
    pub fn client_ip(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        let peer_ip = normalize(peer.ip());
        let trusted = |ip: &IpAddr| self.cfg.trusted_proxies.iter().any(|n| n.contains(ip));
        if !trusted(&peer_ip) {
            return peer_ip;
        }
        let name = self.cfg.client_ip_header.as_str();
        let values: Vec<IpAddr> = headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|s| s.trim().parse::<IpAddr>().ok())
            .map(normalize)
            .collect();
        if name.eq_ignore_ascii_case("x-forwarded-for") {
            values
                .iter()
                .rev()
                .find(|ip| !trusted(ip))
                .or(values.first())
                .copied()
                .unwrap_or(peer_ip)
        } else {
            values.first().copied().unwrap_or(peer_ip)
        }
    }

    pub fn add(&self, net: IpNet, ttl_secs: u64, note: Option<String>) -> std::io::Result<Entry> {
        let mut entries = self.entries.write().unwrap();
        let e = upsert(&mut entries, net, ttl_secs, note);
        save_entries(&self.cfg.whitelist_file, &entries)?;
        Ok(e)
    }

    pub fn remove(&self, net: IpNet) -> std::io::Result<bool> {
        let mut entries = self.entries.write().unwrap();
        let before = entries.len();
        entries.retain(|e| e.net != net);
        let removed = entries.len() != before;
        if removed {
            save_entries(&self.cfg.whitelist_file, &entries)?;
        }
        Ok(removed)
    }

    pub fn list(&self) -> Vec<Entry> {
        let t = now();
        self.entries
            .read()
            .unwrap()
            .iter()
            .filter(|e| e.alive(t))
            .cloned()
            .collect()
    }

    /// Drops expired entries (called periodically).
    pub fn purge_expired(&self) {
        let t = now();
        let mut entries = self.entries.write().unwrap();
        let before = entries.len();
        entries.retain(|e| e.alive(t));
        if entries.len() != before
            && let Err(e) = save_entries(&self.cfg.whitelist_file, &entries)
        {
            tracing::warn!("saving whitelist: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(dir: &Path) -> Access {
        Access::new(AccessConfig {
            enabled: true,
            admin_token: "secret".into(),
            whitelist_file: dir.join("wl.json"),
            ..AccessConfig::default()
        })
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn whitelist_cidr_ttl_persist() {
        let dir = tmpdir("wl");
        let a = access(&dir);
        let ip: IpAddr = "10.1.2.3".parse().unwrap();
        assert!(!a.is_allowed(ip));
        a.add(parse_net("10.1.0.0/16").unwrap(), 0, None).unwrap();
        assert!(a.is_allowed(ip));
        assert!(a.is_allowed("::ffff:10.1.9.9".parse().unwrap()));
        assert!(!a.is_allowed("10.2.0.1".parse().unwrap()));
        // persisted
        let b = access(&dir);
        assert!(b.is_allowed(ip));
        assert!(b.remove(parse_net("10.1.0.0/16").unwrap()).unwrap());
        assert!(!b.is_allowed(ip));

        let mut v = Vec::new();
        upsert(&mut v, parse_net("1.1.1.1").unwrap(), 100, None);
        v[0].expires_at = Some(now() - 1);
        assert!(!v[0].alive(now()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_ip_resolution() {
        let a = access(&tmpdir("ip"));
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.5, 127.0.0.1".parse().unwrap());
        let local: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let remote: SocketAddr = "198.51.100.1:5555".parse().unwrap();
        assert_eq!(
            a.client_ip(local, &h),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
        // header from an untrusted peer is ignored
        assert_eq!(
            a.client_ip(remote, &h),
            "198.51.100.1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            a.client_ip(local, &HeaderMap::new()),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn token() {
        let a = access(&tmpdir("tok"));
        assert!(a.check_token("secret"));
        assert!(!a.check_token("secreT"));
        assert!(!a.check_token(""));
    }
}
