//! The memory one chat turn keeps of where it was served (§429).
//!
//! A turn asks its endpoints the same question more than once, and an endpoint that already
//! looked at it has the context the next one would have to build. So the endpoint one request
//! landed on is where the requests after it go, as long as the turn lasts. What is remembered is
//! short-lived state the gateway keeps for itself, which is the kind the cache's own module says
//! belongs in the cache; it is written through the same handle the response cache uses.
//!
//! The verdict a turn leaves carries the class the request was asked at as well as the endpoint,
//! though nothing acts on the class yet: it is what a classifier, the change asked for after this
//! one, would send back, and putting it here now is why that change needs no new write.

use std::sync::Arc;
use std::time::Duration;

use ip_auth::Identity;
use ip_cache::{Cache, CacheKey, CacheLevel, Ttl};
use ip_core::{Difficulty, Host, Port, Replica, TurnId};

use crate::flow::record::Pending;

/// A turn keeps its endpoint for fifteen minutes. It is a number, not a law: longer than a
/// turn that stops to think between its requests, and shorter than the wait after which any
/// endpoint at all is likelier to beat the one that answered last.
const TURN_TTL: Duration = Duration::from_secs(15 * 60);

/// The class slot of a turn that named none. A name is always written, because the value is
/// read as a whole; a turn that named none has a slot too, and no class is not any class name.
const NO_CLASS: &str = "-";

/// Where one caller's turn landed: what the memory of §429 keeps, and reads back whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The class the request that landed was asked at, if it named one.
    pub class: Option<Difficulty>,
    /// The endpoint it landed on.
    pub endpoint: Replica,
}

impl Verdict {
    /// The two halves with their separator: a space is in neither a class name nor a host, and
    /// the endpoint keeps the form `Replica` displays itself with.
    fn encode(&self) -> String {
        let class = self.class.map_or(NO_CLASS, Difficulty::name);
        format!("{class} {}", self.endpoint)
    }

    /// A value this read cannot make a verdict of — a name no class has, an endpoint no host
    /// was written for — is not an error to answer with but a verdict from a gateway of another
    /// version, which reads as the miss it always was.
    fn decode(stored: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(stored).ok()?;
        let (class, endpoint) = text.split_once(' ')?;
        let class = match class {
            NO_CLASS => None,
            named => Some(Difficulty::named(named).ok()?),
        };
        let (host, port) = endpoint.rsplit_once(':')?;
        let port = port.parse::<u16>().ok()?;
        Some(Self {
            class,
            endpoint: Replica {
                host: Host::new(host).ok()?,
                port: Port::new(port).ok()?,
            },
        })
    }
}

/// Where a turn's memory is kept, and how long a turn keeps it (§429).
#[derive(Clone)]
pub struct TurnAffinity {
    turns: Arc<dyn Cache>,
    pending: Pending,
    ttl: Ttl,
}

impl TurnAffinity {
    /// The memory of every turn, kept in `turns` and written alongside the answers that earned
    /// it. The TTL is the constant the turn is meant to last; no gateway config asks otherwise.
    pub fn new(turns: Arc<dyn Cache>, pending: &Pending) -> Self {
        Self {
            turns,
            pending: pending.clone(),
            ttl: Ttl::new(TURN_TTL)
                .unwrap_or_else(|_| unreachable!("fifteen minutes is a span, not nothing")),
        }
    }

    /// The endpoint this turn landed on, if the memory holds one.
    ///
    /// Affinity keeps a turn from re-building context at an endpoint that has it already; it
    /// guards a latency, not a billing, and so a memory that cannot answer costs it the
    /// landing and not the request. That is the opposite of the quota of M8, which reads a
    /// plan's numbers and refuses when it cannot; this one warns and dispatches as a first one.
    pub async fn landed(&self, identity: &Identity, turn: &TurnId) -> Option<Verdict> {
        match self.turns.get(&key(identity, turn)).await {
            Ok(Some(stored)) => Verdict::decode(&stored),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "the turn memory could not answer; dispatching as a first request");
                None
            }
        }
    }

    /// Keeps where this request landed as where its turn landed, freshening the wait until it
    /// stops being one. The write goes out the way the usage row does — off the path the answer
    /// travels, counted so a shutdown still waits for it — because the answer must not wait on
    /// where it came from. A memory that will not take the write has lost the landing, which is
    /// the whole of the damage.
    pub fn keep(&self, identity: &Identity, turn: &TurnId, verdict: &Verdict) {
        let (key, value) = (key(identity, turn), verdict.encode());
        let turns = Arc::clone(&self.turns);
        let ttl = self.ttl;
        self.pending.writing(async move {
            if let Err(error) = turns.put(&key, value.as_bytes(), Some(ttl)).await {
                tracing::warn!(%error, "a turn's landing could not be kept");
            }
        });
    }
}

/// A turn belongs to one caller under one tenant; another caller's turn, though its id were
/// somehow the same, is not it.
fn key(identity: &Identity, turn: &TurnId) -> CacheKey {
    CacheKey::new(
        CacheLevel::System,
        &format!("turn:{}:{}:{}", identity.tenant, identity.user, turn),
    )
    .unwrap_or_else(|_| unreachable!("an id holds nothing a key would refuse"))
}

