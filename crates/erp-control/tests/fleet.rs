//! Migrating every tenant that already exists.
//!
//! The test that carries this file is
//! [`a_tenant_behind_the_fleet_is_found_and_brought_current`]. It builds a
//! tenant whose database has never been migrated at all — the state every
//! existing tenant would be in the day a new tenant-plane migration ships — and
//! checks that a survey sees it and a migration fixes it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantPools, totp};
use erp_eventlog::{SealingKey, SecretError};
use erp_testkit::{Schema, TestDb};
use erp_types::{TenantId, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);
/// A tenant database that has never had a migration run against it.
static UNMIGRATED: Schema = Schema::sql("unmigrated", &[]);

struct Fixture {
    control: ControlPlane,
    db: TestDb,
    databases: Vec<String>,
}

impl Fixture {
    async fn new() -> Self {
        let db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("template builds")
            .fresh()
            .await
            .expect("clones");

        // Two clusters at the same server, so a walk that visits only one of
        // them is visible.
        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .and_then(|registry| registry.with_url("secondary", &erp_testkit::database_url()))
            .expect("parses");
        let control = ControlPlane::new(
            db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        );
        for cluster in ["primary", "secondary"] {
            control
                .register_cluster(
                    cluster,
                    "ERP_CLUSTER_PRIMARY_URL",
                    None,
                    10_000,
                    10_000,
                    Actor::system(),
                )
                .await
                .expect("cluster registers");
        }

        Self {
            control,
            db,
            databases: Vec::new(),
        }
    }

    /// A tenant whose database is built from `schema` — which is how a tenant
    /// three migrations behind is produced without faking `_sqlx_migrations`.
    async fn tenant_with(&mut self, slug: &str, schema: &Schema) -> erp_control::Tenant {
        self.tenant_on(slug, "primary", schema).await
    }

    async fn tenant_on(
        &mut self,
        slug: &str,
        cluster: &str,
        schema: &Schema,
    ) -> erp_control::Tenant {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, cluster, Actor::system())
            .await
            .expect("registers");
        erp_testkit::create_named_database(&tenant.database_name, schema)
            .await
            .expect("creates the database");
        self.databases.push(tenant.database_name.clone());
        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("activates");
        tenant
    }

    /// A tenant row with no database behind it, for the unreachable case.
    async fn tenant_without_a_database(&self, slug: &str) -> erp_control::Tenant {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("registers");
        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("activates");
        tenant
    }

    async fn cleanup(self) {
        for name in &self.databases {
            let _ = erp_testkit::drop_named_database(name).await;
        }
    }
}

/// **The requirement.** A tenant whose database predates a migration is found
/// by a survey and brought current by a run.
#[tokio::test]
async fn a_tenant_behind_the_fleet_is_found_and_brought_current() {
    let mut fixture = Fixture::new().await;

    fixture.tenant_with("uptodate", &TENANT).await;
    let behind = fixture.tenant_with("behind", &UNMIGRATED).await;

    let latest = ControlPlane::latest_tenant_migration();
    assert!(
        latest > 0,
        "this build expects a real migration version, or every tenant looks current"
    );

    let plan = fixture.control.survey_fleet().await.expect("surveys");
    assert!(!plan.is_uniform(), "one tenant is behind");
    assert_eq!(plan.current.len(), 1);
    assert_eq!(plan.behind.len(), 1);
    assert_eq!(plan.behind[0].tenant, behind.id);
    assert_eq!(
        plan.behind[0].version, None,
        "a database with no migrations table reports `None`, not version zero"
    );
    assert!(plan.failed.is_empty());

    // A survey changes nothing, which is what makes it safe to run before a
    // deploy rather than after.
    let again = fixture.control.survey_fleet().await.expect("surveys");
    assert_eq!(again.behind.len(), 1);

    let done = fixture.control.migrate_fleet().await.expect("migrates");
    assert_eq!(
        done.behind.len(),
        1,
        "reports what it had to do, not what it found afterwards"
    );
    assert!(done.failed.is_empty());

    // And now the fleet agrees with this build.
    let after = fixture.control.survey_fleet().await.expect("surveys");
    assert!(after.is_uniform(), "{after:?}");
    assert_eq!(after.current.len(), 2);
    assert!(after.current.iter().all(|t| t.version == Some(latest)));

    // Running it again is a no-op rather than an error — which is what makes a
    // failed run resumable by simply running it again.
    let repeat = fixture.control.migrate_fleet().await.expect("migrates");
    assert!(repeat.behind.is_empty());
    assert_eq!(repeat.current.len(), 2);

    fixture.cleanup().await;
}

