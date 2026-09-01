//! Customers, against a real tenant.
//!
//! The test that carries this module is [`a_rebuild_reproduces_the_list`]: the
//! customer record is what `sales`, `booking` and `prepaid` will all point at,
//! so a projection that drifts on replay would take every one of them with it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use crm::{
    Address, Contact, Crm, CrmError, CustomerKind, Details, TaxRegistration, amend_customer,
    archive_customer, register_customer, restore_customer,
};
use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}
fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

/// A company with everything filled in.
fn company() -> Details {
    Details {
        name: "نجد للاستشارات".to_owned(),
        name_latin: Some("Najd Consulting".to_owned()),
        kind: CustomerKind::Company,
        contact: Contact {
            phone: Some("+966500000000".to_owned()),
            email: Some("hello@najd.example".to_owned()),
        },
        address: Some(Address {
            street: "طريق الملك فهد".to_owned(),
            building: Some("2322".to_owned()),
            district: Some("العليا".to_owned()),
            city: "الرياض".to_owned(),
            postal_code: Some("12211".to_owned()),
            country: "SA".to_owned(),
        }),
        tax: Some(TaxRegistration {
            vat_number: "399999999900003".to_owned(),
            scheme: Some("CRN".to_owned()),
            identifier: Some("1010101010".to_owned()),
        }),
    }
}

/// A walk-in with only a phone number, which is the common case at a till.
fn person() -> Details {
    Details {
        name: "سارة".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact {
            phone: Some("+966511111111".to_owned()),
            email: None,
        },
        address: None,
        tax: None,
    }
}

struct Fixture {
    db: TenantDb,
    pool: sqlx::PgPool,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    database: String,
}

impl Fixture {
    async fn new() -> Self {
        let control_db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("the test database URL parses");
        let control = Arc::new(ControlPlane::new(
            control_db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ));
        control
            .register_cluster(
                "primary",
                "ERP_CLUSTER_PRIMARY_URL",
                None,
                10_000,
                10_000,
                Actor::system(),
            )
            .await
            .expect("cluster registers");

        let tenant = control
            .register_tenant_on("acme", "Acme", "primary", Actor::system())
            .await
            .expect("tenant registers");
        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("tenant database is created");
        control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("tenant activates");

        let db = control
            .enter_for_maintenance(tenant.id)
            .await
            .expect("maintenance entry");

        let mut conn = db.acquire().await.expect("connection");
        crm::install(&mut conn).await.expect("crm installs");
        ensure_group_schema::<Crm>(&mut conn)
            .await
            .expect("crm checkpoint");
        drop(conn);

        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
        let pool = sqlx::PgPool::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects");

        Self {
            db,
            pool,
            _control: control,
            _control_db: control_db,
            database: tenant.database_name,
        }
    }

    async fn project(&self) {
        let owned = crm::projections();
        let refs: Vec<&dyn Projection<Group = Crm>> = owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Crm>(&self.pool, &refs, crm::upcasters(), 200)
            .await
            .expect("crm projects");
    }

