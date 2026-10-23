//! The snek server binary: configuration from the environment, then the
//! server itself until systemd (SIGTERM) or ctrl-c stops it.

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let shutdown =
        snek_server::shutdown_signal().context("failed to listen for shutdown signals")?;
    let config = snek_server::Config::from_vars(|name| std::env::var(name).ok());
    snek_server::run(&config, shutdown).await
}
