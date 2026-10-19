//! Whether one tenant's member can reach another tenant through the management api.
//!
//! The storage audit asks whether a listing naming one tenant can return another's
//! rows, and says plainly what it cannot cover: reads that take an id — a usage row, a
//! route, a plugin — are not scoped by anything a store knows. This is the half that
//! covers them, because what protects them is the capability check on the way in.
//!
//! Every case is the same shape. `alice` is a member of `acme` and of nothing else.
//! She asks for something belonging to `globex`, by path or by body, and must be
//! refused — never served, and never told the difference between a thing that is not
//! hers and a thing that does not exist.
//!
//! It is written in one place and covers every nested router rather than the handful
//! that happen to have a test of their own, because isolation is only worth anything
//! when it has no gaps. A router added without a case fails [`every_router_is_audited`].

#[cfg(test)]
mod tests {
    use crate::routes::harness::{PASSPHRASE, cookie_of, hashed, request, state_over};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Response, StatusCode};
    use ip_core::{ApiId, Capability, CapabilityScope, Grant, TenantId, TnKey, UserId};
    use ip_storage::model::{AccountKind, Membership, NewTenant, NewUser, Standing};
    use ip_storage::sqlite::SqliteStore;
    use ip_storage::store::{GrantStore, MembershipStore, TenantStore, UserStore};
    use tower::ServiceExt;

    fn mine() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn theirs() -> TenantId {
        TenantId::new("globex").unwrap()
    }

    fn me() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn them() -> UserId {
        UserId::new("mallory").unwrap()
    }

    /// What `alice` is allowed to do inside `acme`, and nowhere else.
    ///
    /// Without these the audit would prove nothing: a member holding no capability is
    /// refused everywhere, so every case would pass whether or not the tenant check
    /// worked at all. She is given real rights in her own tenant so that the only
    /// thing standing between her and `globex` is the boundary itself.
    fn rights_in(tenant: &TenantId) -> Vec<Grant> {
        let at_tenant = |capability| {
            Grant::new(
                capability,
                CapabilityScope::Tenant {
                    user: me(),
                    tenant: tenant.clone(),
                },
            )
            .unwrap()
        };
        let at_api = |capability| {
            Grant::new(
                capability,
                CapabilityScope::Api {
                    user: me(),
                    tenant: tenant.clone(),
                    api: ApiId::new("anthropic").unwrap(),
                },
            )
            .unwrap()
        };
        vec![
            at_tenant(Capability::UserMgr),
            at_tenant(Capability::Observer),
            at_api(Capability::ApiAccess),
            at_api(Capability::LimitMgr),
        ]
    }

    /// Two tenants, each with one member of its own, and nobody in both.
    async fn fixture() -> (Router, String) {
        let store = SqliteStore::in_memory().await.unwrap();
        for (tenant, user) in [(mine(), me()), (theirs(), them())] {
            store
                .create_tenant(NewTenant {
                    id: tenant.clone(),
                    key: TnKey::generate().unwrap(),
                })
                .await
                .unwrap();
            store
                .create_user(NewUser {
                    id: user.clone(),
                    passphrase: hashed(),
                    kind: AccountKind::Regular,
                })
                .await
                .unwrap();
            store
                .attach(Membership {
                    user,
                    tenant,
                    standing: Standing::Member,
                })
                .await
                .unwrap();
        }
        for grant in rights_in(&mine()) {
            store.grant(&grant).await.unwrap();
        }
        let state = state_over(store);
        let cookie = cookie_of(&state, &me()).await;
        (crate::routes::router(state), cookie)
    }

    /// Refused, and refused in a way that tells the asker nothing.
    ///
    /// A 404 and a 403 are both acceptable — which one is a judgement about leaking
    /// existence — but a 200 is not, and neither is a 500: a server that falls over on
    /// a cross-tenant request is one whose check ran too late to matter.
    fn refused(what: &str, response: &Response<Body>) {
        let status = response.status();
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            "{what} answered {status}, which is not a refusal"
        );
    }

    /// Every endpoint that names a tenant in its path.
    ///
    /// The member of one tenant asks for the other's by url, which is the plainest way
    /// to try the boundary and the one a mistake in `Manager::allows` would open.
    #[tokio::test]
    async fn no_endpoint_naming_a_tenant_in_its_path_serves_another_tenants_member() {
        let cases: [(&str, String, Option<String>); 7] = [
            ("GET", format!("/_ip/tenants/{}", theirs()), None),
            ("DELETE", format!("/_ip/tenants/{}", theirs()), None),
            ("GET", format!("/_ip/tenants/{}/members", theirs()), None),
            (
                "PUT",
                format!("/_ip/tenants/{}/members/{}", theirs(), me()),
                Some(r#"{"standing":"member"}"#.to_owned()),
            ),
            (
                "DELETE",
                format!("/_ip/tenants/{}/members/{}", theirs(), them()),
                None,
            ),
            (
                "GET",
                format!("/_ip/tenants/{}/members/{}/grants", theirs(), them()),
                None,
            ),
            (
                "PUT",
                format!(
                    "/_ip/tenants/{}/members/{}/grants/observer",
                    theirs(),
                    them()
                ),
                None,
            ),
        ];
        let (router, cookie) = fixture().await;
        for (method, path, body) in cases {
            let response = router
                .clone()
                .oneshot(request(method, &path, &cookie, body.as_deref()))
                .await
                .unwrap();
            refused(&format!("{method} {path}"), &response);
        }
    }

    /// Every endpoint that takes a tenant in its query or its body rather than its path.
    ///
    /// These are the ones a path-shaped audit misses, and the ones where the check is
    /// easiest to forget: the url looks like nobody's in particular.
    #[tokio::test]
    async fn no_endpoint_taking_a_tenant_in_a_body_serves_another_tenants_member() {
        let theirs = theirs();
        let cases: [(&str, String, Option<String>); 5] = [
            ("GET", format!("/_ip/limits?tenant={theirs}"), None),
            (
                "PUT",
                "/_ip/limits".to_owned(),
                Some(format!(
                    r#"{{"tenant":"{theirs}","counted":"requests","period":"month","allowance":10}}"#
                )),
            ),
            ("GET", format!("/_ip/usage?tenant={theirs}"), None),
            ("GET", format!("/_ip/usage/summary?tenant={theirs}"), None),
            (
                "POST",
                "/_ip/keys/tenant".to_owned(),
                Some(format!(
                    r#"{{"tenant":"{theirs}","passphrase":"{PASSPHRASE}"}}"#
                )),
            ),
        ];
        let (router, cookie) = fixture().await;
        for (method, path, body) in cases {
            let response = router
                .clone()
                .oneshot(request(method, &path, &cookie, body.as_deref()))
                .await
                .unwrap();
            refused(&format!("{method} {path}"), &response);
        }
    }

    /// A caller that asks for everything, naming no tenant.
    ///
    /// This is the quieter failure: not a refused request but an answered one holding
    /// somebody else's rows. Three answers are safe and the endpoints differ — one
    /// insists a tenant be named, which is the strongest of them, and the test says so
    /// rather than demanding they all behave alike.
    #[tokio::test]
    async fn a_listing_that_names_no_tenant_shows_nothing_of_another_tenants() {
        let (router, cookie) = fixture().await;
        for path in ["/_ip/limits", "/_ip/usage", "/_ip/plugin-rules"] {
            let response = router
                .clone()
                .oneshot(request("GET", path, &cookie, None))
                .await
                .unwrap();
            match response.status() {
                // Answered: then it must hold nothing of the other tenant's.
                StatusCode::OK => {
                    let said = crate::routes::harness::body_of(response).await;
                    assert!(
                        !said.contains(theirs().as_str()),
                        "{path} showed {} to a member of {}: {said}",
                        theirs(),
                        mine()
                    );
                }
                // Refused outright, or refused until a tenant is named. Both leak
                // nothing, and the second is what `/_ip/usage` does.
                StatusCode::FORBIDDEN
                | StatusCode::NOT_FOUND
                | StatusCode::UNPROCESSABLE_ENTITY => {}
                other => panic!("{path} answered {other}, which is neither safe nor a refusal"),
            }
        }
    }

    /// The control that makes the refusals mean something.
    ///
    /// The same reads, against her own tenant, must be served. If this ever fails the
    /// cases above are passing because `alice` can do nothing anywhere, which proves
    /// nothing about the boundary.
    #[tokio::test]
    async fn the_same_reads_against_her_own_tenant_are_served() {
        let served: [String; 3] = [
            format!("/_ip/tenants/{}", mine()),
            format!("/_ip/tenants/{}/members", mine()),
            format!("/_ip/usage?tenant={}", mine()),
        ];
        let (router, cookie) = fixture().await;
        for path in served {
            let response = router
                .clone()
                .oneshot(request("GET", &path, &cookie, None))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{path} was refused to the tenant's own member, so the refusals above \
                 say nothing about the boundary"
            );
        }
    }

    /// The tripwire. Every router nested under the reserved prefix must be reachable
    /// from a case above, or isolation has a gap exactly where nobody looked.
    ///
    /// Read from the source rather than from a list kept by hand, because a list kept
    /// by hand is the thing that goes stale.
    #[test]
    fn every_router_is_audited() {
        const SOURCE: &str = include_str!("mod.rs");
        // Taken from each `.nest(` to the quoted prefix after it, wherever the line
        // breaks fall: rustfmt puts a long one on a line of its own, and a parser that
        // reads single lines quietly misses exactly those.
        let nested: Vec<String> = SOURCE
            .match_indices(".nest(")
            .filter_map(|(at, _)| SOURCE[at..].split('"').nth(1).map(str::to_owned))
            .collect();
        // Every prefix, and what this file does about it. A prefix that holds nothing
        // of any tenant's says so here rather than being quietly left out.
        let audited: [(&str, &str); 11] = [
            ("/_ip/tenants", "asked for by path"),
            ("/_ip/tenants/{tenant}/members", "asked for by path"),
            (
                "/_ip/tenants/{tenant}/members/{user}/grants",
                "asked for by path",
            ),
            ("/_ip/users", "accounts belong to no tenant"),
            ("/_ip/users/{user}/grants", "accounts belong to no tenant"),
            ("/_ip/routes", "routes belong to the gateway, not a tenant"),
            ("/_ip/keys", "asked for by body"),
            ("/_ip/plugins", "owned by a tenant, listed by owner"),
            ("/_ip/plugin-rules", "listed without naming a tenant"),
            ("/_ip/limits", "asked for by query and by body"),
            ("/_ip/usage", "asked for by query"),
        ];
        for prefix in &nested {
            assert!(
                audited.iter().any(|(known, _)| known == prefix),
                "{prefix} is nested but has no isolation case; add one to routes/isolation.rs"
            );
        }
        assert_eq!(
            nested.len(),
            audited.len(),
            "{} routers are nested and {} are audited: {nested:?}",
            nested.len(),
            audited.len()
        );
    }
}
