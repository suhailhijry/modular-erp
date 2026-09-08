//! The bell, against a real tenant.
//!
//! The test that carries this file is
//! [`a_booking_reaches_the_stylist_and_otherwise_the_manager`] — the audience
//! model deciding an inbox rather than an address, which is the whole reason
//! this module sits above `messaging` instead of beside it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};
use messaging::template::{Body, Template, Templates};
use messaging::{Channel, Subject, Topic};
use notifications::{AnnounceError, Announcing, Kind};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

/// The stylist's login, and the manager's. Fixed rather than generated, so a
/// failing assertion says which person it was about.
const STYLIST: &str = "11111111-1111-1111-1111-111111111111";
const MANAGER: &str = "22222222-2222-2222-2222-222222222222";

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn at(day: &str, hour: &str) -> Timestamp {
    format!("{day}T{hour}:00:00Z")
        .parse()
        .expect("a valid instant")
}

fn booking_of(id: &str) -> Announcing {
    Announcing {
        kind: Kind::BookingReserved,
        subject: Subject::new(Topic::Reservation, code(id)),
        at: at("2026-05-01", "09"),
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
    async fn new(slug: &str) -> Self {
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
            .register_tenant_on(slug, "Salon", "primary", Actor::system())
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

        {
            let mut conn = db.acquire().await.expect("connection");
            crm::install(&mut conn).await.expect("crm");
            ensure_group_schema::<crm::Crm>(&mut conn).await.expect("c");
            branches::install(&mut conn).await.expect("branches");
            ensure_group_schema::<branches::Branches>(&mut conn)
                .await
                .expect("b");
            hr::install(&mut conn).await.expect("hr");
            ensure_group_schema::<hr::Hr>(&mut conn).await.expect("h");
            booking::install(&mut conn).await.expect("booking");
            ensure_group_schema::<booking::Booking>(&mut conn)
                .await
                .expect("k");
            // **Invoices, because three of the five kinds are about one.** Not
            // for anything this tenant issues — for the bindings a notification
            // about a document renders against.
            sales::install(&mut conn).await.expect("sales");
            ensure_group_schema::<sales::Sales>(&mut conn)
                .await
                .expect("s");
            notifications::install(&mut conn).await.expect("bell");
            ensure_group_schema::<notifications::Notifications>(&mut conn)
                .await
                .expect("n");
        }

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
        macro_rules! run {
            ($module:ident, $group:ty) => {{
                let owned = $module::projections();
                let refs: Vec<&dyn Projection<Group = $group>> =
                    owned.iter().map(AsRef::as_ref).collect();
                run_to_head::<$group>(&self.pool, &refs, $module::upcasters(), 200)
                    .await
                    .expect("projects");
            }};
        }
        run!(crm, crm::Crm);
        run!(branches, branches::Branches);
        run!(hr, hr::Hr);
        run!(booking, booking::Booking);
        run!(sales, sales::Sales);
        run!(notifications, notifications::Notifications);
    }

    /// Throws away the bell's read models and builds them again from the log.
    ///
    /// **What proves read state is derived.** Not a rebuild through the
    /// migrator — the same effect, in one test: drop the rows, replay, and see
    /// what comes back.
    async fn rebuild(&self) {
        sqlx::query("TRUNCATE proj_notifications.inbox, proj_notifications.preference")
            .execute(&self.pool)
            .await
            .expect("empties the read models");
        sqlx::query("UPDATE projection_checkpoint SET position = 0 WHERE group_name = $1")
            .bind(notifications::GROUP_NAME)
            .execute(&self.pool)
            .await
            .expect("rewinds the checkpoint");
        self.project().await;
    }

    async fn announce(
        &self,
        announcing: &Announcing,
    ) -> Result<notifications::Announced, AnnounceError> {
        let mut tx = self.db.begin().await.expect("transaction");
        match notifications::announce(&mut tx, announcing, &Metadata::default()).await {
            Ok(announced) => {
                tx.commit().await.expect("commits");
                self.project().await;
                Ok(announced)
            }
            Err(e) => {
                tx.rollback().await.expect("rolls back");
                Err(e)
            }
        }
    }

    async fn inbox_of(&self, identity: &str) -> Vec<notifications::InboxRow> {
        let mut conn = self.db.acquire().await.expect("connection");
        notifications::inbox(&mut conn, identity, false, 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn unread_of(&self, identity: &str) -> i64 {
        let mut conn = self.db.acquire().await.expect("connection");
        notifications::unread(&mut conn, identity)
            .await
            .expect("counts")
    }

    /// Every message actually promised, oldest first.
    async fn outbox(&self) -> Vec<messaging::Outbound> {
        let rows: Vec<(serde_json::Value,)> =
            sqlx::query_as("SELECT payload FROM outbox ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .expect("reads the outbox");
        rows.into_iter()
            .map(|(payload,)| messaging::Outbound::from_payload(&payload).expect("a message"))
            .collect()
    }

    async fn configure<T: serde::Serialize>(&self, key: &str, value: &T) {
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(&mut conn, key, value, None, None)
            .await
            .expect("configures");
    }

    async fn template(&self, name: &str, template: Template) {
        template.check(name).expect("a valid template");
        let mut templates = Templates::default();
        templates.entries.insert(name.to_owned(), template);
        self.configure(messaging::template::KEY, &templates).await;
    }

    async fn prefers(&self, identity: &str, kind: Kind, channels: &[Channel]) {
        let mut entries = BTreeMap::new();
        entries.insert(kind.as_str().to_owned(), channels.to_vec());
        notifications::set_preferences(
            &self.db,
            identity,
            entries,
            at("2026-05-01", "09"),
            &Metadata::default(),
        )
        .await
        .expect("says what they want");
        self.project().await;
    }

    /// A branch, a manager who logs in, a stylist who logs in, a chair that is
    /// the stylist's, a customer, and a booking on it.
    #[expect(
        clippy::too_many_lines,
        reason = "a whole salon, built once for every test in this file"
    )]
    async fn a_salon(&self) {
        branches::open_branch(
            &self.db,
            &code("BR-1"),
            &branches::Details {
                name: "العليا".to_owned(),
                name_latin: None,
                address: branches::Address {
                    street: "طريق الملك فهد".to_owned(),
                    building: None,
                    district: None,
                    city: "الرياض".to_owned(),
                    postal_code: None,
                    country: "SA".to_owned(),
                },
            },
            at("2026-01-01", "09"),
            &Metadata::default(),
        )
        .await
        .expect("the branch opens");

        for (id, name, reports_to, login) in [
            ("EMP-MANAGER", "المديرة", None, MANAGER),
            ("EMP-STYLIST", "سارة", Some("EMP-MANAGER"), STYLIST),
        ] {
            hr::hire(
                &self.db,
                &code(id),
                &hr::Hire {
                    details: hr::Details {
                        name: name.to_owned(),
                        name_latin: None,
                        national_id: None,
                        email: Some(format!("{}@salon.test", id.to_lowercase())),
                        phone: Some("+966500000009".to_owned()),
                    },
                    reports_to: reports_to.map(code),
                    branch: Some(code("BR-1")),
                    at: at("2026-01-01", "09"),
                },
                &Metadata::default(),
            )
            .await
            .expect("is hired");

            hr::link_login(
                &self.db,
                &code(id),
                login,
                at("2026-01-01", "09"),
                &Metadata::default(),
            )
            .await
            .expect("logs in as themselves");
        }

        crm::register_customer(
            &self.db,
            &code("CUST-1"),
            &crm::Details {
                name: "نورة".to_owned(),
                name_latin: None,
                kind: crm::CustomerKind::Person,
                contact: crm::Contact {
                    phone: Some("+966500000001".to_owned()),
                    email: None,
                },
                address: None,
                tax: None,
            },
            at("2026-01-01", "09"),
            &Metadata::default(),
        )
        .await
        .expect("registers");

        // **The chair is the stylist's**, which is how a booking on it resolves
        // to a worker at all. The second one is nobody's, for the fallback.
        for (chair, employee) in [("CHAIR-1", Some(code("EMP-STYLIST"))), ("CHAIR-2", None)] {
            booking::declare_resource(
                &self.db,
                &code(chair),
                &booking::Details {
                    name: chair.to_owned(),
                    name_latin: None,
                    kind: booking::Kind::Place,
                    capacity: 1,
                    rate: None,
                    branch: Some(code("BR-1")),
                    employee,
                },
                at("2026-01-01", "09"),
                &Metadata::default(),
            )
            .await
            .expect("declares");
        }

        for (id, chair, hour) in [("BK-1", "CHAIR-1", "10"), ("BK-2", "CHAIR-2", "12")] {
            let span = erp_occupancy::Span::new(
                at("2026-05-04", hour),
                at(
                    "2026-05-04",
                    &format!("{:02}", hour.parse::<u8>().expect("an hour") + 1),
                ),
            )
            .expect("a valid span");
            booking::reserve(
                &self.db,
                &code(id),
                &booking::Draft {
                    customer: booking::Customer {
                        id: Some(code("CUST-1")),
                        name: "نورة".to_owned(),
                        phone: Some("+966500000001".to_owned()),
                    },
                    lines: vec![booking::DraftLine {
                        what: "قص".to_owned(),
                        span,
                        takes: vec![booking::Held::one(code(chair))],
                        charge: None,
                    }],
                    note: String::new(),
                    at: at("2026-05-01", "09"),
                },
                &Metadata::default(),
            )
            .await
            .expect("reserves");
        }

        self.project().await;
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// **The audience decides the inbox.**
///
/// A booking on the stylist's chair reaches the stylist. The same kind on a
/// chair that is nobody's falls through to whoever runs the branch — one
/// ordered list on the kind, not a fallback written out at every producer.
#[tokio::test]
async fn a_booking_reaches_the_stylist_and_otherwise_the_manager() {
    let fixture = Fixture::new("bell-audience").await;
    fixture.a_salon().await;

    let announced = fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect("announces");
    assert!(announced.announced);
    assert_eq!(announced.recipients, 1);

    let stylist = fixture.inbox_of(STYLIST).await;
    assert_eq!(
        stylist.len(),
        1,
        "the stylist was not told about their own booking"
    );
    assert_eq!(stylist[0].kind, "booking_reserved");
    assert_eq!(stylist[0].subject_id, "BK-1");
    assert!(
        fixture.inbox_of(MANAGER).await.is_empty(),
        "the manager was told about a booking somebody was assigned to"
    );

    // The chair nobody owns: the branch manager hears about it instead.
    fixture
        .announce(&booking_of("BK-2"))
        .await
        .expect("announces");
    let manager = fixture.inbox_of(MANAGER).await;
    assert_eq!(
        manager.len(),
        1,
        "a booking assigned to nobody reached nobody"
    );
    assert_eq!(manager[0].subject_id, "BK-2");
    assert_eq!(
        fixture.inbox_of(STYLIST).await.len(),
        1,
        "the stylist was told about somebody else's booking"
    );

    // What it says was rendered from the read model, in both languages.
    let english = stylist[0].wording.get("en").expect("English");
    assert!(
        english.body.contains("نورة"),
        "the customer's name is not in it: {}",
        english.body
    );
    assert!(
        !english.body.contains("{{"),
        "a placeholder was left unrendered: {}",
        english.body
    );
    assert!(stylist[0].wording.contains_key("ar"), "no Arabic");

    fixture.cleanup().await;
}

/// **A record before it is a signal**, and one person's.
///
/// The row is in the inbox of whoever it was addressed to, and in nobody
/// else's. Read state comes back from the log after the read models are thrown
/// away, which is the 13c requirement that rules out a flag on a row.
#[tokio::test]
async fn read_state_is_per_person_and_survives_a_rebuild() {
    let fixture = Fixture::new("bell-rebuild").await;
    fixture.a_salon().await;
    fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect("announces");
    fixture
        .announce(&booking_of("BK-2"))
        .await
        .expect("announces");

    assert_eq!(fixture.unread_of(STYLIST).await, 1);
    assert_eq!(fixture.unread_of(MANAGER).await, 1);

    let mine = fixture.inbox_of(STYLIST).await;
    notifications::read(
        &fixture.db,
        &code(&mine[0].id),
        STYLIST,
        at("2026-05-02", "09"),
        &Metadata::default(),
    )
    .await
    .expect("marks it read");
    fixture.project().await;

    assert_eq!(fixture.unread_of(STYLIST).await, 0);
    assert_eq!(
        fixture.unread_of(MANAGER).await,
        1,
        "reading mine cleared somebody else's"
    );

    fixture.rebuild().await;
    assert_eq!(
        fixture.unread_of(STYLIST).await,
        0,
        "a rebuild forgot what had been read"
    );
    assert_eq!(fixture.unread_of(MANAGER).await, 1);

    fixture.cleanup().await;
}

/// **A stranger cannot mark it read** — and gets the answer that says nothing
/// about whether it exists.
#[tokio::test]
async fn a_notification_can_only_be_read_by_whoever_it_was_addressed_to() {
    let fixture = Fixture::new("bell-stranger").await;
    fixture.a_salon().await;
    fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect("announces");

    let theirs = fixture.inbox_of(STYLIST).await;
    let refused = notifications::read(
        &fixture.db,
        &code(&theirs[0].id),
        MANAGER,
        at("2026-05-02", "09"),
        &Metadata::default(),
    )
    .await
    .expect_err("somebody else read it");
    assert!(format!("{refused:?}").contains("NotYours"), "{refused:?}");

    // And an id that names nothing gets the same answer, so the refusal tells
    // nobody which ids exist.
    let missing = notifications::read(
        &fixture.db,
        &code("11111111-0000-0000-0000-000000000000"),
        MANAGER,
        at("2026-05-02", "09"),
        &Metadata::default(),
    )
    .await
    .expect_err("a notification that does not exist was read");
    assert!(format!("{missing:?}").contains("NotYours"), "{missing:?}");

    fixture.cleanup().await;
}

/// **A default does not spend money.**
///
/// With nothing said, announcing writes the bell and promises nothing. Asking
/// for SMS on that kind promises one and charges the meter.
#[tokio::test]
async fn nothing_is_sent_until_somebody_asks_for_it() {
    let fixture = Fixture::new("bell-money").await;
    fixture.a_salon().await;

    let announced = fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect("announces");
    assert_eq!(
        announced.promised, 0,
        "a notification spent money nobody asked to spend"
    );
    assert!(fixture.outbox().await.is_empty());
    assert_eq!(
        fixture.inbox_of(STYLIST).await.len(),
        1,
        "the bell is on by default"
    );

    fixture
        .prefers(
            STYLIST,
            Kind::BookingReserved,
            &[Channel::InSystem, Channel::Sms],
        )
        .await;
    let announced = fixture.announce(&booking_of("BK-2")).await;
    // BK-2 is the manager's; the stylist's preference must not affect it.
    assert_eq!(announced.expect("announces").promised, 0);

    fixture
        .prefers(
            MANAGER,
            Kind::BookingReserved,
            &[Channel::InSystem, Channel::Sms],
        )
        .await;
    // A third booking, so there is something new to announce to the manager.
    let announced = fixture
        .announce(&Announcing {
            kind: Kind::DocumentExpiring,
            subject: Subject::new(Topic::Employee, code("EMP-STYLIST")),
            at: at("2026-05-01", "09"),
        })
        .await
        .expect("announces");
    assert_eq!(announced.promised, 0, "a kind nobody asked for was texted");

    fixture
        .prefers(
            STYLIST,
            Kind::DocumentExpiring,
            &[Channel::InSystem, Channel::Sms],
        )
        .await;
    let announced = fixture
        .announce(&Announcing {
            kind: Kind::DocumentExpiring,
            subject: Subject::new(Topic::Employee, code("EMP-MANAGER")),
            at: at("2026-05-01", "09"),
        })
        .await
        .expect("announces");
    assert_eq!(announced.recipients, 1);

    fixture.cleanup().await;
}

/// **Announcing twice writes one notification**, which is what lets every
/// producer be a scan that runs every tick.
#[tokio::test]
async fn announcing_the_same_thing_twice_writes_one() {
    let fixture = Fixture::new("bell-twice").await;
    fixture.a_salon().await;

    let first = fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect("announces");
    assert!(first.announced);
    let second = fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect("announces again");
    assert!(
        !second.announced,
        "the second call wrote a second notification"
    );

    assert_eq!(fixture.inbox_of(STYLIST).await.len(), 1);
    assert_eq!(fixture.unread_of(STYLIST).await, 1);

    fixture.cleanup().await;
}

/// **A tenant's own words win**, and an inactive template does not.
#[tokio::test]
async fn a_tenants_template_beats_the_compiled_copy() {
    let fixture = Fixture::new("bell-template").await;
    fixture.a_salon().await;

    let mut theirs = Template {
        channel: Channel::InSystem,
        topic: Topic::Reservation,
        audience: messaging::Audience::Worker,
        bodies: BTreeMap::from([
            (
                "en".to_owned(),
                Body {
                    subject: "Chair booked".to_owned(),
                    text: "{{ customer.name }} is in.".to_owned(),
                },
            ),
            (
                "ar".to_owned(),
                Body {
                    subject: "حجز كرسي".to_owned(),
                    text: "{{ customer.name }} قادمة.".to_owned(),
                },
            ),
        ]),
        active: false,
    };
    fixture
        .template(Kind::BookingReserved.as_str(), theirs.clone())
        .await;

    // Off is not deleted, and it is not used either.
    fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect("announces");
    let english = fixture.inbox_of(STYLIST).await[0]
        .wording
        .get("en")
        .expect("English")
        .clone();
    assert_eq!(
        english.title, "New booking",
        "an inactive template was used"
    );

    theirs.active = true;
    fixture
        .template(Kind::BookingReserved.as_str(), theirs)
        .await;
    fixture
        .announce(&booking_of("BK-2"))
        .await
        .expect("announces");
    let english = fixture.inbox_of(MANAGER).await[0]
        .wording
        .get("en")
        .expect("English")
        .clone();
    assert_eq!(
        english.title, "Chair booked",
        "the tenant's own words were ignored"
    );
    assert!(
        english.body.contains("نورة"),
        "the template's bindings were not resolved: {}",
        english.body
    );

    fixture.cleanup().await;
}

/// **Nobody with a login is a refusal, not a quiet nothing.**
///
/// The commonest cause is an org chart where nobody has been linked to a login,
/// and the only thing that can fix it is whoever is watching the worker's logs.
#[tokio::test]
async fn an_audience_with_no_login_is_refused() {
    let fixture = Fixture::new("bell-unlinked").await;
    fixture.a_salon().await;

    for employee in ["EMP-STYLIST", "EMP-MANAGER"] {
        hr::unlink_login(
            &fixture.db,
            &code(employee),
            at("2026-05-01", "09"),
            &Metadata::default(),
        )
        .await
        .expect("unlinks");
    }
    fixture.project().await;

    let refused = fixture
        .announce(&booking_of("BK-1"))
        .await
        .expect_err("announced into a void");
    assert!(
        matches!(refused, AnnounceError::Unreachable { .. }),
        "{refused:?}"
    );
    assert!(fixture.inbox_of(STYLIST).await.is_empty());

    fixture.cleanup().await;
}

/// **A sweep announces what is new and skips what it has already said.**
///
/// This is what every producer does — the loop lives here rather than in four
/// worker jobs — and what makes a cursor unnecessary: the window overlaps the
/// last one on purpose, and saying the same thing twice costs one refused
/// aggregate load.
#[tokio::test]
async fn a_sweep_announces_once_however_often_it_runs() {
    let fixture = Fixture::new("bell-sweep").await;
    fixture.a_salon().await;

    let both = [code("BK-1"), code("BK-2")];
    let first = notifications::announce_all(
        &fixture.db,
        Kind::BookingReserved,
        &both,
        at("2026-05-01", "09"),
    )
    .await
    .expect("sweeps");
    assert_eq!(first.announced, 2);
    assert_eq!(first.skipped, 0);
    assert_eq!(first.unreachable, 0);
    fixture.project().await;

    let again = notifications::announce_all(
        &fixture.db,
        Kind::BookingReserved,
        &both,
        at("2026-05-01", "10"),
    )
    .await
    .expect("sweeps again");
    assert_eq!(again.announced, 0, "the same window announced itself twice");
    assert_eq!(again.skipped, 2);
    fixture.project().await;

    assert_eq!(fixture.inbox_of(STYLIST).await.len(), 1);
    assert_eq!(fixture.inbox_of(MANAGER).await.len(), 1);

    // A subject nobody is listed for is counted, not raised: one booking that
    // reaches nobody must not stop the rest of the window.
    let mixed = [code("BK-1"), code("BK-NOBODY")];
    let swept = notifications::announce_all(
        &fixture.db,
        Kind::BookingReserved,
        &mixed,
        at("2026-05-01", "11"),
    )
    .await
    .expect("carries on past a refusal");
    assert_eq!(swept.skipped, 1);
    assert_eq!(
        swept.unreachable, 1,
        "a booking that reaches nobody stopped the sweep"
    );

    fixture.cleanup().await;
}

/// **A document is the business's, not a place's.**
///
/// An invoice has no branch — where its postings landed is `ledger`'s and a
/// different projection group — so "whoever runs the branch" has to mean
/// whoever runs the business. Without this, three of the five kinds would
/// resolve to nobody for ever: a refused ZATCA document, a settled payment and
/// a failed one are all about an invoice.
#[tokio::test]
async fn something_that_happened_to_a_document_reaches_whoever_runs_the_business() {
    let fixture = Fixture::new("bell-document").await;
    fixture.a_salon().await;

    let announced = fixture
        .announce(&Announcing {
            kind: Kind::TaxRefused,
            subject: Subject::new(Topic::Invoice, code("INV-1")),
            at: at("2026-05-01", "09"),
        })
        .await
        .expect("a refused document reaches nobody");
    assert_eq!(announced.recipients, 1);

    let manager = fixture.inbox_of(MANAGER).await;
    assert_eq!(manager.len(), 1, "whoever runs the business was not told");
    assert_eq!(manager[0].kind, "tax_refused");
    assert!(
        fixture.inbox_of(STYLIST).await.is_empty(),
        "somebody who reports to a manager was told the business's news"
    );

    fixture.cleanup().await;
}
