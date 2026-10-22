use ip_core::{
    AbsPath, ApiId, Dialect, Difficulty, Endpoint, Hedge, HedgeDelay, Host, Port, Protocol,
    RouteKey, RouteRule, RouteTarget, Strategy, Weight, Weighted,
};

use crate::audit::By;
use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::route::RouteStore;
use crate::store::Backend;

/// The account every suite test routes as, through the endpoint that sets a rule.
pub(crate) fn by() -> By {
    By::new(crate::suite::fixture::user_id(), "/_ip/routes")
}

fn route_endpoint(protocol: Protocol, host: &str, port: u16, path: &str) -> Endpoint {
    Endpoint::new(
        protocol,
        Host::new(host).unwrap(),
        Port::new(port).unwrap(),
        AbsPath::new(path).unwrap(),
    )
}

/// A rule taking `path` on `host` to the same path on `upstream`, over https on port 443.
pub(crate) fn rule(host: &str, path: &str, upstream: &str) -> RouteRule {
    RouteRule {
        api: ApiId::new("anthropic").unwrap(),
        key: RouteKey::new(route_endpoint(Protocol::Https, host, 443, path)),
        target: RouteTarget::new(route_endpoint(Protocol::Https, upstream, 443, path)),
        strategy: ip_core::Strategy::default(),
        hedge: Hedge::default(),
        hedge_delay: HedgeDelay::default(),
        caller_speaks: None,
        upstream_speaks: None,
    }
}

pub(crate) async fn a_route_round_trips(store: &impl RouteStore) {
    let rule = rule("gateway.local", "/v1/messages", "api.example.com");
    store.put_route(rule.clone(), &by()).await.unwrap();
    assert_eq!(store.route(&rule.key).await.unwrap(), Some(rule.clone()));
    assert_eq!(store.routes().await.unwrap(), vec![rule]);
}

pub(crate) async fn a_route_names_the_api_it_serves(store: &impl RouteStore) {
    let mut rule = rule("gateway.local", "/internal", "llm.corp");
    rule.api = ApiId::new("internal").unwrap();
    store.put_route(rule.clone(), &by()).await.unwrap();
    assert_eq!(
        store.route(&rule.key).await.unwrap().unwrap().api,
        ApiId::new("internal").unwrap()
    );
    assert_eq!(store.routes().await.unwrap()[0].api, rule.api);
}

pub(crate) async fn writing_the_same_route_key_replaces_its_target(store: &impl RouteStore) {
    store
        .put_route(rule("gateway.local", "/v1", "first.example.com"), &by())
        .await
        .unwrap();
    let second = rule("gateway.local", "/v1", "second.example.com");
    store.put_route(second.clone(), &by()).await.unwrap();
    assert_eq!(store.routes().await.unwrap(), vec![second]);
}

pub(crate) async fn a_route_key_carries_every_part_of_the_tuple(store: &impl RouteStore) {
    let secure = rule("gateway.local", "/v1", "api.example.com");
    let plain = RouteRule {
        api: ApiId::new("anthropic").unwrap(),
        key: RouteKey::new(route_endpoint(Protocol::Http, "gateway.local", 443, "/v1")),
        target: RouteTarget::new(route_endpoint(
            Protocol::Https,
            "api.example.com",
            443,
            "/v1",
        )),
        strategy: ip_core::Strategy::default(),
        hedge: Hedge::default(),
        hedge_delay: HedgeDelay::default(),
        caller_speaks: None,
        upstream_speaks: None,
    };
    store.put_route(secure, &by()).await.unwrap();
    store.put_route(plain, &by()).await.unwrap();
    assert_eq!(store.routes().await.unwrap().len(), 2);
}

pub(crate) async fn every_protocol_survives_a_round_trip(store: &impl RouteStore) {
    for protocol in [
        Protocol::Http,
        Protocol::Https,
        Protocol::Ws,
        Protocol::Wss,
        Protocol::Tcp,
    ] {
        let key = RouteKey::new(route_endpoint(protocol, "gateway.local", 443, "/v1"));
        store
            .put_route(
                RouteRule {
                    api: ApiId::new("anthropic").unwrap(),
                    key: key.clone(),
                    target: RouteTarget::new(route_endpoint(
                        protocol,
                        "api.example.com",
                        443,
                        "/v1",
                    )),
                    strategy: ip_core::Strategy::default(),
                    hedge: Hedge::default(),
                    hedge_delay: HedgeDelay::default(),
                    caller_speaks: None,
                    upstream_speaks: None,
                },
                &by(),
            )
            .await
            .unwrap();
        let read = store.route(&key).await.unwrap().unwrap();
        assert_eq!(read.target.primary().protocol, protocol);
    }
}

