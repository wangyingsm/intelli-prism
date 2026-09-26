use ip_core::{
    AbsPath, ApiId, Endpoint, Host, Port, Protocol, RouteKey, RouteRule, RouteTarget, Strategy,
    Weight, Weighted,
};

use crate::error::{Entity, StorageError};
use crate::route::RouteStore;

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
    }
}

pub(crate) async fn a_route_round_trips(store: &impl RouteStore) {
    let rule = rule("gateway.local", "/v1/messages", "api.example.com");
    store.put_route(rule.clone()).await.unwrap();
    assert_eq!(store.route(&rule.key).await.unwrap(), Some(rule.clone()));
    assert_eq!(store.routes().await.unwrap(), vec![rule]);
}

pub(crate) async fn a_route_names_the_api_it_serves(store: &impl RouteStore) {
    let mut rule = rule("gateway.local", "/internal", "llm.corp");
    rule.api = ApiId::new("internal").unwrap();
    store.put_route(rule.clone()).await.unwrap();
    assert_eq!(
        store.route(&rule.key).await.unwrap().unwrap().api,
        ApiId::new("internal").unwrap()
    );
    assert_eq!(store.routes().await.unwrap()[0].api, rule.api);
}

pub(crate) async fn writing_the_same_route_key_replaces_its_target(store: &impl RouteStore) {
    store
        .put_route(rule("gateway.local", "/v1", "first.example.com"))
        .await
        .unwrap();
    let second = rule("gateway.local", "/v1", "second.example.com");
    store.put_route(second.clone()).await.unwrap();
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
    };
    store.put_route(secure).await.unwrap();
    store.put_route(plain).await.unwrap();
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
            .put_route(RouteRule {
                api: ApiId::new("anthropic").unwrap(),
                key: key.clone(),
                target: RouteTarget::new(route_endpoint(protocol, "api.example.com", 443, "/v1")),
                strategy: ip_core::Strategy::default(),
            })
            .await
            .unwrap();
        let read = store.route(&key).await.unwrap().unwrap();
        assert_eq!(read.target.primary().protocol, protocol);
    }
}

pub(crate) async fn removing_a_route_that_is_absent_reports_it_missing(store: &impl RouteStore) {
    let key = RouteKey::new(route_endpoint(Protocol::Https, "gateway.local", 443, "/v1"));
    assert!(matches!(
        store.remove_route(&key).await,
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
    store.put_route(rule.clone()).await.unwrap();

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
    store.put_route(replicated).await.unwrap();

    let narrowed = rule("gateway.local", "/v1", "three.example.com");
    store.put_route(narrowed.clone()).await.unwrap();
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
    store.put_route(replicated.clone()).await.unwrap();

    let read = store.route(&replicated.key).await.unwrap().unwrap();
    assert_eq!(read, replicated);
    assert_eq!(read.strategy, Strategy::Ratio);
    assert_eq!(read.target.weighted()[0].weight.get(), 3);
    assert_eq!(read.target.weighted()[1].weight.get(), 1);
    assert_eq!(store.routes().await.unwrap(), vec![replicated]);
}

pub(crate) async fn a_rule_stored_without_a_strategy_dispatches_in_turn(store: &impl RouteStore) {
    let plain = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(plain.clone()).await.unwrap();
    let read = store.route(&plain.key).await.unwrap().unwrap();
    assert_eq!(read.strategy, Strategy::RoundRobin);
    assert_eq!(read.target.weighted()[0].weight, Weight::default());
}

pub(crate) async fn rewriting_a_rule_replaces_its_strategy(store: &impl RouteStore) {
    let mut rule = rule("gateway.local", "/v1", "api.example.com");
    store.put_route(rule.clone()).await.unwrap();
    rule.strategy = Strategy::LeastLoad;
    store.put_route(rule.clone()).await.unwrap();
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
    store.put_route(drained.clone()).await.unwrap();

    let read = store.route(&drained.key).await.unwrap().unwrap();
    assert_eq!(read, drained);
    assert!(read.target.weighted()[1].weight.is_drained());
}
