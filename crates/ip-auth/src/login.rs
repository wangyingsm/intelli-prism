use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ip_cache::{Cache, CacheKey, CacheLevel, Ttl};
use ip_core::{PassphraseHash, UserId};
use ip_storage::{Storage, User};

use crate::error::AuthError;
use crate::passphrase::{Passphrase, PassphraseHasher};
use crate::session::{Session, SessionId, SessionToken, SessionTokens};

/// Logs a user in against the stored passphrase verifier, and hands back a session token.
#[derive(Clone)]
pub struct Logins {
    store: Arc<dyn Storage>,
    cache: Arc<dyn Cache>,
    hasher: PassphraseHasher,
    tokens: SessionTokens,
    /// A verifier for a passphrase nobody knows, so a login for a user that does not exist
    /// costs the same work as one for a user that does.
    absent: PassphraseHash,
}

impl Logins {
    /// Reads users out of this store, issues tokens with these settings, and ends sessions
    /// in this cache.
    pub fn new(
        store: Arc<dyn Storage>,
        cache: Arc<dyn Cache>,
        tokens: SessionTokens,
    ) -> Result<Self, AuthError> {
        let hasher = PassphraseHasher::new();
        let unknowable = Passphrase::new(SessionId::generate()?.as_str())?;
        let absent = hasher.hash(&unknowable)?;
        Ok(Self {
            store,
            cache,
            hasher,
            tokens,
            absent,
        })
    }

    /// Reads a token back, refusing one this server did not issue, one that has run out, and
    /// one whose session was logged out.
    pub async fn session(&self, token: &str) -> Result<Session, AuthError> {
        let session = self.tokens.verify(token)?;
        match self.is_ended(&session).await? {
            true => Err(AuthError::SessionRejected),
            false => Ok(session),
        }
    }

    /// Ends a session, so the token stays refused until it would have run out anyway.
    ///
    /// A token cannot be taken back once it is issued, so the cache remembers the ones that
    /// were given up. Remembering them past their own expiry would only waste room.
    pub async fn end(&self, session: &Session) -> Result<(), AuthError> {
        let Some(left) = remaining(session, SystemTime::now()) else {
            return Ok(());
        };
        self.cache
            .put(&ended_key(session.id())?, &[], Some(left))
            .await?;
        Ok(())
    }

    /// Whether this session was logged out before its token ran out.
    pub async fn is_ended(&self, session: &Session) -> Result<bool, AuthError> {
        Ok(self.cache.get(&ended_key(session.id())?).await?.is_some())
    }

    /// Checks a passphrase and issues a session, or refuses without saying which half was wrong.
    pub async fn log_in(
        &self,
        user: &UserId,
        passphrase: &Passphrase,
    ) -> Result<SessionToken, AuthError> {
        let stored = self.store.user(user).await?;
        let matched = self
            .hasher
            .verify(passphrase, self.verifier_of(stored.as_ref()))?;
        match stored {
            Some(stored) if matched => self.tokens.issue(&stored.id),
            _ => Err(AuthError::LoginRefused),
        }
    }

    /// What a login checks against: the stored verifier, or the unknowable one when no such
    /// user exists, so that answering a login costs the same either way.
    fn verifier_of<'a>(&'a self, stored: Option<&'a User>) -> &'a PassphraseHash {
        stored.map_or(&self.absent, |user| &user.passphrase)
    }
}

/// How long a session has left, or nothing when its token has run out already.
fn remaining(session: &Session, now: SystemTime) -> Option<Ttl> {
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let left = session.expires_at().checked_sub(now)?;
    Ttl::new(Duration::from_secs(left)).ok()
}