#[cfg(test)]
mod tests {
    use ip_cache::FailingCache;
    use ip_core::{Role, TenantId, UserId};

    use super::*;

    fn caller(tenant: &str, user: &str) -> Identity {
        Identity {
            user: UserId::new(user).unwrap(),
            tenant: TenantId::new(tenant).unwrap(),
            role: Role::Member,
        }
    }

    fn turn(id: &str) -> TurnId {
        TurnId::new(id).unwrap()
    }

    fn landed_on(host: &str) -> Verdict {
        Verdict {
            class: None,
            endpoint: Replica {
                host: Host::new(host).unwrap(),
                port: Port::new(443).unwrap(),
            },
        }
    }

    /// A memory over its own store, and the handle its writes are counted against: reading
    /// back a kept turn means waiting for the write the answer did not wait for.
    fn memory() -> (TurnAffinity, Pending) {
        let pending = Pending::default();
        let affinity = TurnAffinity::new(
            Arc::new(ip_cache::SledCache::temporary().unwrap()),
            &pending,
        );
        (affinity, pending)
    }

    #[test]
    fn a_verdict_keeps_its_class_and_its_endpoint() {
        for class in Difficulty::ALL {
            let verdict = Verdict {
                class: Some(class),
                ..landed_on("one.example.com")
            };
            assert_eq!(Verdict::decode(verdict.encode().as_bytes()), Some(verdict));
        }
        let unclassified = landed_on("one.example.com");
        assert_eq!(
            Verdict::decode(unclassified.encode().as_bytes()),
            Some(unclassified)
        );
    }

    #[test]
    fn a_verdict_that_is_not_one_reads_as_no_verdict() {
        // The shape is a name, a space and a host with its port. No half of it may be missed,
        // and a name no class has is not a class either.
        for stored in [
            "",
            "one.example.com:443",
            "hard one.example.com",
            "legendary one.example.com:443",
            "hard one.example.com:80000",
            "hard one.example.com:http",
            "hard  one.example.com:443",
        ] {
            assert_eq!(
                Verdict::decode(stored.as_bytes()),
                None,
                "read {stored:?} as a verdict"
            );
        }
    }

    #[tokio::test]
    async fn a_kept_turn_lands_where_it_was_kept() {
        let (memory, pending) = memory();
        let identity = caller("acme", "alice");
        let turn = turn("turn-1");
        assert_eq!(memory.landed(&identity, &turn).await, None);

        let verdict = Verdict {
            class: Some(Difficulty::Hard),
            ..landed_on("one.example.com")
        };
        memory.keep(&identity, &turn, &verdict);
        pending.settled().await;
        assert_eq!(memory.landed(&identity, &turn).await, Some(verdict));
    }

    #[tokio::test]
    async fn another_callers_turn_is_not_this_one() {
        let (memory, pending) = memory();
        let identity = caller("acme", "alice");
        let turn = turn("turn-1");
        memory.keep(&identity, &turn, &landed_on("one.example.com"));
        pending.settled().await;

        assert_eq!(
            memory
                .landed(&identity, &TurnId::new("turn-2").unwrap())
                .await,
            None
        );
        assert_eq!(
            memory.landed(&caller("acme", "bob"), &turn).await,
            None,
            "another user's turn of the same id"
        );
        assert_eq!(
            memory.landed(&caller("other", "alice"), &turn).await,
            None,
            "another tenant's turn of the same id"
        );
    }

    #[tokio::test]
    async fn a_turn_outlasts_its_memory_no_longer_than_the_memory() {
        // The double keeps a verdict for exactly as long as it is given one, and nothing about
        // the wait can be hurried: the expiry is tested at a second's scale standing in for
        // fifteen minutes, which is the same line read the other end of the clock.
        let pending = Pending::default();
        let cache: Arc<dyn Cache> = Arc::new(ip_cache::SledCache::temporary().unwrap());
        let memory = TurnAffinity {
            ttl: Ttl::seconds(1).unwrap(),
            ..TurnAffinity::new(cache, &pending)
        };
        let identity = caller("acme", "alice");
        let turn = turn("turn-1");
        memory.keep(&identity, &turn, &landed_on("one.example.com"));
        pending.settled().await;
        assert!(memory.landed(&identity, &turn).await.is_some());

        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert_eq!(memory.landed(&identity, &turn).await, None);
    }

    #[tokio::test]
    async fn a_memory_that_cannot_answer_has_lost_a_landing_and_nothing_else() {
        let failing = FailingCache::new(Arc::new(ip_cache::SledCache::temporary().unwrap()));
        failing.fail_now();
        let pending = Pending::default();
        let memory = TurnAffinity::new(Arc::new(failing), &pending);
        let identity = caller("acme", "alice");
        let turn = turn("turn-1");
        assert_eq!(memory.landed(&identity, &turn).await, None);
        memory.keep(&identity, &turn, &landed_on("one.example.com"));
        pending.settled().await;
        assert_eq!(memory.landed(&identity, &turn).await, None);
    }
}
