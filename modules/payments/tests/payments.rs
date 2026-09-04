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
use erp_payments::{Gateway, GatewayError};
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
    /// Where a saved card's token goes. One per fixture, because a key that
    /// changed between calls would make every unseal fail for the wrong reason.
    sealing: erp_eventlog::SealingKey,
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
            sealing: erp_eventlog::SealingKey::generate("test").expect("a key is generated"),
        };

        // Everything a card payment touches, plus the invoice's own accounts.
        for (account, kind) in [
            ("1010", AccountKind::Asset),
            ("1100", AccountKind::Asset),
            ("1150", AccountKind::Asset),
            ("1160", AccountKind::Asset),
            ("2100", AccountKind::Liability),
            ("4000", AccountKind::Revenue),
            ("4910", AccountKind::Revenue),
            ("5400", AccountKind::Expense),
            ("5420", AccountKind::Expense),
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
                prepayment: false,
                customer: sales::Customer::new("سارة"),
                issued_on: when(),
                due_on: None,
                currency: sar(),
                lines: vec![DraftLine {
                    allowances: Vec::new(),
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

    /// A payment collecting a **deposit** — money taken before anything was
    /// billed, against a booking rather than an invoice.
    async fn start_deposit(
        &self,
        id: &str,
        provider: &str,
        against: &str,
        net: Money,
        amount: Money,
    ) {
        let mut tx = self.db.begin().await.expect("transaction");
        payments::start_in(
            &mut tx,
            &code(id),
            &Attempt {
                provider: provider.to_owned(),
                gateway_id: id.to_owned(),
                collects: payments::Collects::Advance(payments::Advance {
                    against: code(against),
                    net,
                    buyer: payments::Buyer {
                        name: "سارة".to_owned(),
                        vat_number: None,
                    },
                }),
                amount,
            },
            when(),
            &Metadata::default(),
        )
        .await
        .expect("starts");
        tx.commit().await.expect("commits");
    }

    async fn start(&self, id: &str, provider: &str, invoice: &str, amount: Money) {
        let mut tx = self.db.begin().await.expect("transaction");
        payments::start_in(
            &mut tx,
            &code(id),
            &Attempt {
                provider: provider.to_owned(),
                gateway_id: id.to_owned(),
                collects: payments::Collects::Invoice(code(invoice)),
                amount,
            },
            when(),
            &Metadata::default(),
        )
        .await
        .expect("starts");
        tx.commit().await.expect("commits");
    }

    async fn refund(
        &self,
        id: &str,
        reference: &str,
        amount: Money,
    ) -> Result<(), ExecuteError<PaymentsError>> {
        let mut tx = self.db.begin().await.expect("transaction");
        let outcome = payments::refund_in(
            &mut tx,
            &code(id),
            reference,
            amount,
            "the customer changed their mind",
            when(),
            &Metadata::default(),
        )
        .await
        .map(|_| ());
        if outcome.is_ok() {
            tx.commit().await.expect("commits");
        } else {
            tx.rollback().await.expect("rolls back");
        }
        outcome
    }

    /// The credit note on an invoice, if it has one.
    async fn credit_note_on(&self, invoice: &str) -> Option<String> {
        let mut conn = self.db.read().await.expect("connection");
        sales::invoice(&mut conn, invoice)
            .await
            .expect("reads")
            .and_then(|detail| detail.summary.credit_note)
    }

    async fn save_card(
        &self,
        id: &str,
        customer: &str,
        provider: &str,
        token: &str,
    ) -> Result<(), ExecuteError<PaymentsError>> {
        let mut tx = self.db.begin().await.expect("transaction");
        let outcome = payments::save_card_in(
            &mut tx,
            &self.sealing,
            &code(id),
            &payments::SavedCard {
                customer: code(customer),
                provider: provider.to_owned(),
                brand: "visa".to_owned(),
                last4: "4242".to_owned(),
                expiry_month: 5,
                expiry_year: 2028,
            },
            token,
            when(),
            &Metadata::default(),
        )
        .await
        .map(|_| ());
        if outcome.is_ok() {
            tx.commit().await.expect("commits");
        } else {
            tx.rollback().await.expect("rolls back");
        }
        outcome
    }

    async fn forget_card(&self, id: &str) -> Result<(), ExecuteError<PaymentsError>> {
        let mut tx = self.db.begin().await.expect("transaction");
        let outcome = payments::forget_card_in(&mut tx, &code(id), when(), &Metadata::default())
            .await
            .map(|_| ());
        if outcome.is_ok() {
            tx.commit().await.expect("commits");
        } else {
            tx.rollback().await.expect("rolls back");
        }
        outcome
    }

    /// The sealed token, straight out of the vault. **The only place any test
    /// looks at one**, which is the point.
    async fn token_of(&self, card: &str) -> Option<String> {
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::secrets::get(&mut conn, &self.sealing, &payments::token_key(&code(card)))
            .await
            .expect("reads")
            .map(|bytes| String::from_utf8(bytes).expect("utf-8"))
    }

    async fn request(&self, payment: &str, card: &str, invoice: &str, amount: Money) {
        let mut tx = self.db.begin().await.expect("transaction");
        payments::request_in(
            &mut tx,
            &code(payment),
            &payments::Collection {
                card: code(card),
                provider: "moyasar".to_owned(),
                collects: payments::Collects::Invoice(code(invoice)),
                amount,
                callback_url: "https://bassat.sa/paid".to_owned(),
            },
            when(),
            &Metadata::default(),
        )
        .await
        .expect("records the request");
        tx.commit().await.expect("commits");
    }

    async fn charge_pass(&self, gateway: &dyn Gateway) -> payments::Attempted {
        payments::charge_requested(
            &self.db,
            gateway,
            &self.sealing,
            when(),
            25,
            &Metadata::default(),
        )
        .await
        .expect("the charge pass runs")
    }

    async fn stage_of(&self, payment: &str) -> String {
        let mut conn = self.db.read().await.expect("connection");
        payments::payment(&mut conn, payment)
            .await
            .expect("reads")
            .map_or_else(|| "missing".to_owned(), |row| row.stage)
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
        "the customer changed their mind",
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
    // **Nothing is owed.** The refund puts the receivable back and the credit
    // note the refund issues takes the sale away, which is the point of it: a
    // customer who has had their money back does not also owe for the invoice.
    assert_eq!(fixture.balance("1100").await, money(0), "nothing is owed");
    assert_eq!(fixture.balance("4000").await, money(0), "no sale stands");
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
        "the customer changed their mind",
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

// ---------------------------------------------------------------------------
// The sweep — what actually settles a payment in production
// ---------------------------------------------------------------------------

/// A gateway that answers from a script, and counts what it was asked.
#[derive(Debug)]
struct FakeGateway {
    provider: &'static str,
    answers: std::sync::Mutex<std::collections::HashMap<String, Result<Charged, GatewayError>>>,
    asked: std::sync::atomic::AtomicUsize,
    /// What `charge` answers, and what it was sent — `(reference, token,
    /// amount, callback)`, which is everything a saved-card charge has to get
    /// right.
    charging: std::sync::Mutex<Option<Result<Charged, GatewayError>>>,
    charges: std::sync::Mutex<Vec<(String, String, Money, String)>>,
}

impl FakeGateway {
    fn new(provider: &'static str) -> Self {
        Self {
            provider,
            answers: std::sync::Mutex::new(std::collections::HashMap::new()),
            asked: std::sync::atomic::AtomicUsize::new(0),
            charging: std::sync::Mutex::new(None),
            charges: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn charging(self, answer: Result<Charged, GatewayError>) -> Self {
        *self.charging.lock().expect("not poisoned") = Some(answer);
        self
    }

    fn charged(&self) -> Vec<(String, String, Money, String)> {
        self.charges.lock().expect("not poisoned").clone()
    }

    fn saying(self, id: &str, answer: Result<Charged, GatewayError>) -> Self {
        self.answers
            .lock()
            .expect("not poisoned")
            .insert(id.to_owned(), answer);
        self
    }

    fn asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Gateway for FakeGateway {
    fn provider(&self) -> &'static str {
        self.provider
    }

    async fn fetch(&self, id: &str) -> Result<Charged, GatewayError> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.answers
            .lock()
            .expect("not poisoned")
            .get(id)
            .cloned()
            .unwrap_or_else(|| Err(GatewayError::NoSuchPayment(id.to_owned())))
    }

    async fn charge(&self, charge: &erp_payments::Charge) -> Result<Charged, GatewayError> {
        let token = match &charge.source {
            erp_payments::Source::Token { token } => token.clone(),
            erp_payments::Source::Hosted => String::new(),
        };
        self.charges.lock().expect("not poisoned").push((
            charge.reference.clone(),
            token,
            charge.amount,
            charge.returns.success.clone(),
        ));
        self.charging
            .lock()
            .expect("not poisoned")
            .clone()
            .expect("this gateway was not expecting to be charged")
    }
    async fn capture(&self, _id: &str, _amount: Option<Money>) -> Result<Charged, GatewayError> {
        unreachable!("the sweep never captures")
    }
    async fn refund(&self, _id: &str, _amount: Option<Money>) -> Result<Charged, GatewayError> {
        unreachable!("the sweep never refunds")
    }
    async fn void(&self, _id: &str) -> Result<Charged, GatewayError> {
        unreachable!("the sweep never voids")
    }
}

/// **The loop closing.** A payment started, a sweep, and money in the books —
/// with no callback anywhere in it, which is the point: Moyasar drops a webhook
/// after six attempts and Tamara documents no retry policy at all.
#[tokio::test]
async fn the_sweep_settles_a_payment_the_gateway_says_was_paid() {
    let fixture = Fixture::new("sweep").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    let gateway = FakeGateway::new("moyasar").saying(
        "pay_1",
        Ok(charged(
            "pay_1",
            Status::Paid,
            riyals(115),
            Some(money(316)),
        )),
    );

    let swept = payments::settle_pending(&fixture.db, &gateway, when(), 25, &Metadata::default())
        .await
        .expect("sweeps");
    assert_eq!(swept.resolved, 1);
    assert_eq!(swept.still_pending, 0);
    assert_eq!(swept.stopped, None);

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0));
    assert_eq!(fixture.balance("1150").await, money(11_500 - 316));
    assert_eq!(fixture.balance("5400").await, money(316));
}

/// **A payment still waiting is asked about and left alone.** The next tick
/// asks again, which is what makes this work when a callback never arrives.
#[tokio::test]
async fn the_sweep_leaves_a_payment_the_customer_has_not_finished() {
    let fixture = Fixture::new("sweeppending").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    let gateway = FakeGateway::new("moyasar").saying(
        "pay_1",
        Ok(charged("pay_1", Status::Initiated, riyals(115), None)),
    );

    let swept = payments::settle_pending(&fixture.db, &gateway, when(), 25, &Metadata::default())
        .await
        .expect("sweeps");
    assert_eq!(swept.resolved, 0);
    assert_eq!(swept.still_pending, 1);

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, riyals(115));

    // Asked again next tick, because it is still pending.
    payments::settle_pending(&fixture.db, &gateway, when(), 25, &Metadata::default())
        .await
        .expect("sweeps");
    assert_eq!(gateway.asked(), 2);
}

/// **An unreachable gateway stops the sweep** (L6). The payments stay pending
/// and the next tick tries again; marking them anything else would be inventing
/// a fact about somebody's money.
#[tokio::test]
async fn an_unreachable_gateway_stops_the_sweep_and_settles_nothing() {
    let fixture = Fixture::new("sweepdown").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    let gateway = FakeGateway::new("moyasar").saying(
        "pay_1",
        Err(GatewayError::Unreachable("connection refused".to_owned())),
    );

    let swept = payments::settle_pending(&fixture.db, &gateway, when(), 25, &Metadata::default())
        .await
        .expect("sweeps");
    assert_eq!(swept.resolved, 0);
    assert!(swept.stopped.is_some(), "it should say why it stopped");

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, riyals(115), "still owed");
    assert_eq!(fixture.balance("1150").await, money(0));
}

/// **A sweep only asks about its own provider's payments.** Asking Moyasar
/// about a Tabby id would be a `NoSuchPayment` on every tick, for ever.
#[tokio::test]
async fn a_sweep_does_not_ask_one_gateway_about_anothers_payments() {
    let fixture = Fixture::new("sweepmix").await;
    fixture.invoice("INV-1").await;
    fixture.invoice("INV-2").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture.start("tab_1", "tabby", "INV-2", riyals(115)).await;
    fixture.project().await;

    let moyasar = FakeGateway::new("moyasar").saying(
        "pay_1",
        Ok(charged("pay_1", Status::Paid, riyals(115), None)),
    );
    payments::settle_pending(&fixture.db, &moyasar, when(), 25, &Metadata::default())
        .await
        .expect("sweeps");

    assert_eq!(moyasar.asked(), 1, "only its own");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let tabby = payments::payment(&mut conn, "tab_1")
        .await
        .expect("reads")
        .expect("a payment");
    assert_eq!(tabby.stage, "pending", "untouched by Moyasar's sweep");
}

/// A gateway reporting a different amount is refused, and **the rest of the
/// batch still settles**. One bad answer must not strand every payment behind
/// it.
#[tokio::test]
async fn one_payment_that_will_not_settle_does_not_strand_the_batch() {
    let fixture = Fixture::new("sweepbad").await;
    fixture.invoice("INV-1").await;
    fixture.invoice("INV-2").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .start("pay_2", "moyasar", "INV-2", riyals(115))
        .await;
    fixture.project().await;

    let gateway = FakeGateway::new("moyasar")
        // Reports one riyal against a payment started for a hundred and fifteen.
        .saying("pay_1", Ok(charged("pay_1", Status::Paid, riyals(1), None)))
        .saying(
            "pay_2",
            Ok(charged("pay_2", Status::Paid, riyals(115), None)),
        );

    let swept = payments::settle_pending(&fixture.db, &gateway, when(), 25, &Metadata::default())
        .await
        .expect("sweeps");
    assert_eq!(swept.resolved, 1, "the good one settled");
    assert_eq!(swept.still_pending, 1, "the bad one did not");
    assert_eq!(swept.stopped, None, "and it did not stop the sweep");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    assert_eq!(
        payments::payment(&mut conn, "pay_1")
            .await
            .expect("reads")
            .expect("a payment")
            .stage,
        "pending"
    );
    assert_eq!(
        payments::payment(&mut conn, "pay_2")
            .await
            .expect("reads")
            .expect("a payment")
            .stage,
        "settled"
    );
}

// ---------------------------------------------------------------------------
// Settlement — what the gateway sent, against what it owed
// ---------------------------------------------------------------------------

impl Fixture {
    /// A settled payment, ready to be paid over.
    async fn settled(&self, id: &str, provider: &str, invoice: &str, fee: Option<Money>) {
        self.invoice(invoice).await;
        self.start(id, provider, invoice, riyals(115)).await;
        self.settle(id, &charged(id, Status::Paid, riyals(115), fee))
            .await
            .expect("settles");
    }

    async fn payout(
        &self,
        reference: &str,
        provider: &str,
        amount: Money,
        covers: &[&str],
    ) -> Result<(), ExecuteError<PaymentsError>> {
        let mut tx = self.db.begin().await.expect("transaction");
        let outcome = payments::record_payout_in(
            &mut tx,
            &code(reference),
            &payments::Transfer {
                reference: reference.to_owned(),
                provider: provider.to_owned(),
                amount,
                into: code("1010"),
                covers: covers.iter().map(|s| (*s).to_owned()).collect(),
            },
            when(),
            &Metadata::default(),
        )
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

/// **The reconciliation this module exists to make possible.** Two payments,
/// one transfer, and the clearing account back to zero.
#[tokio::test]
async fn a_payout_that_adds_up_clears_what_the_gateway_was_holding() {
    let fixture = Fixture::new("payout").await;
    fixture
        .settled("pay_1", "moyasar", "INV-1", Some(money(316)))
        .await;
    fixture
        .settled("pay_2", "moyasar", "INV-2", Some(money(316)))
        .await;
    fixture.project().await;

    // Two payments of 115.00 less 3.16 of fees each.
    let net = money(2 * (11_500 - 316));
    assert_eq!(fixture.balance("1150").await, net);

    fixture
        .payout("po_1", "moyasar", net, &["pay_1", "pay_2"])
        .await
        .expect("records");
    fixture.project().await;

    assert_eq!(fixture.balance("1150").await, money(0), "nothing left held");
    assert_eq!(fixture.balance("1010").await, net, "and it is in the bank");
    assert_eq!(
        fixture.balance("5420").await,
        money(0),
        "nothing to explain"
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    let recorded = payments::payouts(&mut conn, 10).await.expect("reads");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].difference, money(0));
    assert_eq!(recorded[0].covered, 2);
    // And nothing is awaiting a payout any more.
    assert!(
        payments::awaiting_payout(&mut conn)
            .await
            .expect("reads")
            .is_empty()
    );
}

/// **A short payout is booked, not refused.** The same call `pos` makes about a
/// till that counts short: a payout that cannot be recorded leaves the books
/// saying the gateway still holds money it has already sent.
#[tokio::test]
async fn a_gateway_that_pays_short_books_the_difference_and_still_clears() {
    let fixture = Fixture::new("payoutshort").await;
    fixture
        .settled("pay_1", "moyasar", "INV-1", Some(money(316)))
        .await;
    fixture.project().await;

    let expected = money(11_500 - 316);
    // A chargeback the settlement report explains and the payment did not.
    let arrived = money(expected.minor() - 5_000);

    fixture
        .payout("po_1", "moyasar", arrived, &["pay_1"])
        .await
        .expect("records");
    fixture.project().await;

    assert_eq!(fixture.balance("1010").await, arrived, "what arrived");
    assert_eq!(
        fixture.balance("1150").await,
        money(0),
        "the whole claim came off"
    );
    assert_eq!(
        fixture.balance("5420").await,
        money(5_000),
        "and the shortfall"
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    let recorded = payments::payouts(&mut conn, 10).await.expect("reads");
    assert_eq!(recorded[0].difference, money(-5_000), "negative is short");

    let balance = trial_balance(&mut conn).await.expect("reads");
    assert!(
        balance.iter().all(ledger::TrialBalance::balances),
        "the books do not balance: {balance:?}"
    );
}

/// Somebody typing from a bank statement has an amount and no transaction list.
/// That posts and **reconciles nothing**, and the read model says so rather
/// than reporting a payout that agreed.
#[tokio::test]
async fn a_payout_with_no_list_posts_and_reconciles_nothing() {
    let fixture = Fixture::new("payoutbare").await;
    fixture
        .settled("pay_1", "moyasar", "INV-1", Some(money(316)))
        .await;
    fixture.project().await;

    fixture
        .payout("po_1", "moyasar", money(11_184), &[])
        .await
        .expect("records");
    fixture.project().await;

    assert_eq!(fixture.balance("1010").await, money(11_184));
    assert_eq!(
        fixture.balance("5420").await,
        money(0),
        "nothing to disagree with"
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    let recorded = payments::payouts(&mut conn, 10).await.expect("reads");
    assert_eq!(recorded[0].covered, 0, "it reconciled nothing");
    assert_eq!(recorded[0].difference, money(0));

    // **And the payment is still awaiting one**, because nothing said it was
    // covered. That is the honest answer: the clearing account and the payout
    // now disagree, and this is where somebody sees it.
    let awaiting = payments::awaiting_payout(&mut conn).await.expect("reads");
    assert_eq!(awaiting.len(), 1);
    assert_eq!(awaiting[0].held, money(11_184));
}

/// **Refused, not skipped.** A payout naming a payment this system never
/// settled would reconcile against a smaller set than the operator thinks, and
/// the missing amount would look like the gateway paying short.
#[tokio::test]
async fn a_payout_naming_a_payment_that_never_settled_is_refused() {
    let fixture = Fixture::new("payoutghost").await;
    fixture
        .settled("pay_1", "moyasar", "INV-1", Some(money(316)))
        .await;
    fixture.project().await;

    let refused = fixture
        .payout("po_1", "moyasar", money(11_184), &["pay_1", "pay_ghost"])
        .await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(PaymentsError::NotSettled(_)))
        ),
        "{refused:?}"
    );

    fixture.project().await;
    assert_eq!(fixture.balance("1010").await, money(0), "nothing posted");
    assert_eq!(fixture.balance("1150").await, money(11_184), "still held");
}

/// A settlement report imported twice records one payout.
#[tokio::test]
async fn the_same_payout_recorded_twice_posts_once() {
    let fixture = Fixture::new("payouttwice").await;
    fixture
        .settled("pay_1", "moyasar", "INV-1", Some(money(316)))
        .await;
    fixture.project().await;

    for _ in 0..3 {
        fixture
            .payout("po_1", "moyasar", money(11_184), &["pay_1"])
            .await
            .expect("records");
    }
    fixture.project().await;

    assert_eq!(
        fixture.balance("1010").await,
        money(11_184),
        "once, not thrice"
    );
    assert_eq!(fixture.balance("1150").await, money(0));

    let mut conn = fixture.db.acquire().await.expect("connection");
    assert_eq!(
        payments::payouts(&mut conn, 10).await.expect("reads").len(),
        1
    );
}

/// **What the gateway still owes is per provider.** Tabby's balance is not
/// Moyasar's, and a payout from one must not clear the other's.
#[tokio::test]
async fn what_is_awaiting_a_payout_is_counted_per_provider() {
    let fixture = Fixture::new("payoutmix").await;
    fixture
        .settled("pay_1", "moyasar", "INV-1", Some(money(316)))
        .await;
    fixture
        .settled("tab_1", "tabby", "INV-2", Some(riyals(7)))
        .await;
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let awaiting = payments::awaiting_payout(&mut conn).await.expect("reads");
    drop(conn);
    assert_eq!(awaiting.len(), 2);
    let held = |provider: &str| {
        awaiting
            .iter()
            .find(|a| a.provider == provider)
            .map(|a| a.held)
    };
    assert_eq!(held("moyasar"), Some(money(11_500 - 316)));
    assert_eq!(held("tabby"), Some(money(11_500 - 700)));

    // Moyasar pays over. Tabby still owes.
    fixture
        .payout("po_1", "moyasar", money(11_184), &["pay_1"])
        .await
        .expect("records");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let awaiting = payments::awaiting_payout(&mut conn).await.expect("reads");
    assert_eq!(awaiting.len(), 1);
    assert_eq!(awaiting[0].provider, "tabby");
    assert_eq!(awaiting[0].held, money(11_500 - 700));

    // And the two clearing accounts moved independently.
    drop(conn);
    assert_eq!(fixture.balance("1150").await, money(0));
    assert_eq!(fixture.balance("1160").await, money(11_500 - 700));
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

// ---------------------------------------------------------------------------
// Saved cards
// ---------------------------------------------------------------------------

/// **The loop closing, with nobody watching.** A card saved at a previous
/// visit, a charge asked for, the worker sending it, and the money in the
/// books — no browser, no customer, no callback anywhere in it. That is the
/// whole reason a saved card exists.
#[tokio::test]
async fn a_saved_card_is_charged_by_the_worker_and_settles() {
    let fixture = Fixture::new("saved-card").await;
    fixture.invoice("INV-1").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    fixture
        .request("pay_1", "card-1", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    assert_eq!(fixture.stage_of("pay_1").await, "requested");

    let gateway = FakeGateway::new("moyasar").charging(Ok(charged(
        "pay_1",
        Status::Paid,
        riyals(115),
        Some(money(316)),
    )));
    let attempted = fixture.charge_pass(&gateway).await;
    assert_eq!(attempted.started, 1);
    assert_eq!(attempted.refused, 0);
    assert_eq!(attempted.stopped, None);

    // **The token went to the gateway and the payment's own id came with it.**
    // The second is Moyasar's `given_id`, which is what makes a retried charge
    // land on the same payment instead of on a second one.
    let sent = gateway.charged();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "pay_1");
    assert_eq!(sent[0].1, "token_abc");
    assert_eq!(sent[0].2, riyals(115));
    assert_eq!(sent[0].3, "https://bassat.sa/paid");

    fixture.project().await;
    assert_eq!(fixture.stage_of("pay_1").await, "pending");

    // The settle pass, which is what actually records money — the charge pass
    // never does, however good the answer it got.
    let gateway = FakeGateway::new("moyasar").saying(
        "pay_1",
        Ok(charged(
            "pay_1",
            Status::Paid,
            riyals(115),
            Some(money(316)),
        )),
    );
    let swept = payments::settle_pending(&fixture.db, &gateway, when(), 25, &Metadata::default())
        .await
        .expect("sweeps");
    assert_eq!(swept.resolved, 1);

    fixture.project().await;
    assert_eq!(fixture.stage_of("pay_1").await, "settled");
    assert_eq!(fixture.balance("1150").await, money(11_184));
    assert_eq!(fixture.balance("5400").await, money(316));
}

/// **A pass that died between charging and recording must not charge again.**
/// The gateway already knows the payment, so it is picked up rather than sent
/// a second time — which is the whole reason the pass asks first.
#[tokio::test]
async fn a_charge_that_already_happened_is_not_sent_twice() {
    let fixture = Fixture::new("charged-once").await;
    fixture.invoice("INV-1").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    fixture
        .request("pay_1", "card-1", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    // The gateway has it already, and would panic if asked to charge.
    let gateway = FakeGateway::new("moyasar").saying(
        "pay_1",
        Ok(charged("pay_1", Status::Paid, riyals(115), None)),
    );

    let attempted = fixture.charge_pass(&gateway).await;
    assert_eq!(attempted.started, 1);
    assert!(gateway.charged().is_empty(), "it charged a second time");

    fixture.project().await;
    assert_eq!(fixture.stage_of("pay_1").await, "pending");
}

/// **"Forget my card" has to mean it**, and the token is the thing that means
/// anything. A charge already in the queue when the customer asks fails with a
/// reason rather than sitting there for ever.
#[tokio::test]
async fn a_card_removed_before_the_worker_gets_to_it_charges_nothing() {
    let fixture = Fixture::new("forgotten-card").await;
    fixture.invoice("INV-1").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    fixture
        .request("pay_1", "card-1", "INV-1", riyals(115))
        .await;
    fixture.forget_card("card-1").await.expect("forgets");
    fixture.project().await;

    assert_eq!(fixture.token_of("card-1").await, None, "the token survived");

    let gateway = FakeGateway::new("moyasar");
    let attempted = fixture.charge_pass(&gateway).await;
    assert_eq!(attempted.refused, 1);
    assert_eq!(attempted.started, 0);
    assert!(gateway.charged().is_empty());

    fixture.project().await;
    assert_eq!(fixture.stage_of("pay_1").await, "failed");
    assert_eq!(fixture.balance("1150").await, money(0));
}

/// The row stays and says the card was removed; **what goes is the token.**
/// That a customer had a card on file and asked for it to go is history
/// somebody may have to answer for.
#[tokio::test]
async fn forgetting_a_card_deletes_the_token_and_keeps_the_history() {
    let fixture = Fixture::new("forget-keeps").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    assert_eq!(
        fixture.token_of("card-1").await,
        Some("token_abc".to_owned())
    );

    fixture.forget_card("card-1").await.expect("forgets");
    fixture.project().await;

    assert_eq!(fixture.token_of("card-1").await, None);

    let mut conn = fixture.db.read().await.expect("connection");
    let row = payments::card(&mut conn, "card-1")
        .await
        .expect("reads")
        .expect("the row is still there");
    assert!(row.forgotten);
    assert_eq!(row.last4, "4242");

    // And it is not offered any more.
    let offered = payments::cards(&mut conn, "CUST-1", 10)
        .await
        .expect("reads");
    assert!(offered.is_empty(), "a removed card was still offered");
}

/// **Forgetting is final.** Saving again means the customer entering their card
/// afresh, which mints a new token under a new id; reviving this one would
/// charge a token they asked to be rid of.
#[tokio::test]
async fn a_forgotten_card_cannot_be_revived() {
    let fixture = Fixture::new("no-revival").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    fixture.forget_card("card-1").await.expect("forgets");

    let again = fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_new")
        .await;
    assert!(
        matches!(
            again,
            Err(ExecuteError::Rejected(PaymentsError::CardForgotten(_)))
        ),
        "{again:?}"
    );
    assert_eq!(fixture.token_of("card-1").await, None);
}

/// **Nothing in the read model can charge a card.** The whole design rests on
/// the token living in the vault and nowhere else, and a column added later
/// would break it in a way no other test would notice.
#[tokio::test]
async fn the_read_model_holds_nothing_that_could_charge_a_card() {
    let fixture = Fixture::new("no-token-column").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    fixture.project().await;

    let pool = fixture.tenant_pool().await;
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::TEXT FROM information_schema.columns
          WHERE table_schema = 'proj_payments' AND table_name = 'card'",
    )
    .fetch_all(&pool)
    .await
    .expect("reads the schema");
    assert!(!columns.is_empty(), "the card table was not found");
    assert!(
        !columns.iter().any(|c| c.contains("token")),
        "there is a token column in the read model: {columns:?}"
    );

    // And no column holds the value either, whatever it is called.
    let row: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(card)::TEXT FROM proj_payments.card WHERE id = 'card-1'",
    )
    .fetch_all(&pool)
    .await
    .expect("reads the row");
    assert_eq!(row.len(), 1);
    assert!(!row[0].contains("token_abc"), "{}", row[0]);
    pool.close().await;
}

/// **Buy-now-pay-later has no card to keep.** The provider lends to the
/// customer and collects from them; there is no token on this side. A row
/// naming one would be a saved card as far as anybody picking from a list is
/// concerned, and they would find out at the till.
#[tokio::test]
async fn a_provider_that_keeps_no_cards_is_refused() {
    let fixture = Fixture::new("no-bnpl-cards").await;
    for provider in ["tabby", "tamara"] {
        let outcome = fixture
            .save_card("card-1", "CUST-1", provider, "token_abc")
            .await;
        assert!(
            matches!(
                outcome,
                Err(ExecuteError::Rejected(PaymentsError::NoSavedCards(_)))
            ),
            "{provider}: {outcome:?}"
        );
    }
    assert_eq!(fixture.token_of("card-1").await, None);
}

/// A retry saves one card and asks for one charge. **The second matters more**:
/// the alternative is charging somebody twice.
#[tokio::test]
async fn a_retried_save_and_a_retried_request_happen_once() {
    let fixture = Fixture::new("retries").await;
    fixture.invoice("INV-1").await;

    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("is a retry");

    fixture
        .request("pay_1", "card-1", "INV-1", riyals(115))
        .await;
    fixture
        .request("pay_1", "card-1", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    let gateway =
        FakeGateway::new("moyasar").charging(Ok(charged("pay_1", Status::Paid, riyals(115), None)));
    let attempted = fixture.charge_pass(&gateway).await;
    assert_eq!(attempted.started, 1);
    assert_eq!(gateway.charged().len(), 1, "the customer was charged twice");
}

/// A gateway that refuses the charge itself — a dead token, a declined card.
/// **Recorded as failed with the reason**, because a charge nobody can collect
/// must not sit in a queue looking like work.
#[tokio::test]
async fn a_refused_charge_is_recorded_and_not_retried_for_ever() {
    let fixture = Fixture::new("refused-charge").await;
    fixture.invoice("INV-1").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_dead")
        .await
        .expect("saves");
    fixture
        .request("pay_1", "card-1", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    let gateway = FakeGateway::new("moyasar").charging(Err(GatewayError::Refused(
        "the token is invalid".to_owned(),
    )));
    let attempted = fixture.charge_pass(&gateway).await;
    assert_eq!(attempted.refused, 1);

    fixture.project().await;
    assert_eq!(fixture.stage_of("pay_1").await, "failed");

    let mut conn = fixture.db.read().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(row.failed_why.as_deref(), Some("the token is invalid"));

    // And a second pass finds nothing to do, rather than charging again.
    drop(conn);
    let attempted = fixture.charge_pass(&gateway).await;
    assert_eq!(attempted.refused, 0);
    assert_eq!(attempted.started, 0);
}

/// **An unreachable gateway is not a fact about anybody's card** (L6). The pass
/// stops and says so; the charges stay requested and the next tick tries again.
#[tokio::test]
async fn an_unreachable_gateway_stops_the_pass_rather_than_failing_the_charges() {
    let fixture = Fixture::new("unreachable-charge").await;
    fixture.invoice("INV-1").await;
    fixture
        .save_card("card-1", "CUST-1", "moyasar", "token_abc")
        .await
        .expect("saves");
    fixture
        .request("pay_1", "card-1", "INV-1", riyals(115))
        .await;
    fixture.project().await;

    let gateway = FakeGateway::new("moyasar")
        .charging(Err(GatewayError::Unreachable("timed out".to_owned())));
    let attempted = fixture.charge_pass(&gateway).await;
    assert_eq!(attempted.started, 0);
    assert_eq!(attempted.refused, 0);
    assert!(attempted.stopped.is_some());

    fixture.project().await;
    assert_eq!(
        fixture.stage_of("pay_1").await,
        "requested",
        "a timeout became a fact about the payment"
    );
}

/// The card is display, and the display has to be a card. A month of 13 or
/// five "last four" digits is a row somebody has to explain later.
#[tokio::test]
async fn what_cannot_be_a_card_is_refused_at_the_boundary() {
    let fixture = Fixture::new("card-shape").await;

    let bad = [
        ("", "12345", 5, 2028),      // five digits
        ("token", "abcd", 5, 2028),  // not digits
        ("token", "4242", 13, 2028), // not a month
        ("token", "4242", 0, 2028),
        ("token", "4242", 5, 28), // not a year
    ];
    for (token, last4, month, year) in bad {
        let mut tx = fixture.db.begin().await.expect("transaction");
        let outcome = payments::save_card_in(
            &mut tx,
            &fixture.sealing,
            &code("card-x"),
            &payments::SavedCard {
                customer: code("CUST-1"),
                provider: "moyasar".to_owned(),
                brand: "visa".to_owned(),
                last4: last4.to_owned(),
                expiry_month: month,
                expiry_year: year,
            },
            token,
            when(),
            &Metadata::default(),
        )
        .await;
        tx.rollback().await.expect("rolls back");
        assert!(
            matches!(
                outcome,
                Err(ExecuteError::Rejected(
                    PaymentsError::NotACard(_) | PaymentsError::NoSavedCards(_)
                ))
            ),
            "{last4}/{month}/{year} was accepted"
        );
    }
}

// ---------------------------------------------------------------------------
// The document a refund owes
// ---------------------------------------------------------------------------

/// **A refund is not just money.** A tax invoice is a statement about a supply
/// and giving the money back changes the supply, so the Kingdom wants a credit
/// note — its own number, its own tax point, its own document. Refunding a
/// gateway payment used to move the money and leave the invoice saying it was
/// for the full amount.
#[tokio::test]
async fn a_full_refund_issues_the_credit_note_zatca_requires() {
    let fixture = Fixture::new("credit-note").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");
    fixture.project().await;
    assert_eq!(fixture.credit_note_on("INV-1").await, None);

    fixture
        .refund("pay_1", "refund-1", riyals(115))
        .await
        .expect("refunds");
    fixture.project().await;

    // A number from the tenant's own gapless series, not the caller's key.
    let credit_note = fixture
        .credit_note_on("INV-1")
        .await
        .expect("a credit note was issued");
    assert_ne!(
        credit_note, "refund-1",
        "the caller's key became the number"
    );
    assert!(!credit_note.is_empty());

    // And the sale is undone in the books as well as on paper: the receivable
    // is back and the clearing account has given the money up.
    assert_eq!(fixture.balance("1100").await, money(0), "nothing is owed");
    assert_eq!(fixture.balance("4000").await, money(0), "no sale stands");
    assert_eq!(fixture.balance("1150").await, money(0));
}

/// **A partial refund gets no credit note, deliberately.** One for part of an
/// invoice carries bands of its own, and how an arbitrary amount divides across
/// a standard-rated line and a zero-rated one is not something this system may
/// guess. What it must not do is look like it succeeded — the money moves, the
/// document does not, and the invoice still holds the rest.
#[tokio::test]
async fn a_partial_refund_moves_money_and_issues_no_document() {
    let fixture = Fixture::new("part-refund").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");

    fixture
        .refund("pay_1", "refund-1", riyals(40))
        .await
        .expect("refunds part");
    fixture.project().await;

    assert_eq!(
        fixture.credit_note_on("INV-1").await,
        None,
        "a partial refund invented a whole-invoice credit note"
    );
    // The money did move, and the invoice is holding what is left.
    assert_eq!(fixture.balance("1150").await, riyals(75));

    // And finishing the refund does issue one.
    fixture
        .refund("pay_1", "refund-2", riyals(75))
        .await
        .expect("refunds the rest");
    fixture.project().await;
    assert!(
        fixture.credit_note_on("INV-1").await.is_some(),
        "the refund that cleared the invoice issued nothing"
    );
}

/// A retried refund gives the money back once **and issues one document**. The
/// credit note is keyed on the caller's reference, so the second attempt finds
/// the cancellation that already happened rather than burning a number.
#[tokio::test]
async fn a_retried_refund_issues_one_credit_note() {
    let fixture = Fixture::new("credit-retry").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");

    fixture
        .refund("pay_1", "refund-1", riyals(115))
        .await
        .expect("refunds");
    fixture.project().await;
    let first = fixture.credit_note_on("INV-1").await.expect("issued");

    fixture
        .refund("pay_1", "refund-1", riyals(115))
        .await
        .expect("is a retry");
    fixture.project().await;

    assert_eq!(
        fixture.credit_note_on("INV-1").await,
        Some(first),
        "a retry issued a second credit note"
    );
    assert_eq!(fixture.balance("1100").await, money(0));
}

/// Two payments against one invoice: a deposit and the balance. **Refunding the
/// deposit alone credits nothing**, because the invoice is still holding the
/// rest — and that is `sales`' answer, not a guess made here.
#[tokio::test]
async fn refunding_one_of_two_payments_credits_nothing_until_both_are_back() {
    let fixture = Fixture::new("two-payments").await;
    fixture.invoice("INV-1").await;
    for (id, amount) in [("pay_1", riyals(40)), ("pay_2", riyals(75))] {
        fixture.start(id, "moyasar", "INV-1", amount).await;
        fixture
            .settle(id, &charged(id, Status::Paid, amount, None))
            .await
            .expect("settles");
    }

    fixture
        .refund("pay_1", "refund-1", riyals(40))
        .await
        .expect("refunds the deposit");
    fixture.project().await;
    assert_eq!(
        fixture.credit_note_on("INV-1").await,
        None,
        "the invoice was credited while it still held the balance"
    );

    fixture
        .refund("pay_2", "refund-2", riyals(75))
        .await
        .expect("refunds the balance");
    fixture.project().await;
    assert!(fixture.credit_note_on("INV-1").await.is_some());
    assert_eq!(fixture.balance("1100").await, money(0));
}

// ---------------------------------------------------------------------------
// Money taken before there is anything to bill
// ---------------------------------------------------------------------------

/// **A deposit becomes a document the moment the money is real.** Receiving
/// consideration is itself a tax point, so the prepayment invoice is raised at
/// settlement and the VAT is declared in the period the customer paid — not in
/// whatever quarter they eventually turn up, or fail to.
#[tokio::test]
async fn a_settled_deposit_is_billed_and_its_tax_declared() {
    let fixture = Fixture::new("deposit").await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", riyals(100), riyals(115))
        .await;
    fixture
        .settle(
            "pay_1",
            &charged("pay_1", Status::Paid, riyals(115), Some(money(316))),
        )
        .await
        .expect("settles");
    fixture.project().await;

    assert_eq!(fixture.balance("1150").await, money(11_184), "less the fee");
    assert_eq!(fixture.balance("2100").await, riyals(-15), "tax declared");
    assert_eq!(fixture.balance("4000").await, riyals(-100), "the supply");
    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "the deposit paid it"
    );
    assert_eq!(fixture.balance("5400").await, money(316), "the fee");

    // **And it is on the return, in the period the money arrived.**
    let mut conn = fixture.db.acquire().await.expect("connection");
    let filed = sales::vat_return(
        &mut conn,
        sar(),
        chrono::DateTime::from_timestamp(0, 0).expect("valid"),
        chrono::Utc::now(),
    )
    .await
    .expect("reads");
    assert_eq!(filed.tax, riyals(15));

    // The document is a prepayment invoice, keyed off the payment's own id.
    let invoice = sales::invoice(&mut conn, "dep-pay_1")
        .await
        .expect("reads")
        .expect("a prepayment invoice was raised");
    assert_eq!(invoice.summary.gross, riyals(115));
    drop(conn);

    let mut conn = fixture.db.read().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(row.stage, "settled");
    assert_eq!(row.invoice.as_deref(), Some("dep-pay_1"));
    assert_eq!(row.advance_for.as_deref(), Some("BOOK-1"));
}

/// **A deposit given back is a credit note and the money** — the same two facts
/// as any other refund, which is the whole point of billing it in the first
/// place. The supply is undone and the tax comes back off the return.
#[tokio::test]
async fn a_returned_deposit_is_credited_like_any_other_sale() {
    let fixture = Fixture::new("deposit-back").await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", riyals(100), riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");

    fixture
        .refund("pay_1", "refund-1", riyals(115))
        .await
        .expect("gives it back");
    fixture.project().await;

    assert_eq!(fixture.balance("1150").await, money(0));
    assert_eq!(fixture.balance("4000").await, money(0), "no supply stands");
    assert_eq!(fixture.balance("2100").await, money(0), "no tax owed");
    assert_eq!(fixture.balance("1100").await, money(0), "nothing owed");

    let mut conn = fixture.db.acquire().await.expect("connection");
    let invoice = sales::invoice(&mut conn, "dep-pay_1")
        .await
        .expect("reads")
        .expect("there");
    assert!(
        invoice.summary.credit_note.is_some(),
        "the deposit went back and nothing credited it"
    );
}

/// **Part of a deposit returned.** The money moves and the invoice is left
/// holding the rest — and no credit note is issued, because crediting part of
/// an invoice needs to know which band the part came out of, and only the
/// caller of a partial credit note knows that. See the note in the plan: for a
/// single-band invoice, which every deposit is, the answer is obvious and this
/// is the next thing to close.
#[tokio::test]
async fn part_of_a_deposit_can_be_returned() {
    let fixture = Fixture::new("deposit-part").await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", riyals(100), riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");

    fixture
        .refund("pay_1", "refund-1", money(5_750))
        .await
        .expect("gives half back");
    fixture.project().await;

    assert_eq!(
        fixture.balance("1150").await,
        money(5_750),
        "half went back"
    );
    // Still settled, not refunded: half the money is here.
    let mut conn = fixture.db.read().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(row.stage, "settled");
    assert_eq!(row.refunded, money(5_750));
}