/// Where a session that was logged out is remembered.
fn ended_key(session: &SessionId) -> Result<CacheKey, AuthError> {
    Ok(CacheKey::new(
        CacheLevel::System,
        &format!("session-ended:{session}"),
    )?)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ip_storage::{AccountKind, NewUser, SqliteStore, UserStore};

    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn passphrase(raw: &str) -> Passphrase {
        Passphrase::new(raw).unwrap()
    }

    fn alice() -> UserId {
        UserId::new("alice").unwrap()
    }

    /// Logins over a store holding one user of `kind` whose passphrase is `correct horse staple`.
    async fn logins(kind: AccountKind) -> Logins {
        let store = SqliteStore::in_memory().await.unwrap();
        let hasher = PassphraseHasher::new();
        store
            .create_user(NewUser {
                id: alice(),
                passphrase: hasher.hash(&passphrase("correct horse staple")).unwrap(),
                kind,
            })
            .await
            .unwrap();
        let tokens = SessionTokens::new("intelli-prism", SECRET, Duration::from_secs(3600));
        let cache = Arc::new(ip_cache::SledCache::temporary().unwrap());
        Logins::new(Arc::new(store), cache, tokens).unwrap()
    }

    #[tokio::test]
    async fn the_right_passphrase_opens_a_session_for_that_user() {
        let logins = logins(AccountKind::Regular).await;
        let token = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        let session = logins.tokens.verify(token.as_str()).unwrap();
        assert_eq!(session.user(), &alice());
    }

    #[tokio::test]
    async fn the_wrong_passphrase_is_refused() {
        let logins = logins(AccountKind::Regular).await;
        assert!(matches!(
            logins
                .log_in(&alice(), &passphrase("incorrect horse staple"))
                .await,
            Err(AuthError::LoginRefused)
        ));
    }

    #[tokio::test]
    async fn a_user_that_does_not_exist_is_refused_the_same_way() {
        let logins = logins(AccountKind::Regular).await;
        let nobody = UserId::new("nobody").unwrap();
        assert!(matches!(
            logins
                .log_in(&nobody, &passphrase("correct horse staple"))
                .await,
            Err(AuthError::LoginRefused)
        ));
    }

    #[tokio::test]
    async fn a_login_for_a_user_that_is_not_there_is_still_verified_against_something() {
        let logins = logins(AccountKind::Regular).await;
        let stored = logins.store.user(&alice()).await.unwrap().unwrap();
        assert_eq!(logins.verifier_of(None), &logins.absent);
        assert_eq!(logins.verifier_of(Some(&stored)), &stored.passphrase);
        assert!(
            logins.absent.as_str().starts_with("$argon2id$"),
            "the unknowable verifier is not one argon2 would have written"
        );
    }

    #[tokio::test]
    async fn every_login_opens_a_session_of_its_own() {
        let logins = logins(AccountKind::Regular).await;
        let first = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        let second = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        assert_ne!(
            logins.tokens.verify(first.as_str()).unwrap().id(),
            logins.tokens.verify(second.as_str()).unwrap().id()
        );
    }

    #[tokio::test]
    async fn a_session_reads_back_until_it_is_ended() {
        let logins = logins(AccountKind::Regular).await;
        let token = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        let session = logins.session(token.as_str()).await.unwrap();
        assert_eq!(session.user(), &alice());

        logins.end(&session).await.unwrap();
        assert!(logins.is_ended(&session).await.unwrap());
        assert!(matches!(
            logins.session(token.as_str()).await,
            Err(AuthError::SessionRejected)
        ));
    }

    #[tokio::test]
    async fn ending_one_session_leaves_another_alone() {
        let logins = logins(AccountKind::Regular).await;
        let ended = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        let kept = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        logins
            .end(&logins.session(ended.as_str()).await.unwrap())
            .await
            .unwrap();
        assert!(logins.session(kept.as_str()).await.is_ok());
    }

    #[tokio::test]
    async fn ending_a_session_whose_token_has_run_out_writes_nothing() {
        let logins = logins(AccountKind::Regular).await;
        let token = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        let mut session = logins.session(token.as_str()).await.unwrap();
        // A token that ran out an hour ago needs no remembering: it is refused on its own.
        session = Session::for_test(session.user().clone(), session.id().clone(), 0);
        logins.end(&session).await.unwrap();
        assert!(!logins.is_ended(&session).await.unwrap());
    }

    #[tokio::test]
    async fn the_system_administrator_logs_in_from_anywhere() {
        let logins = logins(AccountKind::SystemAdministrator).await;
        let token = logins
            .log_in(&alice(), &passphrase("correct horse staple"))
            .await
            .unwrap();
        assert_eq!(
            logins.tokens.verify(token.as_str()).unwrap().user(),
            &alice()
        );
    }
}
