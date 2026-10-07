//! WebSocket pass-through (e.g. signals / SignalR hubs).

use axum::extract::ws::{CloseFrame as ACloseFrame, Message as AMsg, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::Message as TMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TCloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::proxy::{SharedState, upstream_headers};

pub async fn proxy(
    state: SharedState,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    up_host: String,
    up_pq: String,
) -> Response {
    let url = format!("{}{}", state.upstream_origin(&up_host, true), up_pq);
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("bad upstream url: {e}")).into_response();
        }
    };
    for (name, value) in upstream_headers(&state, &headers, &up_host, &up_pq) {
        if let Some(name) = name {
            if name == header::ACCEPT_ENCODING {
                continue;
            }
            req.headers_mut().insert(name, value);
        }
    }
    if let Some(p) = headers.get(header::SEC_WEBSOCKET_PROTOCOL) {
        req.headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, p.clone());
    }
    if let Ok(h) = HeaderValue::from_str(&up_host) {
        req.headers_mut().insert(header::HOST, h);
    }

    let (upstream, resp) = match tokio_tungstenite::connect_async(req).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("websocket upstream {url}: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                format!("civitai-proxy: websocket upstream error: {e}\n"),
            )
                .into_response();
        }
    };
    let mut ws = ws;
    if let Some(p) = resp
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
    {
        ws = ws.protocols([p.to_string()]);
    }
    ws.on_upgrade(move |client| pipe(state, client, upstream))
}

async fn pipe<S>(
    state: SharedState,
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<S>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut c_tx, mut c_rx) = client.split();
    let (mut u_tx, mut u_rx) = upstream.split();

    let to_up = async {
        while let Some(Ok(msg)) = c_rx.next().await {
            let m = match msg {
                AMsg::Text(t) => TMsg::text(state.rewriter.to_upstream_string(t.as_str())),
                AMsg::Binary(b) => TMsg::Binary(b),
                AMsg::Ping(b) => TMsg::Ping(b),
                AMsg::Pong(b) => TMsg::Pong(b),
                AMsg::Close(f) => {
                    let f = f.map(|f| TCloseFrame {
                        code: CloseCode::from(f.code),
                        reason: f.reason.as_str().to_string().into(),
                    });
                    let _ = u_tx.send(TMsg::Close(f)).await;
                    break;
                }
            };
            if u_tx.send(m).await.is_err() {
                break;
            }
        }
    };
    let to_client = async {
        while let Some(Ok(msg)) = u_rx.next().await {
            let m = match msg {
                TMsg::Text(t) => AMsg::text(state.rewriter.to_public_string(t.as_str())),
                TMsg::Binary(b) => AMsg::Binary(b),
                TMsg::Ping(b) => AMsg::Ping(b),
                TMsg::Pong(b) => AMsg::Pong(b),
                TMsg::Close(f) => {
                    let f = f.map(|f| ACloseFrame {
                        code: f.code.into(),
                        reason: f.reason.as_str().to_string().into(),
                    });
                    let _ = c_tx.send(AMsg::Close(f)).await;
                    break;
                }
                TMsg::Frame(_) => continue,
            };
            if c_tx.send(m).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = to_up => {},
        _ = to_client => {},
    }
}
