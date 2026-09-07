use std::fmt;

use argon2::password_hash::Error as HashError;
use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use ip_core::PassphraseHash;
use zeroize::Zeroize;

use crate::error::AuthError;

/// A passphrase in the clear, held only long enough to hash or verify it.
#[derive(Clone)]
pub struct Passphrase(String);

impl Passphrase {
    /// Shortest passphrase accepted.
    pub const MIN_BYTES: usize = 12;
    /// Longest passphrase accepted, bounding the work one request can ask for.
    pub const MAX_BYTES: usize = 1024;

    /// Checks a passphrase against the length policy and takes ownership of it.
    pub fn new(raw: &str) -> Result<Self, AuthError> {
        if raw.len() < Self::MIN_BYTES {
            return Err(AuthError::PassphraseTooShort {
                len: raw.len(),
                min: Self::MIN_BYTES,
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(AuthError::PassphraseTooLong {
                len: raw.len(),
                max: Self::MAX_BYTES,
            });
        }
        Ok(Self(raw.to_owned()))
    }

    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Redacted: a passphrase in the clear never reaches a log.
impl fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Passphrase(redacted)")
    }
}

impl Drop for Passphrase {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// What one hashing costs: the knobs argon2id is tuned by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashingCost {
    /// Memory the hash occupies, in kibibytes.
    pub memory_kib: u32,
    /// Passes over that memory.
    pub iterations: u32,
    /// Lanes hashed in parallel.
    pub parallelism: u32,
}

impl Default for HashingCost {
    /// The owasp recommendation for argon2id: 19 mebibytes, two passes, one lane.
    fn default() -> Self {
        Self {
            memory_kib: 19_456,
            iterations: 2,
            parallelism: 1,
        }
    }
}

/// Hashes and verifies passphrases with argon2id.
#[derive(Debug, Clone)]
pub struct PassphraseHasher {
    argon2: Argon2<'static>,
}

impl PassphraseHasher {
    /// A hasher at the default cost.
    pub fn new() -> Self {
        Self::with_cost(HashingCost::default())
            .expect("the default hashing cost is accepted by argon2")
    }

    /// A hasher at a chosen cost.
    pub fn with_cost(cost: HashingCost) -> Result<Self, AuthError> {
        let params = Params::new(
            cost.memory_kib,
            cost.iterations,
            cost.parallelism,
            Some(Params::DEFAULT_OUTPUT_LEN),
        )
        .map_err(|_| AuthError::Params(HashError::ParamsInvalid))?;
        Ok(Self {
            argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, params),
        })
    }

    /// Hashes a passphrase under a freshly drawn salt.
    pub fn hash(&self, passphrase: &Passphrase) -> Result<PassphraseHash, AuthError> {
        let hash = self
            .argon2
            .hash_password(passphrase.as_bytes())
            .map_err(AuthError::Hashing)?;
        Ok(PassphraseHash::new(&hash.to_string())?)
    }

    /// Whether the passphrase is the one the verifier was made from.
    ///
    /// The cost comes from the stored verifier, not from this hasher, so a verifier
    /// written under an older cost still checks out.
    pub fn verify(
        &self,
        passphrase: &Passphrase,
        hash: &PassphraseHash,
    ) -> Result<bool, AuthError> {
        match self
            .argon2
            .verify_password(passphrase.as_bytes(), hash.as_str())
        {
            Ok(()) => Ok(true),
            Err(HashError::PasswordInvalid) => Ok(false),
            Err(error) => Err(AuthError::Hashing(error)),
        }
    }
}

impl Default for PassphraseHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests hash many times over, so they pay the least cost argon2 accepts.
    fn hasher() -> PassphraseHasher {
        PassphraseHasher::with_cost(HashingCost {
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        })
        .unwrap()
    }

    fn passphrase() -> Passphrase {
        Passphrase::new("correct horse battery staple").unwrap()
    }

    #[test]
    fn the_default_cost_is_the_owasp_recommendation() {
        assert_eq!(
            HashingCost::default(),
            HashingCost {
                memory_kib: 19_456,
                iterations: 2,
                parallelism: 1,
            }
        );
    }

    #[test]
    fn a_passphrase_verifies_against_its_own_hash() {
        let hasher = hasher();
        let hash = hasher.hash(&passphrase()).unwrap();
        assert!(hasher.verify(&passphrase(), &hash).unwrap());
    }

    #[test]
    fn another_passphrase_does_not_verify() {
        let hasher = hasher();
        let hash = hasher.hash(&passphrase()).unwrap();
        let other = Passphrase::new("incorrect horse battery staple").unwrap();
        assert!(!hasher.verify(&other, &hash).unwrap());
    }

    #[test]
    fn hashing_twice_draws_a_new_salt() {
        let hasher = hasher();
        let first = hasher.hash(&passphrase()).unwrap();
        let second = hasher.hash(&passphrase()).unwrap();
        assert_ne!(first, second);
        assert!(hasher.verify(&passphrase(), &first).unwrap());
        assert!(hasher.verify(&passphrase(), &second).unwrap());
    }

    #[test]
    fn the_verifier_names_argon2id() {
        let hash = hasher().hash(&passphrase()).unwrap();
        assert!(hash.as_str().starts_with("$argon2id$v=19$"));
    }

    #[test]
    fn a_verifier_written_at_another_cost_still_checks_out() {
        let written = hasher().hash(&passphrase()).unwrap();
        let reader = PassphraseHasher::with_cost(HashingCost {
            memory_kib: 16,
            iterations: 2,
            parallelism: 1,
        })
        .unwrap();
        assert!(reader.verify(&passphrase(), &written).unwrap());
    }

    #[test]
    fn a_verifier_that_is_not_a_hash_is_an_error_not_a_mismatch() {
        let broken = ip_core::PassphraseHash::new("$argon2id$v=19$m=8,t=1,p=1$bad").unwrap();
        assert!(matches!(
            hasher().verify(&passphrase(), &broken),
            Err(AuthError::Hashing(_))
        ));
    }

    #[test]
    fn a_short_passphrase_is_refused() {
        assert!(matches!(
            Passphrase::new("short"),
            Err(AuthError::PassphraseTooShort { len: 5, min: 12 })
        ));
    }

    #[test]
    fn an_over_long_passphrase_is_refused() {
        let raw = "a".repeat(Passphrase::MAX_BYTES + 1);
        assert!(matches!(
            Passphrase::new(&raw),
            Err(AuthError::PassphraseTooLong { max: 1024, .. })
        ));
    }

    #[test]
    fn a_passphrase_is_redacted_in_debug_output() {
        assert_eq!(format!("{:?}", passphrase()), "Passphrase(redacted)");
    }

    #[test]
    fn an_impossible_cost_is_refused() {
        assert!(matches!(
            PassphraseHasher::with_cost(HashingCost {
                memory_kib: 1,
                iterations: 0,
                parallelism: 0,
            }),
            Err(AuthError::Params(_))
        ));
    }
}
