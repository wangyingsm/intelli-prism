mod admin;
mod auth;
mod cli;
mod cookie;
mod error;
mod routes;
mod state;
mod telemetry;

use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use ip_config::Config;

use crate::cli::{Cli, Command};
use crate::error::StartupError;
use crate::state::AppState;

pub(crate) const DEFAULT_CONFIG_PATH: &str = "intelli-prism.toml";

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Admin { command }) => {
            let passphrase = admin::ask_passphrase()?;
            admin::run(command, &cli.config, &passphrase).await
        }
        None => serve(&cli.config).await,
    }
}

/// Opens everything the configuration names and serves until the kernel says to stop.
async fn serve(config: &Path) -> Result<(), StartupError> {
    let config = Config::load(config)?;
    telemetry::install(&config.telemetry);

    let state = AppState::open(&config).await?;
    let address = config.server.listen;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| StartupError::Bind { address, source })?;

    tracing::info!(%address, "listening");
    axum::serve(
        listener,
        routes::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .map_err(StartupError::Serve)
}

async fn shutdown() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("shutting down");
    }
}

#[cfg(test)]
mod tests {
    /// The shipped example must stay loadable, or it teaches a format the server rejects.
    #[test]
    fn the_example_configuration_parses() {
        let config =
            ip_config::Config::parse(include_str!("../intelli-prism.example.toml")).unwrap();
        assert_eq!(config.upstreams.len(), 1);
    }
}
