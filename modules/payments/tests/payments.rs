//! Collecting money, against a real tenant with real books.
//!
//! The test that carries this file is
//! [`a_gateway_that_reports_a_different_amount_settles_nothing`]. Every
//! callback in this system is unsigned, so the amount check is the only thing
//! standing between a gateway id — which is not a secret — and an invoice
//! marked paid for a number somebody chose.
//!
//! The second one to read is [`a_fee_is_an_expense_and_never_a_smaller_sale`],
//! because netting the gateway's cut against revenue is wrong in a way that
//! only surfaces when a VAT return is filed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::{ExecuteError, Metadata};
use erp_payments::{Charged, Status};
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use ledger::{AccountKind, Ledger, account_balances, open_account, trial_balance};
use payments::{Attempt, Payments, PaymentsError};
use sales::{Draft, DraftLine, Sales, VatCategory};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn sar() -> CurrencyCode {
    CurrencyCode::new("SAR").expect("valid")
}
fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}
fn when() -> Timestamp {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid")
}
fn money(minor: i64) -> Money {
    Money::from_minor(minor, sar())
}
fn riyals(major: i64) -> Money {
    money(major * 100)
}

struct Fixture {
    db: TenantDb,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    tenant_database: String,
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
            .register_tenant_on(slug, "Bassat", "primary", Actor::system())
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
        ledger::install(&mut conn).await.expect("ledger schema");
        ensure_group_schema::<Ledger>(&mut conn)
            .await
            .expect("ledger checkpoint");
        sales::install(&mut conn).await.expect("sales schema");
        ensure_group_schema::<Sales>(&mut conn)
            .await
            .expect("sales checkpoint");
        payments::install(&mut conn).await.expect("payments schema");
        ensure_group_schema::<Payments>(&mut conn)
            .await
            .expect("payments checkpoint");
        drop(conn);

        let fixture = Self {
            db,
            _control: control,
            _control_db: control_db,
            tenant_database: tenant.database_name,
        };

        // Everything a card payment touches, plus the invoice's own accounts.
        for (account, kind) in [
            ("1100", AccountKind::Asset),
            ("1150", AccountKind::Asset),
            ("1160", AccountKind::Asset),
            ("2100", AccountKind::Liability),
            ("4000", AccountKind::Revenue),
            ("5400", AccountKind::Expense),
        ] {
            open_account(
                &fixture.db,
                &code(account),
                account,
                kind,
                sar(),
                &Metadata::default(),
            )
            .await
            .expect("opens");
        }
        fixture
    }

    async fn tenant_pool(&self) -> sqlx::PgPool {
        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgPool::connect(&format!("{base}/{}", self.tenant_database))
            .await
            .expect("connects")
    }

    async fn project(&self) {
        let pool = self.tenant_pool().await;

        let owned = ledger::projections();
        let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
            .await
            .expect("ledger projects");

        let owned = sales::projections();
        let refs: Vec<&dyn Projection<Group = Sales>> = owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Sales>(&pool, &refs, sales::upcasters(), 200)
            .await
            .expect("sales projects");

        let owned = payments::projections();
        let refs: Vec<&dyn Projection<Group = Payments>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Payments>(&pool, &refs, payments::upcasters(), 200)
            .await
            .expect("payments projects");

        pool.close().await;
    }

    async fn balance(&self, account: &str) -> Money {
        let mut conn = self.db.acquire().await.expect("connection");
        account_balances(&mut conn)
            .await
            .expect("reads")
            .into_iter()
            .find(|a| a.code == account)
            .map_or_else(|| money(0), |a| a.balance)
    }

    /// An invoice for a hundred riyals net, which is 115 with VAT.
    async fn invoice(&self, id: &str) {
        sales::issue_invoice(
            &self.db,
            &code(id),
            &Draft {
                customer: sales::Customer::new("سارة"),
                issued_on: when(),
                due_on: None,
                currency: sar(),
                lines: vec![DraftLine {
                    description: "Massage".to_owned(),
                    net: riyals(100),
                    category: VatCategory::Standard,
                }],
                discounts: Vec::new(),
                note: String::new(),
            },
            &Metadata::default(),
        )
        .await
        .expect("issues");
    }

    async fn start(&self, id: &str, provider: &str, invoice: &str, amount: Money) {
        let mut tx = self.db.begin().await.expect("transaction");
        payments::start_in(
            &mut tx,
            &code(id),
            &Attempt {
                provider: provider.to_owned(),
                gateway_id: id.to_owned(),
                invoice: code(invoice),
                amount,
            },
            when(),
            &Metadata::default(),
        )
        .await
        .expect("starts");
        tx.commit().await.expect("commits");
    }

    async fn settle(&self, id: &str, charged: &Charged) -> Result<(), ExecuteError<PaymentsError>> {
        let mut tx = self.db.begin().await.expect("transaction");
        let outcome =
            payments::settle_in(&mut tx, &code(id), charged, when(), &Metadata::default())
                .await
                .map(|_| ());
        if outcome.is_ok() {
            tx.commit().await.expect("commits");
        } else {
            tx.rollback().await.expect("rolls back");
        }
        outcome
    }
}

