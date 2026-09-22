//! Reading lists a page at a time, newest first.

use async_trait::async_trait;
use ip_core::{Grant, PluginRule, RouteRule, TenantId, Timestamp, UserId};

use crate::error::StorageError;
use crate::model::Membership;

/// How many records a page holds when the caller names no size.
pub const DEFAULT_PAGE_LIMIT: u32 = 20;

/// The most records one page holds, whatever the caller asks for.
pub const MAX_PAGE_LIMIT: u32 = 100;

/// One slice of a list ordered newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    limit: u32,
    offset: u32,
    after: Option<Timestamp>,
}

impl Page {
    /// Up to `limit` records, skipping the first `offset`, counting only those created after
    /// `after` when it is set. A limit outside one to [`MAX_PAGE_LIMIT`] is brought inside.
    pub fn new(limit: u32, offset: u32, after: Option<Timestamp>) -> Self {
        Self {
            limit: limit.clamp(1, MAX_PAGE_LIMIT),
            offset,
            after,
        }
    }

    /// How many records the page holds at most.
    pub fn limit(self) -> u32 {
        self.limit
    }

    /// How many records are skipped before it.
    pub fn offset(self) -> u32 {
        self.offset
    }

    /// The moment every record on it was created after, if one was named.
    pub fn after(self) -> Option<Timestamp> {
        self.after
    }

    /// The seconds a query compares `created_at` against, which admits every row when no
    /// moment was named.
    pub fn after_seconds(self) -> i64 {
        self.after.map_or(i64::MIN, |after| after.unix_seconds())
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new(DEFAULT_PAGE_LIMIT, 0, None)
    }
}

/// One record of a list, with when it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed<T> {
    /// The record.
    pub item: T,
    /// When it was created, which is what the list is ordered by.
    pub created_at: Timestamp,
}

/// The lists the management api serves, each read a page at a time, newest first.
#[async_trait]
pub trait ListStore: Send + Sync {
    /// Everyone in one tenant.
    async fn list_members(
        &self,
        tenant: &TenantId,
        page: Page,
    ) -> Result<Vec<Listed<Membership>>, StorageError>;

    /// The grants one member holds inside one tenant.
    async fn list_tenant_grants(
        &self,
        user: &UserId,
        tenant: &TenantId,
        page: Page,
    ) -> Result<Vec<Listed<Grant>>, StorageError>;

    /// The grants held against an account itself rather than inside a tenant.
    async fn list_account_grants(
        &self,
        user: &UserId,
        page: Page,
    ) -> Result<Vec<Listed<Grant>>, StorageError>;

    /// The stored routing rules.
    async fn list_routes(&self, page: Page) -> Result<Vec<Listed<RouteRule>>, StorageError>;

    /// The rules in one chain: a tenant's, or the global chain when none is named.
    async fn list_rules(
        &self,
        tenant: Option<&TenantId>,
        page: Page,
    ) -> Result<Vec<Listed<PluginRule>>, StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_holds_twenty_from_the_start_unless_told_otherwise() {
        let page = Page::default();
        assert_eq!((page.limit(), page.offset(), page.after()), (20, 0, None));
        assert_eq!(page.after_seconds(), i64::MIN);
    }

    #[test]
    fn a_limit_is_brought_between_one_and_the_most_a_page_holds() {
        assert_eq!(Page::new(0, 0, None).limit(), 1);
        assert_eq!(Page::new(5_000, 0, None).limit(), MAX_PAGE_LIMIT);
        assert_eq!(Page::new(50, 7, None).limit(), 50);
    }

    #[test]
    fn a_named_moment_is_compared_in_seconds() {
        let after = Timestamp::from_unix_seconds(1_790_000_000).unwrap();
        assert_eq!(Page::new(20, 0, Some(after)).after_seconds(), 1_790_000_000);
    }
}
