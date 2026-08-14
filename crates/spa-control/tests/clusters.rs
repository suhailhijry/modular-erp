//! Multi-cluster registration, capacity and placement, against a real database.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_control::{
    AccessError, Actor, ClusterRegistry, ClusterStatus, ControlPlane, PlacementPolicy, PoolConfig,
    TenantPools,
};
use spa_testkit::{Schema, Template};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);

async fn control_plane() -> (ControlPlane, spa_testkit::TestDb) {
    let db = Template::get(&CONTROL)
        .await
        .expect("control template builds")
        .fresh()
        .await
        .expect("control database clones");

    let clusters = ClusterRegistry::new()
        .with_url("unused", &spa_testkit::database_url())
        .expect("URL parses");

    (
        ControlPlane::new(
            db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ),
        db,
    )
}

async fn register(control: &ControlPlane, name: &str, max_active: i32, max_dbs: i32) {
    control
        .register_cluster(
            name,
            &format!("SPA_CLUSTER_{}_URL", name.to_uppercase()),
            None,
            max_active,
            max_dbs,
            Actor::system(),
        )
        .await
        .expect("cluster registers");
}

#[tokio::test]
async fn a_tenant_cannot_be_placed_on_a_cluster_that_does_not_exist() {
    let (control, _db) = control_plane().await;

    // The foreign key is the guard: a typo'd cluster name must not produce a
    // tenant nobody can ever reach.
    let result = control
        .register_tenant_on("acme", "Acme", "typo-cluster", Actor::system())
        .await;
    assert!(
        matches!(result, Err(AccessError::Database(_))),
        "expected a constraint violation, got {result:?}"
    );
}

#[tokio::test]
async fn placement_refuses_when_there_is_no_cluster_at_all() {
    let (control, _db) = control_plane().await;

    let result = control
        .register_tenant("acme", "Acme", PlacementPolicy::Balanced, Actor::system())
        .await;
    assert!(
        matches!(
            result,
            Err(AccessError::NoCapacity {
                clusters_at_limit: 0
            })
        ),
        "expected NoCapacity, got {result:?}"
    );
}

/// Balanced placement spreads tenants, because activity is what binds — packing
/// them onto one cluster reaches its connection ceiling while others idle.
#[tokio::test]
async fn balanced_placement_spreads_tenants_across_clusters() {
    let (control, _db) = control_plane().await;
    register(&control, "alpha", 100, 700).await;
    register(&control, "beta", 100, 700).await;
    register(&control, "gamma", 100, 700).await;

    let mut placements = std::collections::BTreeMap::new();
    for i in 0..9 {
        let tenant = control
            .register_tenant(
                &format!("t{i}"),
                "Tenant",
                PlacementPolicy::Balanced,
                Actor::system(),
            )
            .await
            .expect("registers");
        *placements.entry(tenant.cluster).or_insert(0) += 1;
    }

    assert_eq!(placements.len(), 3, "all three clusters should be used");
    for (cluster, count) in &placements {
        assert_eq!(*count, 3, "{cluster} should have an even share");
    }
}

/// The limit that matters. A cluster full of *active* tenants must stop taking
/// more even when it has plenty of storage headroom, because open connections
/// scale with concurrent activity.
#[tokio::test]
async fn the_active_tenant_limit_stops_placement_before_storage_does() {
    let (control, _db) = control_plane().await;
    // Room for 700 databases, but only 2 active tenants.
    register(&control, "tight", 2, 700).await;

    for i in 0..2 {
        let tenant = control
            .register_tenant(
                &format!("t{i}"),
                "Tenant",
                PlacementPolicy::Balanced,
                Actor::system(),
            )
            .await
            .expect("registers");
        control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("activates");
    }

    let result = control
        .register_tenant("third", "Third", PlacementPolicy::Balanced, Actor::system())
        .await;
    assert!(
        matches!(
            result,
            Err(AccessError::NoCapacity {
                clusters_at_limit: 1
            })
        ),
        "the active-tenant limit must bind before storage does, got {result:?}"
    );
}

#[tokio::test]
async fn the_database_limit_binds_even_when_nothing_is_active() {
    let (control, _db) = control_plane().await;
    // Plenty of activity headroom, room for two databases.
    register(&control, "small", 500, 2).await;

    for i in 0..2 {
        control
            .register_tenant(
                &format!("t{i}"),
                "Tenant",
                PlacementPolicy::Balanced,
                Actor::system(),
            )
            .await
            .expect("registers");
    }
    // Both are still `provisioning`, so zero are active — storage must bind.
    assert!(
        control
            .register_tenant("third", "Third", PlacementPolicy::Balanced, Actor::system())
            .await
            .is_err()
    );
}

/// Draining is how hardware is retired: keep serving, stop growing.
#[tokio::test]
async fn a_draining_cluster_keeps_its_tenants_but_takes_no_more() {
    let (control, _db) = control_plane().await;
    register(&control, "old", 100, 700).await;
    register(&control, "new", 100, 700).await;

    let existing = control
        .register_tenant_on("acme", "Acme", "old", Actor::system())
        .await
        .expect("registers");

    control
        .set_cluster_status("old", ClusterStatus::Draining, Actor::system())
        .await
        .expect("drains");

    // Everything new goes elsewhere.
    for i in 0..4 {
        let tenant = control
            .register_tenant(
                &format!("t{i}"),
                "Tenant",
                PlacementPolicy::Balanced,
                Actor::system(),
            )
            .await
            .expect("registers");
        assert_eq!(
            tenant.cluster, "new",
            "a draining cluster must take no more"
        );
    }

    // The tenant already there is untouched.
    let still_there = control
        .tenant(existing.id)
        .await
        .expect("reads")
        .expect("exists");
    assert_eq!(still_there.cluster, "old");
}

