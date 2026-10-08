//! gRPC pass-through: HTTP/2 end to end with request/response trailers preserved
//! (`grpc-status`, `grpc-message`). Inbound h2c comes from the front proxy (Caddy
//! `versions h2c 2`); upstream is HTTP/2 over TLS, optionally through an HTTP CONNECT proxy.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use http::header::{self, HeaderValue};
use http::{HeaderMap, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::TcpStream;
use tower_service::Service;

use crate::proxy::{AppState, upstream_headers};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Whether a request is gRPC (not gRPC-Web, which works over HTTP/1 as normal traffic).
pub fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            let ct = ct.to_ascii_lowercase();
            ct.starts_with("application/grpc") && !ct.starts_with("application/grpc-web")
        })
}

/// TCP connector that goes through an HTTP CONNECT proxy unless the host is in `no_proxy`.
#[derive(Clone)]
pub struct Connector {
    direct: HttpConnector,
    tunnel: Option<Tunnel<HttpConnector>>,
    no_proxy: Vec<String>,
}

impl Connector {
    fn bypass(&self, dst: &Uri) -> bool {
        let host = dst
            .host()
            .unwrap_or("")
            .trim_matches(['[', ']'])
            .to_ascii_lowercase();
        self.no_proxy.iter().any(|n| {
            let n = n.trim_start_matches("*.").trim_start_matches('.');
            n == "*" || host == n || host.ends_with(&format!(".{n}"))
        })
    }
}

impl Service<Uri> for Connector {
    type Response = TokioIo<TcpStream>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        match (&self.tunnel, self.bypass(&dst)) {
            (Some(t), false) => {
                let mut t = t.clone();
                Box::pin(async move { t.call(dst).await.map_err(Into::into) })
            }
            _ => {
                let mut d = self.direct.clone();
                Box::pin(async move { d.call(dst).await.map_err(Into::into) })
            }
        }
    }
}

pub type GrpcClient = Client<HttpsConnector<Connector>, Body>;

/// Builds the HTTP/2 client. `proxy` falls back to `HTTPS_PROXY` / `ALL_PROXY`.
pub fn client(proxy: Option<&str>, connect_timeout: Duration) -> anyhow::Result<GrpcClient> {
    let env = |k: &str| {
        std::env::var(k)
            .ok()
            .or_else(|| std::env::var(k.to_ascii_lowercase()).ok())
            .filter(|v| !v.is_empty())
    };
    let mut direct = HttpConnector::new();
    direct.enforce_http(false);
    direct.set_connect_timeout(Some(connect_timeout));
    direct.set_nodelay(true);
    let proxy = proxy
        .map(str::to_string)
        .or_else(|| env("HTTPS_PROXY"))
        .or_else(|| env("ALL_PROXY"));
    let tunnel = match proxy {
        Some(p) if p.starts_with("http://") => {
            let url = url::Url::parse(&p)?;
            let dst: Uri = format!(
                "http://{}:{}",
                url.host_str().unwrap_or_default(),
                url.port_or_known_default().unwrap_or(80)
            )
            .parse()?;
            let mut t = Tunnel::new(dst, direct.clone());
            if !url.username().is_empty() {
                use base64::Engine;
                let user = format!("{}:{}", url.username(), url.password().unwrap_or(""));
                let auth = base64::engine::general_purpose::STANDARD.encode(user);
                t = t.with_auth(HeaderValue::from_str(&format!("Basic {auth}"))?);
            }
            Some(t)
        }
        Some(p) => {
            tracing::warn!(
                "gRPC: unsupported proxy `{p}` (only http:// CONNECT proxies), connecting directly"
            );
            None
        }
        None => None,
    };
    let no_proxy = env("NO_PROXY")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http2()
        .wrap_connector(Connector {
            direct,
            tunnel,
            no_proxy,
        });
    Ok(Client::builder(TokioExecutor::new())
        .http2_only(true)
        .timer(TokioTimer::new())
        .http2_keep_alive_interval(Some(Duration::from_secs(30)))
        .build(https))
}

/// Forwards a gRPC call unchanged (body frames and trailers in both directions).
pub async fn forward(
    state: &AppState,
    req: Request,
    up_host: &str,
    up_pq: &str,
    credentials: bool,
) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();
    let uri: Uri = format!("{}{}", state.upstream_origin(up_host, false), up_pq).parse()?;
    let mut headers = upstream_headers(state, &parts.headers, up_host, credentials);
    // h2 carries the authority in the URI; gRPC requires `te: trailers`
    headers.remove(header::HOST);
    headers.insert(header::TE, HeaderValue::from_static("trailers"));
    let mut up = http::Request::builder()
        .method(parts.method)
        .uri(uri)
        .version(http::Version::HTTP_2)
        .body(body)?;
    *up.headers_mut() = headers;
    let resp = state.grpc_client().request(up).await?;
    let (mut parts, incoming) = resp.into_parts();
    for h in ["connection", "keep-alive", "transfer-encoding", "upgrade"] {
        parts.headers.remove(h);
    }
    Ok(Response::from_parts(parts, Body::new(incoming)))
}
