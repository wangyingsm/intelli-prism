use ip_core::TenantId;
use ip_retrieve::{
    CollectionName, MaxHits, MetaField, MetaNote, MetaSchema, NodeLabel, PlaneName, Retriever,
    RetrieverAuth, RetrieverEndpoint, RetrieverId, RetrieverKey, RetrieverName, RetrieverParts,
    RetrieverTool, RetrieverTools, Source, VectorProperty,
};

use crate::audit::By;
use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::retriever::{NewRetriever, RetrieverOwner};
use crate::store::Backend;

/// The account every suite test writes as, through the endpoint that writes a retriever.
pub(crate) fn by() -> By {
    By::new(crate::suite::fixture::user_id(), "/_ip/retrievers")
}

/// The tenant that owns nothing anybody created.
fn absent_tenant() -> TenantId {
    TenantId::new("nobody").unwrap()
}

/// A retriever over a collection, under `id`, called `name`, requiring nothing.
pub(crate) fn retriever(id: &str, name: &str) -> Retriever {
    built(id, name, RetrieverAuth::None)
}

/// The same, requiring a key.
pub(crate) fn keyed(id: &str, name: &str, key: &str) -> Retriever {
    built(
        id,
        name,
        RetrieverAuth::Key {
            key: RetrieverKey::new(key).unwrap(),
        },
    )
}

/// One retriever over a collection, which is the simplest thing either backend stores.
fn built(id: &str, name: &str, auth: RetrieverAuth) -> Retriever {
    Retriever::new(RetrieverParts {
        id: RetrieverId::new(id).unwrap(),
        name: RetrieverName::new(name).unwrap(),
        endpoint: RetrieverEndpoint::new("https://qdrant.acme.test:6333").unwrap(),
        auth,
        source: Source::Qdrant {
            collection: CollectionName::new("handbook").unwrap(),
            vector: None,
        },
        tools: RetrieverTools::new(vec![RetrieverTool::Search]).unwrap(),
        meta: MetaSchema::default(),
        max_hits: MaxHits::default(),
    })
    .unwrap()
}

/// That retriever, about to be written for one owner.
pub(crate) fn written_by(owner: RetrieverOwner, id: &str, name: &str) -> NewRetriever {
    NewRetriever {
        owner,
        retriever: retriever(id, name),
    }
}

pub(crate) async fn a_retriever_round_trips(store: &impl Backend) {
    let written = store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "The handbook"),
            &by(),
        )
        .await
        .unwrap();

    assert_eq!(written.retriever, retriever("handbook", "The handbook"));
    assert_eq!(written.owner, RetrieverOwner::Global);

    let read = store
        .retriever(
            &RetrieverOwner::Global,
            &RetrieverId::new("handbook").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(read, Some(written));
}

pub(crate) async fn every_part_of_a_retriever_survives_the_database(store: &impl Backend) {
    let retriever = Retriever::new(RetrieverParts {
        id: RetrieverId::new("everything").unwrap(),
        name: RetrieverName::new("Everything at once").unwrap(),
        endpoint: RetrieverEndpoint::new("https://drsg.acme.test:44453/rpc").unwrap(),
        auth: RetrieverAuth::Key {
            key: RetrieverKey::new("qd-live-9f2c4a7b1e").unwrap(),
        },
        source: Source::Drsg {
            plane: PlaneName::new("handbook").unwrap(),
            label: Some(NodeLabel::new("Document").unwrap()),
            property: VectorProperty::new("embedding").unwrap(),
        },
        tools: RetrieverTools::new(vec![
            RetrieverTool::Search,
            RetrieverTool::Neighbors,
            RetrieverTool::Rank,
            RetrieverTool::Relate,
            RetrieverTool::Cluster,
        ])
        .unwrap(),
        meta: MetaSchema::new(
            [
                (
                    MetaField::new("quarter").unwrap(),
                    MetaNote::new("Which quarter the figure is for, as 2026Q1").unwrap(),
                ),
                (
                    MetaField::new("team").unwrap(),
                    MetaNote::new("Which team the page belongs to").unwrap(),
                ),
            ]
            .into(),
        )
        .unwrap(),
        max_hits: MaxHits::new(12).unwrap(),
    })
    .unwrap();

    store
        .put_retriever(
            NewRetriever {
                owner: RetrieverOwner::Global,
                retriever: retriever.clone(),
            },
            &by(),
        )
        .await
        .unwrap();

    let read = store
        .retriever(&RetrieverOwner::Global, retriever.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.retriever, retriever);
}

