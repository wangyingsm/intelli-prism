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
//!
//! Almost no client says which turn it is in, so a turn is usually recognised from the request:
//! `opening` reads a request body — the first place a *request* body is parsed, as `measure.rs`
//! does to a response — and the anchor of its conversation names the turn. `x-ip-turn` still
//! wins wherever a caller sends one.

use std::sync::Arc;
use std::time::Duration;

use ip_auth::Identity;
use ip_cache::{Cache, CacheKey, CacheLevel, Ttl};
use ip_core::{Difficulty, Host, Port, Replica, TurnId};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::flow::record::Pending;

/// A turn keeps its endpoint for fifteen minutes. It is a number, not a law: longer than a
/// turn that stops to think between its requests, and shorter than the wait after which any
/// endpoint at all is likelier to beat the one that answered last.
const TURN_TTL: Duration = Duration::from_secs(15 * 60);

/// The class slot of a turn that named none. A name is always written, because the value is
/// read as a whole; a turn that named none has a slot too, and no class is not any class name.
const NO_CLASS: &str = "-";

/// The count slot of a turn whose request was never read. As with the class, the slot is always
/// written, and a dash is the one thing a count is never spelled as.
const NO_COUNT: &str = "-";

/// Where one caller's turn landed: what the memory of §429 keeps, and reads back whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The class the request that landed was asked at, if it named one.
    pub class: Option<Difficulty>,
    /// How many messages that request carried, once its body had been read; nothing to
    /// compare a shrink against until it has been.
    pub messages: Option<usize>,
    /// The endpoint it landed on.
    pub endpoint: Replica,
}

impl Verdict {
    /// The three slots with their separator: a space is in no class name, no count and no
    /// host, and the endpoint keeps the form `Replica` displays itself with.
    fn encode(&self) -> String {
        let class = self.class.map_or(NO_CLASS, Difficulty::name);
        let messages = match self.messages {
            Some(count) => count.to_string(),
            None => NO_COUNT.to_owned(),
        };
        format!("{class} {messages} {}", self.endpoint)
    }

    /// A value this read cannot make a verdict of — a name no class has, a count that is not
    /// one, an endpoint no host was written for — is not an error to answer with but a verdict
    /// from a gateway of another version, which reads as the miss it always was.
    fn decode(stored: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(stored).ok()?;
        let mut slots = text.split(' ');
        let (Some(class), Some(messages), Some(endpoint)) =
            (slots.next(), slots.next(), slots.next())
        else {
            return None;
        };
        if slots.next().is_some() {
            return None;
        }
        let class = match class {
            NO_CLASS => None,
            named => Some(Difficulty::named(named).ok()?),
        };
        let messages = match messages {
            NO_COUNT => None,
            count => Some(count.parse::<usize>().ok()?),
        };
        let (host, port) = endpoint.rsplit_once(':')?;
        let port = port.parse::<u16>().ok()?;
        Some(Self {
            class,
            messages,
            endpoint: Replica {
                host: Host::new(host).ok()?,
                port: Port::new(port).ok()?,
            },
        })
    }

    /// Whether a request standing in front of this verdict continues the turn it landed on.
    ///
    /// A turn only ever adds messages; nothing but a compaction takes them away, and a
    /// compaction is a new turn whatever the client means by it, so a lower count starts one.
    /// The message count carries the check rather than a size threshold: a count is exact and
    /// cheap, and a size would need a fraction of the old body that nobody could justify.
    #[must_use]
    pub fn continues(&self, messages: Option<usize>) -> bool {
        !matches!((self.messages, messages), (Some(kept), Some(now)) if now < kept)
    }
}

/// What the opening of one request says about it: the anchor its conversation is known by,
/// and how much conversation there is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opening {
    /// The conversation's anchor: a digest of the system prompt and the first user message,
    /// which every request of the conversation repeats unchanged.
    pub anchor: TurnId,
    /// How many messages the request carries, the whole exchange and not just the opening.
    pub messages: usize,
}

