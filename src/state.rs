use std::net::SocketAddr;
use std::sync::Arc;

use ip_auth::{Logins, RequestVerifier, SessionTokens};
#[cfg(feature = "cluster-cache")]
use ip_cache::RedisCache;
use ip_cache::{Cache, CacheBackend, Counters, Ttl};
#[cfg(feature = "standalone-cache")]
use ip_cache::{LevelLimits, MaxBytes, SledCache};
use ip_config::{CacheConfig, Config, StorageConfig};
use ip_core::RouteRule;
use ip_gateway::{
    Dispatcher, Gateway, HyperUpstream, Limiter, Limits, ResponseCache, RouteError, RoutingTable,
};
use ip_plugin::{PluginChains, PluginHost, PluginLimits};
#[cfg(feature = "fast-storage")]
use ip_storage::PostgresStore;
#[cfg(any(feature = "standalone-storage", test))]
use ip_storage::SqliteStore;
use ip_storage::{Backend, RuleRevision, Storage};

use crate::error::StartupError;
use crate::rules::{Reloader, RuleFeed};

/// The store that was opened, in the shape its own backend has.
///
/// A transaction is a type of its backend's own, so it cannot come out of a trait object.
/// This keeps the concrete store beside the one every other caller reads through.
#[derive(Clone)]
pub enum Stores {
    /// The standalone backend, which a test build always has: the tests open sqlite
    /// whichever backend the binary was compiled for.
    #[cfg(any(feature = "standalone-storage", test))]
    Sqlite(Arc<SqliteStore>),
    /// The cluster backend.
    #[cfg(feature = "fast-storage")]
    Postgres(Arc<PostgresStore>),
}

impl Stores {
    /// The same store, behind the trait object everything else uses.
    pub fn backend(&self) -> Arc<dyn Backend> {
        match self {
            #[cfg(any(feature = "standalone-storage", test))]
            Self::Sqlite(store) => Arc::clone(store) as Arc<dyn Backend>,
            #[cfg(feature = "fast-storage")]
            Self::Postgres(store) => Arc::clone(store) as Arc<dyn Backend>,
        }
    }
}

/// What every handler is given.
#[derive(Clone)]
pub struct AppState {
    stores: Stores,
    store: Arc<dyn Backend>,
    verifier: RequestVerifier,
    logins: Arc<Logins>,
    session_seconds: u64,
    gateway: Arc<Gateway>,
    listen: SocketAddr,
    configured: Arc<[RouteRule]>,
    feed: Arc<RuleFeed>,
    reloader: Arc<Reloader>,
    /// The store a test reads through, when the state was built for one, so the test can
    /// make it fail where it means to.
    #[cfg(test)]
    failing_store: Option<Arc<ip_storage::FailingStore>>,
    /// The cache a test reads through, for the same reason.
    #[cfg(test)]
    failing_cache: Option<Arc<ip_cache::FailingCache>>,
    /// The revision this node's rules were built from, which is where following starts.
    started_at: RuleRevision,
}