/// **One unreachable tenant must not leave the rest of the fleet un-migrated.**
///
/// The property most likely to be got wrong, and the one that decides whether a
/// migration is a deploy step or an incident.
#[tokio::test]
async fn a_tenant_that_cannot_be_reached_does_not_stop_the_run() {
    let mut fixture = Fixture::new().await;

    // Ordered by `created_at`, so the broken one is visited first and the two
    // after it prove the walk carried on.
    let broken = fixture.tenant_without_a_database("ghost").await;
    fixture.tenant_with("behind", &UNMIGRATED).await;
    fixture.tenant_with("uptodate", &TENANT).await;

    let plan = fixture
        .control
        .migrate_fleet()
        .await
        .expect("does not fail");

    assert_eq!(plan.failed.len(), 1);
    assert_eq!(plan.failed[0].0, broken.id);
    assert_eq!(
        plan.behind.len(),
        1,
        "the tenant after the failure was migrated anyway"
    );
    assert_eq!(plan.current.len(), 1);
    assert_eq!(plan.total(), 3);
    assert!(
        !plan.is_uniform(),
        "a tenant nobody could reach is not a migrated tenant, so a deploy gate must say no"
    );

    // Not vacuous: with the broken tenant gone the same fleet is uniform.
    sqlx::query("DELETE FROM tenant WHERE id = $1")
        .bind(broken.id.as_uuid())
        .execute(fixture.db.pool())
        .await
        .expect("removes the ghost");

    let plan = fixture.control.migrate_fleet().await.expect("migrates");
    assert!(plan.is_uniform(), "{plan:?}");

    fixture.cleanup().await;
}

