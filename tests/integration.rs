//! End-to-end tests against a local mock upstream (wired in via `upstream_resolve`).

use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use civitai_proxy::config::{Config, Mode};
use civitai_proxy::proxy::AppState;
use futures_util::{SinkExt, StreamExt};

const R2: &str = "bucket.abc123.r2.cloudflarestorage.com";

async fn mock(req: Request) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let h = req.headers().clone();
    match (host.as_str(), req.uri().path()) {
        ("civitai.com", "/api/v1/models") => {
            let body = br#"{"items":[{"downloadUrl":"https://civitai.com/api/download/models/1","images":[{"url":"https://image.civitai.com/xG1/a/width=450/a.jpeg"}]}]}"#;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(body).unwrap();
            (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CONTENT_ENCODING, "gzip"),
                ],
                e.finish().unwrap(),
            )
                .into_response()
        }
        ("civitai.com", "/api/download/models/1") => {
            assert_eq!(h.get(header::AUTHORIZATION).unwrap(), "Bearer KEY");
            (
                StatusCode::TEMPORARY_REDIRECT,
                [
                    (
                        header::LOCATION,
                        format!("https://{R2}/1/model.safetensors?X-Amz-Signature=sig%2F1"),
                    ),
                    (
                        header::SET_COOKIE,
                        "s=1; Domain=.civitai.com; Path=/; Secure".to_string(),
                    ),
                ],
            )
                .into_response()
        }
        ("civitai.com", "/echo-origin") => {
            let o = h
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            format!("origin={o} path={path}").into_response()
        }
        (R2, "/1/model.safetensors") => {
            assert!(
                h.get(header::AUTHORIZATION).is_none(),
                "credentials must not reach storage"
            );
            assert!(h.get(header::COOKIE).is_none());
            assert!(
                path.contains("X-Amz-Signature=sig%2F1"),
                "signature must be preserved: {path}"
            );
            let data = b"0123456789";
            match h.get(header::RANGE).and_then(|v| v.to_str().ok()) {
                Some("bytes=2-5") => (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, "application/octet-stream"),
                        (header::CONTENT_RANGE, "bytes 2-5/10"),
                    ],
                    data[2..6].to_vec(),
                )
                    .into_response(),
                _ => (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    data.to_vec(),
                )
                    .into_response(),
            }
        }
        ("image.civitai.com", "/xG1/a/width=450/a.jpeg") => {
            ([(header::CONTENT_TYPE, "image/jpeg")], vec![0xffu8, 0xd8]).into_response()
        }
        _ => (StatusCode::NOT_FOUND, format!("mock: {host}{path}")).into_response(),
    }
}

async fn mock_ws(ws: WebSocketUpgrade, headers: HeaderMap) -> Response {
    assert_eq!(headers.get(header::HOST).unwrap(), "signals.civitai.com");
    ws.on_upgrade(|mut s| async move {
        while let Some(Ok(Message::Text(t))) = s.next().await {
            let reply = format!("echo {} https://image.civitai.com/x", t.as_str());
            if s.send(Message::text(reply)).await.is_err() {
                break;
            }
        }
    })
}

async fn spawn(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    addr
}