/// **Exactly one target, until settlement gives a deposit both.** A payment
/// that names neither has nothing to settle against; one that names two at the
/// *start* is a caller who has not decided. After a deposit settles it names
/// its own prepayment invoice as well as the booking, which is the point of
/// raising one.
#[tokio::test]
async fn a_payment_collects_against_exactly_one_thing() {
    let deposit = payments::Advance {
        against: code("BOOK-1"),
        net: riyals(100),
        buyer: payments::Buyer {
            name: "سارة".to_owned(),
            vat_number: None,
        },
    };
    assert!(payments::Collects::of(Some(&code("INV-1")), None).is_some());
    assert!(payments::Collects::of(None, Some(&deposit)).is_some());
    assert!(payments::Collects::of(None, None).is_none());
    assert!(payments::Collects::of(Some(&code("INV-1")), Some(&deposit)).is_none());

    // A deposit's invoice is derived from the payment, so the same answer
    // comes back at settlement and at a refund months later.
    assert_eq!(
        payments::deposit_invoice(&code("pay_1")).as_str(),
        "dep-pay_1"
    );

    // And the database refuses a row that names nothing.
    let fixture = Fixture::new("one-target").await;
    let pool = fixture.tenant_pool().await;
    let refused = sqlx::query(
        "INSERT INTO proj_payments.payment
             (id, provider, gateway_id, amount_minor, currency, stage,
              started_at, position)
         VALUES ('x', 'moyasar', 'x', 100, 'SAR', 'pending', now(), 1)",
    )
    .execute(&pool)
    .await;
    assert!(refused.is_err(), "a row named nothing and was accepted");
    pool.close().await;
}

