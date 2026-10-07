//! `/__cp/*` endpoints: health, whitelist management and a session-cookie import helper.

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::access::parse_net;
use crate::config::Mode;
use crate::proxy::SharedState;

pub const PREFIX: &str = "/__cp";

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/health", get(|| async { "ok\n" }))
        .route("/ip", get(whoami))
        .route("/allow", get(allow))
        .route("/deny", get(deny))
        .route("/list", get(list))
        .route("/login", get(login))
}

type Params = Query<HashMap<String, String>>;

fn token_of(params: &HashMap<String, String>, headers: &HeaderMap) -> String {
    params
        .get("token")
        .cloned()
        .or_else(|| {
            headers
                .get("x-admin-token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[allow(clippy::result_large_err)]
fn authorize(
    state: &SharedState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if !state.access.admin_enabled() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "admin api disabled (access.admin_token is empty)"})),
        )
            .into_response());
    }
    if !state.access.check_token(&token_of(params, headers)) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": "invalid token"})),
        )
            .into_response());
    }
    Ok(())
}

async fn whoami(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = state.access.client_ip(peer, &headers);
    Json(json!({"ip": ip.to_string(), "allowed": state.access.is_allowed(ip), "access_control": state.access.cfg.enabled})).into_response()
}

/// `GET /__cp/allow?token=..[&ip=1.2.3.4|1.2.3.0/24][&ttl=secs][&note=..]`; without `ip` the caller's IP is added.
async fn allow(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Params,
) -> Response {
    if let Err(r) = authorize(&state, &params, &headers) {
        return r;
    }
    let net = match params.get("ip").filter(|s| !s.is_empty()) {
        Some(s) => match parse_net(s) {
            Some(n) => n,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "invalid ip"})),
                )
                    .into_response();
            }
        },
        None => state.access.client_ip(peer, &headers).into(),
    };
    let ttl = match params.get("ttl") {
        Some(t) => match t.parse::<u64>() {
            Ok(t) => t,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "invalid ttl"})),
                )
                    .into_response();
            }
        },
        None => state.access.cfg.default_ttl_secs,
    };
    match state.access.add(net, ttl, params.get("note").cloned()) {
        Ok(e) => {
            tracing::info!("whitelist: added {} (ttl {ttl}s)", e.net);
            Json(json!({"ok": true, "entry": e, "access_control": state.access.cfg.enabled}))
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn deny(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Params,
) -> Response {
    if let Err(r) = authorize(&state, &params, &headers) {
        return r;
    }
    let net = match params.get("ip").filter(|s| !s.is_empty()) {
        Some(s) => match parse_net(s) {
            Some(n) => n,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "invalid ip"})),
                )
                    .into_response();
            }
        },
        None => state.access.client_ip(peer, &headers).into(),
    };
    match state.access.remove(net) {
        Ok(removed) => {
            tracing::info!("whitelist: removed {net}");
            Json(json!({"ok": true, "removed": removed, "net": net})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn list(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(params): Params,
) -> Response {
    if let Err(r) = authorize(&state, &params, &headers) {
        return r;
    }
    Json(json!({"ok": true, "access_control": state.access.cfg.enabled, "entries": state.access.list()})).into_response()
}

const LOGIN_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Import Civitai session</title>
<style>body{font:15px system-ui;max-width:640px;margin:40px auto;padding:0 16px}input{width:100%;padding:6px;margin:4px 0 12px;box-sizing:border-box}</style>
<h2>Import Civitai session cookie</h2>
<p>OAuth logins (Discord / Google / GitHub ...) cannot complete through a proxy. Log in on civitai.com directly,
copy the value of the <code>__Secure-civitai-token</code> cookie from your browser's dev tools and paste it below.</p>
<form method="get">
<label>Cookie name<input name="name" value="__Secure-civitai-token"></label>
<label>Cookie value<input name="value" required></label>
<button>Import</button>
</form>"#;

/// Session cookie import helper. `GET /__cp/login?name=..&value=..` sets the cookie for the proxy domain.
async fn login(State(state): State<SharedState>, Query(params): Params) -> Response {
    let (Some(name), Some(value)) = (params.get("name"), params.get("value")) else {
        return Html(LOGIN_PAGE).into_response();
    };
    let valid_name = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    let valid_value = value
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b';' && b != b',');
    if !valid_name || !valid_value {
        return (StatusCode::BAD_REQUEST, "invalid cookie name or value\n").into_response();
    }
    let m = state.mapper();
    let mut cookie = format!("{name}={value}; Path=/; HttpOnly; Max-Age=2592000; SameSite=Lax");
    if m.public_scheme == "https" {
        cookie.push_str("; Secure");
    }
    if m.mode == Mode::Wildcard {
        let host = m
            .public_domain
            .split(':')
            .next()
            .unwrap_or(&m.public_domain);
        cookie.push_str(&format!("; Domain=.{host}"));
    }
    let mut resp = Redirect::to("/").into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// Rejects non-whitelisted clients when access control is enabled. `/__cp/*` is always reachable.
pub async fn guard(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !state.access.cfg.enabled || req.uri().path().starts_with(PREFIX) {
        return next.run(req).await;
    }
    let ip = state.access.client_ip(peer, req.headers());
    if state.access.is_allowed(ip) {
        return next.run(req).await;
    }
    tracing::debug!("denied {ip} {}", req.uri());
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("civitai-proxy: your IP {ip} is not whitelisted.\nAsk the operator, or visit {PREFIX}/allow?token=<admin token> to add it.\n"),
    )
        .into_response()
}
