use std::fmt;

use crate::error::CacheError;

/// Which cache an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheLevel {
    /// Whole responses, keyed by what was asked for them.
    Response,
    /// Requests recognised by what they mean rather than by their bytes.
    Semantic,
    /// The system's own short lived state, such as spent nonces.
    System,
}

impl CacheLevel {
    /// The word every key of this level carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Response => "resp",
            Self::Semantic => "sem",
            Self::System => "sys",
        }
    }
}

impl fmt::Display for CacheLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A key in one cache level, spelled the same way for every backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    key: String,
    level: CacheLevel,
}

impl CacheKey {
    /// Names an entry in a level, refusing an id a backend could not carry.
    pub fn new(level: CacheLevel, id: &str) -> Result<Self, CacheError> {
        if id.is_empty() {
            return Err(CacheError::EmptyKey);
        }
        if id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(CacheError::UnusableKey { id: id.to_owned() });
        }
        Ok(Self {
            key: format!("ip:{level}:{id}"),
            level,
        })
    }

    /// The key as a backend stores it.
    pub fn as_str(&self) -> &str {
        &self.key
    }

    /// Which cache this key belongs to.
    pub fn level(&self) -> CacheLevel {
        self.level
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_carries_its_level() {
        let key = CacheKey::new(CacheLevel::System, "nonce-abc").unwrap();
        assert_eq!(key.as_str(), "ip:sys:nonce-abc");
        assert_eq!(key.level(), CacheLevel::System);
    }

    #[test]
    fn every_level_spells_itself() {
        for (level, name) in [
            (CacheLevel::Response, "resp"),
            (CacheLevel::Semantic, "sem"),
            (CacheLevel::System, "sys"),
        ] {
            assert_eq!(level.to_string(), name);
        }
    }

    #[test]
    fn keys_of_different_levels_never_collide() {
        let response = CacheKey::new(CacheLevel::Response, "abc").unwrap();
        let system = CacheKey::new(CacheLevel::System, "abc").unwrap();
        assert_ne!(response, system);
    }

    #[test]
    fn an_id_of_nothing_is_refused() {
        assert!(matches!(
            CacheKey::new(CacheLevel::System, ""),
            Err(CacheError::EmptyKey)
        ));
    }

    #[test]
    fn an_id_a_backend_could_not_carry_is_refused() {
        for id in ["two words", "line\nbreak", "tab\there"] {
            assert!(
                matches!(
                    CacheKey::new(CacheLevel::Response, id),
                    Err(CacheError::UnusableKey { .. })
                ),
                "{id:?} should be refused"
            );
        }
    }
}