    async fn list(&self, archived: bool) -> Vec<crm::CustomerSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        crm::customers(&mut conn, archived, 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn get(&self, id: &str) -> Option<crm::CustomerDetail> {
        let mut conn = self.pool.acquire().await.expect("connection");
        crm::customer(&mut conn, id).await.expect("reads")
    }

    async fn cleanup(self) {
        drop(self.db);
        self.pool.close().await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// One row of the refusal table: what it is, the record, and what it must be
/// refused with.
type Refusal = (&'static str, Details, fn(&CrmError) -> bool);

/// What a rejection was, when there was one.
fn rejection(error: &CommandError<CrmError>) -> Option<&CrmError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

/// **The whole module in one pass**: register, read back, amend, archive.
#[tokio::test]
async fn a_customer_is_recorded_and_read_back() {
    let fixture = Fixture::new().await;

    register_customer(
        &fixture.db,
        &code("CUST-1"),
        &company(),
        on("2026-01-15"),
        &Metadata::default(),
    )
    .await
    .expect("registers");
    fixture.project().await;

    let detail = fixture.get("CUST-1").await.expect("it is there");
    assert_eq!(detail.summary.name, "نجد للاستشارات");
    assert_eq!(
        detail.summary.name_latin.as_deref(),
        Some("Najd Consulting")
    );
    assert_eq!(detail.summary.kind, "company");
    assert_eq!(
        detail.summary.vat_number.as_deref(),
        Some("399999999900003")
    );
    assert_eq!(detail.city.as_deref(), Some("الرياض"));
    assert!(!detail.summary.archived);

    // Amending replaces the record and leaves the id alone.
    let mut moved = company();
    moved.name = "نجد للاستشارات المحدودة".to_owned();
    amend_customer(&fixture.db, &code("CUST-1"), &moved, &Metadata::default())
        .await
        .expect("amends");
    fixture.project().await;
    assert_eq!(
        fixture
            .get("CUST-1")
            .await
            .expect("still there")
            .summary
            .name,
        "نجد للاستشارات المحدودة"
    );

    fixture.cleanup().await;
}

/// **Archiving is not deleting.**
///
/// A customer named on a cleared tax invoice is part of a record the authority
/// requires kept. Out of the lists, still on the document, and restorable.
#[tokio::test]
async fn archiving_hides_without_losing() {
    let fixture = Fixture::new().await;
    register_customer(
        &fixture.db,
        &code("CUST-1"),
        &person(),
        on("2026-01-15"),
        &Metadata::default(),
    )
    .await
    .expect("registers");

    archive_customer(
        &fixture.db,
        &code("CUST-1"),
        Some("moved away".to_owned()),
        &Metadata::default(),
    )
    .await
    .expect("archives");
    fixture.project().await;

    assert!(
        fixture.list(false).await.is_empty(),
        "an archived customer is out of the list"
    );
    let both = fixture.list(true).await;
    assert_eq!(both.len(), 1, "and still there when asked for");
    assert!(both[0].archived);
    assert_eq!(
        fixture
            .get("CUST-1")
            .await
            .expect("still a record")
            .archived_why
            .as_deref(),
        Some("moved away")
    );

    // Archiving twice is a no-op, because they wanted them archived and they are.
    let again = archive_customer(&fixture.db, &code("CUST-1"), None, &Metadata::default())
        .await
        .expect("is not an error");
    assert!(again.did_nothing());

    restore_customer(&fixture.db, &code("CUST-1"), &Metadata::default())
        .await
        .expect("restores");
    fixture.project().await;
    assert_eq!(fixture.list(false).await.len(), 1, "back in the list");

    fixture.cleanup().await;
}

/// **Registering an id twice is refused, never ignored.**
///
/// The same call `ledger::open_account` makes: the second caller meant a
/// different customer, and quietly handing back the first would attach their
/// next invoice to somebody else.
#[tokio::test]
async fn the_same_id_twice_is_a_conflict() {
    let fixture = Fixture::new().await;
    register_customer(
        &fixture.db,
        &code("CUST-1"),
        &company(),
        on("2026-01-15"),
        &Metadata::default(),
    )
    .await
    .expect("registers");

    let refused = register_customer(
        &fixture.db,
        &code("CUST-1"),
        &person(),
        on("2026-02-01"),
        &Metadata::default(),
    )
    .await
    .expect_err("the second is refused");
    assert!(matches!(
        rejection(&refused),
        Some(CrmError::AlreadyExists(_))
    ));

    fixture.project().await;
    assert_eq!(
        fixture.get("CUST-1").await.expect("there").summary.name,
        "نجد للاستشارات",
        "the first record is untouched"
    );

    fixture.cleanup().await;
}

/// Everything the record refuses, and why each one matters.
#[tokio::test]
async fn a_record_nobody_could_use_is_refused() {
    let fixture = Fixture::new().await;

    let cases: Vec<Refusal> = vec![
        (
            "a customer with no name",
            {
                let mut d = person();
                d.name = "   ".to_owned();
                d
            },
            |e| matches!(e, CrmError::NoName),
        ),
        (
            "a customer nobody can reach",
            {
                let mut d = person();
                d.contact = Contact::default();
                d
            },
            |e| matches!(e, CrmError::NoContact),
        ),
        (
            "a VAT number that is not one",
            {
                let mut d = company();
                d.tax = Some(TaxRegistration {
                    vat_number: "12345".to_owned(),
                    scheme: None,
                    identifier: None,
                });
                d
            },
            |e| matches!(e, CrmError::NotAVatNumber(_)),
        ),
        (
            "a person holding a VAT registration",
            {
                let mut d = person();
                d.tax = Some(TaxRegistration {
                    vat_number: "399999999900003".to_owned(),
                    scheme: None,
                    identifier: None,
                });
                d
            },
            |e| matches!(e, CrmError::PersonWithVatNumber),
        ),
    ];

    for (what, details, expected) in cases {
        let refused = register_customer(
            &fixture.db,
            &code("CUST-X"),
            &details,
            on("2026-01-15"),
            &Metadata::default(),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("{what} must be refused"));

        let rejected = rejection(&refused)
            .unwrap_or_else(|| panic!("{what} must be a rejection, not {refused}"));
        assert!(expected(rejected), "{what}: got {rejected}");
    }

    fixture.cleanup().await;
}

/// Saving a form twice writes one event.
///
/// What keeps a customer's history readable: every event in it is a change
/// somebody actually made.
#[tokio::test]
async fn an_unchanged_amendment_writes_nothing() {
    let fixture = Fixture::new().await;
    register_customer(
        &fixture.db,
        &code("CUST-1"),
        &company(),
        on("2026-01-15"),
        &Metadata::default(),
    )
    .await
    .expect("registers");

    let again = amend_customer(
        &fixture.db,
        &code("CUST-1"),
        &company(),
        &Metadata::default(),
    )
    .await
    .expect("is not an error");
    assert!(again.did_nothing(), "nothing moved, so nothing was written");

    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM event")
        .fetch_one(&fixture.pool)
        .await
        .expect("counts");
    assert_eq!(events, 1, "one registration and no second event");

    fixture.cleanup().await;
}

/// **The one that carries the module.**
///
/// `sales`, `booking` and `prepaid` will all reference a customer, so a
/// projection that drifts on replay takes every one of them with it. Replay the
/// whole log into an empty copy and diff it against live.
#[tokio::test]
async fn a_rebuild_reproduces_the_list() {
    let fixture = Fixture::new().await;

    register_customer(
        &fixture.db,
        &code("CUST-1"),
        &company(),
        on("2026-01-15"),
        &Metadata::default(),
    )
    .await
    .expect("registers");
    register_customer(
        &fixture.db,
        &code("CUST-2"),
        &person(),
        on("2026-02-20"),
        &Metadata::default(),
    )
    .await
    .expect("registers");

    let mut moved = company();
    moved.name_latin = Some("Najd Consulting Ltd".to_owned());
    amend_customer(&fixture.db, &code("CUST-1"), &moved, &Metadata::default())
        .await
        .expect("amends");
    archive_customer(&fixture.db, &code("CUST-2"), None, &Metadata::default())
        .await
        .expect("archives");
    restore_customer(&fixture.db, &code("CUST-2"), &Metadata::default())
        .await
        .expect("restores");

    fixture.project().await;

    // The witness: the group must have projected something, because `EXCEPT ALL`
    // between two empty tables is clean the way a blank page is correct.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_crm.customer")
        .fetch_one(&fixture.pool)
        .await
        .expect("counts");
    assert_eq!(rows, 2, "both customers projected");

    let owned = crm::projections();
    let refs: Vec<&dyn Projection<Group = Crm>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<Crm>(&fixture.pool, &refs, crm::upcasters(), 200)
        .await
        .expect("replays");
    assert!(
        report.is_reproducible(),
        "a rebuild must reproduce the live tables exactly: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Every message this module can produce has a translation in every locale.
#[test]
fn the_catalog_is_complete() {
    erp_i18n::testing::assert_complete(&crm::CATALOG);
}
