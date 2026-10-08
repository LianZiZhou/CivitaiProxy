//! End-to-end tests for the AI gateway against mock upstreams (wired in via `upstream_resolve`).

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use civitai_proxy::config::{Config, Mode, SiteConfig};
use civitai_proxy::proxy::AppState;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;

const GATEWAY: &str = "ai.test";
const VERTEX: &str = "vertex.test";
/// Model output that mentions hosts the proxy knows about: must never be rewritten.
const ANSWER: &str = r#"See https://api.openai.com/v1/models and github.com/openai/openai-python or https://ai.test/openai"#;

const UPSTREAMS: &[&str] = &[
    "api.openai.com",
    "api.anthropic.com",
    "generativelanguage.googleapis.com",
    "us-central1-aiplatform.googleapis.com",
    "aiplatform.googleapis.com",
    "github.com",
];

async fn mock(req: Request) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // gRPC arrives over h2 with :authority = the mock's address, so route it by path
    if path == "/google.cloud.aiplatform.v1.PredictionService/GenerateContent" {
        assert_eq!(req.headers().get(header::TE).unwrap(), "trailers");
        assert_eq!(auth.as_deref(), Some("Bearer ya29.token"));
        let body = req.into_body().collect().await.unwrap().to_bytes();
        let mut out = b"echo:".to_vec();
        out.extend_from_slice(&body);
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());
        trailers.insert("grpc-message", "ok".parse().unwrap());
        let frames = futures_util::stream::iter(vec![
            Ok::<_, Infallible>(Frame::data(Bytes::from(out))),
            Ok(Frame::trailers(trailers)),
        ]);
        return (
            [(header::CONTENT_TYPE, "application/grpc")],
            Body::new(StreamBody::new(frames)),
        )
            .into_response();
    }
    match (host.as_str(), path.as_str()) {
        ("api.openai.com", "/v1/chat/completions") => {
            assert_eq!(auth.as_deref(), Some("Bearer sk-test"));
            let body = String::from_utf8(req.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
            let stream = body.contains(r#""stream":true"#);
            if stream {
                let chunks = futures_util::stream::unfold(0, |i| async move {
                    match i {
                        0 => Some((Ok::<_, Infallible>(Bytes::from("data: {\"delta\":\"first\"}\n\n")), 1)),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(800)).await;
                            Some((Ok(Bytes::from(format!("data: {{\"delta\":{}}}\n\ndata: [DONE]\n\n", serde_json::json!(ANSWER)))), 2))
                        }
                        _ => None,
                    }
                });
                return ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(chunks)).into_response();
            }
            let resp = serde_json::json!({"echo_request": body, "echo_query": query, "content": ANSWER}).to_string();
            ([(header::CONTENT_TYPE, "application/json")], resp).into_response()
        }
        ("api.openai.com", "/v1/images/generations") => (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"data":[{"url":"https://oaidalleapiprodscus.blob.core.windows.net/private/img.png?sig=abc"}]}"#,
        )
            .into_response(),
        ("api.anthropic.com", "/v1/messages/batches/msgbatch_1") => {
            assert_eq!(req.headers().get("x-api-key").unwrap(), "sk-ant");
            (
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"id":"msgbatch_1","results_url":"https://api.anthropic.com/v1/messages/batches/msgbatch_1/results"}"#,
            )
                .into_response()
        }
        ("api.anthropic.com", "/v1/messages/batches/msgbatch_1/results") => {
            let line = serde_json::json!({"custom_id": "a", "result": {"text": ANSWER}}).to_string();
            ([(header::CONTENT_TYPE, "application/binary")], format!("{line}\n")).into_response()
        }
        ("generativelanguage.googleapis.com", "/upload/v1beta/files") => {
            assert_eq!(query, "key=AIza-test");
            (
                [("x-goog-upload-url", "https://generativelanguage.googleapis.com/upload/v1beta/files?upload_id=u1&upload_protocol=resumable")],
                "",
            )
                .into_response()
        }
        ("generativelanguage.googleapis.com", "/v1beta/models/gemini:generateContent") => (
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({"candidates": [{"content": {"parts": [{"text": ANSWER}]}}]}).to_string(),
        )
            .into_response(),
        ("us-central1-aiplatform.googleapis.com", "/v1/projects/p/locations/us-central1/publishers/google/models/gemini:generateContent") => {
            assert_eq!(auth.as_deref(), Some("Bearer ya29.token"));
            ([(header::CONTENT_TYPE, "application/json")], r#"{"region":"us-central1"}"#).into_response()
        }
        _ => (StatusCode::NOT_FOUND, format!("mock: no route for {host}{path}")).into_response(),
    }
}

