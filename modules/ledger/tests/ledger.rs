//! The ledger, end to end against a real tenant.
//!
//! The test that carries the module is
//! [`any_sequence_of_valid_commands_leaves_the_ledger_balanced`]. Everything
//! else checks a rule; that one checks the *pipeline* — commands, events,
//! projections and the read models together — with one number that can only be
//! zero if all of them are right.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use ledger::{
    AccountKind, BalancedLines, Ledger, LedgerError, Line, account_balances, close_account,
    imbalances, open_account, post_entry, projections, rename_account, trial_balance,
};
use spa_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use spa_eventlog::{ExecuteError, Metadata};
use spa_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use spa_testkit::{Schema, TestDb};
use spa_types::{AggregateId, CurrencyCode, Money, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &spa_eventlog::MIGRATIONS);

fn sar() -> CurrencyCode {
    CurrencyCode::new("SAR").expect("valid")
}
fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("valid")
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

/// Whole riyals, so tests read in the units a person uses. A `150_00` literal
/// would be clearer still, but clippy reads that grouping as a typo.
fn riyals(major: i64) -> Money {
    money(major * 100)
}

struct Fixture {
    db: TenantDb,
    control: Arc<ControlPlane>,
    _control_db: TestDb,
    tenant_database: String,
}

impl Fixture {
    async fn new() -> Self {
        let control_db = spa_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &spa_testkit::database_url())
            .expect("the test database URL parses");

        let control = Arc::new(ControlPlane::new(
            control_db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ));
        control
            .register_cluster(
                "primary",
                "SPA_CLUSTER_PRIMARY_URL",
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
        spa_testkit::create_named_database(&tenant.database_name, &TENANT)
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

        // What `enable_module` will do in Phase 4.
        let mut conn = db.acquire().await.expect("connection");
        ledger::install(&mut conn).await.expect("module schema");
        ensure_group_schema::<Ledger>(&mut conn)
            .await
            .expect("group checkpoint");
        drop(conn);

        Self {
            db,
            control,
            _control_db: control_db,
            tenant_database: tenant.database_name,
        }
    }

    /// Drives the projections to the head of the log.
    async fn project(&self) {
        let owned = projections();
        let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
        let pool = self.tenant_pool().await;
        run_to_head::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
            .await
            .expect("projects");
        pool.close().await;
    }

    /// A pool straight at the tenant database, for the projection runner and the
    /// shadow differ — both operator tools, not request paths.
    async fn tenant_pool(&self) -> sqlx::PgPool {
        let url = spa_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgPool::connect(&format!("{base}/{}", self.tenant_database))
            .await
            .expect("connects")
    }

    async fn account(&self, code_: &str, kind: AccountKind, currency: CurrencyCode) {
        open_account(
            &self.db,
            &code(code_),
            code_,
            kind,
            currency,
            &Metadata::default(),
        )
        .await
        .expect("opens");
    }

    /// One account's balance, or zero if it has never been posted to.
    async fn balance(&self, account: &str) -> Money {
        let mut conn = self.db.acquire().await.expect("connection");
        let accounts = account_balances(&mut conn).await.expect("reads");
        accounts
            .into_iter()
            .find(|a| a.code == account)
            .map_or_else(|| money(0), |a| a.balance)
    }

    async fn imbalances(&self) -> Vec<ledger::TrialBalance> {
        let mut conn = self.db.acquire().await.expect("connection");
        imbalances(&mut conn).await.expect("reads")
    }

    async fn cleanup(self) {
        drop(self.db);
        drop(self.control);
        let _ = spa_testkit::drop_named_database(&self.tenant_database).await;
    }
}

fn rejection(error: &CommandError<LedgerError>) -> Option<&LedgerError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn opening_an_account_puts_it_in_the_chart() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let accounts = account_balances(&mut conn).await.expect("reads");
    drop(conn);

    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].code, "1000");
    assert_eq!(accounts[0].kind, AccountKind::Asset);
    assert_eq!(accounts[0].balance, Money::zero(sar()));
    assert!(!accounts[0].closed);

    fixture.cleanup().await;
}