impl AppState {
    /// Opens the configured backend, then builds the routing table, the plugin chains and the
    /// upstream client.
    pub async fn open(config: &Config) -> Result<Self, StartupError> {
        // The judge first, before any backend is touched: a config that asks through a
        // build without the feature, or for an artefact that is not one, is refused at
        // startup — never a silent fall back to serving unclassified, and never a
        // half-opened process on the way to the refusal.
        #[cfg(feature = "classifier")]
        let judge: Option<std::sync::Arc<dyn ip_gateway::Classifier>> =
            match config.difficulty.as_ref() {
                Some(difficulty) => Some(std::sync::Arc::new(
                    crate::classify::RuntimeJudge::open(difficulty)
                        .map_err(|reason| StartupError::Classifier { reason })?,
                )),
                None => None,
            };
        #[cfg(not(feature = "classifier"))]
        let judge: Option<std::sync::Arc<dyn ip_gateway::Classifier>> = {
            if config.difficulty.is_some() {
                return Err(StartupError::UnsupportedBackend {
                    backend: "difficulty classifier",
                    feature: "classifier",
                });
            }
            None
        };
        // The encoder on the same terms and for the same reason, and before any
        // backend is touched: a level configured and silently off is indistinguishable
        // from one nobody's traffic suits, which is the worst way for this to fail.
        #[cfg(feature = "semantic")]
        let encoder: Option<std::sync::Arc<crate::embed::RuntimeEmbedder>> =
            match config.semantic.as_ref() {
                Some(semantic) => Some(std::sync::Arc::new(
                    crate::embed::RuntimeEmbedder::open(semantic)
                        .map_err(|reason| StartupError::Embedder { reason })?,
                )),
                None => None,
            };
        #[cfg(not(feature = "semantic"))]
        if config.semantic.is_some() {
            return Err(StartupError::UnsupportedBackend {
                backend: "semantic cache",
                feature: "semantic",
            });
        }
        let stores = open_store(&config.storage).await?;
        let store = stores.backend();
        let backend: Arc<dyn Backend> = Arc::clone(&store);
        let published = open_cache(&config.cache).await?;
        let feed = Arc::new(RuleFeed::new(Arc::clone(&store), Arc::clone(&published))?);
        // A cache that cannot take the rules yet leaves this node serving what storage holds.
        if let Err(error) = feed.heal().await {
            tracing::warn!(%error, "could not publish the rules at startup; healing will");
        }
        let counters = Arc::clone(&published) as Arc<dyn Counters>;
        let cache = published as Arc<dyn Cache>;
        // The revision is read first, so a change that lands while the table is read is
        // followed rather than missed.
        let started_at = ip_storage::RevisionStore::rule_revision(store.as_ref()).await?;
        let table = RoutingTable::load(config, store.as_ref()).await?;
        let configured = config
            .upstreams
            .iter()
            .map(|upstream| upstream.route_rule(config.server.listen))
            .collect::<Result<Vec<_>, _>>()
            .map_err(RouteError::from)?;
        let host = Arc::new(PluginHost::new(PluginLimits::default())?);
        let chains = PluginChains::load(Arc::clone(&host), store.as_ref(), store.as_ref()).await?;
        tracing::info!(rules = chains.len(), "plugin chains built");
        let upstream = Arc::new(HyperUpstream::new()?);
        let gateway = Gateway::with_chains(table, Arc::new(chains), upstream);
        let limits = Limits::new(ip_storage::LimitStore::limits(store.as_ref()).await?);
        tracing::info!(limits = limits.len(), "limits in force");
        let limiter = Limiter::new(
            Arc::clone(&counters),
            Arc::clone(&backend) as Arc<dyn ip_storage::UsageStore>,
        );
        let (dispatcher, folding) = Dispatcher::new();
        // The folder stops by itself once the last request that could teach it anything is gone.
        let _folding = folding.keep_folding();
        let mut gateway = gateway
            .recording(Arc::clone(&backend) as Arc<dyn ip_storage::UsageStore>)
            .limiting(Arc::new(limiter))
            .dispatching(Arc::new(dispatcher))
            .sticky_turns(Arc::clone(&cache))
            .with_limits(limits);
        if let Some(judge) = judge {
            tracing::info!("classifying the opening of turns that name no class");
            gateway = gateway.classifying(judge);
        }
        // The larger model the judge's margin refers to, when one is configured: asked
        // through this gateway's own routing, once per turn, and never waited for past
        // the second its config gives it.
        if let Some(arbiter) = &config.arbiter {
            // Parsing already refused an unresolved name; the lookup stays total rather
            // than unwrapping.
            let asked = config
                .upstreams
                .iter()
                .find(|upstream| upstream.id == arbiter.upstream)
                .ok_or(ip_config::ConfigError::ArbiterUpstream {
                    id: arbiter.upstream.clone(),
                })?;
            gateway = gateway.arbitrating(Arc::new(ip_gateway::Arbiter::new(
                arbiter.model.clone(),
                arbiter.ask_path.clone(),
                std::time::Duration::from_secs(arbiter.timeout.get()),
                asked.api_key.clone(),
            )));
            tracing::info!("arbitrating the openings the classifier cannot call");
        }
        // The operator's floor: applied to every request and every answer, whoever is
        // asking, and not reachable by anything a tenant configures.
        if let Some(filter) = &config.filter {
            let secrets = filter.secrets()?;
            // A secret that will not compile is a fault in the configuration, and reads as one.
            let compile = |secrets: &[ip_filter::Secret]| {
                ip_filter::Detectors::of(secrets).map_err(ip_config::ConfigError::from)
            };
            let named = secrets
                .iter()
                .map(|secret| secret.kind.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let mut filtering =
                ip_gateway::Filtering::new(compile(&secrets)?).holding(filter.window);
            // Built once, per tenant that asked for anything, so a request only ever
            // looks one up. Each set is the floor and that tenant's own together.
            for tenant in filter.named_tenants() {
                let theirs = filter.secrets_for(tenant)?;
                filtering = filtering.and_tenant(tenant.clone(), compile(&theirs)?);
                tracing::info!(
                    tenant = %tenant,
                    secrets = theirs.len(),
                    "a tenant asked for more than the floor"
                );
            }
            gateway = gateway.filtering(Arc::new(filtering));
            tracing::info!(
                secrets = named,
                window = filter.window,
                "taking out of every request and answer what must not leave"
            );
        }
        if let Some(ttl) = config.cache.response_ttl() {
            let responses = ResponseCache::new(Arc::clone(&cache), Ttl::new(ttl.as_duration())?);
            gateway = gateway.caching(responses);
            tracing::info!(
                seconds = ttl.get(),
                "answering repeated requests from the cache"
            );
        }
        #[cfg(feature = "semantic")]
        if let Some(encoder) = encoder {
            // Unwrapped against the stanza that opened the encoder: the two are
            // present together or neither is, which the arm above establishes.
            let semantic = config
                .semantic
                .as_ref()
                .unwrap_or_else(|| unreachable!("an encoder is opened only from a stanza"));
            let settings = ip_gateway::SemanticSettings {
                nearness: ip_embed::Nearness::new(semantic.nearness).map_err(|reason| {
                    StartupError::Embedder {
                        reason: reason.to_string(),
                    }
                })?,
                ttl: Ttl::new(semantic.ttl.as_duration())?,
                width: encoder.width(),
                capacity: ip_embed::Capacity::new(semantic.capacity).map_err(|reason| {
                    StartupError::Embedder {
                        reason: reason.to_string(),
                    }
                })?,
                model: encoder.model().to_owned(),
            };
            let by_meaning = ip_gateway::SemanticCache::new(Arc::clone(&cache), settings);
            tracing::info!(
                nearness = semantic.nearness,
                seconds = semantic.ttl.get(),
                capacity = semantic.capacity,
                "answering a question asked before in other words"
            );
            gateway = gateway.recognising(Arc::new(by_meaning), encoder);
        }
        let nonce_ttl = Ttl::new(config.auth.nonce_ttl.as_duration())?;
        let store = Arc::clone(&backend) as Arc<dyn Storage>;
        let tokens = SessionTokens::new(
            &config.auth.jwt.issuer,
            config.auth.jwt.secret.expose().as_bytes(),
            config.auth.jwt.ttl.as_duration(),
        );
        let logins = Logins::new(Arc::clone(&store), Arc::clone(&cache), tokens)?;
        let gateway = Arc::new(gateway);
        let configured: Arc<[RouteRule]> = configured.into();
        let reloader = Arc::new(Reloader::new(
            Arc::clone(&feed),
            Arc::clone(&backend),
            host,
            Arc::clone(&gateway),
            Arc::clone(&configured),
        ));
        Ok(Self {
            stores,
            store: backend,
            verifier: RequestVerifier::new(store, cache, nonce_ttl),
            logins: Arc::new(logins),
            session_seconds: config.auth.jwt.ttl.get(),
            gateway,
            listen: config.server.listen,
            configured,
            feed,
            reloader,
            started_at,
            #[cfg(test)]
            failing_store: None,
            #[cfg(test)]
            failing_cache: None,
        })
    }

    /// Builds state over a store and gateway that are already built, on a cache of its own.
    #[cfg(test)]
    pub fn with_parts(stores: Stores, gateway: Gateway, listen: SocketAddr) -> Self {
        let failing_cache = Arc::new(ip_cache::FailingCache::new(Arc::new(
            ip_cache::SledCache::temporary().expect("a temporary cache"),
        )));
        let published: Arc<dyn CacheBackend> = Arc::clone(&failing_cache) as Arc<dyn CacheBackend>;
        let failing_store = Arc::new(ip_storage::FailingStore::new(stores.backend()));
        let backend: Arc<dyn Backend> = Arc::clone(&failing_store) as Arc<dyn Backend>;
        let feed =
            Arc::new(RuleFeed::new(Arc::clone(&backend), Arc::clone(&published)).expect("a feed"));
        let gateway = Arc::new(gateway);
        let host = Arc::new(
            ip_plugin::PluginHost::on_demand(PluginLimits::default()).expect("a plugin host"),
        );
        let reloader = Arc::new(Reloader::new(
            Arc::clone(&feed),
            Arc::clone(&backend),
            host,
            Arc::clone(&gateway),
            Arc::new([]),
        ));
        let cache = published as Arc<dyn Cache>;
        let store = Arc::clone(&backend) as Arc<dyn Storage>;
        let tokens = SessionTokens::new(
            "intelli-prism",
            b"0123456789abcdef0123456789abcdef",
            std::time::Duration::from_secs(3600),
        );
        let logins = Logins::new(Arc::clone(&store), cache.clone(), tokens).expect("logins");
        Self {
            stores,
            store: backend,
            verifier: RequestVerifier::new(store, cache, Ttl::seconds(300).expect("a nonce ttl")),
            logins: Arc::new(logins),
            session_seconds: 3600,
            gateway,
            listen,
            configured: Arc::new([]),
            feed,
            reloader,
            started_at: RuleRevision::new(0),
            failing_store: Some(failing_store),
            failing_cache: Some(failing_cache),
        }
    }

    /// The store every read goes through, which a test can tell to fail.
    #[cfg(test)]
    pub fn store_failure(&self) -> &ip_storage::FailingStore {
        self.failing_store
            .as_ref()
            .expect("state built for a test reads through a store that can fail")
    }

    /// The cache every read goes through, which a test can tell to fail.
    #[cfg(test)]
    pub fn cache_failure(&self) -> &ip_cache::FailingCache {
        self.failing_cache
            .as_ref()
            .expect("state built for a test reads through a cache that can fail")
    }

    /// The same state, as though the configuration file had contributed these rules.
    #[cfg(test)]
    pub fn configuring(mut self, rules: Vec<RouteRule>) -> Self {
        self.configured = rules.into();
        self
    }

    /// Checks request signatures against the store.
    pub fn verifier(&self) -> &RequestVerifier {
        &self.verifier
    }

    /// The store in the shape its backend has, for the transactions that need it.
    pub fn stores(&self) -> &Stores {
        &self.stores
    }

    /// Everything the store holds: the tenants, users and grants the management api works
    /// on, and the routes, plugins and revision the gateway is built from. Where only the
    /// identity half is wanted, it is the same handle read as `&dyn Storage`.
    pub fn store(&self) -> &Arc<dyn Backend> {
        &self.store
    }

    /// Opens and ends the sessions the web carries.
    pub fn logins(&self) -> &Logins {
        &self.logins
    }

    /// How long a session cookie is kept, which is how long its token is good for.
    pub fn session_seconds(&self) -> u64 {
        self.session_seconds
    }

    /// Carries proxied requests through the dataflow.
    pub fn gateway(&self) -> &Gateway {
        &self.gateway
    }

    /// Where the gateway listens, supplying the port a `Host` header omits.
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Publishes the rules for every node, and follows what is published.
    pub fn feed(&self) -> &Arc<RuleFeed> {
        &self.feed
    }

    /// Follows the published rules from the revision this node started at, putting each
    /// change in force as it lands.
    pub fn keep_rules_in_step(&self) -> tokio::task::JoinHandle<()> {
        Arc::clone(&self.reloader).keep_following(self.started_at)
    }

    /// Puts the rules published now in force, which is what a change leads to.
    #[cfg(test)]
    pub async fn reload_rules(&self) -> Result<Option<RuleRevision>, crate::error::FeedError> {
        self.reloader.reload().await
    }

    /// The rules the configuration file contributes, which no stored rule can displace.
    pub fn configured(&self) -> &[RouteRule] {
        &self.configured
    }
}

pub(crate) async fn open_store(config: &StorageConfig) -> Result<Stores, StartupError> {
    match config {
        #[cfg(feature = "standalone-storage")]
        StorageConfig::Sqlite { path } => {
            Ok(Stores::Sqlite(Arc::new(SqliteStore::open(path).await?)))
        }
        #[cfg(not(feature = "standalone-storage"))]
        StorageConfig::Sqlite { .. } => Err(StartupError::UnsupportedBackend {
            backend: "sqlite",
            feature: "standalone-storage",
        }),
        #[cfg(feature = "fast-storage")]
        StorageConfig::Postgres { url } => Ok(Stores::Postgres(Arc::new(
            PostgresStore::open(url.expose()).await?,
        ))),
        #[cfg(not(feature = "fast-storage"))]
        StorageConfig::Postgres { .. } => Err(StartupError::UnsupportedBackend {
            backend: "postgres",
            feature: "fast-storage",
        }),
    }
}

async fn open_cache(config: &CacheConfig) -> Result<Arc<dyn CacheBackend>, StartupError> {
    match config {
        #[cfg(feature = "standalone-cache")]
        CacheConfig::Sled {
            path,
            response_max_bytes,
            semantic_max_bytes,
            system_max_bytes,
            ..
        } => {
            let mut limits = LevelLimits::none();
            if let Some(bytes) = response_max_bytes {
                limits = limits.with_response(MaxBytes::new(*bytes)?);
            }
            if let Some(bytes) = semantic_max_bytes {
                limits = limits.with_semantic(MaxBytes::new(*bytes)?);
            }
            if let Some(bytes) = system_max_bytes {
                limits = limits.with_system(MaxBytes::new(*bytes)?);
            }
            Ok(Arc::new(SledCache::with_limits(path, limits)?))
        }
        #[cfg(not(feature = "standalone-cache"))]
        CacheConfig::Sled { .. } => Err(StartupError::UnsupportedBackend {
            backend: "sled",
            feature: "standalone-cache",
        }),
        #[cfg(feature = "cluster-cache")]
        CacheConfig::Redis { url, .. } => Ok(Arc::new(RedisCache::connect(url.expose()).await?)),
        #[cfg(not(feature = "cluster-cache"))]
        CacheConfig::Redis { .. } => Err(StartupError::UnsupportedBackend {
            backend: "redis",
            feature: "cluster-cache",
        }),
    }
}

#[cfg(test)]
mod tests {
    use ip_config::Config;

