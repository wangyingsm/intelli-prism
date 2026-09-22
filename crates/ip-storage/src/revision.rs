//! How far the rules every node holds in memory have moved on.

use std::fmt;

use async_trait::async_trait;
use ip_core::{PluginRule, RouteRule};

use crate::error::StorageError;

/// A count that moves on with every change to a route or a plugin rule, and never goes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuleRevision(u64);

impl RuleRevision {
    /// Wraps a revision as the store counts it.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The revision as a number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RuleRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Every route and plugin rule, as they stood at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSet {
    /// The revision the rules were read at.
    pub revision: RuleRevision,
    /// Every stored routing rule.
    pub routes: Vec<RouteRule>,
    /// Every plugin rule, global and tenant.
    pub rules: Vec<PluginRule>,
}

/// Reads how far the rules have moved on, and the rules themselves as of one moment.
#[async_trait]
pub trait RevisionStore: Send + Sync {
    /// The revision the rules stand at now.
    async fn rule_revision(&self) -> Result<RuleRevision, StorageError>;

    /// Every route and plugin rule with the revision they stand at, read in one transaction
    /// so the set is never a mix of two moments.
    async fn rule_set(&self) -> Result<RuleSet, StorageError>;
}
