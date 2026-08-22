use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_path =
        PathBuf::from(std::env::var("SNEK_DB").unwrap_or_else(|_| "snek.db".to_string()));
    let (app, registry) = snek_server::build(&db_path);

    // WebTransport (QUIC) — the game transport.
    let wt_port: u16 = std::env::var("SNEK_WT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4433);
    let identity = snek_server::wt::load_identity()
        .await
        .expect("failed to load TLS identity");
    let endpoint =
        snek_server::wt::build_endpoint(wt_port, &identity).expect("failed to bind WT endpoint");
    tracing::info!("webtransport listening on udp {wt_port}");
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    // HTTP — static frontend + read-only APIs (behind Caddy).
    let port: u16 = std::env::var("SNEK_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("snek listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