/// Suspended tenants are migrated; half-built ones are left alone.
///
/// A suspended tenant is one that may come back, and coming back to a schema
/// three versions behind is the failure this whole thing exists to prevent. A
/// `provisioning` one has no finished database and is somebody else's problem
/// right now.
#[tokio::test]
async fn suspended_tenants_are_migrated_and_half_built_ones_are_skipped() {
    let mut fixture = Fixture::new().await;

    let suspended = fixture.tenant_with("paused", &UNMIGRATED).await;
    fixture
        .control
        .suspend_tenant(suspended.id, "unpaid", Actor::system())
        .await
        .expect("suspends");

    // Registered and never activated: still `provisioning`.
    let half_built = fixture
        .control
        .register_tenant_on("halfbuilt", "Half Built", "primary", Actor::system())
        .await
        .expect("registers");

    let plan = fixture.control.migrate_fleet().await.expect("migrates");

    assert_eq!(plan.total(), 1, "only the suspended tenant was visited");
    assert_eq!(plan.behind.len(), 1);
    assert_eq!(plan.behind[0].tenant, suspended.id);
    assert!(
        !plan.failed.iter().any(|(id, _)| *id == half_built.id),
        "a half-built tenant is skipped, not reported as a failure"
    );

    let after = fixture.control.survey_fleet().await.expect("surveys");
    assert!(
        after.is_uniform(),
        "the suspended tenant was brought current"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Sealing-key rotation
// ---------------------------------------------------------------------------

fn ring(keys: &[(&str, u8)]) -> SealingKey {
    let list: Vec<String> = keys
        .iter()
        .map(|(id, byte)| format!("{id}:{}", hex::encode([*byte; 32])))
        .collect();
    SealingKey::parse(&list.join(",")).expect("a ring")
}

/// Writes a secret the way a module does, through maintenance entry.
async fn put(
    control: &ControlPlane,
    tenant: TenantId,
    sealing: &SealingKey,
    name: &str,
    value: &[u8],
) {
    let db = control.enter_for_maintenance(tenant).await.expect("enters");
    let mut conn = db.acquire().await.expect("a connection");
    erp_eventlog::secrets::put(&mut conn, sealing, name, value)
        .await
        .expect("stores");
}

async fn get(
    control: &ControlPlane,
    tenant: TenantId,
    sealing: &SealingKey,
    name: &str,
) -> Result<Option<Vec<u8>>, SecretError> {
    let db = control.enter_for_maintenance(tenant).await.expect("enters");
    let mut conn = db.acquire().await.expect("a connection");
    erp_eventlog::secrets::get(&mut conn, sealing, name).await
}

fn at(unix: i64) -> Timestamp {
    chrono::DateTime::from_timestamp(unix, 0).expect("a timestamp")
}

fn code(secret: &[u8], at: Timestamp) -> String {
    let seconds = u64::try_from(at.timestamp()).expect("after 1970");
    totp::code_at(secret, seconds, totp::DIGITS).expect("a code")
}

/// **A rotation reaches everything sealed**: the control plane's second
/// factors, and module secrets in tenants on both clusters, the suspended one
/// included. Looking moves nothing; applying moves all of it; after that the
/// old key can leave `SEALING_KEY` and nothing stops working.
#[tokio::test]
async fn a_rotation_reseals_every_tenant_on_every_cluster_and_the_control_plane() {
    let mut fixture = Fixture::new().await;
    let old = ring(&[("old", 1)]);
    let rotating = ring(&[("new", 2), ("old", 1)]);
    let new = ring(&[("new", 2)]);

    let here = fixture.tenant_with("here", &TENANT).await;
    let there = fixture.tenant_on("there", "secondary", &TENANT).await;
    let paused = fixture.tenant_with("paused", &TENANT).await;
    for tenant in [&here, &there, &paused] {
        put(
            &fixture.control,
            tenant.id,
            &old,
            "tax_sa.csid",
            tenant.slug.as_bytes(),
        )
        .await;
    }
    fixture
        .control
        .suspend_tenant(paused.id, "unpaid", Actor::system())
        .await
        .expect("suspends");

    // Somebody's authenticator app, enrolled under the old key and used once,
    // which leaves the spent-code marker behind the sealed secret.
    let now = at(1_700_000_000);
    let identity = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("an identity")
        .id;
    let enrolment = fixture
        .control
        .begin_second_factor(identity, "ERP", "sara@erp.test", &old, None)
        .await
        .expect("enrols");
    let secret = totp::unbase32(&enrolment.secret).expect("base32");
    fixture
        .control
        .confirm_second_factor(identity, &code(&secret, now), None, now, &old, None, None)
        .await
        .expect("confirms");
    let used = at(1_700_000_060);
    fixture
        .control
        .verify_second_factor(identity, &code(&secret, used), used, &old)
        .await
        .expect("verifies");

    let everything_under = |id: &str| BTreeMap::from([(id.to_owned(), 4)]);

    let look = fixture
        .control
        .reseal_fleet(&rotating, false)
        .await
        .expect("looks");
    assert_eq!(look.census.under, everything_under("old"), "{look:?}");
    assert_eq!(look.census.resealed, 0);
    assert!(!look.is_settled("new"));
    assert!(
        matches!(
            get(&fixture.control, here.id, &new, "tax_sa.csid").await,
            Err(SecretError::UnknownKey { .. })
        ),
        "looking moved a secret"
    );

    let done = fixture
        .control
        .reseal_fleet(&rotating, true)
        .await
        .expect("reseals");
    assert_eq!(done.census.under, everything_under("new"), "{done:?}");
    assert_eq!(done.census.resealed, 4);
    assert!(done.failed.is_empty(), "{:?}", done.failed);
    assert!(done.is_settled("new"));

    // `old` is retired, and nothing notices.
    for tenant in [&here, &there, &paused] {
        assert_eq!(
            get(&fixture.control, tenant.id, &new, "tax_sa.csid")
                .await
                .expect("opens under the new key alone"),
            Some(tenant.slug.as_bytes().to_vec()),
            "{} was not resealed",
            tenant.slug
        );
    }
    let later = at(1_700_000_180);
    fixture
        .control
        .verify_second_factor(identity, &code(&secret, later), later, &new)
        .await
        .expect("the authenticator app still works under the new key alone");

    let again = fixture
        .control
        .reseal_fleet(&new, true)
        .await
        .expect("reseals");
    assert_eq!(again.census.resealed, 0, "a second run found work");
    assert!(again.is_settled("new"));

    fixture.cleanup().await;
}

/// **What a rotation cannot finish keeps it unsettled, and is left alone**: a
/// secret under a key the deployment does not hold is not overwritten or
/// deleted — the key that sealed it still opens it — and the readable secret
/// beside it is moved anyway.
#[tokio::test]
async fn what_a_rotation_cannot_finish_is_reported_and_left_alone() {
    let mut fixture = Fixture::new().await;
    let stranger = ring(&[("stranger", 9)]);
    let rotating = ring(&[("new", 2), ("old", 1)]);

    let acme = fixture.tenant_with("acme", &TENANT).await;
    put(
        &fixture.control,
        acme.id,
        &stranger,
        "payments.card.A",
        b"tok_a",
    )
    .await;
    put(
        &fixture.control,
        acme.id,
        &ring(&[("old", 1)]),
        "tax_sa.csid",
        b"csid",
    )
    .await;

    let plan = fixture
        .control
        .reseal_fleet(&rotating, true)
        .await
        .expect("does not fail");

    assert_eq!(plan.census.unsealable, ["acme: payments.card.A"]);
    assert_eq!(
        plan.census.under,
        BTreeMap::from([("new".to_owned(), 1), ("stranger".to_owned(), 1)])
    );
    assert_eq!(
        plan.census.resealed, 1,
        "the readable secret was moved anyway"
    );
    assert!(plan.failed.is_empty(), "{:?}", plan.failed);
    assert!(!plan.is_settled("new"));

    assert_eq!(
        get(&fixture.control, acme.id, &stranger, "payments.card.A")
            .await
            .expect("still opens"),
        Some(b"tok_a".to_vec()),
        "a secret the sweep could not read was changed"
    );

    fixture.cleanup().await;
}

/// **A tenant nobody reached keeps the rotation unsettled**, even when every
/// secret the sweep did reach is under the new key: nobody knows what the
/// unreached one still holds, and retiring the old key would lose it.
#[tokio::test]
async fn an_unreachable_tenant_keeps_the_rotation_unsettled() {
    let mut fixture = Fixture::new().await;
    let rotating = ring(&[("new", 2), ("old", 1)]);

    let acme = fixture.tenant_with("acme", &TENANT).await;
    put(
        &fixture.control,
        acme.id,
        &ring(&[("old", 1)]),
        "tax_sa.csid",
        b"csid",
    )
    .await;
    let ghost = fixture.tenant_without_a_database("ghost").await;

    let plan = fixture
        .control
        .reseal_fleet(&rotating, true)
        .await
        .expect("does not fail");

    assert!(
        plan.census.is_settled("new"),
        "everything reached was moved: {plan:?}"
    );
    assert_eq!(plan.failed.len(), 1);
    assert_eq!(plan.failed[0].0, ghost.id);
    assert!(
        !plan.is_settled("new"),
        "a tenant nobody reached let the old key go"
    );

    fixture.cleanup().await;
}

/// **Every row is opened, those already under the current id included.** The
/// current id reused for new bytes — `2026-09` rotated inside September, the
/// old entry renamed `2026-09-leaked` — leaves every row naming `2026-09` and
/// sealed by bytes that id no longer means. Nothing is stale by id, so a sweep
/// that opened only stale rows reported the rotation settled, and the gate
/// waved out the one key that opens everything.
#[tokio::test]
async fn a_current_id_over_the_wrong_bytes_keeps_the_rotation_unsettled() {
    let mut fixture = Fixture::new().await;
    let before = ring(&[("2026-09", 1)]);
    let reused = ring(&[("2026-09", 2), ("2026-09-leaked", 1)]);

    let acme = fixture.tenant_with("acme", &TENANT).await;
    put(&fixture.control, acme.id, &before, "tax_sa.csid", b"csid").await;
    let identity = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("an identity")
        .id;
    fixture
        .control
        .begin_second_factor(identity, "ERP", "sara@erp.test", &before, None)
        .await
        .expect("enrols");

    let check = fixture
        .control
        .reseal_fleet(&reused, false)
        .await
        .expect("looks");

    assert_eq!(
        check.census.unsealable,
        [
            "acme: tax_sa.csid".to_owned(),
            format!("control: second_factor:{identity}"),
        ],
        "{check:?}"
    );
    assert!(
        !check.is_settled("2026-09"),
        "a check that opened nothing under the current id let the leaked key go"
    );

    fixture.cleanup().await;
}
