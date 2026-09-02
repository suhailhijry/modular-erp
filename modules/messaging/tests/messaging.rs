//! Messaging, against a real tenant.
//!
//! The test that carries this file is
//! [`a_reminder_says_what_is_true_now_and_reaches_where_somebody_is_now`] —
//! Phase 11's exit criterion, and the two corrections this module exists to
//! make: a template that fetches its own data, and an audience that is resolved
//! rather than frozen.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};
use messaging::template::{Body, Template, Templates};
use messaging::{Budget, Channel, SendError, Sending, Subject, Topic};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn at(day: &str, hour: &str) -> Timestamp {
    format!("{day}T{hour}:00:00Z")
        .parse()
        .expect("a valid instant")
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
    }

    async fn configure<T: serde::Serialize>(&self, key: &str, value: &T) {
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(&mut conn, key, value, None)
            .await
            .expect("configures");
    }

    /// Saves one template under a name.
    async fn template(&self, name: &str, template: Template) {
        template.check(name).expect("a valid template");
        let mut templates = Templates::default();
        templates.entries.insert(name.to_owned(), template);
        self.configure(messaging::template::KEY, &templates).await;
    }

    async fn send(&self, sending: &Sending) -> Result<messaging::Sent, SendError> {
        let mut tx = self.db.begin().await.expect("transaction");
        match messaging::send(&mut tx, sending).await {
            Ok(sent) => {
                tx.commit().await.expect("commits");
                Ok(sent)
            }
            Err(e) => {
                tx.rollback().await.expect("rolls back");
                Err(e)
            }
        }
    }

    /// Every message actually promised, oldest first.
    async fn outbox(&self) -> Vec<messaging::Outbound> {
        let rows: Vec<(serde_json::Value,)> =
            sqlx::query_as("SELECT payload FROM outbox ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .expect("reads the outbox");

        rows.into_iter()
            .map(|(payload,)| {
                messaging::Outbound::from_payload(&payload).expect("a message effect")
            })
            .collect()
    }

    async fn spent(&self, channel: Channel, period: &str) -> i32 {
        let mut conn = self.db.acquire().await.expect("connection");
        messaging::budget::spent(&mut conn, period)
            .await
            .expect("reads")
            .into_iter()
            .find(|s| s.channel == channel)
            .map_or(0, |s| s.segments)
    }

    /// A customer, a chair, and a confirmed booking on it.
    async fn a_salon(&self) {
        crm::register_customer(
            &self.db,
            &code("CUST-1"),
            &customer(Some("+966500000001"), None),
            at("2026-01-01", "09"),
            &Metadata::default(),
        )
        .await
        .expect("registers");

        booking::declare_resource(
            &self.db,
            &code("CHAIR-1"),
            &booking::Details {
                name: "كرسي ١".to_owned(),
                name_latin: None,
                kind: booking::Kind::Place,
                capacity: 1,
                branch: None,
                employee: None,
            },
            at("2026-01-01", "09"),
            &Metadata::default(),
        )
        .await
        .expect("declares");

        let span = erp_occupancy::Span::new(at("2026-05-04", "10"), at("2026-05-04", "11"))
            .expect("a valid span");
        booking::reserve(
            &self.db,
            &code("BK-1"),
            &booking::Draft {
                customer: booking::Customer {
                    id: Some(code("CUST-1")),
                    name: "نورة".to_owned(),
                    phone: Some("+966500000001".to_owned()),
                },
                lines: vec![booking::DraftLine {
                    what: "قص".to_owned(),
                    span,
                    takes: vec![booking::Held::one(code("CHAIR-1"))],
                    charge: None,
                }],
                note: String::new(),
                at: at("2026-05-01", "09"),
            },
            &Metadata::default(),
        )
        .await
        .expect("reserves");

        self.project().await;
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// The one customer these tests use, with whichever contact details.
fn customer(phone: Option<&str>, email: Option<&str>) -> crm::Details {
    crm::Details {
        name: "نورة".to_owned(),
        name_latin: None,
        kind: crm::CustomerKind::Person,
        contact: crm::Contact {
            phone: phone.map(str::to_owned),
            email: email.map(str::to_owned),
        },
        address: None,
        tax: None,
    }
}

fn sms(text_en: &str, text_ar: &str) -> Template {
    Template {
        channel: Channel::Sms,
        topic: Topic::Reservation,
        audience: messaging::Audience::Client,
        bodies: BTreeMap::from([
            (
                "en".to_owned(),
                Body {
                    subject: String::new(),
                    text: text_en.to_owned(),
                },
            ),
            (
                "ar".to_owned(),
                Body {
                    subject: String::new(),
                    text: text_ar.to_owned(),
                },
            ),
        ]),
        active: true,
    }
}

fn reminder(key: &str, at: Timestamp) -> Sending {
    Sending {
        template: "booking.reminder".to_owned(),
        subject: Subject::new(Topic::Reservation, code("BK-1")),
        key: key.to_owned(),
        operator: None,
        extra: BTreeMap::from([("link".to_owned(), "/l/abc123".to_owned())]),
        locale: None,
        at,
    }
}

// ---------------------------------------------------------------------------

/// **Phase 11's exit criterion, and the two corrections this module makes.**
///
/// The caller supplies a booking id and a key. It does not know the customer's
/// name, their number, what language they read, or what the message says — and
/// when the booking moves and the customer changes their number, the next
/// message says the new time and goes to the new number without the caller
/// doing anything.
#[tokio::test]
async fn a_reminder_says_what_is_true_now_and_reaches_where_somebody_is_now() {
    let fixture = Fixture::new("remind").await;
    fixture.a_salon().await;

    fixture
        .configure(
            messaging::settings::KEY,
            &messaging::Settings {
                business: "صالون بسمة".to_owned(),
                language: erp_i18n::Locale::Arabic,
            },
        )
        .await;
    fixture
        .template(
            "booking.reminder",
            sms(
                "{{ business }}: {{ customer.name }}, {{ reservation.starts_at }}. {{ link }}",
                "{{ business }}: {{ customer.name }}، موعدك {{ reservation.starts_at }}. {{ link }}",
            ),
        )
        .await;

    let sent = fixture
        .send(&reminder("booking.reminder.BK-1", at("2026-05-03", "10")))
        .await
        .expect("sends");
    assert_eq!(sent.recipients, 1);
    assert_eq!(sent.promised, 1);
    assert_eq!(sent.channel, Channel::Sms);

    let promised = fixture.outbox().await;
    assert_eq!(promised.len(), 1);
    let message = &promised[0];
    assert_eq!(message.to, "+966500000001", "the number was resolved");
    assert_eq!(message.locale, erp_i18n::Locale::Arabic);
    assert!(
        message.body.contains("صالون بسمة") && message.body.contains("نورة"),
        "the template asked the read model: {}",
        message.body
    );
    assert!(
        message.body.contains("2026-05-04 10:00"),
        "the time is the booking's: {}",
        message.body
    );
    assert!(message.body.contains("/l/abc123"), "{}", message.body);
    assert!(
        !message.body.contains("{{"),
        "an unresolved binding: {}",
        message.body
    );

    // The booking moves, and the customer changes their number.
    let moved = erp_occupancy::Span::new(at("2026-05-04", "14"), at("2026-05-04", "15"))
        .expect("a valid span");
    booking::reschedule(
        &fixture.db,
        &code("BK-1"),
        &[booking::DraftLine {
            what: "قص".to_owned(),
            span: moved,
            takes: vec![booking::Held::one(code("CHAIR-1"))],
            charge: None,
        }],
        at("2026-05-03", "11"),
        &Metadata::default(),
    )
    .await
    .expect("reschedules");

    crm::amend_customer(
        &fixture.db,
        &code("CUST-1"),
        &customer(Some("+966500000002"), None),
        &Metadata::default(),
    )
    .await
    .expect("amends");
    fixture.project().await;

    // A new key, because this is a new reminder rather than a retry of the one
    // already sent.
    fixture
        .send(&reminder(
            "booking.reminder.BK-1.moved",
            at("2026-05-03", "12"),
        ))
        .await
        .expect("sends again");

    let promised = fixture.outbox().await;
    assert_eq!(promised.len(), 2);
    let second = &promised[1];
    assert_eq!(
        second.to, "+966500000002",
        "a number that changed this morning gets this afternoon's message"
    );
    assert!(
        second.body.contains("2026-05-04 14:00"),
        "a reminder for a booking that moved says the new time: {}",
        second.body
    );

    fixture.cleanup().await;
}

/// **A retry promises one message and charges once.**
///
/// The property that matters most in practice: a reminder job runs every few
/// minutes and calls this for the same booking over and over. Charging each
/// time would spend a month's budget on one reminder.
#[tokio::test]
async fn sending_the_same_key_again_promises_nothing_and_charges_nothing() {
    let fixture = Fixture::new("retry").await;
    fixture.a_salon().await;
    fixture
        .template(
            "booking.reminder",
            sms(
                "at {{ reservation.starts_at }}",
                "في {{ reservation.starts_at }}",
            ),
        )
        .await;

    let first = fixture
        .send(&reminder("booking.reminder.BK-1", at("2026-05-03", "10")))
        .await
        .expect("sends");
    assert_eq!(first.promised, 1);
    assert_eq!(first.units, 1);
    assert_eq!(fixture.spent(Channel::Sms, "2026-05").await, 1);

    for _ in 0..5 {
        let again = fixture
            .send(&reminder("booking.reminder.BK-1", at("2026-05-03", "10")))
            .await
            .expect("sends again");
        assert_eq!(again.recipients, 1, "the audience still resolves");
        assert_eq!(again.promised, 0, "nothing new was promised");
        assert_eq!(again.units, 0, "and nothing was charged");
    }

    assert_eq!(fixture.outbox().await.len(), 1, "one message, not six");
    assert_eq!(fixture.spent(Channel::Sms, "2026-05").await, 1);

    fixture.cleanup().await;
}

/// **An Arabic message over seventy characters costs two segments.**
///
/// Not an edge case in this market — it is most messages. The meter has to
/// count what is billed rather than what is sent, or a business is out by a
/// factor nobody predicted.
#[tokio::test]
async fn an_arabic_message_over_seventy_characters_is_metered_as_two() {
    let fixture = Fixture::new("segments").await;
    fixture.a_salon().await;
    fixture
        .configure(
            messaging::settings::KEY,
            &messaging::Settings {
                business: "صالون بسمة".to_owned(),
                language: erp_i18n::Locale::Arabic,
            },
        )
        .await;

    // Comfortably over seventy characters, and every one of them Arabic.
    let long = "نذكّركم بموعدكم في صالون بسمة يوم الاثنين، ونرجو الحضور قبل الموعد بعشر دقائق حتى نتمكن من خدمتكم على أكمل وجه";
    fixture
        .template("booking.reminder", sms("short", long))
        .await;

    let sent = fixture
        .send(&reminder("booking.reminder.BK-1", at("2026-05-03", "10")))
        .await
        .expect("sends");

    assert!(sent.units >= 2, "billed as one segment: {}", sent.units);
    assert_eq!(fixture.spent(Channel::Sms, "2026-05").await, sent.units);

    fixture.cleanup().await;
}

/// **A budget refuses rather than overspending (L6), and the refusal spends
/// nothing.**
#[tokio::test]
async fn a_spent_budget_refuses_and_leaves_the_meter_where_it_was() {
    let fixture = Fixture::new("budget").await;
    fixture.a_salon().await;
    fixture
        .template("booking.reminder", sms("hello", "مرحبا"))
        .await;
    fixture
        .configure(
            messaging::budget::KEY,
            &Budget {
                sms: Some(1),
                whatsapp: None,
                email: None,
                push: None,
                configured: true,
            },
        )
        .await;

    fixture
        .send(&reminder("booking.reminder.one", at("2026-05-03", "10")))
        .await
        .expect("the first fits");
    assert_eq!(fixture.spent(Channel::Sms, "2026-05").await, 1);

    let refused = fixture
        .send(&reminder("booking.reminder.two", at("2026-05-03", "11")))
        .await
        .expect_err("the second does not");
    assert!(
        matches!(refused, SendError::Spend(messaging::SpendError::Refused(_))),
        "expected a budget refusal, got {refused:?}"
    );

    // **The refusal rolled back the meter as well as the message.** The charge
    // is written before the limit is checked — that write is the lock — so a
    // refusal that committed would have spent budget on a message nobody got.
    assert_eq!(fixture.spent(Channel::Sms, "2026-05").await, 1);
    assert_eq!(fixture.outbox().await.len(), 1);

    fixture.cleanup().await;
}

/// A customer with no mobile number is a fact, and it is a refusal rather than
/// a silent success. A reminder that went to nobody is a chair that stays empty.
#[tokio::test]
async fn a_customer_with_no_number_is_a_refusal_and_not_a_quiet_nothing() {
    let fixture = Fixture::new("unreachable").await;
    fixture.a_salon().await;
    fixture
        .template("booking.reminder", sms("hello", "مرحبا"))
        .await;

    // No number, and an email so the record stays valid — `crm` refuses a
    // customer with no way to reach them at all.
    crm::amend_customer(
        &fixture.db,
        &code("CUST-1"),
        &customer(None, Some("noura@example.test")),
        &Metadata::default(),
    )
    .await
    .expect("amends");
    fixture.project().await;

    let refused = fixture
        .send(&reminder("booking.reminder.BK-1", at("2026-05-03", "10")))
        .await
        .expect_err("nobody to text");
    assert!(
        matches!(refused, SendError::Unreachable { .. }),
        "got {refused:?}"
    );
    assert!(fixture.outbox().await.is_empty());

    fixture.cleanup().await;
}
