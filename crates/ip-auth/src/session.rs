use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use ip_core::UserId;
use sha2::Sha256;

use crate::error::AuthError;

/// Bytes a session id is drawn from, which is what makes one unguessable.
const ID_BYTES: usize = 16;

/// The only header this server writes or accepts: `{"alg":"HS256","typ":"JWT"}`.
///
/// Comparing it byte for byte is what pins the scheme. A token naming `none`, or any
/// algorithm but this one, is refused before anything in it is read.
const HEADER: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";

/// The id one session is known by, so a logout can name the token it ends.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Draws an id no one can guess.
    pub fn generate() -> Result<Self, AuthError> {
        let mut bytes = [0u8; ID_BYTES];
        getrandom::fill(&mut bytes).map_err(AuthError::Random)?;
        Ok(Self(hex::encode(bytes)))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A session token, as the browser carries it in its cookie.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(String);

impl SessionToken {
    /// The token as it is written into the cookie.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wraps text as a token, for tests that need one without a server to issue it.
    #[doc(hidden)]
    pub fn from_str_for_test(raw: &str) -> Self {
        Self(raw.to_owned())
    }
}

impl fmt::Debug for SessionToken {
    /// Redacted, since a token that has been printed is a token someone else can use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(redacted)")
    }
}

/// Who a checked token belongs to, and which session it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    user: UserId,
    id: SessionId,
    expires_at: u64,
}

impl Session {
    /// The user this session logged in as.
    pub fn user(&self) -> &UserId {
        &self.user
    }

    /// Which session this is, which is what a logout ends.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Seconds since the unix epoch at which the token stops being accepted.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Builds a session with an expiry of its own, so a test can age one.
    #[cfg(test)]
    pub(crate) fn for_test(user: UserId, id: SessionId, expires_at: u64) -> Self {
        Self {
            user,
            id,
            expires_at,
        }
    }
}

/// What a session token carries, in the claim names jwt reserves.
#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    sub: String,
    iss: String,
    jti: String,
    iat: u64,
    exp: u64,
}

/// Issues the session tokens the web logs in with, and checks the ones it sends back.
///
/// Only HS256 exists here: this server both writes and reads these tokens, so it needs one
/// scheme rather than a library's worth of them.
#[derive(Clone)]
pub struct SessionTokens {
    issuer: String,
    secret: Vec<u8>,
    ttl: Duration,
}

impl SessionTokens {
    /// Signs tokens for `issuer` with `secret`, each good for `ttl`.
    pub fn new(issuer: &str, secret: &[u8], ttl: Duration) -> Self {
        Self {
            issuer: issuer.to_owned(),
            secret: secret.to_vec(),
            ttl,
        }
    }

    /// Issues a token for a user who has just proved who they are.
    pub fn issue(&self, user: &UserId) -> Result<SessionToken, AuthError> {
        self.issue_at(user, SystemTime::now())
    }

    /// Reads a token back, refusing one this server did not issue or that has run out.
    pub fn verify(&self, token: &str) -> Result<Session, AuthError> {
        self.verify_at(token, SystemTime::now())
    }

    fn issue_at(&self, user: &UserId, now: SystemTime) -> Result<SessionToken, AuthError> {
        let issued_at = seconds_since_epoch(now);
        let claims = Claims {
            sub: user.as_str().to_owned(),
            iss: self.issuer.clone(),
            jti: SessionId::generate()?.0,
            iat: issued_at,
            exp: issued_at.saturating_add(self.ttl.as_secs()),
        };
        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).map_err(AuthError::SessionIssue)?);
        let signed = format!("{HEADER}.{payload}");
        let signature = URL_SAFE_NO_PAD.encode(self.mac(signed.as_bytes()).finalize().into_bytes());
        Ok(SessionToken(format!("{signed}.{signature}")))
    }

    fn verify_at(&self, token: &str, now: SystemTime) -> Result<Session, AuthError> {
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(AuthError::SessionRejected);
        };
        if header != HEADER {
            return Err(AuthError::SessionRejected);
        }
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| AuthError::SessionRejected)?;
        let signed = format!("{header}.{payload}");
        self.mac(signed.as_bytes())
            .verify_slice(&signature)
            .map_err(|_| AuthError::SessionRejected)?;

        let claims: Claims = URL_SAFE_NO_PAD
            .decode(payload)
            .ok()
            .and_then(|claims| serde_json::from_slice(&claims).ok())
            .ok_or(AuthError::SessionRejected)?;
        if claims.iss != self.issuer || claims.exp <= seconds_since_epoch(now) {
            return Err(AuthError::SessionRejected);
        }
        Ok(Session {
            user: UserId::new(&claims.sub).map_err(|_| AuthError::SessionRejected)?,
            id: SessionId(claims.jti),
            expires_at: claims.exp,
        })
    }

    /// An hmac over the secret, which takes a key of any length.
    fn mac(&self, message: &[u8]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.secret)
            .unwrap_or_else(|_| unreachable!("hmac takes a key of any length"));
        mac.update(message);
        mac
    }
}

