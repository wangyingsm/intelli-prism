use ip_config::TelemetryConfig;
use tracing_subscriber::filter::LevelFilter;

/// Installs the log subscriber for the process.
pub fn install(config: &TelemetryConfig) {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::from(config.log_level))
        .init();
}