fn charged(id: &str, status: Status, amount: Money, fee: Option<Money>) -> Charged {
    Charged {
        id: id.to_owned(),
        status,
        amount,
        refunded: money(0),
        fee,
        challenge: None,
        message: None,
    }
}

// ---------------------------------------------------------------------------

/// **The test this file carries.**
///
/// Every callback in this system is unsigned, so anybody who learns a gateway
/// id — which is not a secret and travels in a customer's browser — could
/// otherwise settle an invoice for a number of their choosing.
#[tokio::test]
async fn a_gateway_that_reports_a_different_amount_settles_nothing() {
    let fixture = Fixture::new("amount").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;

    let refused = fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(1), None))
        .await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(PaymentsError::WrongAmount { .. }))
        ),
        "{refused:?}"
    );

    fixture.project().await;
    // Nothing moved, and the customer still owes the money.
    assert_eq!(fixture.balance("1150").await, money(0));
    assert_eq!(fixture.balance("1100").await, riyals(115));
}

/// The gateway's cut is an expense, and the sale is still the whole sale.
#[tokio::test]
async fn a_fee_is_an_expense_and_never_a_smaller_sale() {
    let fixture = Fixture::new("fee").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;

    fixture
        .settle(
            "pay_1",
            &charged("pay_1", Status::Paid, riyals(115), Some(money(316))),
        )
        .await
        .expect("settles");
    fixture.project().await;

    assert_eq!(fixture.balance("1100").await, money(0));
    // The gateway holds the money **net of what it kept**, which is what it
    // will actually pay over — so a payout has something to reconcile to.
    assert_eq!(fixture.balance("1150").await, money(11_500 - 316));
    assert_eq!(fixture.balance("5400").await, money(316));
    // And the sale is still a hundred riyals with fifteen of VAT on it.
    // Negative because revenue and VAT are credits — the sign convention
    // `ledger::account_balances` reports and `sales`' own tests assert.
    assert_eq!(fixture.balance("4000").await, riyals(-100));
    assert_eq!(fixture.balance("2100").await, riyals(-15));

    let mut conn = fixture.db.acquire().await.expect("connection");
    let balance = trial_balance(&mut conn).await.expect("reads");
    assert!(!balance.is_empty(), "there should be something to balance");
    assert!(
        balance.iter().all(ledger::TrialBalance::balances),
        "the books do not balance: {balance:?}"
    );
}

/// **Buy-now-pay-later is not a card.** The lender has paid the merchant and is
/// collecting from the customer, so what is owed afterwards is owed by Tabby.
#[tokio::test]
async fn an_instalment_provider_owes_the_money_and_not_the_card_gateway() {
    let fixture = Fixture::new("bnpl").await;
    fixture.invoice("INV-1").await;
    fixture.start("tab_1", "tabby", "INV-1", riyals(115)).await;

    fixture
        .settle(
            "tab_1",
            &charged("tab_1", Status::Paid, riyals(115), Some(riyals(7))),
        )
        .await
        .expect("settles");
    fixture.project().await;

    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "the customer is clear"
    );
    assert_eq!(fixture.balance("1160").await, riyals(115 - 7));
    assert_eq!(
        fixture.balance("1150").await,
        money(0),
        "nothing belongs to the card gateway"
    );
    assert_eq!(fixture.balance("5400").await, riyals(7));
}

/// A callback arrives more than once. It has to settle once.
#[tokio::test]
async fn a_callback_delivered_three_times_records_one_payment() {
    let fixture = Fixture::new("retry").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;

    let said = charged("pay_1", Status::Paid, riyals(115), Some(money(316)));
    for _ in 0..3 {
        fixture.settle("pay_1", &said).await.expect("settles");
    }
    fixture.project().await;

    assert_eq!(fixture.balance("1150").await, money(11_500 - 316));
    assert_eq!(
        fixture.balance("5400").await,
        money(316),
        "one fee, not three"
    );
    assert_eq!(fixture.balance("1100").await, money(0));
}