async fn realtime(ws: WebSocketUpgrade, headers: HeaderMap) -> Response {
    assert_eq!(headers.get(header::HOST).unwrap(), "api.openai.com");
    assert_eq!(
        headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer sk-test"
    );
    ws.on_upgrade(|mut s| async move {
        while let Some(Ok(Message::Text(t))) = s.next().await {
            let reply = format!("{} | {}", t.as_str(), ANSWER);
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

async fn setup() -> SocketAddr {
    let upstream = spawn(
        Router::new()
            .route("/v1/realtime", axum::routing::get(realtime))
            .fallback(mock),
    )
    .await;
    let resolve: BTreeMap<String, String> = UPSTREAMS
        .iter()
        .map(|h| (h.to_string(), format!("http://{upstream}")))
        .collect();
    let site = |p: &str, d: &str| SiteConfig {
        preset: Some(p.into()),
        mode: Mode::Single,
        public_domain: d.into(),
        ..Default::default()
    };
    let cfg = Config {
        sites: vec![
            site("ai", GATEWAY),
            site("vertex", VERTEX),
            site("github", "gh.test"),
        ],
        upstream_resolve: resolve,
        ..Config::default()
    };
    spawn(civitai_proxy::app(Arc::new(AppState::new(cfg).unwrap()))).await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn openai_content_is_untouched() {
    let addr = setup().await;
    let body = serde_json::json!({"model": "gpt", "messages": [{"role": "user", "content": "mirror https://ai.test/openai/v1 please"}]}).to_string();
    let r = client()
        .post(format!(
            "http://{addr}/openai/v1/chat/completions?note=https%3A%2F%2Fai.test%2Fx"
        ))
        .header("host", GATEWAY)
        .header("authorization", "Bearer sk-test")
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    assert_eq!(v["content"], ANSWER, "model output was rewritten");
    assert_eq!(v["echo_request"], body, "request body was rewritten");
    assert_eq!(
        v["echo_query"], "note=https%3A%2F%2Fai.test%2Fx",
        "query was rewritten"
    );
}

#[tokio::test]
async fn openai_sse_streams_incrementally() {
    let addr = setup().await;
    let start = Instant::now();
    let r = client()
        .post(format!("http://{addr}/openai/v1/chat/completions"))
        .header("host", GATEWAY)
        .header("authorization", "Bearer sk-test")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt","stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut stream = r.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    let first_at = start.elapsed();
    assert!(String::from_utf8_lossy(&first).contains("first"));
    assert!(
        first_at < Duration::from_millis(600),
        "first event buffered: {first_at:?}"
    );
    let mut rest = Vec::new();
    while let Some(c) = stream.next().await {
        rest.extend_from_slice(&c.unwrap());
    }
    assert!(start.elapsed() >= Duration::from_millis(800));
    assert!(
        String::from_utf8(rest)
            .unwrap()
            .contains(&serde_json::json!(ANSWER).to_string())
    );
}

#[tokio::test]
async fn openai_images_and_realtime() {
    let addr = setup().await;
    // image URLs on Azure blob storage are mapped back through the proxy
    let r = client()
        .post(format!("http://{addr}/openai/v1/images/generations"))
        .header("host", GATEWAY)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"data":[{"url":"https://ai.test/__h/oaidalleapiprodscus.blob.core.windows.net/private/img.png?sig=abc"}]}"#
    );
    // Realtime WebSocket: frames are user content, never rewritten; auth header forwarded
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://{addr}/openai/v1/realtime?model=gpt-realtime"),
    )
    .unwrap();
    req.headers_mut().insert("host", GATEWAY.parse().unwrap());
    req.headers_mut()
        .insert("authorization", "Bearer sk-test".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::text(
        "hello https://ai.test/openai",
    ))
    .await
    .unwrap();
    let msg = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(
        msg.as_str(),
        format!("hello https://ai.test/openai | {ANSWER}")
    );
}

#[tokio::test]
async fn anthropic_batches() {
    let addr = setup().await;
    let r = client()
        .get(format!(
            "http://{addr}/anthropic/v1/messages/batches/msgbatch_1"
        ))
        .header("host", GATEWAY)
        .header("x-api-key", "sk-ant")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"id":"msgbatch_1","results_url":"https://ai.test/anthropic/v1/messages/batches/msgbatch_1/results"}"#
    );
    let r = client()
        .get(format!(
            "http://{addr}/anthropic/v1/messages/batches/msgbatch_1/results"
        ))
        .header("host", GATEWAY)
        .send()
        .await
        .unwrap();
    assert!(
        r.text()
            .await
            .unwrap()
            .contains(&serde_json::json!(ANSWER).to_string())
    );
}

