//! The proxy itself: the server, its management api and the command line that starts them.

mod admin;
mod auth;
mod classify;
mod cli;
mod cookie;
mod embed;
mod error;
mod manage;
#[cfg(feature = "classifier")]
mod model;
mod routes;
mod rules;
mod scrape;
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
    let telemetry = std::sync::Arc::new(telemetry::install(&config.telemetry));
    let _scraping = match &config.telemetry.scrape {
        Some(scrape) => {
            let (_, serving) = scrape::serve(scrape, std::sync::Arc::clone(&telemetry)).await?;
            Some(serving)
        }
        None => None,
    };

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

    /// The whole filter section, uncommented as an operator would take it up.
    ///
    /// M15 taught that uncommenting two lines proves almost nothing: it cannot catch a
    /// key written under the wrong table or a block in the wrong place. This takes the
    /// section whole — every table and every key from `[filter]` to the end of the
    /// file — so a misplaced one fails here.
    #[test]
    fn the_whole_filter_section_parses_when_taken_up() {
        let taken = taken_from("# [filter]");
        let config = ip_config::Config::parse(&taken).expect("the filter section parses");
        let filter = config.filter.expect("the section is there");
        assert_eq!(filter.window, 256);
        let secrets = filter.secrets().expect("every secret it names resolves");
        let named: Vec<&str> = secrets.iter().map(|one| one.kind.as_str()).collect();
        assert_eq!(
            named,
            vec![
                "credit-card",
                "prc-id",
                "iban",
                "private-key",
                "openai-key",
                "corp-token",
                "staff-mail",
                "account",
            ],
            "the shipped ones it names, then the operator's own, in order"
        );
        // And they compile, which is what an operator finds out at startup.
        ip_filter::Detectors::of(&secrets).expect("the example's own secrets compile");

        // The tenant block the example shows resolves to the floor and then its own,
        // and a tenant it does not name is scanned for the floor.
        let acme = ip_core::TenantId::new("acme").unwrap();
        let theirs = filter
            .secrets_for(&acme)
            .expect("the tenant block resolves");
        let named: Vec<&str> = theirs.iter().map(|one| one.kind.as_str()).collect();
        assert_eq!(
            named,
            vec![
                "credit-card",
                "prc-id",
                "iban",
                "private-key",
                "openai-key",
                "corp-token",
                "staff-mail",
                "account",
                "email",
                "acme-id",
            ],
            "the floor whole, then what the tenant asked for"
        );
        ip_filter::Detectors::of(&theirs).expect("the tenant's own secrets compile");
        let stranger = ip_core::TenantId::new("stranger").unwrap();
        assert_eq!(
            filter.secrets_for(&stranger).unwrap().len(),
            secrets.len(),
            "a tenant the example does not name gets the floor, no more and no less"
        );
    }

    /// The semantic section, uncommented as an operator would take it up, and read
    /// back as what it resolves to rather than merely proved to parse.
    #[test]
    fn the_whole_semantic_section_parses_when_taken_up() {
        let config = ip_config::Config::parse(&taken_from("# [semantic]"))
            .expect("the semantic section parses");
        let semantic = config.semantic.expect("the section is there");
        assert_eq!(
            semantic.artifact,
            std::path::PathBuf::from("/var/lib/intelli-prism/embed")
        );
        assert!((semantic.nearness - 0.97).abs() < f32::EPSILON);
        assert_eq!(semantic.ttl.get(), 3_600);
        assert_eq!(semantic.capacity, 4_096);
        // The example shows no library key, because the loader's own search answers
        // first; an operator who needs one reads the difficulty section for it.
        assert_eq!(semantic.library, None);
    }

    /// The scrape endpoint, uncommented as an operator would take it up.
    ///
    /// The log file the section above it shows cannot be taken up here: it names a
    /// path under `/var/log`, and the key is refused when that directory does not
    /// exist, which is the point of checking it as the file is read.
    #[test]
    fn the_scrape_endpoint_parses_when_taken_up() {
        let config = ip_config::Config::parse(&taken_from("# [telemetry.scrape]"))
            .expect("the scrape section parses");
        let scrape = config.telemetry.scrape.expect("the section is there");
        assert_eq!(scrape.listen.to_string(), "127.0.0.1:9464");
        assert_eq!(
            scrape.token.map(|token| token.expose().to_owned()),
            Some("replace-me".to_owned())
        );
    }

    /// The example with one commented section taken up, from its table header to the
    /// end of the file.
    ///
    /// Only the lines that are toml, never the prose around them: a table header, or
    /// a key and its value.
    fn taken_from(header: &str) -> String {
        let lines: Vec<&str> = EXAMPLE.lines().collect();
        let from = lines
            .iter()
            .position(|line| line.trim_start() == header)
            .unwrap_or_else(|| panic!("the example shows a {header} section"));
        lines
            .iter()
            .enumerate()
            .map(|(at, line)| {
                if at < from {
                    return (*line).to_owned();
                }
                match line.trim_start().strip_prefix("# ") {
                    Some(rest)
                        if rest.starts_with('[')
                            || rest.split_once(" = ").is_some_and(|(name, _)| {
                                !name.is_empty()
                                    && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                            }) =>
                    {
                        rest.to_owned()
                    }
                    _ => (*line).to_owned(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