pub(crate) async fn a_key_survives_the_database(store: &impl Backend) {
    const KEY: &str = "qd-live-9f2c4a7b1e";
    store
        .put_retriever(
            NewRetriever {
                owner: RetrieverOwner::Global,
                retriever: keyed("handbook", "The handbook", KEY),
            },
            &by(),
        )
        .await
        .unwrap();

    let read = store
        .retriever(
            &RetrieverOwner::Global,
            &RetrieverId::new("handbook").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        read.retriever.auth().key().map(RetrieverKey::expose),
        Some(KEY),
        "a backend that needs a key is unreachable without it"
    );
}

pub(crate) async fn a_retriever_that_stops_requiring_a_key_keeps_none(store: &impl Backend) {
    let owner = RetrieverOwner::Global;
    let id = RetrieverId::new("handbook").unwrap();
    store
        .put_retriever(
            NewRetriever {
                owner: owner.clone(),
                retriever: keyed("handbook", "The handbook", "qd-live-9f2c4a7b1e"),
            },
            &by(),
        )
        .await
        .unwrap();
    store
        .put_retriever(written_by(owner.clone(), "handbook", "The handbook"), &by())
        .await
        .unwrap();

    let read = store.retriever(&owner, &id).await.unwrap().unwrap();
    assert_eq!(
        read.retriever.auth(),
        &RetrieverAuth::None,
        "the key the row held is not kept behind a write that requires none"
    );
}

pub(crate) async fn one_id_names_a_different_retriever_under_each_owner(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = RetrieverOwner::Tenant(tenant.id.clone());

    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_retriever(
            written_by(theirs.clone(), "handbook", "The tenant's"),
            &by(),
        )
        .await
        .unwrap();

    let id = RetrieverId::new("handbook").unwrap();
    let global = store
        .retriever(&RetrieverOwner::Global, &id)
        .await
        .unwrap()
        .unwrap();
    let owned = store.retriever(&theirs, &id).await.unwrap().unwrap();

    assert_eq!(global.retriever.name().as_str(), "The deployment's");
    assert_eq!(owned.retriever.name().as_str(), "The tenant's");
    assert_eq!(owned.owner, theirs);
}

/// The assertion ruling 4 exists for: a corpus is a tenant's own, and an id is not a way in.
pub(crate) async fn one_tenant_cannot_read_another_tenants_retriever_by_id(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = RetrieverOwner::Tenant(tenant.id.clone());
    let other = store
        .create_tenant(
            crate::model::NewTenant {
                id: TenantId::new("other").unwrap(),
                key: ip_core::TnKey::generate().unwrap(),
            },
            &crate::suite::identity::tenants_by(),
        )
        .await
        .unwrap();
    let theirs_too = RetrieverOwner::Tenant(other.id);

    store
        .put_retriever(
            written_by(theirs.clone(), "handbook", "The first tenant's"),
            &by(),
        )
        .await
        .unwrap();

    let id = RetrieverId::new("handbook").unwrap();
    assert_eq!(
        store.retriever(&theirs_too, &id).await.unwrap(),
        None,
        "a tenant knowing another's id must not reach its corpus"
    );
    assert!(
        store
            .retrievers(&theirs_too, Page::new(10, 0, None))
            .await
            .unwrap()
            .is_empty(),
        "nor by listing its own"
    );
    assert!(store.retriever(&theirs, &id).await.unwrap().is_some());
}

pub(crate) async fn writing_a_retriever_again_replaces_it(store: &impl Backend) {
    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "First"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "Second"),
            &by(),
        )
        .await
        .unwrap();

    let held = store
        .retrievers(&RetrieverOwner::Global, Page::new(10, 0, None))
        .await
        .unwrap();

    assert_eq!(held.len(), 1, "one owner, one id, one retriever");
    assert_eq!(held[0].retriever.name().as_str(), "Second");
}

