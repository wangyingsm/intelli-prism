//! Tests every storage backend must pass, written once and run against each backend.

pub(crate) mod identity;
pub(crate) mod plugin;
pub(crate) mod route;

use crate::plugin::{PluginRuleStore, PluginStore};
use crate::route::RouteStore;
use crate::store::Storage;

/// Everything a backend implements, so a shared test can run against any of them.
pub(crate) trait AnyStore: Storage + RouteStore + PluginStore + PluginRuleStore {}

impl<T> AnyStore for T where T: Storage + RouteStore + PluginStore + PluginRuleStore {}

/// The records every backend test builds from.
pub(crate) mod fixture {
    use ip_core::{PassphraseHash, TenantId, TnKey, UserId};

    use crate::model::{AccountKind, NewTenant, NewUser, Tenant, User};
    use crate::store::{TenantStore, UserStore};

    pub(crate) fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    pub(crate) fn user_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    pub(crate) fn hash() -> PassphraseHash {
        PassphraseHash::new("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA").unwrap()
    }

    pub(crate) fn new_tenant() -> NewTenant {
        NewTenant {
            id: tenant_id(),
            key: TnKey::generate().unwrap(),
        }
    }

    pub(crate) fn new_user() -> NewUser {
        NewUser {
            id: user_id(),
            passphrase: hash(),
            kind: AccountKind::Regular,
        }
    }

    pub(crate) async fn tenant_with_user(store: &(impl TenantStore + UserStore)) -> (Tenant, User) {
        let tenant = store.create_tenant(new_tenant()).await.unwrap();
        let user = store.create_user(new_user()).await.unwrap();
        (tenant, user)
    }
}

/// Writes one `#[tokio::test]` per shared test, each on a fresh store from `$open`, an async fn
/// that returns `None` when its backend cannot run here.
macro_rules! backend_suite {
    (@module $module:ident, $open:path, [$($test:ident),* $(,)?]) => {
        mod $module {
            $(
                #[tokio::test]
                async fn $test() {
                    let Some(store) = $open().await else {
                        return;
                    };
                    $crate::suite::$module::$test(&store).await;
                }
            )*
        }
    };
    ($open:path) => {
        $crate::suite::backend_suite!(@module identity, $open, [
            a_fresh_database_is_migrated_and_empty,
            a_tenant_round_trips_with_its_key_intact,
            the_primary_key_is_an_integer_the_backend_assigns,
            a_repeated_tenant_id_conflicts,
            a_repeated_user_id_conflicts,
            a_user_round_trips_and_its_passphrase_can_be_replaced,
            the_account_kind_survives_a_round_trip,
            deleting_what_is_absent_reports_it_missing,
            attaching_twice_replaces_the_standing,
            attaching_to_a_tenant_that_is_not_there_reports_it_missing,
            a_tenant_is_listed_from_both_sides,
            deleting_a_tenant_takes_its_memberships_and_grants_with_it,
            granting_the_same_capability_twice_changes_nothing,
            every_scope_shape_round_trips,
            grants_in_a_tenant_exclude_the_user_scoped_ones,
            revoking_what_is_not_held_reports_it_missing,
            revoking_removes_only_the_named_grant,
        ]);
        $crate::suite::backend_suite!(@module route, $open, [
            a_route_round_trips,
            a_route_names_the_api_it_serves,
            writing_the_same_route_key_replaces_its_target,
            a_route_key_carries_every_part_of_the_tuple,
            every_protocol_survives_a_round_trip,
            removing_a_route_that_is_absent_reports_it_missing,
            a_route_may_stand_for_several_endpoints,
            rewriting_a_route_replaces_its_whole_endpoint_list,
        ]);
        $crate::suite::backend_suite!(@module plugin, $open, [
            a_plugin_round_trips_with_its_wasm,
            storing_the_same_plugin_twice_keeps_one_row,
            listing_plugins_reports_their_size,
            removing_a_plugin_that_is_absent_reports_it_missing,
            a_plugin_a_rule_still_uses_is_not_removed_until_the_rule_goes,
            every_rule_scope_round_trips,
            a_stored_rule_takes_its_kind_from_the_plugin,
            a_rule_naming_an_absent_plugin_is_refused,
            a_rule_in_a_tenant_that_is_not_there_is_refused,
            an_order_a_tenant_already_uses_for_that_kind_is_refused,
            the_same_order_under_another_kind_is_allowed,
            a_tenant_reads_its_own_rules_and_the_global_ones,
            rules_come_back_highest_order_first,
            deleting_a_tenant_takes_its_rules_with_it,
            removing_a_global_rule_leaves_a_tenant_rule_at_the_same_kind,
        ]);
    };
}

pub(crate) use backend_suite;