/// A callback can legitimately arrive while the customer is still deciding.
/// That is not an error and it is not a settlement.
#[tokio::test]
async fn a_payment_still_waiting_on_the_customer_posts_nothing() {
    let fixture = Fixture::new("waiting").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;

    for status in [Status::Initiated, Status::Authorized] {
        fixture
            .settle("pay_1", &charged("pay_1", status, riyals(115), None))
            .await
            .expect("is not an error");
    }
    fixture.project().await;

    assert_eq!(fixture.balance("1150").await, money(0));
    assert_eq!(fixture.balance("1100").await, riyals(115));

    let mut conn = fixture.db.acquire().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("a payment");
    assert_eq!(row.stage, "pending");
}

/// A refused card moves no money and says why.
#[tokio::test]
async fn a_refused_card_is_recorded_and_posts_nothing() {
    let fixture = Fixture::new("refused").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;

    let mut said = charged("pay_1", Status::Failed, riyals(115), None);
    said.message = Some("Insufficient funds".to_owned());
    fixture.settle("pay_1", &said).await.expect("records");
    fixture.project().await;

    assert_eq!(fixture.balance("1150").await, money(0));
    assert_eq!(fixture.balance("1100").await, riyals(115));

    let mut conn = fixture.db.acquire().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("a payment");
    assert_eq!(row.stage, "failed");
    assert_eq!(row.failed_why.as_deref(), Some("Insufficient funds"));
}

/// Money goes back out of the account it went into, and the customer owes it
/// again. **The fee does not come back** — a gateway keeps its cut on a
/// refunded payment, which is why a refund costs more than the sale earned.
#[tokio::test]
async fn a_refund_takes_the_money_back_out_of_where_it_landed() {
    let fixture = Fixture::new("refund").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .settle(
            "pay_1",
            &charged("pay_1", Status::Paid, riyals(115), Some(money(316))),
        )
        .await
        .expect("settles");

    let mut tx = fixture.db.begin().await.expect("transaction");
    payments::refund_in(
        &mut tx,
        &code("pay_1"),
        "refund-1",
        riyals(115),
        when(),
        &Metadata::default(),
    )
    .await
    .expect("refunds");
    tx.commit().await.expect("commits");
    fixture.project().await;

    assert_eq!(
        fixture.balance("1150").await,
        money(-316),
        "only the fee is left"
    );
    assert_eq!(fixture.balance("1100").await, riyals(115), "owed again");
    assert_eq!(
        fixture.balance("5400").await,
        money(316),
        "the fee stays spent"
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("a payment");
    assert_eq!(row.stage, "refunded");
}

/// Giving back more than was taken is refused rather than posted.
#[tokio::test]
async fn a_refund_larger_than_the_payment_is_refused() {
    let fixture = Fixture::new("toobig").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");

    let mut tx = fixture.db.begin().await.expect("transaction");
    let refused = payments::refund_in(
        &mut tx,
        &code("pay_1"),
        "refund-1",
        riyals(200),
        when(),
        &Metadata::default(),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(PaymentsError::RefundTooLarge(_)))
        ),
        "{refused:?}"
    );
    tx.rollback().await.expect("rolls back");
}

/// A payment nobody started cannot be settled, whatever a callback says.
#[tokio::test]
async fn a_gateway_id_this_system_never_issued_settles_nothing() {
    let fixture = Fixture::new("unknown").await;
    let refused = fixture
        .settle(
            "pay_ghost",
            &charged("pay_ghost", Status::Paid, riyals(115), None),
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(PaymentsError::NotStarted(_)))
        ),
        "{refused:?}"
    );
}

/// The read model is a pure function of the log (L2): a rebuild from nothing
/// reaches the same rows.
#[tokio::test]
async fn a_rebuild_reproduces_every_payment() {
    let fixture = Fixture::new("rebuild").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .settle(
            "pay_1",
            &charged("pay_1", Status::Paid, riyals(115), Some(money(316))),
        )
        .await
        .expect("settles");
    fixture.project().await;

    let before = {
        let mut conn = fixture.db.acquire().await.expect("connection");
        payments::against(&mut conn, "INV-1", 100)
            .await
            .expect("reads")
    };
    assert_eq!(before.len(), 1);

    let pool = fixture.tenant_pool().await;
    sqlx::query("TRUNCATE proj_payments.payment")
        .execute(&pool)
        .await
        .expect("empties");
    sqlx::query("UPDATE projection_checkpoint SET position = 0 WHERE group_name = 'payments'")
        .execute(&pool)
        .await
        .expect("rewinds");
    pool.close().await;

    fixture.project().await;
    let after = {
        let mut conn = fixture.db.acquire().await.expect("connection");
        payments::against(&mut conn, "INV-1", 100)
            .await
            .expect("reads")
    };
    assert_eq!(before, after);
}
