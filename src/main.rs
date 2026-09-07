mod auth;
mod error;
mod routes;
mod state;
mod telemetry;

use std::net::SocketAddr;
use std::path::PathBuf;

use ip_config::Config;

use crate::error::StartupError;
use crate::state::AppState;

const DEFAULT_CONFIG_PATH: &str = "intelli-prism.toml";

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    let config = Config::load(config_path())?;
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

fn config_path() -> PathBuf {
    std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

async fn shutdown() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("shutting down");
    }
}