    use super::*;

    fn config_with(storage: &str, cache: &str) -> Config {
        Config::parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8080"

{storage}

{cache}

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
"#
        ))
        .unwrap()
    }

    /// A cache block naming `path`, which sled creates when the cache is opened.
    fn sled_cache(path: &std::path::Path) -> String {
        format!("[cache]\nbackend = \"sled\"\npath = {:?}", path.display())
    }

    /// A path under the temporary directory, distinct per test and per process.
    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ip-{tag}-{}", std::process::id()))
    }

    /// A schema of this test's own on the database `DATABASE_URL` names, so a startup that
    /// migrates cannot touch what anything else is using.
    #[cfg(feature = "fast-storage")]
    struct Schema {
        name: String,
        url: String,
        server: String,
    }

    #[cfg(feature = "fast-storage")]
    impl Schema {
        async fn create(url: &str) -> Self {
            use sqlx::{AssertSqlSafe, Connection, PgConnection};

            let name = format!("ip_startup_{}", std::process::id());
            let mut connection = PgConnection::connect(url).await.expect("a postgres server");
            sqlx::query(AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {name} CASCADE"
            )))
            .execute(&mut connection)
            .await
            .unwrap();
            sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {name}")))
                .execute(&mut connection)
                .await
                .unwrap();
            connection.close().await.unwrap();
            Self {
                url: format!("{url}?options=-c%20search_path%3D{name}"),
                name,
                server: url.to_owned(),
            }
        }
    }

    #[cfg(feature = "fast-storage")]
    impl Drop for Schema {
        fn drop(&mut self) {
            use sqlx::{AssertSqlSafe, Connection, PgConnection};

            let sql = format!("DROP SCHEMA {} CASCADE", self.name);
            let server = self.server.clone();
            // A drop inside the test's runtime cannot wait on that runtime, so it waits on its own.
            let dropped = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime to drop a scratch schema")
                    .block_on(async move {
                        let mut connection = PgConnection::connect(&server).await?;
                        sqlx::query(AssertSqlSafe(sql))
                            .execute(&mut connection)
                            .await?;
                        connection.close().await
                    })
            })
            .join();
            if !matches!(dropped, Ok(Ok(()))) {
                eprintln!("could not drop scratch schema {}", self.name);
            }
        }
    }

    #[cfg(all(feature = "fast-storage", feature = "cluster-cache"))]
    #[tokio::test]
    async fn opening_the_cluster_backends_builds_a_routing_table() {
        let (Ok(database), Ok(redis)) = (std::env::var("DATABASE_URL"), std::env::var("REDIS_URL"))
        else {
            eprintln!("DATABASE_URL and REDIS_URL are not both set, so this test checks nothing");
            return;
        };
        let schema = Schema::create(&database).await;
        let config = config_with(
            &format!("[storage]\nbackend = \"postgres\"\nurl = {:?}", schema.url),
            &format!("[cache]\nbackend = \"redis\"\nurl = {redis:?}"),
        );
        let state = AppState::open(&config).await.unwrap();
        assert_eq!(state.listen(), config.server.listen);
        assert!(state.gateway().table().is_empty());
    }

    #[cfg(not(feature = "classifier"))]
    #[tokio::test]
    async fn a_classifier_this_build_does_not_carry_stops_startup() {
        let dir = scratch("unbuilt-classifier");
        let config = Config::parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "/nonexistent/ip.db"

{}

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[difficulty]
artifact = {artifact:?}
confidence = 0.4
"#,
            sled_cache(&scratch("unopened-classifier-cache")),
            artifact = dir.display().to_string(),
        ))
        .expect("the file itself is well-formed");
        assert!(matches!(
            AppState::open(&config).await,
            Err(StartupError::UnsupportedBackend {
                backend: "difficulty classifier",
                feature: "classifier",
            })
        ));
    }

    #[cfg(feature = "classifier")]
    #[tokio::test]
    async fn an_artefact_that_is_not_one_stops_startup() {
        let dir = scratch("no-such-artefact");
        let config = Config::parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "/nonexistent/ip.db"

{}

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[difficulty]
artifact = {artifact:?}
confidence = 0.4
"#,
            sled_cache(&scratch("unopened-classifier-cache")),
            artifact = dir.display().to_string(),
        ))
        .expect("the file itself is well-formed");
        let Err(StartupError::Classifier { reason }) = AppState::open(&config).await else {
            panic!("a directory with no model in it must refuse startup");
        };
        assert!(reason.contains("model.onnx"), "{reason}");
    }

    /// A semantic section in a build with no encoder in it is refused, never ignored.
    ///
    /// Silently ignoring it is the worst of the three possible behaviours: a level
    /// that never hits is indistinguishable from one nobody's traffic suits, so an
    /// operator would read a working gateway and a flat hit rate and conclude the
    /// feature does not pay — having never once run it.
    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn a_semantic_cache_this_build_does_not_carry_stops_startup() {
        let config = Config::parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "/nonexistent/ip.db"

{}

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[semantic]
artifact = {artifact:?}
nearness = 0.97
"#,
            sled_cache(&scratch("unopened-semantic-cache")),
            artifact = scratch("unbuilt-semantic").display().to_string(),
        ))
        .expect("the file itself is well-formed");
        assert!(matches!(
            AppState::open(&config).await,
            Err(StartupError::UnsupportedBackend {
                backend: "semantic cache",
                feature: "semantic",
            })
        ));
    }

    /// And a build that does carry one still refuses a directory with no model in it,
    /// rather than coming up with the level quietly off.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn an_embedding_artefact_that_is_not_one_stops_startup() {
        let config = Config::parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "/nonexistent/ip.db"