async fn setup(mode: Mode, domain: &str, access: bool) -> (SocketAddr, Arc<AppState>) {
    let upstream = spawn(
        Router::new()
            .route("/hub", axum::routing::get(mock_ws))
            .fallback(mock),
    )
    .await;
    let origin = format!("http://{upstream}");
    let resolve: BTreeMap<String, String> = [
        "civitai.com",
        "image.civitai.com",
        "signals.civitai.com",
        R2,
    ]
    .iter()
    .map(|h| (h.to_string(), origin.clone()))
    .collect();
    let mut cfg = Config {
        mode,
        public_domain: domain.into(),
        upstream_resolve: resolve,
        ..Config::default()
    };
    cfg.access.enabled = access;
    cfg.access.admin_token = "tok".into();
    cfg.access.whitelist_file = std::env::temp_dir().join(format!(
        "cp-it-{}-{}.json",
        std::process::id(),
        rand_suffix()
    ));
    let state = Arc::new(AppState::new(cfg).unwrap());
    let addr = spawn(civitai_proxy::app(state.clone())).await;
    (addr, state)
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn single_mode_api_and_download() {
    let (addr, _) = setup(Mode::Single, "cv.example.com", false).await;
    let c = client();
    let base = format!("http://{addr}");

    // API JSON (gzip upstream) is decoded and rewritten
    let r = c
        .get(format!("{base}/api/v1/models"))
        .header("host", "cv.example.com")
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get(header::CONTENT_ENCODING).is_none());
    let body = r.text().await.unwrap();
    assert!(
        body.contains(r#""downloadUrl":"https://cv.example.com/api/download/models/1""#),
        "{body}"
    );
    assert!(
        body.contains("https://cv.example.com/__h/image.civitai.com/xG1/a/width=450/a.jpeg"),
        "{body}"
    );

    // download redirect -> storage host through the proxy
    let r = c
        .get(format!("{base}/api/download/models/1"))
        .header("host", "cv.example.com")
        .header("authorization", "Bearer KEY")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 307);
    let loc = r.headers()[header::LOCATION].to_str().unwrap().to_string();
    assert_eq!(
        loc,
        format!("https://cv.example.com/__h/{R2}/1/model.safetensors?X-Amz-Signature=sig%2F1")
    );
    assert_eq!(r.headers()[header::SET_COOKIE], "s=1; Path=/; Secure");

    // follow it (as a client would, keeping the Authorization header on the same host)
    let path = loc.strip_prefix("https://cv.example.com").unwrap();
    let r = c
        .get(format!("{base}{path}"))
        .header("host", "cv.example.com")
        .header("authorization", "Bearer KEY")
        .header("cookie", "a=b")
        .header("range", "bytes=2-5")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"2345");

    // image host
    let r = c
        .get(format!(
            "{base}/__h/image.civitai.com/xG1/a/width=450/a.jpeg"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), &[0xff, 0xd8]);

    // Origin and query strings are mapped back to upstream
    let r = c
        .get(format!(
            "{base}/echo-origin?cb=https%3A%2F%2Fcv.example.com%2Fx"
        ))
        .header("origin", "https://cv.example.com")
        .send()
        .await
        .unwrap();
    // the echoed upstream values are mapped to public form again on the way back (bare host)
    let t = r.text().await.unwrap();
    assert_eq!(
        t,
        "origin=https://cv.example.com path=/echo-origin?cb=https%3A%2F%2Fcv.example.com%2Fx"
    );

    // non-allowed hosts are refused
    let r = c.get(format!("{base}/__h/evil.com/")).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn wildcard_mode_hosts() {
    let (addr, _) = setup(Mode::Wildcard, "example.com", false).await;
    let c = client();
    let base = format!("http://{addr}");
    let r = c
        .get(format!("{base}/api/v1/models"))
        .header("host", "example.com")
        .send()
        .await
        .unwrap();
    let body = r.text().await.unwrap();
    assert!(
        body.contains("https://example.com/api/download/models/1"),
        "{body}"
    );
    assert!(body.contains("https://image.example.com/xG1/"), "{body}");

    let r = c
        .get(format!("{base}/api/download/models/1"))
        .header("host", "example.com")
        .header("authorization", "Bearer KEY")
        .send()
        .await
        .unwrap();
    let loc = r.headers()[header::LOCATION].to_str().unwrap().to_string();
    assert_eq!(
        loc,
        format!("https://ext.example.com/{R2}/1/model.safetensors?X-Amz-Signature=sig%2F1")
    );
    assert_eq!(
        r.headers()[header::SET_COOKIE],
        "s=1; Domain=.example.com; Path=/; Secure"
    );

    let path = loc.strip_prefix("https://ext.example.com").unwrap();
    let r = c
        .get(format!("{base}{path}"))
        .header("host", "ext.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"0123456789");

    let r = c
        .get(format!("{base}/xG1/a/width=450/a.jpeg"))
        .header("host", "image.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = c
        .get(format!("{base}/"))
        .header("host", "unrelated.org")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn whitelist_flow() {
    let (addr, _) = setup(Mode::Single, "cv.example.com", true).await;
    let c = client();
    let base = format!("http://{addr}");
    let r = c
        .get(format!(
            "{base}/__h/image.civitai.com/xG1/a/width=450/a.jpeg"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .get(format!("{base}/__cp/allow?token=wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .get(format!("{base}/__cp/allow?token=tok&ttl=60"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    assert_eq!(v["entry"]["net"], "127.0.0.1/32");
    let r = c
        .get(format!(
            "{base}/__h/image.civitai.com/xG1/a/width=450/a.jpeg"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .get(format!("{base}/__cp/list"))
        .header("x-admin-token", "tok")
        .send()
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 1);
    let r = c
        .get(format!("{base}/__cp/deny?token=tok"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn websocket_passthrough() {
    let (addr, _) = setup(Mode::Single, "cv.example.com", false).await;
    let url = format!("ws://{addr}/__h/signals.civitai.com/hub");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::text("hi"))
        .await
        .unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    assert_eq!(
        msg.into_text().unwrap().as_str(),
        "echo hi https://cv.example.com/__h/image.civitai.com/x"
    );
}