pub(crate) async fn removing_a_route_that_is_absent_reports_it_missing(store: &impl RouteStore) {
    let key = RouteKey::new(route_endpoint(Protocol::Https, "gateway.local", 443, "/v1"));
    assert!(matches!(
        store.remove_route(&key, &by()).await,
        Err(StorageError::NotFound {
            entity: Entity::Route,
            ..
        })
    ));
}

pub(crate) async fn a_route_may_stand_for_several_endpoints(store: &impl RouteStore) {
    let mut rule = rule("gateway.local", "/v1", "one.example.com");
    rule.target = RouteTarget::from_endpoints(vec![
        route_endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
        route_endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
        route_endpoint(Protocol::Http, "127.0.0.1", 9000, "/v1"),
    ])
    .unwrap();
    store.put_route(rule.clone(), &by()).await.unwrap();

    let read = store.route(&rule.key).await.unwrap().unwrap();
    assert_eq!(read, rule);
    assert_eq!(read.target.endpoints().len(), 3);
    assert_eq!(read.target.primary().host.as_str(), "one.example.com");
    assert_eq!(store.routes().await.unwrap(), vec![rule]);
}

pub(crate) async fn rewriting_a_route_replaces_its_whole_endpoint_list(store: &impl RouteStore) {
    let mut replicated = rule("gateway.local", "/v1", "one.example.com");
    replicated.target = RouteTarget::from_endpoints(vec![
        route_endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
        route_endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
    ])
    .unwrap();
    store.put_route(replicated, &by()).await.unwrap();

    let narrowed = rule("gateway.local", "/v1", "three.example.com");
    store.put_route(narrowed.clone(), &by()).await.unwrap();
    assert_eq!(store.routes().await.unwrap(), vec![narrowed]);
}

pub(crate) async fn a_rule_keeps_the_strategy_and_the_shares_it_was_stored_with(
    store: &impl RouteStore,
) {
    let mut replicated = rule("gateway.local", "/v2", "one.example.com");
    replicated.strategy = Strategy::Ratio;
    let mut second = replicated.target.primary().clone();
    second.host = Host::new("two.example.com").unwrap();
    replicated.target = RouteTarget::from_weighted(vec![
        Weighted::weighing(replicated.target.primary().clone(), Weight::new(3)),
        Weighted::new(second),
    ])
    .unwrap();
    store.put_route(replicated.clone(), &by()).await.unwrap();

    let read = store.route(&replicated.key).await.unwrap().unwrap();
    assert_eq!(read, replicated);
    assert_eq!(read.strategy, Strategy::Ratio);
    assert_eq!(read.target.weighted()[0].weight.get(), 3);
    assert_eq!(read.target.weighted()[1].weight.get(), 1);
    assert_eq!(store.routes().await.unwrap(), vec![replicated]);
}

pub(crate) async fn a_rule_stored_without_a_strategy_dispatches_in_turn(store: &impl RouteStore) {
    let plain = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(plain.clone(), &by()).await.unwrap();
    let read = store.route(&plain.key).await.unwrap().unwrap();
    assert_eq!(read.strategy, Strategy::RoundRobin);
    assert_eq!(read.target.weighted()[0].weight, Weight::default());
}

pub(crate) async fn rewriting_a_rule_replaces_its_strategy(store: &impl RouteStore) {
    let mut rule = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(rule.clone(), &by()).await.unwrap();
    rule.strategy = Strategy::LeastLoad;
    store.put_route(rule.clone(), &by()).await.unwrap();
    assert_eq!(
        store.route(&rule.key).await.unwrap().unwrap().strategy,
        Strategy::LeastLoad
    );
}

pub(crate) async fn an_endpoint_may_be_stored_drained(store: &impl RouteStore) {
    let mut drained = rule("gateway.local", "/v3", "one.example.com");
    let mut spare = drained.target.primary().clone();
    spare.host = Host::new("two.example.com").unwrap();
    drained.target = RouteTarget::from_weighted(vec![
        Weighted::new(drained.target.primary().clone()),
        Weighted::weighing(spare, Weight::DRAINED),
    ])
    .unwrap();
    store.put_route(drained.clone(), &by()).await.unwrap();

    let read = store.route(&drained.key).await.unwrap().unwrap();
    assert_eq!(read, drained);
    assert!(read.target.weighted()[1].weight.is_drained());
}