{}

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[semantic]
artifact = {artifact:?}
nearness = 0.97
"#,
            sled_cache(&scratch("unopened-semantic-cache")),
            artifact = scratch("no-such-embedding-artefact").display().to_string(),
        ))
        .expect("the file itself is well-formed");
        let Err(StartupError::Embedder { reason }) = AppState::open(&config).await else {
            panic!("a directory with no model in it must refuse startup");
        };
        assert!(reason.contains("model.onnx"), "{reason}");
    }

    /// The whole level wired into a standing gateway, against a real export, for
    /// whoever runs with `IP_EMBED_ARTIFACT`. Without one it says so and passes.
    ///
    /// What it is really checking is the assembly between the manifest and the cache:
    /// the nearness, the span and the bound all come out of the stanza as types the
    /// cache takes, and the width and the model's digest come out of the artefact.
    /// Every one of those is a conversion that can refuse, and a refusal here is a
    /// gateway that will not start.
    #[cfg(all(feature = "semantic", feature = "standalone-storage"))]
    #[tokio::test]
    async fn a_real_embedding_artefact_wires_the_level_into_the_gateway() {
        let Ok(artifact) = std::env::var("IP_EMBED_ARTIFACT") else {
            eprintln!("IP_EMBED_ARTIFACT unset: no artefact to wire up");
            return;
        };
        let library = std::env::var("ORT_DYLIB_PATH")
            .map(|path| format!("library = {path:?}\n"))
            .unwrap_or_default();
        let config = Config::parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = {store:?}

{cache}

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[semantic]
artifact = {artifact:?}
nearness = 0.97
ttl = 900
capacity = 128
{library}"#,
            store = scratch("semantic-wired.db").display().to_string(),
            cache = sled_cache(&scratch("semantic-wired-cache")),
        ))
        .expect("the file itself is well-formed");
        let state = AppState::open(&config)
            .await
            .expect("the gateway stands up");
        assert_eq!(state.listen(), config.server.listen);
        assert!(
            state.gateway().recognises(),
            "the level was built and then not attached, which fails exactly the way \
             never configuring it does"
        );
    }

    #[cfg(not(feature = "fast-storage"))]
    #[tokio::test]
    async fn a_backend_this_build_does_not_carry_stops_startup() {
        let config = config_with(
            "[storage]\nbackend = \"postgres\"\nurl = \"postgres://ip@db/ip\"",
            &sled_cache(&scratch("unopened-cache")),
        );
        assert!(matches!(
            AppState::open(&config).await,
            Err(StartupError::UnsupportedBackend {
                backend: "postgres",
                feature: "fast-storage",
            })
        ));
    }

    #[cfg(not(feature = "standalone-storage"))]
    #[tokio::test]
    async fn a_sqlite_backend_this_build_does_not_carry_stops_startup() {
        let config = config_with(
            "[storage]\nbackend = \"sqlite\"\npath = \"/nonexistent/ip.db\"",
            &sled_cache(&scratch("unopened-cache")),
        );
        assert!(matches!(
            AppState::open(&config).await,
            Err(StartupError::UnsupportedBackend {
                backend: "sqlite",
                feature: "standalone-storage",
            })
        ));
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn opening_the_sqlite_backend_builds_a_routing_table() {
        let path = std::env::temp_dir().join(format!("ip-state-{}.db", std::process::id()));
        let cache = scratch("state-cache");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &sled_cache(&cache),
        );
        let state = AppState::open(&config).await.unwrap();
        assert_eq!(state.listen(), config.server.listen);
        assert!(state.gateway().table().is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn opening_remembers_the_rules_the_configuration_contributes() {
        let path = std::env::temp_dir().join(format!("ip-configured-{}.db", std::process::id()));
        let cache = scratch("configured-cache");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
        let upstream = "[[upstream]]\nid = \"anthropic\"\nbase_url = \"https://api.anthropic.com/\"\n\
                        api_key = \"sk-test\"\nmodel = \"claude-opus-5\"";
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &format!("{}\n\n{upstream}", sled_cache(&cache)),
        );
        let state = AppState::open(&config).await.unwrap();
        assert_eq!(state.configured().len(), 1);
        assert_eq!(state.configured()[0].api.as_str(), "anthropic");
        assert_eq!(state.gateway().table().len(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn a_level_limit_of_no_bytes_stops_startup() {
        let path = std::env::temp_dir().join(format!("ip-nolimit-{}.db", std::process::id()));
        let cache = scratch("nolimit-cache");
        let _ = std::fs::remove_file(&path);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &format!(
                "[cache]\nbackend = \"sled\"\npath = {:?}\nresponse_max_bytes = 0",
                cache.display()
            ),
        );
        let outcome = AppState::open(&config).await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
        assert!(matches!(
            outcome,
            Err(StartupError::Cache(ip_cache::CacheError::ZeroLimit))
        ));
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn a_response_ttl_of_no_time_stops_startup() {
        let path = std::env::temp_dir().join(format!("ip-nottl-{}.db", std::process::id()));
        let cache = scratch("nottl-cache");
        let _ = std::fs::remove_file(&path);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &format!(
                "[cache]\nbackend = \"sled\"\npath = {:?}\nresponse_ttl = 0",
                cache.display()
            ),
        );
        let outcome = AppState::open(&config).await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
        assert!(matches!(
            outcome,
            Err(StartupError::Cache(ip_cache::CacheError::ZeroTtl))
        ));
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn a_cache_answering_responses_under_every_limit_opens() {
        let path = std::env::temp_dir().join(format!("ip-limited-{}.db", std::process::id()));
        let cache = scratch("limited-cache");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &format!(
                "[cache]\nbackend = \"sled\"\npath = {:?}\nresponse_ttl = 60\n\
                 response_max_bytes = 4096\nsemantic_max_bytes = 4096\nsystem_max_bytes = 4096",
                cache.display()
            ),
        );
        let state = AppState::open(&config).await.unwrap();
        assert_eq!(state.listen(), config.server.listen);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn the_opened_cache_is_the_one_the_configuration_named() {
        let path = std::env::temp_dir().join(format!("ip-cached-{}.db", std::process::id()));
        let cache = scratch("named-cache");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &sled_cache(&cache),
        );
        let state = AppState::open(&config).await.unwrap();
        assert_eq!(state.listen(), config.server.listen);
        assert!(cache.is_dir(), "sled opened the directory the cache named");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[cfg(all(feature = "standalone-storage", feature = "cluster-cache"))]
    #[tokio::test]
    async fn opening_the_redis_cache_needs_only_the_url_it_was_given() {
        let Ok(url) = std::env::var("REDIS_URL") else {
            eprintln!("REDIS_URL is unset, so this redis test checks nothing");
            return;
        };
        let path = std::env::temp_dir().join(format!("ip-redis-state-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &format!("[cache]\nbackend = \"redis\"\nurl = {url:?}"),
        );
        let state = AppState::open(&config).await.unwrap();
        assert_eq!(state.listen(), config.server.listen);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(all(feature = "standalone-storage", not(feature = "cluster-cache")))]
    #[tokio::test]
    async fn a_cache_backend_this_build_does_not_carry_stops_startup() {
        let path = std::env::temp_dir().join(format!("ip-nocache-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            "[cache]\nbackend = \"redis\"\nurl = \"redis://cache:6379\"",
        );
        let outcome = AppState::open(&config).await;
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            outcome,
            Err(StartupError::UnsupportedBackend {
                backend: "redis",
                feature: "cluster-cache",
            })
        ));
    }

    #[cfg(all(feature = "standalone-storage", not(feature = "standalone-cache")))]
    #[tokio::test]
    async fn a_sled_cache_this_build_does_not_carry_stops_startup() {
        let path = std::env::temp_dir().join(format!("ip-nosled-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &sled_cache(&scratch("never-opened")),
        );
        let outcome = AppState::open(&config).await;
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            outcome,
            Err(StartupError::UnsupportedBackend {
                backend: "sled",
                feature: "standalone-cache",
            })
        ));
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn a_global_plugin_that_will_not_load_stops_startup() {
        use ip_core::{NewPluginRule, PluginKind, PluginOrder, PluginScope};
        use ip_plugin::ChainError;
        use ip_storage::{NewPlugin, PluginOwner, PluginRuleStore, PluginStore};

        let path = std::env::temp_dir().join(format!("ip-broken-plugin-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path).await.unwrap();
        let broken = store
            .put_plugin(NewPlugin {
                kind: PluginKind::ReqBody,
                wasm: wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap(),
                owner: PluginOwner::Global,
            })
            .await
            .unwrap();
        store
            .put_rule(
                NewPluginRule::new(broken.checksum, PluginOrder::new(10), PluginScope::Global)
                    .unwrap(),
            )
            .await
            .unwrap();
        store.close().await;
        let cache = scratch("plugin-cache");
        let _ = std::fs::remove_dir_all(&cache);
        let config = config_with(
            &format!(
                "[storage]\nbackend = \"sqlite\"\npath = {:?}",
                path.display().to_string()
            ),
            &sled_cache(&cache),
        );
        let outcome = AppState::open(&config).await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&cache);
        assert!(matches!(
            outcome,
            Err(StartupError::Chains(ChainError::GlobalPlugin { .. }))
        ));
    }
}