#[tokio::test]
async fn opening_the_same_code_twice_is_refused() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;

    let again = open_account(
        &fixture.db,
        &code("1000"),
        "Cash again",
        AccountKind::Asset,
        sar(),
        &Metadata::default(),
    )
    .await
    .expect_err("must refuse");

    assert!(
        matches!(rejection(&again), Some(LedgerError::AccountExists(_))),
        "{again:?}"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn posting_moves_both_balances() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;

    let lines = BalancedLines::new(vec![
        Line::new(code("1000"), riyals(150)),
        Line::new(code("4000"), riyals(-150)),
    ])
    .expect("balances");

    post_entry(
        &fixture.db,
        &code("inv-1"),
        when(),
        "Invoice 1",
        lines,
        &Metadata::default(),
    )
    .await
    .expect("posts");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let accounts = account_balances(&mut conn).await.expect("reads");
    drop(conn);

    let cash = accounts.iter().find(|a| a.code == "1000").expect("cash");
    let sales = accounts.iter().find(|a| a.code == "4000").expect("sales");
    assert_eq!(cash.balance, riyals(150), "an asset grows by debit");
    assert_eq!(sales.balance, riyals(-150), "revenue grows by credit");

    assert!(fixture.imbalances().await.is_empty());
    fixture.cleanup().await;
}