/// A deposit and an invoice payment against the same thing are both listed by
/// it: a caller asking what has been collected does not care which shape it is.
#[tokio::test]
async fn a_booking_lists_the_deposits_taken_against_it() {
    let fixture = Fixture::new("deposit-list").await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", money(4_348), riyals(50))
        .await;
    fixture
        .start_deposit("pay_2", "moyasar", "BOOK-1", money(2_174), riyals(25))
        .await;
    fixture.project().await;

    let mut conn = fixture.db.read().await.expect("connection");
    let rows = payments::against(&mut conn, "BOOK-1", 10)
        .await
        .expect("reads");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.invoice.is_none()));
}

// ---------------------------------------------------------------------------
// Keeping a deposit the customer did not come back for
// ---------------------------------------------------------------------------

impl Fixture {
    async fn set_supply(&self, supply: bool) {
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(
            &mut conn,
            payments::Retention::KEY,
            &payments::Retention { supply },
            None,
        )
        .await
        .expect("stores the policy");
    }

    async fn retain(&self, id: &str) -> Result<(), ExecuteError<PaymentsError>> {
        let mut tx = self.db.begin().await.expect("transaction");
        let outcome = payments::retain_in(&mut tx, &code(id), when(), &Metadata::default())
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

/// **The default: keeping it is a sale, and there is nothing to do.** The
/// prepayment invoice already recognised the supply and already declared the
/// tax, in the period the customer paid. Retention records that nobody is
/// getting it back and posts nothing at all.
#[tokio::test]
async fn a_kept_deposit_is_a_sale_by_default_and_posts_nothing() {
    let fixture = Fixture::new("kept-supply").await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", riyals(100), riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");
    fixture.project().await;
    let before = fixture.balance("4000").await;

    fixture.retain("pay_1").await.expect("keeps it");
    fixture.project().await;

    assert_eq!(fixture.balance("4000").await, before, "it was billed twice");
    assert_eq!(fixture.balance("2100").await, riyals(-15), "tax unchanged");
    assert_eq!(fixture.balance("4910").await, money(0), "not forfeited");

    let mut conn = fixture.db.read().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(row.stage, "retained");
}

/// **The business's call, and what it moves is the revenue.** A tenant whose
/// adviser reads a forfeited deposit as compensation rather than a service
/// books it in a line of its own — so a year later they can say how much of
/// their income was selling something.
///
/// **The tax stays declared.** Reclaiming it would be reversing a prepayment
/// the buyer never got back, which is the one thing the authority's guidance
/// says not to do.
#[tokio::test]
async fn a_business_can_decide_a_kept_deposit_is_not_a_sale() {
    let fixture = Fixture::new("kept-forfeit").await;
    fixture.set_supply(false).await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", riyals(100), riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");