pub(crate) async fn a_rule_keeps_the_hedge_it_was_stored_with(store: &impl RouteStore) {
    let mut raced = rule("gateway.local", "/raced", "api.example.com");
    raced.hedge = Hedge::new(3).unwrap();
    store.put_route(raced.clone(), &by()).await.unwrap();

    let read = store.route(&raced.key).await.unwrap().unwrap();
    assert_eq!(read, raced);
    assert_eq!(read.hedge.get(), 3);
    assert_eq!(store.routes().await.unwrap(), vec![raced]);
}

pub(crate) async fn a_rule_stored_without_a_hedge_reaches_one_endpoint(store: &impl RouteStore) {
    let plain = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(plain.clone(), &by()).await.unwrap();
    let read = store.route(&plain.key).await.unwrap().unwrap();
    assert_eq!(read.hedge, Hedge::NONE);
    assert!(!read.hedge.is_hedged());
}

pub(crate) async fn rewriting_a_rule_replaces_its_hedge(store: &impl RouteStore) {
    let mut rule = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(rule.clone(), &by()).await.unwrap();
    rule.hedge = Hedge::new(2).unwrap();
    store.put_route(rule.clone(), &by()).await.unwrap();
    assert_eq!(
        store.route(&rule.key).await.unwrap().unwrap().hedge.get(),
        2
    );
}

/// One rule that waits a quarter of a second between arms, the delay round-tripping onto
/// the row and out of the paged list alike.
pub(crate) async fn a_rule_keeps_the_hedge_delay_it_was_stored_with(store: &impl RouteStore) {
    let mut waited = rule("gateway.local", "/waited", "api.example.com");
    waited.hedge = Hedge::new(2).unwrap();
    waited.hedge_delay = HedgeDelay::new(250);
    store.put_route(waited.clone(), &by()).await.unwrap();

    let read = store.route(&waited.key).await.unwrap().unwrap();
    assert_eq!(read, waited);
    assert_eq!(read.hedge_delay.get(), 250);
    assert_eq!(store.routes().await.unwrap(), vec![waited]);
}

/// Every pairing of families a rule can bridge survives the round trip and the listing,
/// which is where M10 first went wrong by reading a column and discarding it.
pub(crate) async fn a_rule_keeps_the_dialects_it_bridges(store: &impl Backend) {
    for (caller, upstream) in [
        (Some(Dialect::OpenAi), Some(Dialect::Anthropic)),
        (Some(Dialect::Anthropic), Some(Dialect::Gemini)),
        (Some(Dialect::Gemini), Some(Dialect::OpenAi)),
        (Some(Dialect::OpenAi), Some(Dialect::OpenAi)),
        (Some(Dialect::OpenAi), None),
        (None, Some(Dialect::Gemini)),
    ] {
        let mut bridging = rule("gateway.local", "/bridged", "api.example.com");
        bridging.caller_speaks = caller;
        bridging.upstream_speaks = upstream;
        store.put_route(bridging.clone(), &by()).await.unwrap();

        let read = store.route(&bridging.key).await.unwrap().unwrap();
        assert_eq!(read, bridging, "{caller:?} to {upstream:?} did not survive");
        let listed = store.routes().await.unwrap();
        assert!(
            listed.contains(&bridging),
            "{caller:?} to {upstream:?} was lost on the way out of the listing"
        );
        // And out of the paged listing, which is a query of its own: M10's mistake was
        // a column read by one path and discarded by the other.
        let paged = store.list_routes(Page::default()).await.unwrap();
        assert!(
            paged.iter().any(|listed| listed.item == bridging),
            "{caller:?} to {upstream:?} was lost on the way out of the paged listing"
        );
    }
}

/// A rule written before any of this existed names neither family and translates
/// nothing, which is what the migration must leave every stored rule doing.
pub(crate) async fn a_rule_stored_without_dialects_translates_nothing(store: &impl RouteStore) {
    let plain = rule("gateway.local", "/v1", "api.example.com");
    assert_eq!(plain.caller_speaks, None);
    store.put_route(plain.clone(), &by()).await.unwrap();
    let read = store.route(&plain.key).await.unwrap().unwrap();
    assert_eq!(read.caller_speaks, None);
    assert_eq!(read.upstream_speaks, None);
}

