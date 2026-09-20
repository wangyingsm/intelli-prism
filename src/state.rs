use std::net::SocketAddr;
use std::sync::Arc;

use ip_auth::{Logins, RequestVerifier, SessionTokens};
#[cfg(feature = "cluster-cache")]
use ip_cache::RedisCache;
use ip_cache::{Cache, Ttl};
#[cfg(feature = "standalone-cache")]
use ip_cache::{LevelLimits, MaxBytes, SledCache};
use ip_config::{CacheConfig, Config, StorageConfig};
use ip_gateway::{Gateway, HyperUpstream, ResponseCache, RoutingTable};
use ip_plugin::{PluginChains, PluginHost, PluginLimits};
#[cfg(feature = "fast-storage")]
use ip_storage::PostgresStore;
#[cfg(feature = "standalone-storage")]
use ip_storage::SqliteStore;
use ip_storage::{Backend, Storage};

use crate::error::StartupError;

/// What every handler is given.
#[derive(Clone)]
pub struct AppState {
    verifier: RequestVerifier,
    logins: Arc<Logins>,
    session_seconds: u64,
    gateway: Arc<Gateway>,
    listen: SocketAddr,
}

impl AppState {
    /// Opens the configured backend, then builds the routing table, the plugin chains and the
    /// upstream client.
    pub async fn open(config: &Config) -> Result<Self, StartupError> {
        let store = open_store(&config.storage).await?;
        let cache = open_cache(&config.cache).await?;
        let table = RoutingTable::load(config, store.as_ref()).await?;
        let host = Arc::new(PluginHost::new(PluginLimits::default())?);
        let chains = PluginChains::load(host, store.as_ref(), store.as_ref()).await?;
        tracing::info!(rules = chains.len(), "plugin chains built");
        let upstream = Arc::new(HyperUpstream::new()?);
        let mut gateway = Gateway::with_chains(table, Arc::new(chains), upstream);
        if let Some(ttl) = config.cache.response_ttl() {
            let responses = ResponseCache::new(Arc::clone(&cache), Ttl::new(ttl.as_duration())?);
            gateway = gateway.caching(responses);
            tracing::info!(
                seconds = ttl.get(),
                "answering repeated requests from the cache"
            );
        }
        let nonce_ttl = Ttl::new(config.auth.nonce_ttl.as_duration())?;
        let store = store as Arc<dyn Storage>;
        let tokens = SessionTokens::new(
            &config.auth.jwt.issuer,
            config.auth.jwt.secret.expose().as_bytes(),
            config.auth.jwt.ttl.as_duration(),
        );
        let logins = Logins::new(Arc::clone(&store), Arc::clone(&cache), tokens)?;
        Ok(Self {
            verifier: RequestVerifier::new(store, cache, nonce_ttl),
            logins: Arc::new(logins),
            session_seconds: config.auth.jwt.ttl.get(),
            gateway: Arc::new(gateway),
            listen: config.server.listen,
        })
    }

    /// Builds state over a store and gateway that are already built, on a cache of its own.
    #[cfg(test)]
    pub fn with_parts(store: Arc<dyn Storage>, gateway: Gateway, listen: SocketAddr) -> Self {
        let cache = Arc::new(ip_cache::SledCache::temporary().expect("a temporary cache"));
        let tokens = SessionTokens::new(
            "intelli-prism",
            b"0123456789abcdef0123456789abcdef",
            std::time::Duration::from_secs(3600),
        );
        let logins = Logins::new(Arc::clone(&store), cache.clone(), tokens).expect("logins");
        Self {
            verifier: RequestVerifier::new(store, cache, Ttl::seconds(300).expect("a nonce ttl")),
            logins: Arc::new(logins),
            session_seconds: 3600,
            gateway: Arc::new(gateway),
            listen,
        }
    }

    /// Checks request signatures against the store.
    pub fn verifier(&self) -> &RequestVerifier {
        &self.verifier
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
}

pub(crate) async fn open_store(config: &StorageConfig) -> Result<Arc<dyn Backend>, StartupError> {
    match config {
        #[cfg(feature = "standalone-storage")]
        StorageConfig::Sqlite { path } => Ok(Arc::new(SqliteStore::open(path).await?)),
        #[cfg(not(feature = "standalone-storage"))]
        StorageConfig::Sqlite { .. } => Err(StartupError::UnsupportedBackend {
            backend: "sqlite",
            feature: "standalone-storage",
        }),
        #[cfg(feature = "fast-storage")]
        StorageConfig::Postgres { url } => Ok(Arc::new(PostgresStore::open(url.expose()).await?)),
        #[cfg(not(feature = "fast-storage"))]
        StorageConfig::Postgres { .. } => Err(StartupError::UnsupportedBackend {
            backend: "postgres",
            feature: "fast-storage",
        }),
    }
}

async fn open_cache(config: &CacheConfig) -> Result<Arc<dyn Cache>, StartupError> {
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
        use ip_storage::{NewPlugin, PluginRuleStore, PluginStore};

        let path = std::env::temp_dir().join(format!("ip-broken-plugin-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path).await.unwrap();
        let broken = store
            .put_plugin(NewPlugin {
                kind: PluginKind::ReqBody,
                wasm: wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap(),
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
