//! Conversations, against a real tenant.
//!
//! The test that carries this file is [`a_reply_lands_on_what_it_answers`] —
//! the whole point of 13d, and the reason `messaging` started remembering what
//! it sent.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use conversations::{ConversationError, Inbound};
use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};
use messaging::template::{Body, Template, Templates};
use messaging::{Channel, Subject, Topic};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

/// The one customer these tests talk to.
const HER_NUMBER: &str = "+966500000001";

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn at(day: &str, hour: &str) -> Timestamp {
    format!("{day}T{hour}:00:00Z")
        .parse()
        .expect("a valid instant")
}

fn booking(id: &str) -> Subject {
    Subject::new(Topic::Reservation, code(id))
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
            conversations::install(&mut conn).await.expect("threads");
            ensure_group_schema::<conversations::Conversations>(&mut conn)
                .await
                .expect("t");
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
        run!(conversations, conversations::Conversations);
    }

    /// Throws the read models away and builds them again from the log.
    async fn rebuild(&self) {
        sqlx::query(
            "TRUNCATE proj_conversations.conversation_message, \
             proj_conversations.conversation_thread",
        )
        .execute(&self.pool)
        .await
        .expect("empties the read models");
        sqlx::query("UPDATE projection_checkpoint SET position = 0 WHERE group_name = $1")
            .bind(conversations::GROUP_NAME)
            .execute(&self.pool)
            .await
            .expect("rewinds the checkpoint");
        self.project().await;
    }

    async fn lines(&self, thread: &AggregateId) -> Vec<conversations::Line> {
        let mut conn = self.pool.acquire().await.expect("connection");
        let page = conversations::messages(&mut conn, thread.as_str(), 50, None)
            .await
            .expect("reads");
        page.items
    }

    async fn tray(&self) -> Vec<conversations::Unmatched> {
        let mut conn = self.pool.acquire().await.expect("connection");
        conversations::unmatched(&mut conn, 20)
            .await
            .expect("reads")
    }

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

    async fn spent(&self, channel: Channel, period: &str) -> i32 {
        let mut conn = self.db.acquire().await.expect("connection");
        messaging::budget::spent(&mut conn, period)
            .await
            .expect("reads")
            .into_iter()
            .find(|s| s.channel == channel)
            .map_or(0, |s| s.segments)
    }

    /// Says that a message went to a number about a subject, the way a reminder
    /// would have.
    async fn sent_about(&self, subject: &Subject, to: &str, at: Timestamp) {
        let mut conn = self.db.acquire().await.expect("connection");
        messaging::sent::record(
            &mut conn,
            &format!("reminder.{}.{}", subject.id.as_str(), at.timestamp()),
            &messaging::Outbound {
                channel: Channel::Sms,
                to: to.to_owned(),
                subject: String::new(),
                body: "your appointment".to_owned(),
                locale: erp_i18n::Locale::Arabic,
                platform: None,
            },
            Some(subject),
            at,
        )
        .await
        .expect("records");
    }

    /// Puts a webhook on the record, the way `POST /v1/hooks/messages` does.
    async fn arrives(&self, inbound: &Inbound) {
        sqlx::query(
            "INSERT INTO webhook_event (provider, event_id, kind, payload)
             VALUES ($1,$2,'message',$3)
             ON CONFLICT (provider, event_id) DO UPDATE SET deliveries = webhook_event.deliveries + 1",
        )
        .bind(conversations::PROVIDER)
        .bind(&inbound.id)
        .bind(serde_json::to_value(inbound).expect("serializes"))
        .execute(&self.pool)
        .await
        .expect("records the webhook");
    }

    async fn configure<T: serde::Serialize>(&self, key: &str, value: &T) {
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(&mut conn, key, value, None, None)
            .await
            .expect("configures");
    }

    /// A customer with a number, a chair, and a booking on it.
    async fn a_salon(&self) {
        crm::register_customer(
            &self.db,
            &code("CUST-1"),
            &crm::Details {
                name: "نورة".to_owned(),
                name_latin: None,
                kind: crm::CustomerKind::Person,
                contact: crm::Contact {
                    phone: Some(HER_NUMBER.to_owned()),
                    email: Some("noura@example.test".to_owned()),
                },
                address: None,
                tax: None,
            },
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
                rate: None,
                branch: None,
                employee: None,
            },
            at("2026-01-01", "09"),
            &Metadata::default(),
        )
        .await
        .expect("declares");

        for (id, hour) in [("BK-1", "10"), ("BK-2", "12")] {
            let span = erp_occupancy::Span::new(
                at("2026-05-04", hour),
                at(
                    "2026-05-04",
                    &format!("{}", hour.parse::<u8>().expect("an hour") + 1),
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
                        phone: Some(HER_NUMBER.to_owned()),
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
        }

        self.project().await;
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

// ---------------------------------------------------------------------------

/// **A reply lands on what it answers.**
///
/// The whole of 13d in one test: a reminder went out about a booking, the
/// customer answered the number it came from, and the answer is in that
/// booking's conversation rather than in a void.
#[tokio::test]
async fn a_reply_lands_on_what_it_answers() {
    let fixture = Fixture::new("talk-answers").await;
    fixture.a_salon().await;
    fixture
        .sent_about(&booking("BK-1"), HER_NUMBER, at("2026-05-04", "08"))
        .await;

    fixture
        .arrives(&Inbound {
            id: "gw-1".to_owned(),
            from: HER_NUMBER.to_owned(),
            body: "نعم، سأحضر".to_owned(),
            sent_at: at("2026-05-04", "09"),
        })
        .await;

    let landing = conversations::land(
        &fixture.db,
        at("2026-05-01", "00"),
        chrono::TimeDelta::days(7),
        50,
    )
    .await
    .expect("lands");
    assert_eq!(landing.landed, 1);
    assert_eq!(
        landing.unmatched, 0,
        "an answered reminder went to the tray"
    );
    fixture.project().await;

    let lines = fixture
        .lines(&conversations::thread_id(&booking("BK-1")))
        .await;
    assert_eq!(lines.len(), 1, "the reply is not in the booking's thread");
    assert_eq!(lines[0].kind, "heard");
    assert_eq!(lines[0].body, "نعم، سأحضر");
    assert_eq!(lines[0].address.as_deref(), Some(HER_NUMBER));
    assert!(
        lines[0].who.is_none(),
        "somebody here was credited with what a customer said"
    );

    // Not in the other booking's, and not in the tray.
    assert!(
        fixture
            .lines(&conversations::thread_id(&booking("BK-2")))
            .await
            .is_empty()
    );
    assert!(fixture.tray().await.is_empty());

    fixture.cleanup().await;
}

/// **Correlation does not drift.**
///
/// A later reminder about a different booking must not move a reply that was
/// already answered — which is what "as of the reply's own instant" buys, and
/// what lets the sweep re-run over an overlapping window for ever.
#[tokio::test]
async fn a_reply_is_answered_by_what_was_said_before_it() {
    let fixture = Fixture::new("talk-drift").await;
    fixture.a_salon().await;
    fixture
        .sent_about(&booking("BK-1"), HER_NUMBER, at("2026-05-04", "08"))
        .await;
    // …and then, after she replied, a reminder about the other booking.
    fixture
        .sent_about(&booking("BK-2"), HER_NUMBER, at("2026-05-04", "11"))
        .await;

    fixture
        .arrives(&Inbound {
            id: "gw-1".to_owned(),
            from: HER_NUMBER.to_owned(),
            body: "نعم".to_owned(),
            sent_at: at("2026-05-04", "09"),
        })
        .await;

    for _ in 0..2 {
        conversations::land(
            &fixture.db,
            at("2026-05-01", "00"),
            chrono::TimeDelta::days(7),
            50,
        )
        .await
        .expect("lands");
        fixture.project().await;
    }

    assert_eq!(
        fixture
            .lines(&conversations::thread_id(&booking("BK-1")))
            .await
            .len(),
        1,
        "the reply moved to a booking it could not have been about"
    );
    assert!(
        fixture
            .lines(&conversations::thread_id(&booking("BK-2")))
            .await
            .is_empty()
    );

    fixture.cleanup().await;
}

/// With nothing recently said, a known number is still a known person.
#[tokio::test]
async fn a_reply_from_a_known_number_lands_on_that_customer() {
    let fixture = Fixture::new("talk-customer").await;
    fixture.a_salon().await;

    fixture
        .arrives(&Inbound {
            id: "gw-2".to_owned(),
            from: HER_NUMBER.to_owned(),
            body: "هل أنتم مفتوحون؟".to_owned(),
            sent_at: at("2026-05-04", "09"),
        })
        .await;

    let landing = conversations::land(
        &fixture.db,
        at("2026-05-01", "00"),
        chrono::TimeDelta::days(7),
        50,
    )
    .await
    .expect("lands");
    assert_eq!(landing.landed, 1);
    assert_eq!(landing.unmatched, 0);
    fixture.project().await;

    let hers = conversations::thread_id(&Subject::new(Topic::Customer, code("CUST-1")));
    assert_eq!(fixture.lines(&hers).await.len(), 1);
    assert!(fixture.tray().await.is_empty());

    fixture.cleanup().await;
}

/// **A number nobody has goes in the tray, and assigning it moves what
/// arrived.**
#[tokio::test]
async fn a_reply_from_a_stranger_waits_in_the_tray_until_somebody_says() {
    let fixture = Fixture::new("talk-tray").await;
    fixture.a_salon().await;
    let stranger = "+966599999999";

    for (id, body) in [("gw-3", "مرحبا"), ("gw-4", "أريد موعد")] {
        fixture
            .arrives(&Inbound {
                id: id.to_owned(),
                from: stranger.to_owned(),
                body: body.to_owned(),
                sent_at: at("2026-05-04", "09"),
            })
            .await;
    }

    let landing = conversations::land(
        &fixture.db,
        at("2026-05-01", "00"),
        chrono::TimeDelta::days(7),
        50,
    )
    .await
    .expect("lands");
    assert_eq!(landing.unmatched, 2);
    fixture.project().await;

    let tray = fixture.tray().await;
    assert_eq!(tray.len(), 1, "two messages from one number made two trays");
    assert_eq!(tray[0].address, stranger);
    assert_eq!(tray[0].messages, 2);

    // Somebody says what it was about.
    conversations::assign(
        &fixture.db,
        stranger,
        &booking("BK-2"),
        at("2026-05-04", "10"),
        &Metadata::default(),
    )
    .await
    .expect("assigns");
    fixture.project().await;

    assert_eq!(
        fixture
            .lines(&conversations::thread_id(&booking("BK-2")))
            .await
            .len(),
        2,
        "assigning did not move what had arrived"
    );
    assert!(
        fixture.tray().await.is_empty(),
        "an assigned conversation is still waiting in the tray"
    );

    // And it cannot be assigned twice.
    let refused = conversations::assign(
        &fixture.db,
        stranger,
        &booking("BK-1"),
        at("2026-05-04", "11"),
        &Metadata::default(),
    )
    .await
    .expect_err("assigned twice");
    assert!(
        format!("{refused:?}").contains("AlreadyAssigned"),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// The same webhook landed twice writes one message, however often the sweep
/// runs.
#[tokio::test]
async fn hearing_the_same_reply_twice_writes_one_message() {
    let fixture = Fixture::new("talk-twice").await;
    fixture.a_salon().await;

    let reply = Inbound {
        id: "gw-5".to_owned(),
        from: HER_NUMBER.to_owned(),
        body: "نعم".to_owned(),
        sent_at: at("2026-05-04", "09"),
    };
    fixture.arrives(&reply).await;

    let first = conversations::land(
        &fixture.db,
        at("2026-05-01", "00"),
        chrono::TimeDelta::days(7),
        50,
    )
    .await
    .expect("lands");
    assert_eq!(first.landed, 1);
    fixture.project().await;

    // The gateway retries; the sweep runs again over the same window.
    fixture.arrives(&reply).await;
    let again = conversations::land(
        &fixture.db,
        at("2026-05-01", "00"),
        chrono::TimeDelta::days(7),
        50,
    )
    .await
    .expect("lands again");
    assert_eq!(again.landed, 0, "the same reply was written twice");
    assert_eq!(again.known, 1);
    fixture.project().await;

    let hers = conversations::thread_id(&Subject::new(Topic::Customer, code("CUST-1")));
    assert_eq!(fixture.lines(&hers).await.len(), 1);

    fixture.cleanup().await;
}

/// **A note never leaves; a message does — and the meter knows the difference.**
#[tokio::test]
async fn a_note_stays_inside_and_a_message_goes_out() {
    let fixture = Fixture::new("talk-note").await;
    fixture.a_salon().await;

    conversations::note(
        &fixture.db,
        &booking("BK-1"),
        "اتصلت، تريد الخميس",
        at("2026-05-04", "09"),
        &Metadata::default(),
    )
    .await
    .expect("notes");
    fixture.project().await;

    assert!(
        fixture.outbox().await.is_empty(),
        "a note promised something to somebody"
    );
    assert_eq!(fixture.spent(Channel::Sms, "2026-05").await, 0);

    conversations::say(
        &fixture.db,
        &booking("BK-1"),
        "الخميس الساعة ١٠ مناسب",
        Channel::Sms,
        at("2026-05-04", "10"),
        &Metadata::default(),
    )
    .await
    .expect("says");
    fixture.project().await;

    let outbox = fixture.outbox().await;
    assert_eq!(outbox.len(), 1, "a message reached nobody");
    assert_eq!(outbox[0].to, HER_NUMBER);
    assert_eq!(outbox[0].channel, Channel::Sms);
    assert!(
        fixture.spent(Channel::Sms, "2026-05").await > 0,
        "nothing was metered"
    );

    let lines = fixture
        .lines(&conversations::thread_id(&booking("BK-1")))
        .await;
    assert_eq!(lines.len(), 2, "the thread does not hold both");
    assert_eq!(lines[0].kind, "note");
    assert!(lines[0].channel.is_none(), "a note went out on a channel");
    assert_eq!(lines[1].kind, "said");
    assert_eq!(lines[1].channel.as_deref(), Some("sms"));

    // And what was said is remembered against the booking, so her reply to it
    // will find its way back here.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let about = messaging::last_sent_to(
        &mut conn,
        HER_NUMBER,
        at("2026-05-04", "11"),
        chrono::TimeDelta::days(7),
    )
    .await
    .expect("reads")
    .expect("something was sent");
    assert_eq!(about.id.as_str(), "BK-1");
    drop(conn);

    fixture.cleanup().await;
}

/// **The channel is the sender's to pick, and two of the four are refused.**
#[tokio::test]
async fn a_person_may_not_type_into_every_channel() {
    let fixture = Fixture::new("talk-channel").await;
    fixture.a_salon().await;

    for channel in [Channel::WhatsApp, Channel::Push, Channel::InSystem] {
        let refused = conversations::say(
            &fixture.db,
            &booking("BK-1"),
            "مرحبا",
            channel,
            at("2026-05-04", "10"),
            &Metadata::default(),
        )
        .await
        .expect_err("typed into a channel that does not take typing");
        assert!(
            format!("{refused:?}").contains("NotAChannelForThis"),
            "{channel}: {refused:?}"
        );
    }

    // Email is allowed, and she has an address.
    conversations::say(
        &fixture.db,
        &booking("BK-1"),
        "الخميس مناسب",
        Channel::Email,
        at("2026-05-04", "10"),
        &Metadata::default(),
    )
    .await
    .expect("email is a channel a person types into");

    assert!(
        fixture
            .outbox()
            .await
            .iter()
            .any(|m| m.channel == Channel::Email)
    );

    fixture.cleanup().await;
}

/// **A subject with no client is notes-only**, and says so rather than
/// resolving to nobody.
#[tokio::test]
async fn a_thread_about_an_employee_is_notes_only() {
    let fixture = Fixture::new("talk-employee").await;
    fixture.a_salon().await;
    let about_somebody = Subject::new(Topic::Employee, code("EMP-1"));

    conversations::note(
        &fixture.db,
        &about_somebody,
        "مراجعة الأداء يوم الأحد",
        at("2026-05-04", "09"),
        &Metadata::default(),
    )
    .await
    .expect("a note about anybody is allowed");

    let refused = conversations::say(
        &fixture.db,
        &about_somebody,
        "مرحبا",
        Channel::Sms,
        at("2026-05-04", "10"),
        &Metadata::default(),
    )
    .await
    .expect_err("sent a message to an employee record's customer");
    assert!(
        matches!(
            refused,
            erp_tenant::CommandError::Execute(erp_eventlog::ExecuteError::Rejected(
                ConversationError::NoClient
            ))
        ),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **A spent budget refuses the message and records nothing.**
#[tokio::test]
async fn a_spent_budget_leaves_no_trace_of_a_message() {
    let fixture = Fixture::new("talk-budget").await;
    fixture.a_salon().await;
    fixture
        .configure(
            messaging::budget::KEY,
            &messaging::Budget {
                sms: Some(0),
                whatsapp: None,
                email: None,
                push: None,
                configured: true,
            },
        )
        .await;

    let refused = conversations::say(
        &fixture.db,
        &booking("BK-1"),
        "الخميس مناسب",
        Channel::Sms,
        at("2026-05-04", "10"),
        &Metadata::default(),
    )
    .await
    .expect_err("spent the budget it did not have");
    assert!(
        matches!(
            refused,
            erp_tenant::CommandError::Execute(erp_eventlog::ExecuteError::Rejected(
                ConversationError::OverBudget { .. }
            ))
        ),
        "a spent budget did not reach the caller as one: {refused:?}"
    );
    fixture.project().await;

    assert!(
        fixture.outbox().await.is_empty(),
        "a refused message was promised anyway"
    );
    assert_eq!(
        fixture.spent(Channel::Sms, "2026-05").await,
        0,
        "a refused message spent the meter"
    );
    assert!(
        fixture
            .lines(&conversations::thread_id(&booking("BK-1")))
            .await
            .is_empty(),
        "a message nobody sent is in the thread"
    );

    fixture.cleanup().await;
}

/// A conversation is what the log says it is, message order included.
#[tokio::test]
async fn a_conversation_survives_a_rebuild() {
    let fixture = Fixture::new("talk-rebuild").await;
    fixture.a_salon().await;

    conversations::note(
        &fixture.db,
        &booking("BK-1"),
        "اتصلت",
        at("2026-05-04", "09"),
        &Metadata::default(),
    )
    .await
    .expect("notes");
    conversations::say(
        &fixture.db,
        &booking("BK-1"),
        "الخميس مناسب",
        Channel::Sms,
        at("2026-05-04", "10"),
        &Metadata::default(),
    )
    .await
    .expect("says");
    fixture.project().await;

    let before = fixture
        .lines(&conversations::thread_id(&booking("BK-1")))
        .await;
    fixture.rebuild().await;
    let after = fixture
        .lines(&conversations::thread_id(&booking("BK-1")))
        .await;

    assert_eq!(
        before, after,
        "a rebuild did not reproduce the conversation"
    );
    assert_eq!(after.len(), 2);

    fixture.cleanup().await;
}

/// A template on the in-system channel is not what a conversation sends, and
/// the tenant's templates are irrelevant here — a person is typing.
#[tokio::test]
async fn what_a_person_types_is_what_goes_out() {
    let fixture = Fixture::new("talk-typed").await;
    fixture.a_salon().await;

    let mut templates = Templates::default();
    templates.entries.insert(
        "booking.reminder".to_owned(),
        Template {
            channel: Channel::Sms,
            topic: Topic::Reservation,
            audience: messaging::Audience::Client,
            bodies: BTreeMap::from([
                (
                    "en".to_owned(),
                    Body {
                        subject: String::new(),
                        text: "A reminder nobody asked for".to_owned(),
                    },
                ),
                (
                    "ar".to_owned(),
                    Body {
                        subject: String::new(),
                        text: "تذكير".to_owned(),
                    },
                ),
            ]),
            active: true,
        },
    );
    fixture
        .configure(messaging::template::KEY, &templates)
        .await;

    conversations::say(
        &fixture.db,
        &booking("BK-1"),
        "الخميس الساعة ١٠",
        Channel::Sms,
        at("2026-05-04", "10"),
        &Metadata::default(),
    )
    .await
    .expect("says");

    let outbox = fixture.outbox().await;
    assert_eq!(outbox.len(), 1);
    assert_eq!(
        outbox[0].body, "الخميس الساعة ١٠",
        "a template was rendered over what a person typed"
    );

    fixture.cleanup().await;
}