#[tokio::test]
async fn cluster_load_reports_what_each_cluster_carries() {
    let (control, _db) = control_plane().await;
    register(&control, "alpha", 50, 100).await;
    register(&control, "beta", 50, 100).await;

    let first = control
        .register_tenant_on("a1", "A1", "alpha", Actor::system())
        .await
        .expect("registers");
    control
        .register_tenant_on("a2", "A2", "alpha", Actor::system())
        .await
        .expect("registers");
    control
        .activate_tenant(first.id, Actor::system())
        .await
        .expect("activates");

    let load = control.cluster_load().await.expect("reads load");
    let alpha = load.iter().find(|c| c.name == "alpha").expect("alpha");
    let beta = load.iter().find(|c| c.name == "beta").expect("beta");

    assert_eq!(alpha.live_tenants, 2);
    assert_eq!(alpha.active_tenants, 1, "only one has been activated");
    assert_eq!(beta.live_tenants, 0);

    // Utilization takes the binding limit: 1/50 activity (200bp) vs 2/100
    // storage (200bp) — equal here, and exact because it is integer arithmetic.
    assert_eq!(alpha.utilization_bp(), 200);
}

/// A cluster still holding tenants must not be deletable — that would orphan
/// databases nobody can find.
#[tokio::test]
async fn a_cluster_with_tenants_cannot_be_deleted() {
    let (control, _db) = control_plane().await;
    register(&control, "alpha", 50, 100).await;
    control
        .register_tenant_on("acme", "Acme", "alpha", Actor::system())
        .await
        .expect("registers");

    let result = sqlx::query("DELETE FROM cluster WHERE name = 'alpha'")
        .execute(control.pool())
        .await;
    assert!(
        result.is_err(),
        "deleting a cluster that still holds tenants must be refused"
    );
}

/// A taken slug is a normal signup outcome, not a database failure — the user
/// should be told to pick another name, not shown "something went wrong".
#[tokio::test]
async fn a_duplicate_slug_is_a_typed_error() {
    let (control, _db) = control_plane().await;
    register(&control, "alpha", 50, 100).await;

    control
        .register_tenant("acme", "Acme", PlacementPolicy::Balanced, Actor::system())
        .await
        .expect("registers");

    let result = control
        .register_tenant(
            "acme",
            "Acme Two",
            PlacementPolicy::Balanced,
            Actor::system(),
        )
        .await;
    assert!(
        matches!(&result, Err(AccessError::SlugTaken(slug)) if slug == "acme"),
        "expected SlugTaken, got {result:?}"
    );
}

#[tokio::test]
async fn packed_placement_fills_one_cluster_before_the_next() {
    let (control, _db) = control_plane().await;
    register(&control, "alpha", 3, 3).await;
    register(&control, "beta", 100, 700).await;

    let mut on_alpha = 0;
    for i in 0..3 {
        let tenant = control
            .register_tenant(
                &format!("t{i}"),
                "Tenant",
                PlacementPolicy::Packed,
                Actor::system(),
            )
            .await
            .expect("registers");
        if tenant.cluster == "alpha" {
            on_alpha += 1;
        }
    }
    assert_eq!(
        on_alpha, 3,
        "packed placement should fill alpha before touching beta"
    );

    // Once alpha is full, placement rolls over rather than failing.
    let overflow = control
        .register_tenant("t4", "Tenant", PlacementPolicy::Packed, Actor::system())
        .await
        .expect("registers");
    assert_eq!(overflow.cluster, "beta");
}

/// Registering is declaring a cluster's configuration, not creating a row once.
///
/// An operator raising a capacity, or repointing the variable a cluster's
/// credentials come from, should not have to know whether the cluster was
/// registered before. The first version was a bare `INSERT`, which made
/// `just demo` fail the second time it was run against the same deployment.
#[tokio::test]
async fn registering_a_cluster_again_updates_it_rather_than_failing() {
    let (control, _db) = control_plane().await;

    register(&control, "primary", 100, 200).await;
    register(&control, "primary", 500, 900).await;

    let load = control.cluster_load().await.expect("reads load");
    assert_eq!(load.len(), 1, "one cluster, not two");
    assert_eq!(load[0].max_active_tenants, 500);
    assert_eq!(load[0].max_databases, 900);
}

/// ...but it must not undo an operational decision.
///
/// A cluster is drained to retire hardware. Re-declaring its capacity while
/// that is happening — a config-management run, a re-run of the seeder — must
/// not put it back into service and start placing tenants on it.
#[tokio::test]
async fn re_registering_does_not_undrain_a_cluster() {
    let (control, _db) = control_plane().await;

    register(&control, "primary", 100, 200).await;
    control
        .set_cluster_status("primary", ClusterStatus::Draining, Actor::system())
        .await
        .expect("drains");

    register(&control, "primary", 100, 200).await;

    let load = control.cluster_load().await.expect("reads load");
    assert_eq!(load[0].status, ClusterStatus::Draining);
}
