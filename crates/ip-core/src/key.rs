use std::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use crate::error::CoreError;
use crate::id::{Nonce, UserId};

/// Length of a tenant root key.
pub const TN_KEY_BYTES: usize = 16;
/// Length of a sha256 digest.
pub const DIGEST_BYTES: usize = 32;

fn decode_hex<const N: usize>(kind: &'static str, raw: &str) -> Result<[u8; N], CoreError> {
    if raw.len() != N * 2 {
        return Err(CoreError::HexLength {
            kind,
            expected: N * 2,
        });
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(raw, &mut out).map_err(|_| CoreError::HexDigit { kind })?;
    Ok(out)
}

fn digest_of(secret: &[u8], nonce: &Nonce) -> [u8; DIGEST_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(secret);
    hasher.update(nonce.as_bytes());
    hasher.finalize().into()
}

/// Root secret of a tenant: every `UtKey` under the tenant is derived from it.
#[derive(Clone, Eq)]
pub struct TnKey([u8; TN_KEY_BYTES]);

impl TnKey {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "tenant key";

    /// Draws a new tenant key from the operating system entropy source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; TN_KEY_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Parses the hex form.
    pub fn from_hex(raw: &str) -> Result<Self, CoreError> {
        decode_hex(Self::KIND, raw).map(Self)
    }

    /// The hex form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The raw key bytes.
    pub fn as_bytes(&self) -> &[u8; TN_KEY_BYTES] {
        &self.0
    }
}

impl From<[u8; TN_KEY_BYTES]> for TnKey {
    fn from(bytes: [u8; TN_KEY_BYTES]) -> Self {
        Self(bytes)
    }
}

/// Constant time: a secret is never compared with a short circuiting equality.
impl PartialEq for TnKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl fmt::Debug for TnKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TnKey(redacted)")
    }
}

impl Drop for TnKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Per-(user, tenant) secret: `sha256(user_id || tn_key)`.
#[derive(Clone, Eq)]
pub struct UtKey([u8; DIGEST_BYTES]);

impl UtKey {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "user tenant key";

    /// Derives the key of one user inside one tenant.
    pub fn derive(user: &UserId, tn_key: &TnKey) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(user.as_bytes());
        hasher.update(tn_key.as_bytes());
        Self(hasher.finalize().into())
    }

    /// Parses the hex form.
    pub fn from_hex(raw: &str) -> Result<Self, CoreError> {
        decode_hex(Self::KIND, raw).map(Self)
    }

    /// The hex form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The raw key bytes.
    pub fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }
}

/// Constant time: a secret is never compared with a short circuiting equality.
impl PartialEq for UtKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl fmt::Debug for UtKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UtKey(redacted)")
    }
}

impl Drop for UtKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Per-request proof of identity carried by `X-Ip-Signature`.
#[derive(Clone, Eq)]
pub struct Signature([u8; DIGEST_BYTES]);

impl Signature {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "signature";

    /// The scheme every ordinary user signs with.
    pub fn of_user(ut_key: &UtKey, nonce: &Nonce) -> Self {
        Self(digest_of(ut_key.as_bytes(), nonce))
    }

    /// The tenant owner signs with the tenant root key instead of a `UtKey`.
    pub fn of_tenant_owner(tn_key: &TnKey, nonce: &Nonce) -> Self {
        Self(digest_of(tn_key.as_bytes(), nonce))
    }

    /// Parses the hex form carried by the request header.
    pub fn from_hex(raw: &str) -> Result<Self, CoreError> {
        decode_hex(Self::KIND, raw).map(Self)
    }

    /// The hex form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    /// Constant time: the candidate is attacker supplied, so no comparison may short circuit.
    pub fn verify(&self, candidate: &Self) -> bool {
        self.0.ct_eq(&candidate.0).into()
    }
}

impl PartialEq for Signature {
    fn eq(&self, other: &Self) -> bool {
        self.verify(other)
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Signature").field(&self.to_hex()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce() -> Nonce {
        Nonce::new("0123456789abcdef").unwrap()
    }

    #[test]
    fn generated_keys_differ() {
        assert_ne!(TnKey::generate().unwrap(), TnKey::generate().unwrap());
    }