impl Opening {
    /// Reads the opening out of a request body: its system prompt — the `system` field of one
    /// payload shape, the first `system` message of the other — and the content of its first
    /// user message, hashed together.
    ///
    /// An agent loop sends the whole history every time, so request N+1 carries request N's
    /// opening verbatim and one digest names the conversation for all of them. The opening is
    /// worth hashing for a reason past latency: both providers cache prompt *prefixes* per
    /// endpoint, so a turn kept on the endpoint that already holds its prefix keeps that cache
    /// warm too, and the caller never pays the input tokens for it twice. Affinity keyed on the
    /// opening is affinity with the provider's own cache, not only with the gateway's dispatch.
    ///
    /// Content is hashed *serialised*, not walked block by block: a block may be text, an image
    /// or a tool result, and reaching into each shape is work this does not need. One client
    /// sending one conversation serialises its own content the same way every time, which is
    /// all an anchor requires; two clients spelling the same conversation differently are two
    /// turns, and that costs an affinity, not a correctness.
    ///
    /// A body that is not json, carries no `messages`, or holds no user message — embeddings
    /// and plain completions fall here — has no opening, and is simply not sticky.
    pub fn read(body: &[u8]) -> Option<Self> {
        let value = serde_json::from_slice::<Value>(body).ok()?;
        let messages = value.get("messages")?.as_array()?;
        let user = first_with_role(messages, "user")?;
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, system_of(&value));
        // A serialised json value never holds a raw NUL, so it cannot fuse the two halves.
        hasher.update([0u8]);
        hash_field(&mut hasher, user.get("content"));
        let anchor = TurnId::new(&hex::encode(hasher.finalize()))
            .unwrap_or_else(|_| unreachable!("a hex digest is exactly what an id takes"));
        Some(Self {
            anchor,
            messages: messages.len(),
        })
    }
}

/// The prompt a conversation opens with, if it opens with one: the top-level `system` field,
/// else a message whose role is `system`, whose content is the half of the anchor either way.
fn system_of(value: &Value) -> Option<&Value> {
    if let Some(system) = value.get("system") {
        return Some(system);
    }
    let messages = value.get("messages")?.as_array()?;
    first_with_role(messages, "system")?.get("content")
}

/// The first entry with this role, if there is one.
fn first_with_role<'a>(messages: &'a [Value], role: &str) -> Option<&'a Value> {
    messages
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some(role))
}