/// Posting the same entry id twice is a no-op, so a retried request is safe.
#[tokio::test]
async fn re_posting_an_entry_changes_nothing() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;

    let lines = || {
        BalancedLines::new(vec![
            Line::new(code("1000"), riyals(100)),
            Line::new(code("4000"), riyals(-100)),
        ])
        .expect("balances")
    };

    let first = post_entry(
        &fixture.db,
        &code("inv-1"),
        when(),
        "Invoice 1",
        lines(),
        &Metadata::default(),
    )
    .await
    .expect("posts");
    let second = post_entry(
        &fixture.db,
        &code("inv-1"),
        when(),
        "Invoice 1",
        lines(),
        &Metadata::default(),
    )
    .await
    .expect("is a no-op, not an error");

    assert_eq!(first.events.len(), 1);
    assert!(second.did_nothing(), "the second post wrote nothing");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let accounts = account_balances(&mut conn).await.expect("reads");
    drop(conn);
    assert_eq!(
        accounts.iter().find(|a| a.code == "1000").unwrap().balance,
        riyals(100),
        "not 200.00 — a retried request must not post twice"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn posting_to_a_missing_or_closed_account_is_refused() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;

    let to = |account: &str| {
        BalancedLines::new(vec![
            Line::new(code("1000"), riyals(100)),
            Line::new(code(account), riyals(-100)),
        ])
        .expect("balances")
    };

    let missing = post_entry(
        &fixture.db,
        &code("e1"),
        when(),
        "",
        to("9999"),
        &Metadata::default(),
    )
    .await
    .expect_err("must refuse");
    assert!(matches!(
        rejection(&missing),
        Some(LedgerError::NoSuchAccount(_))
    ));

    close_account(&fixture.db, &code("4000"), &Metadata::default())
        .await
        .expect("closes");

    let closed = post_entry(
        &fixture.db,
        &code("e2"),
        when(),
        "",
        to("4000"),
        &Metadata::default(),
    )
    .await
    .expect_err("must refuse");
    assert!(matches!(
        rejection(&closed),
        Some(LedgerError::AccountClosed(_))
    ));

    // And neither attempt left anything behind.
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let postings: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_ledger.posting")
        .fetch_one(&mut *conn)
        .await
        .expect("counts");
    drop(conn);
    assert_eq!(postings, 0);

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_entry_cannot_mix_an_accounts_currency() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    // Opened in USD, so a SAR line into it is wrong even though the entry
    // itself balances.
    fixture.account("1010", AccountKind::Asset, usd()).await;

    let lines = BalancedLines::new(vec![
        Line::new(code("1000"), riyals(100)),
        Line::new(code("1010"), riyals(-100)),
    ])
    .expect("the entry itself balances, in SAR");

    let error = post_entry(
        &fixture.db,
        &code("e1"),
        when(),
        "",
        lines,
        &Metadata::default(),
    )
    .await
    .expect_err("must refuse");

    assert!(
        matches!(rejection(&error), Some(LedgerError::Unbalanced(_))),
        "{error:?}"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn renaming_is_idempotent_and_shows_up_in_the_chart() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;

    let changed = rename_account(
        &fixture.db,
        &code("1000"),
        "Cash at bank",
        &Metadata::default(),
    )
    .await
    .expect("renames");
    let again = rename_account(
        &fixture.db,
        &code("1000"),
        "Cash at bank",
        &Metadata::default(),
    )
    .await
    .expect("is a no-op");

    assert_eq!(changed.events.len(), 1);
    assert!(again.did_nothing());

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let accounts = account_balances(&mut conn).await.expect("reads");
    drop(conn);
    assert_eq!(accounts[0].name, "Cash at bank");

    fixture.cleanup().await;
}

/// **The invariant, under a generated command sequence.**
///
/// The point is not that any individual entry balances — `BalancedLines` makes
/// that unconstructable. It is that after an arbitrary run of commands, the
/// *stored* postings still sum to zero in every currency, which is only true if
/// the commands, the events, the projections and the read models all agree.
#[tokio::test]
async fn any_sequence_of_valid_commands_leaves_the_ledger_balanced() {
    let fixture = Fixture::new().await;

    let codes = ["1000", "1100", "2000", "4000", "5000"];
    for (i, c) in codes.iter().enumerate() {
        let kind = match i {
            0 | 1 => AccountKind::Asset,
            2 => AccountKind::Liability,
            3 => AccountKind::Revenue,
            _ => AccountKind::Expense,
        };
        fixture.account(c, kind, sar()).await;
    }

    // A deterministic pseudo-random walk: no RNG, so a failure is reproducible
    // from the test alone.
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for entry in 0..40u32 {
        let line_count = 2 + usize::try_from(next() % 3).unwrap_or(0);
        let mut lines = Vec::new();
        let mut running = 0i64;

        for line in 0..line_count - 1 {
            let account = codes[usize::try_from(next() % 5).unwrap_or(0)];
            let amount = i64::try_from(next() % 200_000).unwrap_or(1) - 100_000;
            let amount = if amount == 0 { 1 } else { amount };
            running += amount;
            lines.push(Line::new(code(account), money(amount)).with_memo(format!("line {line}")));
        }
        if running == 0 {
            continue;
        }
        // The closing line is what makes the entry legal.
        lines.push(Line::new(code(codes[4]), money(-running)));

        let Ok(balanced) = BalancedLines::new(lines) else {
            continue;
        };
        post_entry(
            &fixture.db,
            &code(&format!("e{entry}")),
            when(),
            "generated",
            balanced,
            &Metadata::default(),
        )
        .await
        .expect("posts");
    }

    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let balance = trial_balance(&mut conn).await.expect("reads");
    drop(conn);

    assert_eq!(balance.len(), 1, "one currency was used");
    let sar_side = &balance[0];
    assert!(
        sar_side.postings > 60,
        "the walk should have produced a real number of postings, got {}",
        sar_side.postings
    );
    assert_eq!(
        sar_side.difference,
        Money::zero(sar()),
        "debits {} and credits {} must agree",
        sar_side.debits,
        sar_side.credits
    );
    assert_eq!(sar_side.debits, sar_side.credits);

    fixture.cleanup().await;
}

/// The ledger's read models rebuild identically. If this fails, `replay` is not
/// something an operator can run.
#[tokio::test]
async fn the_ledger_replays_identically() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;
    rename_account(
        &fixture.db,
        &code("1000"),
        "Cash at bank",
        &Metadata::default(),
    )
    .await
    .expect("renames");

    for n in 0..5i64 {
        let lines = BalancedLines::new(vec![
            Line::new(code("1000"), money((n + 1) * 1000)),
            Line::new(code("4000"), money(-(n + 1) * 1000)),
        ])
        .expect("balances");
        post_entry(
            &fixture.db,
            &code(&format!("inv-{n}")),
            when(),
            "Invoice",
            lines,
            &Metadata::default(),
        )
        .await
        .expect("posts");
    }
    fixture.project().await;

    let owned = projections();
    let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
    let pool = fixture.tenant_pool().await;
    let report = replay_shadow::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
        .await
        .expect("replays");
    pool.close().await;

    assert!(
        report.is_reproducible(),
        "the ledger must rebuild to exactly what is live; differences: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Every literal name in the crate is a valid `EventName`, and every one is
/// declared to the upcaster registry.
#[test]
fn names_are_valid_and_declared() {
    let upcasters = ledger::upcasters();
    for name in ledger::AccountEvent::NAMES
        .iter()
        .chain(ledger::JournalEntryEvent::NAMES.iter())
    {
        let parsed = spa_types::EventName::new(*name).expect("a valid event name");
        assert!(
            upcasters.current_version(&parsed).is_some(),
            "{name} is not declared; events would be written that cannot be read"
        );
    }
    assert!(upcasters.gaps().is_empty(), "{:?}", upcasters.gaps());
}

#[test]
fn every_message_has_a_translation() {
    spa_i18n::testing::assert_complete(&ledger::CATALOG);
}

// ---------------------------------------------------------------------------
// Charts of accounts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn installing_a_chart_opens_its_accounts() {
    let fixture = Fixture::new().await;
    let services = ledger::chart("services").expect("the services chart ships");

    let installed = ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        spa_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    assert_eq!(installed.opened, services.accounts.len());
    assert_eq!(installed.skipped, 0);

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let accounts = account_balances(&mut conn).await.expect("reads");
    drop(conn);

    assert_eq!(accounts.len(), services.accounts.len());
    // Every account starts at zero, so the ledger balances before anything is
    // posted — and the invariant is checkable from the first minute.
    assert!(accounts.iter().all(|a| a.balance == Money::zero(sar())));
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

/// **The property that makes a half-finished install recoverable.**
#[tokio::test]
async fn installing_a_chart_twice_changes_nothing() {
    let fixture = Fixture::new().await;
    let services = ledger::chart("services").expect("ships");

    let first = ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        spa_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");
    let second = ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        spa_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("is a no-op, not an error");

    assert_eq!(second.opened, 0);
    assert_eq!(
        second.skipped, first.opened,
        "the retry must recognise every account it already opened"
    );

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    assert_eq!(
        account_balances(&mut conn).await.expect("reads").len(),
        services.accounts.len(),
        "installing twice must not duplicate the chart"
    );

    fixture.cleanup().await;
}

/// Charts layer: retail on top of services opens only the difference.
#[tokio::test]
async fn a_second_chart_opens_only_what_is_missing() {
    let fixture = Fixture::new().await;
    let services = ledger::chart("services").expect("ships");
    let retail = ledger::chart("retail").expect("ships");

    ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        spa_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    let added = ledger::install_chart(
        &fixture.db,
        retail,
        sar(),
        spa_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    let shared = retail
        .accounts
        .iter()
        .filter(|a| services.accounts.iter().any(|s| s.code == a.code))
        .count();
    assert_eq!(added.skipped, shared);
    assert_eq!(added.opened, retail.accounts.len() - shared);
    assert!(added.opened > 0, "retail must add something");

    fixture.cleanup().await;
}

/// Installed in Arabic, the accounts are named in Arabic.
#[tokio::test]
async fn a_chart_installs_in_the_callers_language() {
    let fixture = Fixture::new().await;
    let services = ledger::chart("services").expect("ships");

    ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        spa_i18n::Locale::Arabic,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let accounts = account_balances(&mut conn).await.expect("reads");
    drop(conn);

    let bank = accounts.iter().find(|a| a.code == "1010").expect("bank");
    assert!(
        bank.name
            .chars()
            .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
        "a Saudi bookkeeper should not have to rename eighteen accounts: {}",
        bank.name
    );

    fixture.cleanup().await;
}

/// A chart is a starting point, not a fixture: everything in it is ordinary.
#[tokio::test]
async fn an_installed_chart_is_ordinary_accounts() {
    let fixture = Fixture::new().await;
    let services = ledger::chart("services").expect("ships");

    ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        spa_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    // Rename one, close another, and post against a third.
    rename_account(
        &fixture.db,
        &code("1010"),
        "Al Rajhi current account",
        &Metadata::default(),
    )
    .await
    .expect("renames");
    close_account(&fixture.db, &code("5900"), &Metadata::default())
        .await
        .expect("closes");

    let lines = BalancedLines::new(vec![
        Line::new(code("1010"), riyals(5000)),
        Line::new(code("4000"), riyals(-5000)),
    ])
    .expect("balances");
    post_entry(
        &fixture.db,
        &code("inv-1"),
        when(),
        "First invoice",
        lines,
        &Metadata::default(),
    )
    .await
    .expect("posts");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let accounts = account_balances(&mut conn).await.expect("reads");
    drop(conn);

    let bank = accounts.iter().find(|a| a.code == "1010").expect("bank");
    assert_eq!(bank.name, "Al Rajhi current account");
    assert_eq!(bank.balance, riyals(5000));
    assert!(
        accounts
            .iter()
            .find(|a| a.code == "5900")
            .expect("other")
            .closed
    );
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Reversal
// ---------------------------------------------------------------------------

/// **The requirement.** A mistake can be corrected, and the books show both.
#[tokio::test]
async fn an_entry_posted_in_error_can_be_reversed() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;

    let lines = BalancedLines::new(vec![
        Line::new(code("1000"), riyals(500)),
        Line::new(code("4000"), riyals(-500)),
    ])
    .expect("balances");

    post_entry(
        &fixture.db,
        &code("E-1"),
        when(),
        "wrong",
        lines,
        &Metadata::default(),
    )
    .await
    .expect("posts");
    fixture.project().await;
    assert_eq!(fixture.balance("1000").await, riyals(500));

    ledger::reverse_entry(
        &fixture.db,
        &code("E-1"),
        &code("E-1R"),
        when(),
        "correcting E-1",
        &Metadata::default(),
    )
    .await
    .expect("reverses");
    fixture.project().await;

    assert_eq!(fixture.balance("1000").await, money(0), "undone");
    assert_eq!(fixture.balance("4000").await, money(0));

    // Nothing was deleted: both the mistake and the correction are on the
    // books, which is what makes them auditable.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let postings: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proj_ledger.posting WHERE entry_id IN ($1, $2)")
            .bind("E-1")
            .bind("E-1R")
            .fetch_one(&mut *conn)
            .await
            .expect("counts");
    drop(conn);
    assert_eq!(postings, 4, "two lines each, both still there");

    assert!(fixture.imbalances().await.is_empty());
    fixture.cleanup().await;
}

/// Reversing twice would swing the balance the other way, so the second attempt
/// is refused — unless it is the same request arriving again.
#[tokio::test]
async fn an_entry_cannot_be_reversed_twice() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;

    let lines = BalancedLines::new(vec![
        Line::new(code("1000"), riyals(500)),
        Line::new(code("4000"), riyals(-500)),
    ])
    .expect("balances");
    post_entry(
        &fixture.db,
        &code("E-2"),
        when(),
        "",
        lines,
        &Metadata::default(),
    )
    .await
    .expect("posts");

    reverse(&fixture, "E-2R").await.expect("reverses");

    // The same request again: a no-op, so a retry is safe.
    let retry = reverse(&fixture, "E-2R").await.expect("is not an error");
    assert!(retry.events.is_empty(), "a retry writes nothing");

    // A different one: refused, and it says what already undid it.
    let error = reverse(&fixture, "E-2R2")
        .await
        .expect_err("already reversed");
    assert!(
        matches!(
            rejection(&error),
            Some(LedgerError::AlreadyReversed { by, .. }) if by == "E-2R"
        ),
        "{error:?}"
    );

    fixture.project().await;
    assert_eq!(
        fixture.balance("1000").await,
        money(0),
        "reversed exactly once"
    );
    fixture.cleanup().await;
}

/// An entry nobody posted cannot be undone, and the attempt writes nothing.
#[tokio::test]
async fn reversing_an_entry_that_does_not_exist_leaves_no_trace() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;

    let error = ledger::reverse_entry(
        &fixture.db,
        &code("NOPE"),
        &code("NOPE-R"),
        when(),
        "",
        &Metadata::default(),
    )
    .await
    .expect_err("there is no such entry");
    assert!(matches!(
        rejection(&error),
        Some(LedgerError::NoSuchEntry(_))
    ));

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let postings: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_ledger.posting")
        .fetch_one(&mut *conn)
        .await
        .expect("counts");
    drop(conn);
    assert_eq!(postings, 0, "the failed attempt posted nothing");

    fixture.cleanup().await;
}

/// A reversal is an ordinary entry, so the log still rebuilds to what is live.
#[tokio::test]
async fn reversals_replay_like_anything_else() {
    let fixture = Fixture::new().await;
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("4000", AccountKind::Revenue, sar()).await;

    for n in 0..4_i64 {
        let id = format!("E-R{n}");
        let lines = BalancedLines::new(vec![
            Line::new(code("1000"), money(n * 101 + 7)),
            Line::new(code("4000"), money(-(n * 101 + 7))),
        ])
        .expect("balances");
        post_entry(
            &fixture.db,
            &code(&id),
            when(),
            "",
            lines,
            &Metadata::default(),
        )
        .await
        .expect("posts");

        if n % 2 == 0 {
            ledger::reverse_entry(
                &fixture.db,
                &code(&id),
                &code(&format!("{id}-REV")),
                when(),
                "",
                &Metadata::default(),
            )
            .await
            .expect("reverses");
        }
    }
    fixture.project().await;

    let pool = fixture.tenant_pool().await;
    let owned = projections();
    let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
        .await
        .expect("replays");
    pool.close().await;

    assert!(report.is_reproducible(), "{:?}", report.differences());
    assert!(fixture.imbalances().await.is_empty());
    fixture.cleanup().await;
}

/// Reverses `E-2` under a chosen id, for the twice-reversal test.
async fn reverse(
    fixture: &Fixture,
    reversal: &str,
) -> Result<spa_eventlog::Committed<ledger::JournalEntryEvent>, CommandError<LedgerError>> {
    ledger::reverse_entry(
        &fixture.db,
        &code("E-2"),
        &code(reversal),
        when(),
        "",
        &Metadata::default(),
    )
    .await
}
