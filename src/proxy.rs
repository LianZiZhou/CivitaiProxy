//! Generic HTTP forwarding with header, cookie, redirect and body rewriting.

use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, stream};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};

use crate::access::Access;
use crate::config::{Config, Mode};
use crate::mapping::Mapper;
use crate::rewrite::{Rewriter, is_rewritable, is_rewritable_request, strip_integrity};

pub struct AppState {
    pub cfg: Config,
    pub rewriter: Rewriter,
    pub client: reqwest::Client,
    pub access: Access,
    /// Redirect metadata to re-attach on the redirect target, keyed by public path.
    carry: Mutex<HashMap<String, CarryEntry>>,
}

/// When a carried header set was stored, and the headers.
type CarryEntry = (Instant, Vec<(HeaderName, HeaderValue)>);

const CARRY_TTL: Duration = Duration::from_secs(600);
const CARRY_MAX: usize = 4096;

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(cfg: Config) -> anyhow::Result<Self> {
        let mapper = Arc::new(Mapper::new(&cfg)?);
        let rewriter = Rewriter::new(mapper, cfg.rewrite_bare_hosts);
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .read_timeout(Duration::from_secs(cfg.read_timeout_secs))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60));
        if let Some(p) = &cfg.upstream_proxy {
            builder = builder.proxy(reqwest::Proxy::all(p)?);
        }
        let client = builder.build()?;
        let access = Access::new(cfg.access.clone());
        Ok(Self {
            cfg,
            rewriter,
            client,
            access,
            carry: Mutex::new(HashMap::new()),
        })
    }

    pub fn mapper(&self) -> &Mapper {
        self.rewriter.mapper()
    }

    /// Origin (scheme://authority) used to reach an upstream host.
    pub fn upstream_origin(&self, host: &str, ws: bool) -> String {
        if let Some(o) = self.cfg.upstream_resolve.get(host) {
            let o = o.trim_end_matches('/');
            if ws {
                return o
                    .replacen("https://", "wss://", 1)
                    .replacen("http://", "ws://", 1);
            }
            return o.to_string();
        }
        format!("{}://{host}", if ws { "wss" } else { "https" })
    }
}

/// Request headers never forwarded upstream.
const DROP_REQUEST: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "forwarded",
    "via",
    "x-real-ip",
    "true-client-ip",
    "cdn-loop",
    "content-length",
    "accept-encoding",
];

/// Response headers never returned to the client.
const DROP_RESPONSE: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "trailer",
    "content-security-policy",
    "content-security-policy-report-only",
    "strict-transport-security",
    "alt-svc",
    "report-to",
    "reporting-endpoints",
    "nel",
    "server-timing",
    "cf-ray",
    "cf-cache-status",
    "expect-ct",
];

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        msg.into(),
    )
        .into_response()
}

pub fn request_host(headers: &HeaderMap, uri: &http::Uri) -> String {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub async fn handle(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let host = request_host(req.headers(), req.uri());
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let Some((up_host, up_pq)) = state.mapper().route(&host, &pq) else {
        return error(
            StatusCode::NOT_FOUND,
            format!("civitai-proxy: no upstream for {host}{pq}\n"),
        );
    };
    let up_pq = rewrite_query(&state, &up_pq);

    let is_ws = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_ws {
        let (mut parts, _body) = req.into_parts();
        return match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(ws) => crate::ws::proxy(state.clone(), ws, parts.headers, up_host, up_pq).await,
            Err(e) => e.into_response(),
        };
    }

    let method = req.method().clone();
    match forward(&state, req, &up_host, &up_pq, &pq).await {
        Ok(r) => {
            tracing::debug!(%peer, "{method} {host}{pq} -> {up_host}{up_pq} {}", r.status().as_u16());
            r
        }
        Err(e) => {
            tracing::warn!(%peer, "upstream {up_host}{up_pq}: {e:#}");
            error(
                StatusCode::BAD_GATEWAY,
                format!("civitai-proxy: upstream error: {e}\n"),
            )
        }
    }
}