/// Rewriting a rule replaces what it bridges, including back to bridging nothing — the
/// upsert must clear a column, not only set one.
pub(crate) async fn rewriting_a_rule_replaces_its_dialects(store: &impl RouteStore) {
    let mut bridging = rule("gateway.local", "/bridged", "api.example.com");
    bridging.caller_speaks = Some(Dialect::OpenAi);
    bridging.upstream_speaks = Some(Dialect::Gemini);
    store.put_route(bridging.clone(), &by()).await.unwrap();

    let plain = rule("gateway.local", "/bridged", "api.example.com");
    store.put_route(plain.clone(), &by()).await.unwrap();
    let read = store.route(&plain.key).await.unwrap().unwrap();
    assert_eq!(read.caller_speaks, None, "a cleared dialect stayed set");
    assert_eq!(read.upstream_speaks, None);
}

pub(crate) async fn a_rule_stored_without_a_hedge_delay_sends_arms_together(
    store: &impl RouteStore,
) {
    let plain = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(plain.clone(), &by()).await.unwrap();
    let read = store.route(&plain.key).await.unwrap().unwrap();
    assert_eq!(read.hedge_delay, HedgeDelay::NONE);
    assert!(read.hedge_delay.is_none());
}

pub(crate) async fn rewriting_a_rule_replaces_its_hedge_delay(store: &impl RouteStore) {
    let mut rule = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(rule.clone(), &by()).await.unwrap();
    rule.hedge_delay = HedgeDelay::new(50);
    store.put_route(rule.clone(), &by()).await.unwrap();
    assert_eq!(
        store
            .route(&rule.key)
            .await
            .unwrap()
            .unwrap()
            .hedge_delay
            .get(),
        50
    );
}

pub(crate) async fn a_rule_keeps_the_classes_its_endpoints_serve(store: &impl RouteStore) {
    let mut ceiled = rule("gateway.local", "/ceiled", "one.example.com");
    let spare = route_endpoint(Protocol::Https, "two.example.com", 443, "/ceiled");
    ceiled.target = RouteTarget::from_weighted(vec![
        Weighted::serving(
            ceiled.target.primary().clone(),
            Weight::default(),
            Some(Difficulty::Median),
        ),
        Weighted::serving(spare, Weight::default(), Some(Difficulty::Research)),
    ])
    .unwrap();
    store.put_route(ceiled.clone(), &by()).await.unwrap();

    let read = store.route(&ceiled.key).await.unwrap().unwrap();
    assert_eq!(read, ceiled);
    assert_eq!(read.target.weighted()[0].serves, Some(Difficulty::Median));
    assert_eq!(read.target.weighted()[1].serves, Some(Difficulty::Research));
    assert_eq!(store.routes().await.unwrap(), vec![ceiled]);
}

pub(crate) async fn a_rule_stored_without_a_ceiling_serves_anything(store: &impl RouteStore) {
    let plain = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(plain.clone(), &by()).await.unwrap();
    let read = store.route(&plain.key).await.unwrap().unwrap();
    assert_eq!(read.target.weighted()[0].serves, None);
    assert_eq!(store.routes().await.unwrap(), vec![plain]);
}

pub(crate) async fn rewriting_a_rule_replaces_its_endpoints_ceilings(store: &impl RouteStore) {
    let mut lowered = rule("gateway.local", "/v1", "api.example.com");
    lowered.target = RouteTarget::from_weighted(vec![Weighted::serving(
        lowered.target.primary().clone(),
        Weight::default(),
        Some(Difficulty::Easy),
    )])
    .unwrap();
    store.put_route(lowered, &by()).await.unwrap();

    let mut raised = rule("gateway.local", "/v1", "api.example.com");
    raised.target = RouteTarget::from_weighted(vec![Weighted::serving(
        raised.target.primary().clone(),
        Weight::default(),
        Some(Difficulty::Hard),
    )])
    .unwrap();
    store.put_route(raised.clone(), &by()).await.unwrap();

    let read = store.route(&raised.key).await.unwrap().unwrap();
    assert_eq!(read.target.weighted()[0].serves, Some(Difficulty::Hard));
    assert_eq!(store.routes().await.unwrap(), vec![raised]);
}