    #[test]
    fn tenant_key_round_trips_through_hex() {
        let key = TnKey::generate().unwrap();
        assert_eq!(key.to_hex().len(), TN_KEY_BYTES * 2);
        assert_eq!(TnKey::from_hex(&key.to_hex()).unwrap(), key);
    }

    #[test]
    fn rejects_hex_of_wrong_length() {
        assert_eq!(
            TnKey::from_hex("00ff"),
            Err(CoreError::HexLength {
                kind: "tenant key",
                expected: TN_KEY_BYTES * 2,
            })
        );
    }

    #[test]
    fn rejects_non_hex_digits() {
        let raw = "z".repeat(TN_KEY_BYTES * 2);
        assert_eq!(
            TnKey::from_hex(&raw),
            Err(CoreError::HexDigit { kind: "tenant key" })
        );
    }

    #[test]
    fn user_tenant_key_is_sha256_of_user_and_tenant_key() {
        let tn_key = TnKey::from(*b"0123456789abcdef");
        let user = UserId::new("alice").unwrap();
        let expected = Sha256::digest(b"alice0123456789abcdef");
        assert_eq!(UtKey::derive(&user, &tn_key).as_bytes()[..], expected[..]);
    }

    #[test]
    fn user_tenant_key_is_bound_to_the_user() {
        let tn_key = TnKey::from(*b"0123456789abcdef");
        let alice = UtKey::derive(&UserId::new("alice").unwrap(), &tn_key);
        let bob = UtKey::derive(&UserId::new("bob").unwrap(), &tn_key);
        assert_ne!(alice, bob);
    }

    #[test]
    fn user_signature_matches_the_specified_scheme() {
        let tn_key = TnKey::from(*b"0123456789abcdef");
        let ut_key = UtKey::derive(&UserId::new("alice").unwrap(), &tn_key);
        let mut expected = Sha256::new();
        expected.update(ut_key.as_bytes());
        expected.update(b"0123456789abcdef");
        assert_eq!(
            Signature::of_user(&ut_key, &nonce()).as_bytes()[..],
            expected.finalize()[..]
        );
    }

    #[test]
    fn signature_changes_with_the_nonce() {
        let tn_key = TnKey::from(*b"0123456789abcdef");
        let ut_key = UtKey::derive(&UserId::new("alice").unwrap(), &tn_key);
        let other = Nonce::new("fedcba9876543210").unwrap();
        assert_ne!(
            Signature::of_user(&ut_key, &nonce()),
            Signature::of_user(&ut_key, &other)
        );
    }

    #[test]
    fn tenant_owner_signature_differs_from_a_user_signature() {
        let tn_key = TnKey::from(*b"0123456789abcdef");
        let ut_key = UtKey::derive(&UserId::new("alice").unwrap(), &tn_key);
        assert_ne!(
            Signature::of_tenant_owner(&tn_key, &nonce()),
            Signature::of_user(&ut_key, &nonce())
        );
    }

    #[test]
    fn verify_accepts_only_the_matching_signature() {
        let tn_key = TnKey::from(*b"0123456789abcdef");
        let ut_key = UtKey::derive(&UserId::new("alice").unwrap(), &tn_key);
        let signature = Signature::of_user(&ut_key, &nonce());
        let other = Signature::of_user(&ut_key, &Nonce::new("fedcba9876543210").unwrap());
        assert!(signature.verify(&Signature::of_user(&ut_key, &nonce())));
        assert!(!signature.verify(&other));
    }

    #[test]
    fn signature_round_trips_through_hex() {
        let tn_key = TnKey::generate().unwrap();
        let signature = Signature::of_tenant_owner(&tn_key, &nonce());
        assert_eq!(Signature::from_hex(&signature.to_hex()).unwrap(), signature);
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let tn_key = TnKey::generate().unwrap();
        let ut_key = UtKey::derive(&UserId::new("alice").unwrap(), &tn_key);
        assert_eq!(format!("{tn_key:?}"), "TnKey(redacted)");
        assert_eq!(format!("{ut_key:?}"), "UtKey(redacted)");
    }
}