#[tokio::test]
async fn gemini_and_vertex() {
    let addr = setup().await;
    // resumable upload: the session URL header points back at the proxy
    let r = client()
        .post(format!(
            "http://{addr}/gemini/upload/v1beta/files?key=AIza-test"
        ))
        .header("host", GATEWAY)
        .header("x-goog-upload-protocol", "resumable")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.headers()["x-goog-upload-url"],
        "https://ai.test/gemini/upload/v1beta/files?upload_id=u1&upload_protocol=resumable"
    );
    let r = client()
        .post(format!(
            "http://{addr}/gemini/v1beta/models/gemini:generateContent"
        ))
        .header("host", GATEWAY)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert!(
        r.text()
            .await
            .unwrap()
            .contains(&serde_json::json!(ANSWER).to_string())
    );
    // regional Vertex endpoint through a templated prefix
    let r = client()
        .post(format!("http://{addr}/vertex-us-central1/v1/projects/p/locations/us-central1/publishers/google/models/gemini:generateContent"))
        .header("host", GATEWAY)
        .header("authorization", "Bearer ya29.token")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), r#"{"region":"us-central1"}"#);
}

#[tokio::test]
async fn grpc_trailers_pass_through() {
    let addr = setup().await;
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(tcp),
    )
    .await
    .unwrap();
    tokio::spawn(conn);
    let req = http::Request::builder()
        .method("POST")
        .uri(format!(
            "http://{VERTEX}/google.cloud.aiplatform.v1.PredictionService/GenerateContent"
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("authorization", "Bearer ya29.token")
        .body(http_body_util::Full::new(Bytes::from_static(
            b"\0\0\0\0\x02hi",
        )))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/grpc");
    let collected = resp.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().expect("trailers lost");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(trailers["grpc-message"], "ok");
    assert_eq!(collected.to_bytes().as_ref(), b"echo:\0\0\0\0\x02hi");
}

#[tokio::test]
async fn gateway_index() {
    let addr = setup().await;
    let r = client()
        .get(format!("http://{addr}/"))
        .header("host", GATEWAY)
        .header("accept", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    let providers = v["providers"].as_array().unwrap();
    assert!(providers.len() >= 40);
    let openai = providers.iter().find(|p| p["name"] == "openai").unwrap();
    assert_eq!(openai["base_url"], "https://ai.test/openai/v1");
    let vertex = providers.iter().find(|p| p["name"] == "vertex").unwrap();
    assert!(
        vertex["prefixes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["prefix"] == "/vertex-<x>")
    );
    let r = client()
        .get(format!("http://{addr}/nope/v1"))
        .header("host", GATEWAY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert!(r.text().await.unwrap().contains("AI API gateway"));
}
