mod admin;
mod auth;
mod classify;
mod cli;
mod cookie;
mod error;
mod manage;
#[cfg(feature = "classifier")]
mod model;
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
        Some(Command::Model { command }) => asked_of_the_model(command).await,
        None => serve(&cli.config).await,
    }
}

/// Getting or checking the classifier's artefact, when this build carries one.
///
/// A build without the classifier cannot use an artefact, so fetching one would leave
/// an operator with hundreds of megabytes and a gateway that ignores them. It is
/// refused the same way a `[difficulty]` stanza is, naming the feature that carries it.
#[cfg(feature = "classifier")]
async fn asked_of_the_model(command: cli::ModelCommand) -> Result<(), StartupError> {
    model::run(command).await
}

#[cfg(not(feature = "classifier"))]
#[expect(
    clippy::unused_async,
    reason = "the same shape as the build that carries it"
)]
async fn asked_of_the_model(_: cli::ModelCommand) -> Result<(), StartupError> {
    Err(StartupError::UnsupportedBackend {
        backend: "difficulty classifier",
        feature: "classifier",
    })
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
    let serving = state.clone();
    let served = axum::serve(
        listener,
        routes::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await;
    // The requests are answered; their records are still being written.
    if tokio::time::timeout(SETTLE, serving.gateway().settled())
        .await
        .is_err()
    {
        tracing::warn!("gave up waiting for the record of the last requests");
    }
    telemetry.shutdown();
    served.map_err(StartupError::Serve)
}

/// How long a shutdown waits for the records of the requests it has just answered.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

async fn shutdown() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("shutting down");
    }
}

#[cfg(test)]
mod tests {
    const EXAMPLE: &str = include_str!("../intelli-prism.example.toml");

    /// The shipped example must stay loadable, or it teaches a format the server rejects.
    #[test]
    fn the_example_configuration_parses() {
        let config = ip_config::Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.upstreams.len(), 1);
    }

    /// And the keys it only shows commented must parse when an operator uncomments
    /// them, which is the whole reason they are written out rather than described.
    #[test]
    fn the_keys_the_example_only_shows_commented_parse_when_taken_up() {
        let taken: String = EXAMPLE
            .lines()
            .map(|line| match line.trim_start().strip_prefix("# ") {
                Some(rest)
                    if rest.starts_with("caller_speaks =")
                        || rest.starts_with("upstream_speaks =") =>
                {
                    rest
                }
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config = ip_config::Config::parse(&taken).expect("the example's own keys parse");
        let upstream = &config.upstreams[0];
        assert_eq!(upstream.caller_speaks, Some(ip_core::Dialect::OpenAi));
        assert_eq!(upstream.upstream_speaks, Some(ip_core::Dialect::Anthropic));
    }
}
