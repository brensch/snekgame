//! The snek server binary: configuration from the environment, then the
//! HTTP and WebTransport listeners.

use anyhow::Context;
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_path = PathBuf::from(std::env::var("SNEK_DB").unwrap_or_else(|_| "snek.db".to_owned()));
    let (app, registry) = snek_server::build(&db_path)
        .with_context(|| format!("failed to open the database at {}", db_path.display()))?;

    // WebTransport (QUIC) — the game transport.
    let wt_port: u16 = std::env::var("SNEK_WT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4433);
    let identity = snek_server::wt::load_identity()
        .await
        .context("failed to load TLS identity")?;
    let endpoint = snek_server::wt::build_endpoint(wt_port, &identity)
        .context("failed to bind WT endpoint")?;
    tracing::info!("webtransport listening on udp {wt_port}");
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    // Local dev only: with no real certificate the identity is self-signed,
    // which a browser will accept only if told its hash up front. A page
    // served from localhost asks for it here (see wt_target in net.rs). A
    // server with real certs never registers the route.
    let self_signed = std::env::var("SNEK_CERT").is_err();
    let app = if self_signed {
        let cert_hash: Vec<u8> = identity
            .certificate_chain()
            .as_slice()
            .first()
            .context("self-signed identity has no certificate")?
            .hash()
            .as_ref()
            .to_vec();
        app.route(
            "/api/dev/wt",
            axum::routing::get(move || {
                let cert_hash = cert_hash.clone();
                async move {
                    axum::Json(serde_json::json!({ "port": wt_port, "certHash": cert_hash }))
                }
            }),
        )
    } else {
        app
    };

    // HTTP — static frontend + read-only APIs (behind Caddy).
    let port: u16 = std::env::var("SNEK_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("snek listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind http on {addr}"))?;
    axum::serve(listener, app)
        .await
        .context("http server failed")?;
    Ok(())
}
