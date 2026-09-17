use crate::error::CacheError;
use crate::key::CacheLevel;

/// How many bytes one cache level may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaxBytes(u64);

impl MaxBytes {
    /// Wraps a limit, refusing one that leaves room for nothing.
    pub fn new(bytes: u64) -> Result<Self, CacheError> {
        if bytes == 0 {
            return Err(CacheError::ZeroLimit);
        }
        Ok(Self(bytes))
    }

    /// The limit in bytes.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// What each level may hold before its least recently used entries go.
///
/// A level with no limit is never evicted by size, and only its ttls drop anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LevelLimits {
    response: Option<MaxBytes>,
    semantic: Option<MaxBytes>,
    system: Option<MaxBytes>,
}

impl LevelLimits {
    /// Limits nothing, which is what a deployment that only sets ttls wants.
    pub fn none() -> Self {
        Self::default()
    }

    /// Limits the response cache.
    pub fn with_response(mut self, limit: MaxBytes) -> Self {
        self.response = Some(limit);
        self
    }

    /// Limits the semantic cache.
    pub fn with_semantic(mut self, limit: MaxBytes) -> Self {
        self.semantic = Some(limit);
        self
    }

    /// Limits the system cache.
    pub fn with_system(mut self, limit: MaxBytes) -> Self {
        self.system = Some(limit);
        self
    }

    /// What this level may hold, if anything limits it.
    pub fn of(&self, level: CacheLevel) -> Option<MaxBytes> {
        match level {
            CacheLevel::Response => self.response,
            CacheLevel::Semantic => self.semantic,
            CacheLevel::System => self.system,
        }
    }

    /// Whether any level is limited at all.
    pub fn any(&self) -> bool {
        self.response.is_some() || self.semantic.is_some() || self.system.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_keeps_what_it_was_given() {
        assert_eq!(MaxBytes::new(4_096).unwrap().get(), 4_096);
    }

    #[test]
    fn a_limit_of_no_bytes_is_refused() {
        assert!(matches!(MaxBytes::new(0), Err(CacheError::ZeroLimit)));
    }

    #[test]
    fn each_level_carries_its_own_limit() {
        let limits = LevelLimits::none()
            .with_response(MaxBytes::new(10).unwrap())
            .with_system(MaxBytes::new(20).unwrap());
        assert_eq!(limits.of(CacheLevel::Response), MaxBytes::new(10).ok());
        assert_eq!(limits.of(CacheLevel::System), MaxBytes::new(20).ok());
        assert_eq!(limits.of(CacheLevel::Semantic), None);
        assert!(limits.any());
    }

    #[test]
    fn limiting_nothing_limits_no_level() {
        let limits = LevelLimits::none();
        assert!(!limits.any());
        assert_eq!(limits.of(CacheLevel::Response), None);
    }
}