    fixture.retain("pay_1").await.expect("keeps it");
    fixture.project().await;

    assert_eq!(fixture.balance("4000").await, money(0), "out of revenue");
    assert_eq!(fixture.balance("4910").await, riyals(-100), "forfeited");
    assert_eq!(fixture.balance("2100").await, riyals(-15), "tax stays");

    // And it is still on the return, because the supply was still declared.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let filed = sales::vat_return(
        &mut conn,
        sar(),
        chrono::DateTime::from_timestamp(0, 0).expect("valid"),
        chrono::Utc::now(),
    )
    .await
    .expect("reads");
    assert_eq!(filed.tax, riyals(15));
}

/// **Refund half, keep the rest** — the cancellation-policy shape, as two facts
/// rather than one number meaning both.
#[tokio::test]
async fn half_returned_and_half_kept_is_a_refund_and_a_retention() {
    let fixture = Fixture::new("kept-half").await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", riyals(200), riyals(230))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(230), None))
        .await
        .expect("settles");

    fixture
        .refund("pay_1", "refund-1", riyals(115))
        .await
        .expect("gives half back");
    fixture.retain("pay_1").await.expect("keeps the rest");
    fixture.project().await;

    assert_eq!(fixture.balance("1150").await, riyals(115), "half went back");

    let mut conn = fixture.db.read().await.expect("connection");
    let row = payments::payment(&mut conn, "pay_1")
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(row.stage, "retained");
    assert_eq!(row.refunded, riyals(115));
}