/// Maps public URLs in the query string back to upstream form.
fn rewrite_query(state: &AppState, pq: &str) -> String {
    match pq.split_once('?') {
        Some((p, q)) => format!("{p}?{}", state.rewriter.to_upstream_string(q)),
        None => pq.to_string(),
    }
}

/// Query parameters that mark a presigned URL (S3, GCS, Azure SAS, CloudFront, Cloudflare).
const SIGNATURE_PARAMS: &[&str] = &[
    "x-amz-signature",
    "x-goog-signature",
    "signature",
    "sig",
    "key-pair-id",
    "verify",
];

/// Whether the URL carries its own authorization; extra credentials would be rejected.
pub fn is_presigned(pq: &str) -> bool {
    let Some((_, q)) = pq.split_once('?') else {
        return false;
    };
    q.split('&').any(|kv| {
        let k = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
        SIGNATURE_PARAMS.contains(&k.as_str())
    })
}

/// Builds the header map sent upstream.
pub fn upstream_headers(
    state: &AppState,
    src: &HeaderMap,
    up_host: &str,
    up_pq: &str,
) -> HeaderMap {
    let credentials = state.mapper().is_credential_host(up_host) && !is_presigned(up_pq);
    let mut out = HeaderMap::new();
    for (name, value) in src {
        let n = name.as_str();
        if DROP_REQUEST.contains(&n)
            || n.starts_with("x-forwarded-")
            || n.starts_with("cf-")
            || n.starts_with("sec-websocket-")
        {
            continue;
        }
        // Credentials only go to the site's own hosts; presigned storage URLs reject extra auth.
        if !credentials && (n == "cookie" || n == "authorization") {
            continue;
        }
        if n == "origin" || n == "referer" {
            if let Ok(v) = value.to_str()
                && let Ok(v) = HeaderValue::from_str(&state.rewriter.to_upstream_string(v))
            {
                out.append(name.clone(), v);
            }
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    // Only encodings we can decode for rewriting.
    let ae = src
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(|s| s.trim())
                .filter(|s| {
                    let enc = s
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase();
                    matches!(enc.as_str(), "gzip" | "br" | "deflate" | "identity")
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    if !ae.is_empty() {
        out.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_str(&ae).unwrap_or(HeaderValue::from_static("gzip")),
        );
    }
    if state.cfg.upstream_resolve.contains_key(up_host)
        && let Ok(v) = HeaderValue::from_str(up_host)
    {
        out.insert(header::HOST, v);
    }
    out
}

async fn forward(
    state: &SharedState,
    req: Request,
    up_host: &str,
    up_pq: &str,
    public_pq: &str,
) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();
    let url = format!("{}{}", state.upstream_origin(up_host, false), up_pq);
    let mut headers = upstream_headers(state, &parts.headers, up_host, up_pq);

    let content_length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let chunked = parts.headers.contains_key(header::TRANSFER_ENCODING);
    let req_ct = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let has_body = chunked || content_length.is_some_and(|l| l > 0);

    let mut rb = state.client.request(parts.method.clone(), &url);
    if has_body {
        let small =
            content_length.is_some_and(|l| l as usize <= state.cfg.max_request_rewrite_bytes);
        let encoded = parts.headers.contains_key(header::CONTENT_ENCODING);
        if small && !encoded && is_rewritable_request(req_ct) && !is_presigned(up_pq) {
            let bytes = axum::body::to_bytes(body, state.cfg.max_request_rewrite_bytes).await?;
            let bytes = state
                .rewriter
                .to_upstream(&bytes)
                .map(Bytes::from)
                .unwrap_or(bytes);
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
            rb = rb.body(bytes);
        } else {
            if let Some(l) = content_length {
                headers.insert(header::CONTENT_LENGTH, HeaderValue::from(l));
            }
            rb = rb.body(reqwest::Body::wrap_stream(body.into_data_stream()));
        }
    }
    let resp = rb.headers(headers).send().await?;
    let accept = parts
        .headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut resp = build_response(state, &parts.method, up_host, up_pq, accept, resp).await;
    carry_over(state, up_host, public_pq, &mut resp);
    Ok(resp)
}

async fn build_response(
    state: &AppState,
    method: &Method,
    up_host: &str,
    up_pq: &str,
    accept: &str,
    resp: reqwest::Response,
) -> Response {
    let up_url = format!("{up_host}{up_pq}");
    let status = resp.status();
    let mut headers = HeaderMap::new();
    for (name, value) in resp.headers() {
        let n = name.as_str();
        if DROP_RESPONSE.contains(&n) {
            continue;
        }
        if n == "set-cookie" {
            if let Some(v) = value.to_str().ok().map(|v| rewrite_set_cookie(state, v))
                && let Ok(v) = HeaderValue::from_str(&v)
            {
                headers.append(name.clone(), v);
            }
            continue;
        }
        if n == "location"
            && let Some(v) = value
                .to_str()
                .ok()
                .and_then(|v| rewrite_location(state, up_host, up_pq, v))
        {
            if let Ok(v) = HeaderValue::from_str(&v) {
                headers.append(name.clone(), v);
            }
            continue;
        }
        // Any other header carrying a URL (Link, WWW-Authenticate realm, X-Xet-Cas-Url, ...)
        if let Ok(v) = value.to_str()
            && (v.contains("://") || v.contains("%3A%2F%2F") || v.contains("%3a%2f%2f"))
        {
            if let Ok(v) = HeaderValue::from_str(&state.rewriter.to_public_string(v)) {
                headers.append(name.clone(), v);
            }
            continue;
        }
        headers.append(name.clone(), value.clone());
    }

    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let encoding = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase();
    let length = resp.content_length();
    let no_body = *method == Method::HEAD
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || status.is_informational();
    let path = up_pq.split('?').next().unwrap_or(up_pq);
    let rewrite = !no_body
        && is_rewritable(&ct)
        && !state.mapper().is_passthrough(up_host, path, accept)
        && matches!(
            encoding.as_str(),
            "identity" | "gzip" | "x-gzip" | "deflate" | "br"
        )
        && length.is_none_or(|l| l as usize <= state.cfg.max_rewrite_bytes);

    if no_body {
        return with_headers(Response::new(Body::empty()), status, headers);
    }
    if !rewrite {
        let body = Body::from_stream(resp.bytes_stream());
        return with_headers(Response::new(body), status, headers);
    }

    // Buffer (bounded), decode, rewrite.
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(length.unwrap_or(64 * 1024) as usize);
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => {
                buf.extend_from_slice(&c);
                if buf.len() > state.cfg.max_rewrite_bytes {
                    // Too large: hand over what we have plus the rest, untouched.
                    let head =
                        stream::once(async move { Ok::<Bytes, reqwest::Error>(Bytes::from(buf)) });
                    let body = Body::from_stream(head.chain(stream));
                    return with_headers(Response::new(body), status, headers);
                }
            }
            Err(e) => {
                tracing::warn!("reading upstream body {up_url}: {e}");
                return error(
                    StatusCode::BAD_GATEWAY,
                    format!("civitai-proxy: upstream body error: {e}\n"),
                );
            }
        }
    }
    let decoded = match decode(&encoding, buf) {
        Ok(d) => d,
        Err((raw, e)) => {
            tracing::warn!("decoding {encoding} body from {up_url}: {e}");
            return with_headers(Response::new(Body::from(raw)), status, headers);
        }
    };
    let mut body = state.rewriter.to_public(&decoded).unwrap_or(decoded);
    if ct.starts_with("text/html")
        && let Some(b) = strip_integrity(&body)
    {
        body = b;
    }
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::ETAG);
    headers.remove("content-md5");
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    with_headers(Response::new(Body::from(body)), status, headers)
}

