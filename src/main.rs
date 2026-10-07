use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use civitai_proxy::access::{self, parse_net};
use civitai_proxy::config::Config;
use civitai_proxy::proxy::AppState;

#[derive(Parser)]
#[command(
    version,
    about = "Reverse proxy for Civitai (web, API and model downloads)"
)]
struct Cli {
    /// Config file (TOML). Missing file = defaults + CP_* environment variables.
    #[arg(
        short,
        long,
        env = "CP_CONFIG",
        default_value = "config.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the proxy (default).
    Serve,
    /// Add an IP or CIDR to the whitelist.
    Allow {
        ip: String,
        /// Lifetime in seconds (0 = permanent; default from config).
        #[arg(long)]
        ttl: Option<u64>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove an IP or CIDR from the whitelist.
    Deny { ip: String },
    /// List whitelist entries.
    List,
    /// Print the effective configuration.
    Config,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,civitai_proxy=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => civitai_proxy::serve(Arc::new(AppState::new(cfg)?)).await,
        Cmd::Allow { ip, ttl, note } => {
            let net = parse_net(&ip).ok_or_else(|| anyhow::anyhow!("invalid ip/cidr: {ip}"))?;
            let ttl = ttl.unwrap_or(cfg.access.default_ttl_secs);
            let mut q = vec![("ip", net.to_string()), ("ttl", ttl.to_string())];
            if let Some(n) = &note {
                q.push(("note", n.clone()));
            }
            admin_or_file(&cfg, "allow", &q, |entries| {
                let e = access::upsert(entries, net, ttl, note.clone());
                serde_json::json!({"ok": true, "entry": e})
            })
            .await
        }
        Cmd::Deny { ip } => {
            let net = parse_net(&ip).ok_or_else(|| anyhow::anyhow!("invalid ip/cidr: {ip}"))?;
            admin_or_file(&cfg, "deny", &[("ip", net.to_string())], |entries| {
                let before = entries.len();
                entries.retain(|e| e.net != net);
                serde_json::json!({"ok": true, "removed": entries.len() != before, "net": net})
            })
            .await
        }
        Cmd::List => {
            admin_or_file(&cfg, "list", &[], |entries| {
                let t = access::now();
                entries.retain(|e| e.expires_at.is_none_or(|x| x > t));
                serde_json::json!({"ok": true, "entries": entries})
            })
            .await
        }
        Cmd::Config => {
            println!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}

/// Talks to the running server's admin API so its in-memory whitelist stays in sync;
/// falls back to editing the whitelist file when the server is not reachable.
async fn admin_or_file(
    cfg: &Config,
    action: &str,
    query: &[(&str, String)],
    offline: impl FnOnce(&mut Vec<access::Entry>) -> serde_json::Value,
) -> anyhow::Result<()> {
    if !cfg.access.admin_token.is_empty() {
        let mut addr: SocketAddr = cfg.listen;
        if addr.ip().is_unspecified() {
            addr.set_ip(if addr.is_ipv4() {
                [127, 0, 0, 1].into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        let url = url::Url::parse_with_params(&format!("http://{addr}/__cp/{action}"), query)?;
        let res = client
            .get(url)
            .header("x-admin-token", &cfg.access.admin_token)
            .send()
            .await;
        match res {
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
                    Err(_) => println!("{body}"),
                }
                anyhow::ensure!(status.is_success(), "server returned {status}");
                return Ok(());
            }
            Err(e) if e.is_connect() => {
                eprintln!(
                    "server not reachable at {addr}, editing {} directly",
                    cfg.access.whitelist_file.display()
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    let path = &cfg.access.whitelist_file;
    let mut entries = access::load_entries(path);
    let out = offline(&mut entries);
    if action != "list" {
        access::save_entries(path, &entries)?;
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