/// A deposit that has been kept **cannot then be refunded**: the money has been
/// recognised, possibly on a filed return, and handing it back afterwards would
/// be revenue that never existed.
#[tokio::test]
async fn a_kept_deposit_cannot_be_given_back() {
    let fixture = Fixture::new("kept-final").await;
    fixture
        .start_deposit("pay_1", "moyasar", "BOOK-1", riyals(100), riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");
    fixture.retain("pay_1").await.expect("keeps it");

    let refused = fixture.refund("pay_1", "refund-1", riyals(115)).await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(PaymentsError::NotCollectable { .. }))
        ),
        "{refused:?}"
    );

    // And keeping it again is a no-op.
    fixture.retain("pay_1").await.expect("is a retry");
    fixture.project().await;
    assert_eq!(fixture.balance("4000").await, riyals(-100), "billed twice");
}

/// **There is nothing to keep on an invoice payment.** The supply it paid for
/// already happened and was already invoiced; billing it again would be a
/// second sale of the same thing.
#[tokio::test]
async fn an_invoice_payment_has_no_deposit_to_keep() {
    let fixture = Fixture::new("kept-invoice").await;
    fixture.invoice("INV-1").await;
    fixture
        .start("pay_1", "moyasar", "INV-1", riyals(115))
        .await;
    fixture
        .settle("pay_1", &charged("pay_1", Status::Paid, riyals(115), None))
        .await
        .expect("settles");

    let refused = fixture.retain("pay_1").await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(PaymentsError::NotADeposit(_)))
        ),
        "{refused:?}"
    );
}
