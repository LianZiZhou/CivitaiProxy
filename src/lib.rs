pub mod access;
pub mod admin;
pub mod config;
pub mod mapping;
pub mod proxy;
pub mod rewrite;
pub mod ws;

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::middleware;

use crate::proxy::SharedState;

pub fn app(state: SharedState) -> Router {
    Router::new()
        .nest(admin::PREFIX, admin::router())
        .fallback(proxy::handle)
        .layer(middleware::from_fn_with_state(state.clone(), admin::guard))
        .with_state(state)
}

pub async fn serve(state: SharedState) -> anyhow::Result<()> {
    let cfg = &state.cfg;
    if cfg.access.enabled && cfg.access.admin_token.is_empty() {
        tracing::warn!(
            "access control is enabled but access.admin_token is empty: /__cp/allow is disabled, use the CLI"
        );
    }
    let purge = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            purge.access.purge_expired();
        }
    });
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    tracing::info!(
        "civitai-proxy listening on http://{} ({:?} mode, public {}://{})",
        cfg.listen,
        cfg.mode,
        cfg.public_scheme,
        cfg.public_domain
    );
    axum::serve(
        listener,
        app(state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
