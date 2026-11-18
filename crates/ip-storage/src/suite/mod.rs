//! Tests every storage backend must pass, written once and run against each backend.

// This is a test suite, but it lives behind the `testing` feature rather than under
// `cfg(test)` so that every backend's own tests can run it, which puts it outside what
// `allow-unwrap-in-tests` can see.
#![allow(
    clippy::unwrap_used,
    reason = "a suite of tests, compiled as a library"
)]

pub(crate) mod agent;
pub(crate) mod audit;
pub(crate) mod identity;
pub(crate) mod isolation;
pub(crate) mod limit;
pub(crate) mod list;
pub(crate) mod mcp;
pub(crate) mod member_add;
pub(crate) mod member_remove;
pub(crate) mod plugin;
pub(crate) mod price;
pub(crate) mod retriever;
pub(crate) mod revision;
pub(crate) mod route;
pub(crate) mod usage;
pub(crate) mod user_create;

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
        let tenant = store
            .create_tenant(new_tenant(), &crate::suite::identity::tenants_by())
            .await
            .unwrap();
        let user = store
            .create_user(new_user(), &crate::suite::identity::accounts_by())
            .await
            .unwrap();
        (tenant, user)
    }
}

/// Writes one `#[tokio::test]` per shared test, each on a fresh store from `$open`, an async fn
/// that returns `None` when its backend cannot run here. Naming a module runs only its tests.
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
    (identity, $open:path) => {
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
            attaching_a_user_that_is_not_there_reports_it_missing,
            deleting_a_user_takes_its_memberships_with_it,
            a_tenant_is_listed_from_both_sides,
            deleting_a_tenant_takes_its_memberships_and_grants_with_it,
            granting_the_same_capability_twice_changes_nothing,
            every_scope_shape_round_trips,
            grants_in_a_tenant_exclude_the_user_scoped_ones,
            revoking_what_is_not_held_reports_it_missing,
            revoking_removes_only_the_named_grant,
        ]);
    };
    (member_add, $open:path) => {
        $crate::suite::backend_suite!(@module member_add, $open, [
            a_member_lands_with_its_attachment,
            a_member_nobody_commits_leaves_no_user_behind,
            a_tenant_that_is_not_there_takes_the_user_with_it,
            a_member_may_be_added_as_the_owner,
            a_user_id_that_is_taken_conflicts,
        ]);
    };
    (list, $open:path) => {
        $crate::suite::backend_suite!(@module list, $open, [
            members_come_newest_first_a_page_at_a_time,
            a_moment_to_list_after_keeps_what_came_before_it_out,
            grants_are_listed_newest_first_by_where_they_are_held,
            routes_are_listed_newest_first_with_every_target,
            rules_are_listed_by_chain_newest_first,
            plugins_are_listed_to_the_chain_that_owns_them,
        ]);
    };
    (revision, $open:path) => {
        $crate::suite::backend_suite!(@module revision, $open, [
            every_change_to_a_route_moves_the_revision_on,
            every_change_to_a_plugin_rule_moves_the_revision_on,
            rules_a_deleted_tenant_takes_with_it_move_the_revision_on,
            rules_a_deleted_user_takes_with_it_move_the_revision_on,
            changes_to_nothing_a_node_holds_leave_the_revision_alone,
            the_rule_set_is_read_at_the_revision_it_names,
        ]);
    };
    (member_remove, $open:path) => {
        $crate::suite::backend_suite!(@module member_remove, $open, [
            a_member_leaves_with_every_grant_it_held_inside,
            a_removal_nobody_commits_leaves_the_member_and_its_grants,
            leaving_a_tenant_it_is_not_in_reports_it_missing,
        ]);
    };
    (user_create, $open:path) => {
        $crate::suite::backend_suite!(@module user_create, $open, [
            a_committed_transaction_lands_every_write,
            a_transaction_nobody_commits_leaves_nothing,
            a_write_that_fails_part_way_undoes_the_ones_before_it,
            a_transaction_reads_back_what_it_has_written,
            an_owner_is_attached_to_the_tenant_the_transaction_wrote,
            writes_outside_a_transaction_are_not_rolled_back,
        ]);
    };
    (route, $open:path) => {
        $crate::suite::backend_suite!(@module route, $open, [
            a_route_round_trips,
            a_route_names_the_api_it_serves,
            writing_the_same_route_key_replaces_its_target,
            a_route_key_carries_every_part_of_the_tuple,
            every_protocol_survives_a_round_trip,
            removing_a_route_that_is_absent_reports_it_missing,
            a_route_may_stand_for_several_endpoints,
            rewriting_a_route_replaces_its_whole_endpoint_list,
            a_rule_keeps_the_strategy_and_the_shares_it_was_stored_with,
            a_rule_stored_without_a_strategy_dispatches_in_turn,
            rewriting_a_rule_replaces_its_strategy,
            an_endpoint_may_be_stored_drained,
            a_rule_keeps_the_hedge_it_was_stored_with,
            a_rule_keeps_the_hedge_delay_it_was_stored_with,
            a_rule_stored_without_a_hedge_delay_sends_arms_together,
            a_rule_keeps_the_dialects_it_bridges,
            a_rule_stored_without_dialects_translates_nothing,
            rewriting_a_rule_replaces_its_dialects,
            rewriting_a_rule_replaces_its_hedge_delay,
            a_rule_stored_without_a_hedge_reaches_one_endpoint,
            rewriting_a_rule_replaces_its_hedge,
            a_rule_keeps_the_classes_its_endpoints_serve,
            a_rule_stored_without_a_ceiling_serves_anything,
            rewriting_a_rule_replaces_its_endpoints_ceilings,
        ]);
    };
    (plugin, $open:path) => {
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
            the_same_wasm_is_stored_once_and_goes_with_its_last_owner,
            wasm_stored_as_another_kind_is_refused,
            a_chain_places_only_the_plugins_it_owns,
            an_owner_that_is_not_there_leaves_no_wasm_behind,
        ]);
    };
    (limit, $open:path) => {
        $crate::suite::backend_suite!(@module limit, $open, [
            a_limit_round_trips,
            every_scope_shape_round_trips,
            setting_the_same_limit_again_replaces_what_it_allows,
            one_scope_holds_a_limit_per_thing_counted_and_period,
            removing_a_limit_leaves_the_others,
            removing_a_limit_that_is_absent_reports_it_missing,
            a_limit_on_a_tenant_that_is_not_there_is_refused,
            a_limit_on_a_user_that_is_not_there_is_refused,
            deleting_a_tenant_takes_its_limits_with_it,
            deleting_a_user_takes_the_limits_named_for_it,
            every_change_to_a_limit_moves_the_revision_on,
            limits_come_newest_first_a_page_at_a_time,
            a_listed_limit_is_the_one_that_was_set,
            a_list_holds_only_the_tenant_it_names,
        ]);
    };
    (agent, $open:path) => {
        $crate::suite::backend_suite!(@module agent, $open, [
            a_template_round_trips,
            every_part_of_a_template_survives_the_database,
            one_id_names_a_different_template_under_each_owner,
            writing_a_template_again_replaces_it,
            removing_a_template_leaves_the_others,
            removing_a_template_that_is_absent_reports_it_missing,
            removing_one_owners_template_leaves_the_other_owners,
            a_template_for_a_tenant_that_is_not_there_is_refused,
            a_read_for_a_tenant_that_is_not_there_answers_with_nothing_of_the_deployments,
            templates_come_newest_first_a_page_at_a_time,
            a_list_holds_only_the_owner_it_names,
            deleting_a_tenant_takes_its_templates_with_it,
        ]);
    };
    (mcp, $open:path) => {
        $crate::suite::backend_suite!(@module mcp, $open, [
            a_server_round_trips,
            a_credential_survives_the_database,
            registering_a_server_again_replaces_it,
            a_server_that_stops_requiring_a_token_keeps_none,
            rotating_a_credential_leaves_one_server_holding_the_new_one,
            removing_a_server_leaves_the_others,
            removing_a_server_that_is_absent_reports_it_missing,
            servers_come_newest_first_a_page_at_a_time,
            deleting_a_tenant_leaves_every_server_where_it_was,
        ]);
    };
    (retriever, $open:path) => {
        $crate::suite::backend_suite!(@module retriever, $open, [
            a_retriever_round_trips,
            every_part_of_a_retriever_survives_the_database,
            a_key_survives_the_database,
            a_retriever_that_stops_requiring_a_key_keeps_none,
            one_id_names_a_different_retriever_under_each_owner,
            one_tenant_cannot_read_another_tenants_retriever_by_id,
            writing_a_retriever_again_replaces_it,
            removing_a_retriever_leaves_the_others,
            removing_a_retriever_that_is_absent_reports_it_missing,
            removing_one_owners_retriever_leaves_the_other_owners,
            a_retriever_for_a_tenant_that_is_not_there_is_refused,
            a_read_for_a_tenant_that_is_not_there_answers_with_nothing_of_the_deployments,
            retrievers_come_newest_first_a_page_at_a_time,
            deleting_a_tenant_takes_its_retrievers_with_it,
        ]);
    };
    (audit, $open:path) => {
        $crate::suite::backend_suite!(@module audit, $open, [
            setting_a_limit_records_who_set_it,
            setting_a_limit_again_records_what_it_allowed,
            removing_a_limit_records_what_was_removed,
            a_change_that_was_refused_records_nothing,
            a_write_that_fails_leaves_no_record,
            the_trail_is_narrowed_to_what_the_filter_names,
            pricing_a_model_records_that_it_belongs_to_nobody,
            repricing_and_unpricing_a_model_record_what_it_cost,
            a_tenant_sees_none_of_what_belongs_to_the_deployment,
            writing_a_template_records_who_wrote_it,
            writing_a_tenants_template_records_it_against_that_tenant,
            rewriting_and_removing_a_template_record_what_it_said,
            a_template_that_was_refused_records_nothing,
            registering_a_server_records_everything_but_its_token,
            rewriting_and_removing_a_server_record_what_it_said,
            writing_a_retriever_records_everything_but_its_key,
            rewriting_and_removing_a_retriever_record_what_it_said,
            routing_a_key_somewhere_else_records_both_targets,
            removing_a_route_records_where_it_pointed,
            whose_a_stored_plugin_is_comes_from_who_stored_it,
            letting_go_of_a_plugin_records_what_was_held,
            placing_a_plugin_in_a_chain_records_where_it_was_put,
            taking_a_plugin_out_of_the_global_chain_names_no_tenant,
            creating_and_deleting_a_tenant_record_both_ends,
            attaching_an_account_records_the_tenant_it_joined,
            promoting_an_account_records_what_it_was,
            adding_a_member_records_the_account_and_the_attachment,
            taking_a_member_out_records_the_attachment_that_went,
            a_tenants_record_never_holds_its_root_key,
            creating_a_tenant_with_its_owner_records_both,
            an_accounts_record_never_holds_its_passphrase,
            changing_a_passphrase_records_that_it_changed_and_never_what_to,
            creating_and_deleting_an_account_record_both_ends,
            the_trail_names_the_account_that_asked,
            handing_a_grant_over_records_who_handed_it,
            handing_over_a_grant_already_held_records_nothing_twice,
            taking_a_grant_back_records_what_went,
            taking_back_a_grant_that_is_not_held_records_nothing,
            a_grant_against_an_account_belongs_to_no_tenant,
            leaving_a_tenant_records_every_grant_it_took_away,
            leaving_a_tenant_leaves_what_is_held_outside_it,
            leaving_a_tenant_holding_nothing_records_only_the_attachment,
        ]);
    };
    (price, $open:path) => {
        $crate::suite::backend_suite!(@module price, $open, [
            a_price_round_trips,
            pricing_the_same_model_again_replaces_what_it_costs,
            a_model_costing_nothing_is_priced_at_nothing,
            every_model_holds_its_own_price,
            removing_a_price_leaves_the_others,
            removing_a_price_that_is_absent_reports_it_missing,
            every_change_to_a_price_moves_the_revision_on,
            a_price_belongs_to_no_tenant,
        ]);
    };
    (isolation, $open:path) => {
        $crate::suite::backend_suite!(@module isolation, $open, [
            members_of_one_tenant_are_not_listed_for_another,
            grants_inside_one_tenant_are_not_listed_for_another,
            grants_on_one_account_are_not_listed_for_another,
            plugins_one_tenant_owns_are_not_listed_for_another,
            limits_on_one_tenant_are_not_listed_for_another,
            what_one_tenant_spent_is_not_listed_for_another,
            what_one_tenant_spent_is_not_summed_for_another,
            rules_in_one_tenants_chain_are_not_listed_for_another,
            routes_belong_to_no_tenant_and_the_audit_says_so,
        ]);
    };
    (usage, $open:path) => {
        $crate::suite::backend_suite!(@module usage, $open, [
            a_recorded_request_round_trips,
            the_primary_key_is_an_integer_the_backend_assigns,
            an_answer_out_of_the_cache_is_recorded_spending_nothing,
            where_an_answer_came_from_round_trips,
            a_request_carrying_no_turn_or_model_round_trips,
            what_a_tenant_spent_outlives_the_tenant,
            reading_a_request_nobody_recorded_is_nothing,
            what_was_spent_comes_newest_first_a_page_at_a_time,
            a_list_is_narrowed_to_what_the_filter_names,
            a_moment_to_list_after_keeps_what_came_before_it_out,
            a_listed_row_carries_everything_it_was_recorded_with,
            rows_past_their_keeping_are_swept_away,
            what_was_spent_is_summed_for_a_scope_since_a_moment,
            what_was_spent_before_the_moment_is_not_counted,
            nothing_spent_sums_to_nothing,
            a_row_says_which_endpoint_behind_the_rule_answered,
            a_row_says_how_many_endpoints_it_was_raced_across,
            a_row_says_what_class_it_was_dispatched_at,
            a_row_says_which_level_decided_its_class,
            a_row_says_what_was_taken_out_of_it,
            a_row_a_filter_looked_at_and_found_nothing_in_says_so,
            a_row_no_filter_looked_at_says_nothing_at_all,
            the_summary_counts_what_each_level_decided,
            a_row_the_cache_answered_names_no_endpoint,
            a_cache_hit_raises_what_was_answered_not_what_was_sent,
            what_was_sent_upstream_counts_one_per_request_when_none_raced,
            what_was_sent_upstream_adds_the_arms_of_a_raced_request,
            what_was_sent_upstream_over_nobody_is_nothing,
            what_was_sent_upstream_narrows_to_the_filter,
            what_was_sent_upstream_after_a_moment_excludes_the_moment,
            a_row_says_what_it_cost,
            a_row_nothing_priced_costs_nothing_known,
            a_cost_past_what_a_column_holds_is_refused,
            a_listed_row_carries_what_it_cost,
            a_window_sums_what_its_rows_cost,
            a_window_says_how_many_of_its_rows_nothing_priced,
            a_window_of_nothing_costs_nothing,
            what_a_scope_cost_is_summed_the_way_its_tokens_are,
            an_unpriced_row_costs_a_scope_nothing,
            a_scope_that_cost_nothing_sums_to_nothing,
        ]);
    };
    ($open:path) => {
        $crate::suite::backend_suite!(identity, $open);
        $crate::suite::backend_suite!(route, $open);
        $crate::suite::backend_suite!(plugin, $open);
        $crate::suite::backend_suite!(user_create, $open);
        $crate::suite::backend_suite!(member_add, $open);
        $crate::suite::backend_suite!(member_remove, $open);
        $crate::suite::backend_suite!(list, $open);
        $crate::suite::backend_suite!(revision, $open);
        $crate::suite::backend_suite!(usage, $open);
        $crate::suite::backend_suite!(limit, $open);
        $crate::suite::backend_suite!(audit, $open);
        $crate::suite::backend_suite!(price, $open);
        $crate::suite::backend_suite!(agent, $open);
        $crate::suite::backend_suite!(mcp, $open);
        $crate::suite::backend_suite!(retriever, $open);
        $crate::suite::backend_suite!(isolation, $open);
    };
}

pub(crate) use backend_suite;