pub(crate) async fn removing_a_retriever_leaves_the_others(store: &impl Backend) {
    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "The handbook"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "tickets", "The tickets"),
            &by(),
        )
        .await
        .unwrap();

    store
        .remove_retriever(
            &RetrieverOwner::Global,
            &RetrieverId::new("handbook").unwrap(),
            &by(),
        )
        .await
        .unwrap();

    let held = store
        .retrievers(&RetrieverOwner::Global, Page::new(10, 0, None))
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].retriever.id().as_str(), "tickets");
}

pub(crate) async fn removing_a_retriever_that_is_absent_reports_it_missing(store: &impl Backend) {
    let error = store
        .remove_retriever(
            &RetrieverOwner::Global,
            &RetrieverId::new("handbook").unwrap(),
            &by(),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            StorageError::NotFound { entity: Entity::Retriever, ref id } if id == "handbook"
        ),
        "{error:?}"
    );
}

pub(crate) async fn removing_one_owners_retriever_leaves_the_other_owners(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = RetrieverOwner::Tenant(tenant.id.clone());
    let id = RetrieverId::new("handbook").unwrap();

    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_retriever(
            written_by(theirs.clone(), "handbook", "The tenant's"),
            &by(),
        )
        .await
        .unwrap();

    store
        .remove_retriever(&RetrieverOwner::Global, &id, &by())
        .await
        .unwrap();

    assert_eq!(
        store.retriever(&RetrieverOwner::Global, &id).await.unwrap(),
        None
    );
    assert!(store.retriever(&theirs, &id).await.unwrap().is_some());
}

pub(crate) async fn a_retriever_for_a_tenant_that_is_not_there_is_refused(store: &impl Backend) {
    let error = store
        .put_retriever(
            written_by(
                RetrieverOwner::Tenant(absent_tenant()),
                "handbook",
                "The handbook",
            ),
            &by(),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            StorageError::NotFound {
                entity: Entity::Tenant,
                ..
            }
        ),
        "{error:?}"
    );
}

pub(crate) async fn a_read_for_a_tenant_that_is_not_there_answers_with_nothing_of_the_deployments(
    store: &impl Backend,
) {
    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();

    let absent = RetrieverOwner::Tenant(absent_tenant());
    let one = store
        .retriever(&absent, &RetrieverId::new("handbook").unwrap())
        .await
        .unwrap_err();
    let listed = store
        .retrievers(&absent, Page::new(10, 0, None))
        .await
        .unwrap_err();

    for error in [one, listed] {
        assert!(
            matches!(
                error,
                StorageError::NotFound {
                    entity: Entity::Tenant,
                    ..
                }
            ),
            "a tenant nobody created must not read as the deployment: {error:?}"
        );
    }
}

pub(crate) async fn retrievers_come_newest_first_a_page_at_a_time(store: &impl Backend) {
    for id in ["first", "second", "third"] {
        store
            .put_retriever(written_by(RetrieverOwner::Global, id, "Named"), &by())
            .await
            .unwrap();
    }

    let newest = store
        .retrievers(&RetrieverOwner::Global, Page::new(2, 0, None))
        .await
        .unwrap();
    let rest = store
        .retrievers(&RetrieverOwner::Global, Page::new(2, 2, None))
        .await
        .unwrap();

    let named: Vec<&str> = newest
        .iter()
        .chain(rest.iter())
        .map(|held| held.retriever.id().as_str())
        .collect();
    assert_eq!(named, ["third", "second", "first"]);
    assert_eq!(newest.len(), 2);
    assert_eq!(rest.len(), 1);
}

pub(crate) async fn deleting_a_tenant_takes_its_retrievers_with_it(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = RetrieverOwner::Tenant(tenant.id.clone());

    store
        .put_retriever(
            written_by(theirs.clone(), "handbook", "The tenant's"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_retriever(
            written_by(RetrieverOwner::Global, "handbook", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();

    store
        .delete_tenant(&tenant.id, &crate::suite::identity::tenants_by())
        .await
        .unwrap();

    let id = RetrieverId::new("handbook").unwrap();
    assert!(
        store.retriever(&theirs, &id).await.is_err(),
        "the tenant is gone, so asking for its retrievers names no tenant"
    );
    assert!(
        store
            .retriever(&RetrieverOwner::Global, &id)
            .await
            .unwrap()
            .is_some(),
        "the deployment's own survives a tenant going"
    );
}
