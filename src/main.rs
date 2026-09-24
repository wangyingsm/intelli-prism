mod admin;
mod auth;
mod cli;
mod cookie;
mod error;
mod manage;
mod routes;
mod rules;
mod state;
mod telemetry;
mod usage;

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
    let telemetry = telemetry::install(&config.telemetry);

    let state = AppState::open(&config).await?;
    let _healing = std::sync::Arc::clone(state.feed()).keep_healing();
    let _sweeping = usage::Sweeper::new(
        std::sync::Arc::clone(state.store()) as std::sync::Arc<dyn ip_storage::UsageStore>,
        &config.usage,
    )
    .map(|sweeper| {
        tracing::info!(
            seconds = config.usage.retention.map(ip_config::Seconds::get),
            "sweeping away what is past its keeping"
        );
        sweeper.keep_sweeping()
    });
    let _following = state.keep_rules_in_step();
    let address = config.server.listen;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| StartupError::Bind { address, source })?;

    tracing::info!(%address, "listening");
    let served = axum::serve(
        listener,
        routes::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await;
    telemetry.shutdown();
    served.map_err(StartupError::Serve)
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