fn seconds_since_epoch(moment: SystemTime) -> u64 {
    moment
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many parts a token has: header, claims and signature.
    const PARTS: usize = 3;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn tokens() -> SessionTokens {
        SessionTokens::new("intelli-prism", SECRET, hour())
    }

    fn hour() -> Duration {
        Duration::from_secs(3600)
    }

    fn alice() -> UserId {
        UserId::new("alice").unwrap()
    }

    /// A token whose header says `alg`, carrying a signature that is correct for it.
    ///
    /// Signed properly, so only the pinned header can refuse it. A token still signed for
    /// the real header would be refused by the signature instead, proving nothing.
    fn token_headed(alg: &str) -> String {
        let tokens = tokens();
        let header = URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"{alg}","typ":"JWT"}}"#));
        let issued = tokens.issue(&alice()).unwrap();
        let payload = issued.as_str().split('.').nth(1).unwrap().to_owned();
        let signed = format!("{header}.{payload}");
        let signature =
            URL_SAFE_NO_PAD.encode(tokens.mac(signed.as_bytes()).finalize().into_bytes());
        format!("{signed}.{signature}")
    }

    #[test]
    fn the_pinned_header_is_the_one_it_claims_to_be() {
        let decoded = URL_SAFE_NO_PAD.decode(HEADER).unwrap();
        assert_eq!(decoded, br#"{"alg":"HS256","typ":"JWT"}"#);
    }

    #[test]
    fn a_token_reads_back_as_the_user_it_was_issued_for() {
        let tokens = tokens();
        let token = tokens.issue(&alice()).unwrap();
        let session = tokens.verify(token.as_str()).unwrap();
        assert_eq!(session.user(), &alice());
        assert!(session.expires_at() > seconds_since_epoch(SystemTime::now()));
        assert_eq!(token.as_str().split('.').count(), PARTS);
    }

    #[test]
    fn every_token_names_a_session_of_its_own() {
        let tokens = tokens();
        let first = tokens
            .verify(tokens.issue(&alice()).unwrap().as_str())
            .unwrap();
        let second = tokens
            .verify(tokens.issue(&alice()).unwrap().as_str())
            .unwrap();
        assert_ne!(first.id(), second.id());
        assert_eq!(first.id().as_str().len(), ID_BYTES * 2);
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let token = tokens().issue(&alice()).unwrap();
        let elsewhere = SessionTokens::new("intelli-prism", b"another secret entirely!", hour());
        assert!(matches!(
            elsewhere.verify(token.as_str()),
            Err(AuthError::SessionRejected)
        ));
    }

    #[test]
    fn a_token_from_another_issuer_is_refused() {
        let token = SessionTokens::new("somewhere-else", SECRET, hour())
            .issue(&alice())
            .unwrap();
        assert!(matches!(
            tokens().verify(token.as_str()),
            Err(AuthError::SessionRejected)
        ));
    }

    #[test]
    fn a_token_that_has_run_out_is_refused() {
        let tokens = tokens();
        let long_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
        let token = tokens.issue_at(&alice(), long_ago).unwrap();
        assert!(matches!(
            tokens.verify(token.as_str()),
            Err(AuthError::SessionRejected)
        ));
    }

    #[test]
    fn a_token_expiring_this_very_second_is_refused() {
        let tokens = SessionTokens::new("intelli-prism", SECRET, Duration::ZERO);
        let now = SystemTime::now();
        let token = tokens.issue_at(&alice(), now).unwrap();
        assert!(matches!(
            tokens.verify_at(token.as_str(), now),
            Err(AuthError::SessionRejected)
        ));
    }

    #[test]
    fn a_token_naming_another_scheme_is_refused() {
        for alg in ["none", "HS384", "RS256"] {
            assert!(
                matches!(
                    tokens().verify(&token_headed(alg)),
                    Err(AuthError::SessionRejected)
                ),
                "a token headed {alg} was accepted"
            );
        }
    }

    #[test]
    fn a_token_that_is_not_three_parts_is_refused() {
        let tokens = tokens();
        let token = tokens.issue(&alice()).unwrap();
        let whole = token.as_str();
        let two = whole.rsplit_once('.').unwrap().0.to_owned();
        for malformed in [two, format!("{whole}.extra"), String::new(), ".".to_owned()] {
            assert!(
                matches!(tokens.verify(&malformed), Err(AuthError::SessionRejected)),
                "{malformed:?} was accepted"
            );
        }
    }

    #[test]
    fn a_token_someone_edited_is_refused() {
        let tokens = tokens();
        let token = tokens.issue(&alice()).unwrap();
        let (signed, signature) = token.as_str().rsplit_once('.').unwrap();
        let (header, payload) = signed.split_once('.').unwrap();
        let mut claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        claims["sub"] = serde_json::Value::String("root".to_owned());
        let forged = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        assert!(matches!(
            tokens.verify(&format!("{header}.{forged}.{signature}")),
            Err(AuthError::SessionRejected)
        ));
    }

    #[test]
    fn a_token_that_is_not_even_base64_is_refused() {
        let tokens = tokens();
        for rubbish in [
            format!("{HEADER}.not base64!.signature"),
            format!("{HEADER}.e30.not base64!"),
        ] {
            assert!(matches!(
                tokens.verify(&rubbish),
                Err(AuthError::SessionRejected)
            ));
        }
    }

    #[test]
    fn a_token_never_prints_itself() {
        let token = tokens().issue(&alice()).unwrap();
        assert_eq!(format!("{token:?}"), "SessionToken(redacted)");
        assert!(!format!("{token:?}").contains(token.as_str()));
    }
}