/// One half of the anchor, written as its serialised value; a missing half as the null it is.
fn hash_field(hasher: &mut Sha256, field: Option<&Value>) {
    match field {
        Some(value) => hasher.update(value.to_string().as_bytes()),
        None => hasher.update("null"),
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
            messages: None,
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

    #[test]
    fn a_verdict_keeps_the_count_it_landed_with() {
        let verdict = Verdict {
            messages: Some(7),
            ..landed_on("one.example.com")
        };
        assert_eq!(Verdict::decode(verdict.encode().as_bytes()), Some(verdict));
    }

    #[test]
    fn a_count_that_is_not_one_is_no_verdict() {
        for stored in [
            "hard many one.example.com:443",
            "hard -1 one.example.com:443",
            "hard 3.5 one.example.com:443",
            "hard 3 one.example.com:443 extra",
        ] {
            assert_eq!(
                Verdict::decode(stored.as_bytes()),
                None,
                "read {stored:?} as a verdict"
            );
        }
    }

    #[test]
    fn only_more_messages_than_the_turn_landed_with_continue_it() {
        let kept = Verdict {
            messages: Some(9),
            ..landed_on("one.example.com")
        };
        assert!(
            kept.continues(Some(9)),
            "repeating a count is no compaction"
        );
        assert!(kept.continues(Some(13)), "adding messages continues a turn");
        assert!(
            !kept.continues(Some(8)),
            "fewer messages than the turn landed with is a compaction, not a continuation"
        );
        // A request whose body was never read, or a verdict from before the count existed,
        // has nothing to compare and continues on trust.
        assert!(kept.continues(None));
        assert!(landed_on("one.example.com").continues(Some(1)));
    }

    /// A conversation body: one system prompt (empty for none), then the exchange.
    fn conversation(system: &str, turns: &[(&str, &str)]) -> String {
        let system_field = if system.is_empty() {
            String::new()
        } else {
            format!(", \"system\": \"{system}\"")
        };
        let messages = turns
            .iter()
            .map(|(role, content)| format!("{{\"role\": \"{role}\", \"content\": \"{content}\"}}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{{\"model\": \"claude\"{system_field}, \"messages\": [{messages}]}}")
    }

    fn anchor_of(body: &str) -> Option<TurnId> {
        Opening::read(body.as_bytes()).map(|opening| opening.anchor)
    }

    #[test]
    fn one_conversation_is_the_same_turn_whatever_it_has_become() {
        let opening = conversation("talk", &[("user", "hi")]);
        let next = conversation(
            "talk",
            &[("user", "hi"), ("assistant", "hello"), ("user", "again")],
        );
        assert_eq!(anchor_of(&opening), anchor_of(&next));
    }

    #[test]
    fn two_first_user_messages_are_two_turns() {
        assert_ne!(
            anchor_of(&conversation("talk", &[("user", "hi")])),
            anchor_of(&conversation("talk", &[("user", "yo")]))
        );
    }

    #[test]
    fn the_same_first_message_under_another_system_prompt_is_another_turn() {
        assert_ne!(
            anchor_of(&conversation("talk", &[("user", "hi")])),
            anchor_of(&conversation("walk", &[("user", "hi")]))
        );
    }

    #[test]
    fn a_system_message_opens_the_same_turn_a_system_field_does() {
        let field = conversation("talk", &[("user", "hi")]);
        let as_message = "{\"messages\": [{\"role\": \"system\", \"content\": \"talk\"}, \
            {\"role\": \"user\", \"content\": \"hi\"}]}";
        assert_eq!(anchor_of(&field), anchor_of(as_message));
    }

    #[test]
    fn content_blocks_are_read_as_well_as_strings() {
        let blocks = |text: &str| {
            format!(
                "{{\"system\": [{{\"type\": \"text\", \"text\": \"{text}\"}}], \
                 \"messages\": [{{\"role\": \"user\", \"content\": \"hi\"}}]}}"
            )
        };
        assert_eq!(anchor_of(&blocks("talk")), anchor_of(&blocks("talk")));
        assert_ne!(anchor_of(&blocks("talk")), anchor_of(&blocks("walk")));
    }

    #[test]
    fn no_conversation_has_no_anchor_and_no_complaint() {
        for body in [
            "",
            "not json at all",
            "{\"model\": \"embed\", \"input\": \"text\"}",
            "{\"messages\": []}",
            "{\"messages\": [{\"role\": \"assistant\", \"content\": \"only\"}]}",
        ] {
            assert_eq!(anchor_of(body), None, "anchored {body:?}");
        }
    }

    #[test]
    fn every_anchor_is_an_id() {
        // The whole trick rests on a hex digest fitting `TurnId`: assert it, do not assume
        // the charset.
        for body in [
            conversation("talk", &[("user", "hi")]),
            conversation("", &[("user", "hi")]),
            conversation(
                "talk",
                &[("user", "hi"), ("assistant", "hello"), ("user", "again")],
            ),
        ] {
            let anchor = anchor_of(&body).expect("a conversation has an anchor");
            assert!(TurnId::new(anchor.as_str()).is_ok(), "{anchor} is no id");
        }
    }

    #[test]
    fn an_opening_counts_the_whole_exchange_not_just_its_opening() {
        let next = conversation(
            "talk",
            &[("user", "hi"), ("assistant", "hello"), ("user", "again")],
        );
        assert_eq!(Opening::read(next.as_bytes()).unwrap().messages, 3);
    }

    #[test]
    fn a_shrunk_history_starts_the_turn_and_a_grown_one_carries_it() {
        // Both directions of the shrink check as the landing verdict actually applies them:
        // a middle compaction keeps the opening whole, and only the count notices.
        let opening = conversation("talk", &[("user", "hi")]);
        let grown = conversation(
            "talk",
            &[("user", "hi"), ("assistant", "hello"), ("user", "again")],
        );
        let read = |body: &str| Opening::read(body.as_bytes()).unwrap().messages;
        let landed = Verdict {
            messages: Some(read(&grown)),
            ..landed_on("one.example.com")
        };
        assert!(!landed.continues(Some(read(&opening))));
        assert!(landed.continues(Some(read(&grown) + 2)));
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