fn path_of(pq: &str) -> &str {
    pq.split('?').next().unwrap_or(pq)
}

/// Remembers selected headers of a redirect and re-attaches them to the response of its target.
fn carry_over(state: &AppState, up_host: &str, public_pq: &str, resp: &mut Response) {
    let mut carry = state.carry.lock().unwrap();
    if let Some((at, hs)) = carry.get(path_of(public_pq))
        && at.elapsed() < CARRY_TTL
    {
        for (k, v) in hs {
            if !resp.headers().contains_key(k) {
                resp.headers_mut().insert(k.clone(), v.clone());
            }
        }
    }
    let Some(site) = state.mapper().site_for_upstream(up_host) else {
        return;
    };
    if site.carry_headers.is_empty() || !resp.status().is_redirection() {
        return;
    }
    let Some(loc) = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
    else {
        return;
    };
    let Ok(loc) = url::Url::parse(loc) else {
        return;
    };
    let hs: Vec<(HeaderName, HeaderValue)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| site.carry_headers.iter().any(|c| c == k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if hs.is_empty() {
        return;
    }
    if carry.len() >= CARRY_MAX {
        carry.retain(|_, (at, _)| at.elapsed() < CARRY_TTL);
        if carry.len() >= CARRY_MAX {
            carry.clear();
        }
    }
    carry.insert(loc.path().to_string(), (Instant::now(), hs));
}

fn with_headers(mut resp: Response, status: StatusCode, headers: HeaderMap) -> Response {
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

/// Decodes a content-encoded body. On failure the raw bytes are handed back.
fn decode(encoding: &str, raw: Vec<u8>) -> Result<Vec<u8>, (Vec<u8>, std::io::Error)> {
    let mut out = Vec::with_capacity(raw.len() * 3);
    let res = match encoding {
        "identity" => return Ok(raw),
        "gzip" | "x-gzip" => flate2::read::MultiGzDecoder::new(&raw[..]).read_to_end(&mut out),
        "deflate" => flate2::read::ZlibDecoder::new(&raw[..])
            .read_to_end(&mut out)
            .or_else(|_| {
                out.clear();
                flate2::read::DeflateDecoder::new(&raw[..]).read_to_end(&mut out)
            }),
        "br" => brotli::Decompressor::new(&raw[..], 64 * 1024).read_to_end(&mut out),
        _ => return Err((raw, std::io::Error::other("unsupported encoding"))),
    };
    match res {
        Ok(_) => Ok(out),
        Err(e) => Err((raw, e)),
    }
}

/// Resolves a (possibly relative) upstream `Location` and maps it to the public URL space.
/// Returns `None` to keep the header untouched.
pub fn rewrite_location(state: &AppState, up_host: &str, up_pq: &str, loc: &str) -> Option<String> {
    let base = url::Url::parse(&format!("https://{up_host}{up_pq}")).ok()?;
    let abs = base.join(loc).ok()?;
    let relative = url::Url::parse(loc).is_err() && !loc.starts_with("//");
    // Root-relative redirects stay valid when the host is served at the root of its public host.
    if relative
        && loc.starts_with('/')
        && state
            .mapper()
            .public_base(up_host)
            .is_some_and(|b| b.prefix.is_empty())
    {
        return Some(loc.to_string());
    }
    Some(state.rewriter.to_public_string(abs.as_str()))
}

/// Rewrites the `Domain` attribute of a `Set-Cookie` header for the public domain.
pub fn rewrite_set_cookie(state: &AppState, v: &str) -> String {
    let m = state.mapper();
    let https = m.public_scheme == "https";
    let mut parts: Vec<String> = Vec::new();
    for (i, part) in v.split(';').enumerate() {
        let t = part.trim();
        if i == 0 {
            parts.push(t.to_string());
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if let Some(d) = lower.strip_prefix("domain=") {
            let d = d.trim_start_matches('.');
            // Wildcard sites: `.civitai.com` -> `.example.com`. Single sites: host-only cookie.
            if let Some(site) = m.site_for_upstream(d).filter(|s| s.mode == Mode::Wildcard) {
                let host = if d == site.sub_root || d == site.root {
                    Some(site.public.clone())
                } else {
                    m.public_bare_host(d)
                };
                if let Some(host) = host {
                    // cookies do not carry a port
                    let host = host.split(':').next().unwrap_or(&host).to_string();
                    parts.push(format!("Domain=.{host}"));
                }
            }
            continue;
        }
        if !https && lower == "secure" {
            continue;
        }
        if !https && lower == "samesite=none" {
            parts.push("SameSite=Lax".into());
            continue;
        }
        parts.push(t.to_string());
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(mode: Mode, domain: &str) -> AppState {
        AppState::new(Config {
            mode,
            public_domain: domain.into(),
            ..Config::default()
        })
        .unwrap()
    }

    #[test]
    fn location() {
        let s = state(Mode::Single, "cv.example.com");
        let r2 = "https://x.r2.cloudflarestorage.com/1/m.safetensors?X-Amz-Signature=a%2Fb";
        assert_eq!(
            rewrite_location(&s, "civitai.com", "/api/download/models/1", r2).unwrap(),
            "https://cv.example.com/__h/x.r2.cloudflarestorage.com/1/m.safetensors?X-Amz-Signature=a%2Fb"
        );
        assert_eq!(
            rewrite_location(&s, "civitai.com", "/a", "/login?x=1").unwrap(),
            "/login?x=1"
        );
        assert_eq!(
            rewrite_location(&s, "image.civitai.com", "/a", "/b").unwrap(),
            "https://cv.example.com/__h/image.civitai.com/b"
        );
        assert_eq!(
            rewrite_location(&s, "civitai.com", "/a", "https://discord.com/oauth").unwrap(),
            "https://discord.com/oauth"
        );

        let w = state(Mode::Wildcard, "example.com");
        assert_eq!(
            rewrite_location(&w, "civitai.com", "/a", "https://www.civitai.com/b").unwrap(),
            "https://example.com/b"
        );
        assert_eq!(
            rewrite_location(&w, "image.civitai.com", "/a", "b").unwrap(),
            "https://image.example.com/b"
        );
    }

    #[test]
    fn set_cookie() {
        let w = state(Mode::Wildcard, "example.com");
        assert_eq!(
            rewrite_set_cookie(
                &w,
                "__Secure-civitai-token=abc; Path=/; Domain=.civitai.com; HttpOnly; Secure; SameSite=Lax"
            ),
            "__Secure-civitai-token=abc; Path=/; Domain=.example.com; HttpOnly; Secure; SameSite=Lax"
        );
        let s = state(Mode::Single, "cv.example.com");
        assert_eq!(
            rewrite_set_cookie(&s, "a=b; Domain=civitai.com; Path=/"),
            "a=b; Path=/"
        );
        let h = AppState::new(Config {
            public_scheme: "http".into(),
            ..Config::default()
        })
        .unwrap();
        assert_eq!(
            rewrite_set_cookie(&h, "a=b; Secure; SameSite=None"),
            "a=b; SameSite=Lax"
        );
    }

    #[test]
    fn headers_upstream() {
        let s = state(Mode::Wildcard, "example.com");
        let mut h = HeaderMap::new();
        h.insert("cookie", "a=b".parse().unwrap());
        h.insert("authorization", "Bearer k".parse().unwrap());
        h.insert("origin", "https://example.com".parse().unwrap());
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        h.insert(
            "accept-encoding",
            "gzip, deflate, br, zstd".parse().unwrap(),
        );
        let out = upstream_headers(&s, &h, "civitai.com", "/api/download/models/1");
        assert_eq!(out["origin"], "https://civitai.com");
        assert_eq!(out["authorization"], "Bearer k");
        assert_eq!(out["accept-encoding"], "gzip, deflate, br");
        assert!(!out.contains_key("x-forwarded-for"));
        let out = upstream_headers(&s, &h, "x.r2.cloudflarestorage.com", "/x");
        assert!(!out.contains_key("cookie"));
        assert!(!out.contains_key("authorization"));
        // presigned URL on a credential host
        let out = upstream_headers(&s, &h, "civitai.com", "/x?X-Amz-Signature=abc");
        assert!(!out.contains_key("cookie"));
        assert!(!out.contains_key("authorization"));
    }

    #[test]
    fn decode_gzip() {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(b"hello").unwrap();
        assert_eq!(decode("gzip", e.finish().unwrap()).unwrap(), b"hello");
    }
}
