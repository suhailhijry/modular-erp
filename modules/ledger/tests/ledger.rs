//! The ledger, end to end against a real tenant.
//!
//! The test that carries the module is
//! [`any_sequence_of_valid_commands_leaves_the_ledger_balanced`]. Everything
//! else checks a rule; that one checks the *pipeline* — commands, events,
//! projections and the read models together — with one number that can only be
//! zero if all of them are right.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use ledger::{
    AccountKind, BalancedLines, JournalFilter, Ledger, LedgerError, Line, account_balances,
    balance_sheet, balances_at, close_account, imbalances, journal, journal_entry, open_account,
    post_entry, profit_and_loss, projections, rename_account, trial_balance,
};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

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
        let url = erp_testkit::database_url();
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

    /// Opens branches a posting may name — `post_entry_in` refuses one that
    /// names nothing open.
    async fn branches(&self, ids: &[&str]) {
        let mut conn = self.db.acquire().await.expect("connection");
        branches::install(&mut conn).await.expect("branches");
        ensure_group_schema::<branches::Branches>(&mut conn)
            .await
            .expect("the branches' checkpoint");
        drop(conn);
        for id in ids {
            branches::open_branch(
                &self.db,
                &code(id),
                &branches::Details {
                    name: (*id).to_owned(),
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
                when(),
                &Metadata::default(),
            )
            .await
            .expect("the branch opens");
        }
    }

    async fn cleanup(self) {
        drop(self.db);
        drop(self.control);
        let _ = erp_testkit::drop_named_database(&self.tenant_database).await;
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

/// **The statements are sums over the postings at the instants asked.** Four
/// entries across two fiscal years and two branches, and every statement read
/// from them: the profit and loss over a range and at a branch, balances as at
/// a date, the balance sheet with the two trading results split at the fiscal
/// year, and the journal newest first, paged and filtered.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one set of postings, and every statement that reads it"
)]
async fn statements_are_sums_over_the_postings_at_the_instants_asked() {
    let fixture = Fixture::new().await;
    for (account, kind) in [
        ("1000", AccountKind::Asset),
        ("2000", AccountKind::Liability),
        ("3000", AccountKind::Equity),
        ("4000", AccountKind::Revenue),
        ("5000", AccountKind::Expense),
    ] {
        fixture.account(account, kind, sar()).await;
    }
    fixture.branches(&["olaya", "malaz"]).await;
    let at = |text: &str| -> Timestamp { text.parse().expect("an instant") };
    let post =
        |id: &str, on: &str, debit: &str, credit: &str, amount: i64, branch: Option<&str>| {
            let lines = BalancedLines::new(vec![
                Line::new(code(debit), riyals(amount)),
                Line::new(code(credit), riyals(-amount)).with_memo("the other side"),
            ])
            .expect("balances");
            let metadata = match branch {
                Some(branch) => Metadata::default().at_branch(branch),
                None => Metadata::default(),
            };
            let id = code(id);
            let on = at(on);
            let db = &fixture.db;
            async move {
                post_entry(db, &id, on, "statement", lines, &metadata)
                    .await
                    .expect("posts");
            }
        };
    // Capital in 2025, a 2025 sale, then a 2026 sale and a 2026 expense.
    post(
        "capital",
        "2025-06-01T00:00:00Z",
        "1000",
        "3000",
        1_000,
        None,
    )
    .await;
    post(
        "sale-2025",
        "2025-12-15T00:00:00Z",
        "1000",
        "4000",
        500,
        Some("olaya"),
    )
    .await;
    post(
        "sale-2026",
        "2026-02-10T00:00:00Z",
        "1000",
        "4000",
        300,
        Some("malaz"),
    )
    .await;
    post(
        "rent-2026",
        "2026-02-20T00:00:00Z",
        "5000",
        "1000",
        100,
        Some("olaya"),
    )
    .await;
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");

    // **Profit and loss over a range**: only what fell in it, signed as posted.
    let year = profit_and_loss(
        &mut conn,
        at("2026-01-01T00:00:00Z"),
        at("2027-01-01T00:00:00Z"),
        None,
    )
    .await
    .expect("reads");
    let line = |lines: &[ledger::StatementLine], code: &str| {
        lines
            .iter()
            .find(|l| l.code == code)
            .expect("listed")
            .balance
    };
    assert_eq!(
        line(&year, "4000"),
        riyals(-300),
        "the 2025 sale is outside the range"
    );
    assert_eq!(line(&year, "5000"), riyals(100));
    assert!(
        year.iter()
            .all(|l| matches!(l.kind, AccountKind::Revenue | AccountKind::Expense)),
        "a profit and loss carries trading accounts only"
    );
    // At one branch: Malaz sold, Olaya paid the rent.
    let olaya = profit_and_loss(
        &mut conn,
        at("2026-01-01T00:00:00Z"),
        at("2027-01-01T00:00:00Z"),
        Some("olaya"),
    )
    .await
    .expect("reads");
    assert_eq!(line(&olaya, "4000"), riyals(0));
    assert_eq!(line(&olaya, "5000"), riyals(100));

    // **Balances as at a date**: before 2026, cash holds the capital and the
    // first sale; all-time, everything.
    let then = balances_at(&mut conn, Some(at("2026-01-01T00:00:00Z")))
        .await
        .expect("reads");
    let cash = |accounts: &[ledger::AccountBalance]| {
        accounts
            .iter()
            .find(|a| a.code == "1000")
            .expect("cash")
            .balance
    };
    assert_eq!(cash(&then), riyals(1_500));
    let now = balances_at(&mut conn, None).await.expect("reads");
    assert_eq!(cash(&now), riyals(1_700));
    assert_eq!(
        now,
        account_balances(&mut conn).await.expect("reads"),
        "no date is all-time"
    );

    // **The balance sheet as at 1 March 2026**, with the fiscal year from 1
    // January: the standing accounts at their balances, this year's result
    // (300 sold less 100 rent) and last year's (500) as the two equity lines
    // no account holds, and nothing out of balance.
    let sheet = balance_sheet(
        &mut conn,
        at("2026-03-01T00:00:00Z"),
        at("2026-01-01T00:00:00Z"),
    )
    .await
    .expect("reads");
    assert_eq!(line(&sheet.lines, "1000"), riyals(1_700));
    assert_eq!(line(&sheet.lines, "3000"), riyals(-1_000));
    assert!(
        sheet
            .lines
            .iter()
            .all(|l| !matches!(l.kind, AccountKind::Revenue | AccountKind::Expense)),
        "the trading accounts are the results, not lines"
    );
    assert_eq!(sheet.results.len(), 1, "one currency, one result");
    let result = &sheet.results[0];
    assert_eq!(result.currency, sar());
    assert_eq!(
        result.current_year,
        riyals(-200),
        "a profit is net credits, so negative as posted"
    );
    assert_eq!(result.prior_years, riyals(-500));
    assert_eq!(result.difference, riyals(0));
    // Assets equal liabilities plus equity plus both results: 1700 = 1000 + 200 + 500.
    assert_eq!(
        line(&sheet.lines, "1000").minor(),
        -(line(&sheet.lines, "3000").minor()
            + result.current_year.minor()
            + result.prior_years.minor())
    );

    // **The journal, newest first, in pages**, then filtered by account and by
    // branch and by range.
    let first = journal(&mut conn, &JournalFilter::default(), 3, None)
        .await
        .expect("reads");
    let ids = |page: &erp_types::Page<ledger::JournalEntryView>| {
        page.items.iter().map(|e| e.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), ["rent-2026", "sale-2026", "sale-2025"]);
    let cursor = first.next.clone().expect("a fourth entry is left");
    let second = journal(&mut conn, &JournalFilter::default(), 3, Some(&cursor))
        .await
        .expect("reads");
    assert_eq!(ids(&second), ["capital"]);
    assert!(second.next.is_none(), "the journal ended");
    let sales = journal(
        &mut conn,
        &JournalFilter {
            account: Some("4000"),
            ..JournalFilter::default()
        },
        10,
        None,
    )
    .await
    .expect("reads");
    assert_eq!(ids(&sales), ["sale-2026", "sale-2025"]);
    let at_olaya = journal(
        &mut conn,
        &JournalFilter {
            branch: Some("olaya"),
            ..JournalFilter::default()
        },
        10,
        None,
    )
    .await
    .expect("reads");
    assert_eq!(ids(&at_olaya), ["rent-2026", "sale-2025"]);
    let in_2025 = journal(
        &mut conn,
        &JournalFilter {
            from: Some(at("2025-01-01T00:00:00Z")),
            until: Some(at("2026-01-01T00:00:00Z")),
            ..JournalFilter::default()
        },
        10,
        None,
    )
    .await
    .expect("reads");
    assert_eq!(ids(&in_2025), ["sale-2025", "capital"]);

    // One entry, with its lines in order and their memos.
    let rent = journal_entry(&mut conn, "rent-2026")
        .await
        .expect("reads")
        .expect("exists");
    assert_eq!(rent.branch.as_deref(), Some("olaya"));
    assert_eq!(rent.lines.len(), 2);
    assert_eq!(
        (rent.lines[0].account.as_str(), rent.lines[0].amount),
        ("5000", riyals(100))
    );
    assert_eq!(rent.lines[0].name, "5000", "the account's name rides along");
    assert_eq!(rent.lines[1].memo.as_deref(), Some("the other side"));
    assert!(
        journal_entry(&mut conn, "nothing")
            .await
            .expect("reads")
            .is_none()
    );
    drop(conn);

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
        let parsed = erp_types::EventName::new(*name).expect("a valid event name");
        assert!(
            upcasters.current_version(&parsed).is_some(),
            "{name} is not declared; events would be written that cannot be read"
        );
    }
    assert!(upcasters.gaps().is_empty(), "{:?}", upcasters.gaps());
}

#[test]
fn every_message_has_a_translation() {
    erp_i18n::testing::assert_complete(&ledger::CATALOG);
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
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    assert_eq!(installed.opened(), services.accounts.len());
    assert_eq!(installed.skipped(), 0);

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

/// **Every shipped chart, against a real tenant.**
///
/// ARCHITECTURE §1147 asks for exactly this — *"every shipped blueprint
/// previewed against a fresh tenant in CI"* — and until now only `booking`'s
/// trades had it. `real_estate` had never once been installed against a
/// database, which is the chart the whole property vertical posts into.
///
/// What it adds over the unit tests in `charts.rs`: those read the static
/// array, and this runs every account through `open_account_in`. A chart
/// declaring a code the domain refuses, a name too long for an aggregate, or a
/// kind that will not post fails here rather than in front of the first
/// business to pick it.
#[tokio::test]
async fn every_shipped_chart_installs_into_a_fresh_tenant_and_twice_is_harmless() {
    for chart in ledger::CHARTS {
        let fixture = Fixture::new().await;

        let installed = ledger::install_chart(
            &fixture.db,
            chart,
            sar(),
            erp_i18n::Locale::Arabic,
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{} does not install: {e}", chart.id));
        assert_eq!(
            installed.opened(),
            chart.accounts.len(),
            "{} did not open every account it declares",
            chart.id
        );
        assert_eq!(installed.skipped(), 0, "{}", chart.id);

        let again = ledger::install_chart(
            &fixture.db,
            chart,
            sar(),
            erp_i18n::Locale::Arabic,
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{} does not install twice: {e}", chart.id));
        assert_eq!(again.opened(), 0, "{} opened something twice", chart.id);
        assert_eq!(again.skipped(), chart.accounts.len(), "{}", chart.id);

        fixture.project().await;
        let mut conn = fixture.db.acquire().await.expect("connection");
        let accounts = account_balances(&mut conn).await.expect("reads");
        drop(conn);
        assert_eq!(accounts.len(), chart.accounts.len(), "{}", chart.id);
        // The names came out in Arabic, which is what the locale asked for.
        assert!(
            accounts.iter().all(|a| !a.name.trim().is_empty()),
            "{} opened an account with no name",
            chart.id
        );
        assert!(
            fixture.imbalances().await.is_empty(),
            "{} does not balance from the first minute",
            chart.id
        );

        fixture.cleanup().await;
    }
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
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");
    let second = ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("is a no-op, not an error");

    assert_eq!(second.opened(), 0);
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
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    let added = ledger::install_chart(
        &fixture.db,
        retail,
        sar(),
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    let shared = retail
        .accounts
        .iter()
        .filter(|a| services.accounts.iter().any(|s| s.code == a.code))
        .count();
    assert_eq!(added.skipped(), shared);
    assert_eq!(added.opened(), retail.accounts.len() - shared);
    assert!(added.opened() > 0, "retail must add something");

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
        erp_i18n::Locale::Arabic,
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
        erp_i18n::Locale::English,
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
) -> Result<erp_eventlog::Committed<ledger::JournalEntryEvent>, CommandError<LedgerError>> {
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

/// A date the business chose, as these tests write them.
fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

/// Two accounts, so there is something balanced to post between.
async fn open_cash_and_capital(fixture: &Fixture) {
    fixture.account("1000", AccountKind::Asset, sar()).await;
    fixture.account("3000", AccountKind::Equity, sar()).await;
}

/// One balanced entry on a given date.
async fn post_on(
    fixture: &Fixture,
    id: &str,
    day: &str,
) -> Result<erp_eventlog::Committed<ledger::JournalEntryEvent>, CommandError<LedgerError>> {
    let lines = BalancedLines::new(vec![
        Line::new(code("1000"), riyals(100)),
        Line::new(code("3000"), riyals(-100)),
    ])
    .expect("balances");

    post_entry(
        &fixture.db,
        &code(id),
        on(day),
        "capital introduced",
        lines,
        &Metadata::default(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Closing the books
//
// A VAT return is filed for a period and the tax on it is paid. An entry
// back-dated into that period afterwards changes the numbers behind a
// declaration that has already been made — and nothing records that it happened.
// ---------------------------------------------------------------------------

/// The date on the entry decides, not the date it was written.
#[tokio::test]
async fn an_entry_dated_into_a_closed_period_is_refused() {
    let fixture = Fixture::new().await;
    open_cash_and_capital(&fixture).await;

    // January's books are final.
    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-02-01")), Some("the-accountant"))
        .await
        .expect("closes");
    drop(conn);

    let refused = post_on(&fixture, "JE-JAN", "2026-01-15").await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(ledger::LedgerError::PeriodClosed { .. })
        ),
        "a January entry went in after January was closed"
    );

    // The boundary itself is open: `closed_before` is the first instant that is
    // still open, so an entry stamped exactly on it goes through.
    post_on(&fixture, "JE-FEB-0", "2026-02-01")
        .await
        .expect("the first instant of February is open");
    post_on(&fixture, "JE-FEB", "2026-02-15")
        .await
        .expect("February is open");

    // And the last moment of January is not.
    let refused = post_on(&fixture, "JE-JAN-LAST", "2026-01-31").await;
    assert!(refused.is_err(), "the last day of January is still January");

    fixture.cleanup().await;
}

/// Reopening puts it back, because an accountant who closes the wrong month has
/// to be able to put it right.
#[tokio::test]
async fn reopening_lets_the_period_take_entries_again() {
    let fixture = Fixture::new().await;
    open_cash_and_capital(&fixture).await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-02-01")), Some("the-accountant"))
        .await
        .expect("closes");
    drop(conn);
    assert!(post_on(&fixture, "JE-1", "2026-01-15").await.is_err());

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, None, Some("the-accountant"))
        .await
        .expect("reopens");
    let books = ledger::period::books(&mut conn).await.expect("reads");
    drop(conn);
    assert_eq!(books.closed_before, None);

    post_on(&fixture, "JE-1", "2026-01-15")
        .await
        .expect("January is open again");

    fixture.cleanup().await;
}

/// **A reversal cannot be dated into a closed period either.**
///
/// This is the one that would have been forgotten with a per-caller check.
/// `reverse_entry` takes its own `occurred_on` — usually today, sometimes not —
/// and it routes through `post_entry_in`, so it inherits the refusal rather than
/// needing to remember it.
#[tokio::test]
async fn a_reversal_cannot_be_back_dated_into_a_closed_period() {
    let fixture = Fixture::new().await;
    open_cash_and_capital(&fixture).await;

    post_on(&fixture, "JE-1", "2026-01-15")
        .await
        .expect("posts while January is open");

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-02-01")), Some("the-accountant"))
        .await
        .expect("closes");
    drop(conn);

    let refused = ledger::reverse_entry(
        &fixture.db,
        &code("JE-1"),
        &code("JE-1-R"),
        on("2026-01-20"),
        "put right",
        &Metadata::default(),
    )
    .await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(ledger::LedgerError::PeriodClosed { .. })
        ),
        "a correction went into a period that had already been declared"
    );

    // Dated into the open period, it goes through — which is where a correction
    // belongs, and what an auditor expects to find.
    ledger::reverse_entry(
        &fixture.db,
        &code("JE-1"),
        &code("JE-1-R"),
        on("2026-02-20"),
        "put right",
        &Metadata::default(),
    )
    .await
    .expect("reverses into the open period");

    fixture.cleanup().await;
}

/// Posting an entry does not need the books to be reread from scratch: a period
/// closed a moment ago refuses the very next entry.
#[tokio::test]
async fn a_close_takes_effect_on_the_next_entry() {
    let fixture = Fixture::new().await;
    open_cash_and_capital(&fixture).await;

    post_on(&fixture, "JE-1", "2026-01-15")
        .await
        .expect("posts");

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-02-01")), Some("the-accountant"))
        .await
        .expect("closes");
    drop(conn);

    assert!(
        post_on(&fixture, "JE-2", "2026-01-16").await.is_err(),
        "the entry immediately after the close still got in"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------

/// **A preview writes nothing.** The whole point of running it against a real
/// transaction is undone if the rollback is not.
#[tokio::test]
async fn a_preview_leaves_no_account_behind() {
    let fixture = Fixture::new().await;
    let services = ledger::chart("services").expect("ships");

    let would = ledger::preview_chart(
        &fixture.db,
        services,
        sar(),
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("previews");
    assert!(would.opened() > 20, "it would open a chart's worth");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let balances = ledger::account_balances(&mut conn).await.expect("lists");
    drop(conn);
    assert!(
        balances.is_empty(),
        "a preview must leave nothing behind, found {}",
        balances.len()
    );

    fixture.cleanup().await;
}

/// **The preview and the install agree, because they are the same code.**
///
/// This is the property the box asked for: a *predicted* preview is a second
/// implementation of the install's rules, and two implementations drift. Here
/// the only difference is whether the transaction commits.
#[tokio::test]
async fn a_preview_says_exactly_what_the_install_does() {
    let fixture = Fixture::new().await;
    let retail = ledger::chart("retail").expect("ships");

    let would = ledger::preview_chart(
        &fixture.db,
        retail,
        sar(),
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("previews");

    let did = ledger::install_chart(
        &fixture.db,
        retail,
        sar(),
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");

    assert_eq!(
        would.opened, did.opened,
        "the same accounts, in the same order"
    );
    assert_eq!(would.skipped, did.skipped);

    fixture.cleanup().await;
}

/// **A preview over a chart already installed reports skips, not opens** — and
/// still writes nothing.
#[tokio::test]
async fn a_preview_over_an_installed_chart_says_everything_is_already_there() {
    let fixture = Fixture::new().await;
    let services = ledger::chart("services").expect("ships");

    let did = ledger::install_chart(
        &fixture.db,
        services,
        sar(),
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("installs");
    assert_eq!(did.skipped(), 0, "a fresh tenant skips nothing");

    let would = ledger::preview_chart(
        &fixture.db,
        services,
        sar(),
        erp_i18n::Locale::English,
        &Metadata::default(),
    )
    .await
    .expect("previews");
    assert_eq!(would.opened(), 0, "nothing left to open");
    assert_eq!(
        would.skipped, did.opened,
        "everything the install opened, the preview now skips"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// The period close, and the year-end close
// ---------------------------------------------------------------------------

/// **Periods close in order and a year books into retained earnings.** One
/// year of trade in two currencies; the periods close in order (and refuse out
/// of it), the year refuses to book while a period is open and while a
/// currency has nowhere to close into, then books one entry per currency dated
/// the year's last day, flagged so the profit and loss ignores it and every
/// balance counts it. Reopening runs in reverse and a second booking gets
/// fresh entry ids.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one year, closed, reopened and closed again"
)]
async fn a_year_books_into_retained_earnings_and_reopens_in_reverse() {
    use ledger::period::{
        CloseError, ClosingAccounts, close_period_in, close_year_in, reopen_period_in,
        reopen_year_in,
    };

    let fixture = Fixture::new().await;
    for (account, kind, currency) in [
        ("1000", AccountKind::Asset, sar()),
        ("3000", AccountKind::Equity, sar()),
        ("3100", AccountKind::Equity, sar()),
        ("4000", AccountKind::Revenue, sar()),
        ("5000", AccountKind::Expense, sar()),
        ("1100", AccountKind::Asset, usd()),
        ("4100", AccountKind::Revenue, usd()),
        ("3900", AccountKind::Equity, usd()),
    ] {
        fixture.account(account, kind, currency).await;
    }
    let post = |id: &str, day: &str, debit: &str, credit: &str, amount: Money| {
        let lines = BalancedLines::new(vec![
            Line::new(code(debit), amount),
            Line::new(code(credit), amount.checked_neg().expect("negates")),
        ])
        .expect("balances");
        let id = code(id);
        let at = on(day);
        let db = &fixture.db;
        async move {
            post_entry(db, &id, at, "trade", lines, &Metadata::default())
                .await
                .expect("posts");
        }
    };
    post("capital", "2025-03-01", "1000", "3000", riyals(1_000)).await;
    post("sale", "2025-06-15", "1000", "4000", riyals(500)).await;
    post("rent", "2025-09-01", "5000", "1000", riyals(100)).await;
    post(
        "usd-sale",
        "2025-07-01",
        "1100",
        "4100",
        Money::from_minor(20_000, usd()),
    )
    .await;
    post("jan-sale", "2026-01-15", "1000", "4000", riyals(50)).await;
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let clock = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .expect("the tenant's clock");
    let metadata = Metadata::default();

    // A year books only once every period of it is closed.
    assert!(matches!(
        close_year_in(&mut conn, 2025, "close", &metadata, None).await,
        Err(CloseError::YearOpen { year: 2025, period }) if period == "2025-P01"
    ));
    // The first close may be any period; after it, order.
    let books = close_period_in(&mut conn, "2025-P11", None)
        .await
        .expect("the first close");
    assert_eq!(books.closed_before, Some(clock.start_of(day("2025-12-01"))));
    assert!(matches!(
        close_period_in(&mut conn, "2026-P01", None).await,
        Err(CloseError::OutOfOrder { period, next }) if period == "2026-P01" && next == "2025-P12"
    ));
    assert!(matches!(
        close_period_in(&mut conn, "2025-P13", None).await,
        Err(CloseError::NoSuchPeriod(_))
    ));
    let books = close_period_in(&mut conn, "2025-P12", None)
        .await
        .expect("closes December");
    assert_eq!(books.closed_before, Some(clock.start_of(day("2026-01-01"))));
    let again = close_period_in(&mut conn, "2025-P12", None)
        .await
        .expect("a retry is a no-op");
    assert_eq!(again, books);

    // Dollars have nowhere to go until an account is named.
    assert!(matches!(
        close_year_in(&mut conn, 2025, "close", &metadata, None).await,
        Err(CloseError::NeedsAccount { year: 2025, currency }) if currency == usd()
    ));
    erp_eventlog::configuration::set(
        &mut conn,
        ClosingAccounts::KEY,
        &ClosingAccounts {
            by_currency: [(usd(), code("3900"))].into_iter().collect(),
        },
        None,
        None,
    )
    .await
    .expect("names the dollar account");
    let books = close_year_in(&mut conn, 2025, "Closing 2025", &metadata, None)
        .await
        .expect("books 2025");
    assert!(books.is_booked(2025));
    assert_eq!(
        books.years[&2025].entries,
        ["closing-2025-SAR-1", "closing-2025-USD-1"]
    );
    drop(conn);
    fixture.project().await;

    // Every balance counts the close; the profit and loss does not.
    assert_eq!(
        fixture.balance("4000").await,
        riyals(-50),
        "January's sale remains"
    );
    assert_eq!(fixture.balance("5000").await, riyals(0));
    assert_eq!(
        fixture.balance("3100").await,
        riyals(-400),
        "the year's profit"
    );
    assert_eq!(
        fixture.balance("3900").await,
        Money::from_minor(-20_000, usd())
    );
    assert_eq!(fixture.balance("1000").await, riyals(1_450));
    let mut conn = fixture.db.acquire().await.expect("connection");
    let pnl = profit_and_loss(&mut conn, on("2025-01-01"), on("2026-01-01"), None)
        .await
        .expect("reads");
    let line = |code: &str| pnl.iter().find(|l| l.code == code).expect("shown").balance;
    assert_eq!(line("4000"), riyals(-500));
    assert_eq!(line("5000"), riyals(100));
    let sheet = balance_sheet(&mut conn, on("2026-01-01"), on("2026-01-01"))
        .await
        .expect("reads");
    let sar_result = sheet
        .results
        .iter()
        .find(|r| r.currency == sar())
        .expect("a riyal result");
    assert_eq!(sar_result.prior_years, riyals(0), "2025 is in 3100 now");
    assert_eq!(sar_result.difference, riyals(0));
    let closing = journal_entry(&mut conn, "closing-2025-SAR-1")
        .await
        .expect("reads")
        .expect("posted");
    assert!(closing.closing);
    assert_eq!(closing.occurred_on, clock.start_of(day("2025-12-31")));
    let amounts: Vec<(String, Money)> = closing
        .lines
        .iter()
        .map(|l| (l.account.clone(), l.amount))
        .collect();
    assert_eq!(
        amounts,
        [
            ("4000".to_owned(), riyals(500)),
            ("5000".to_owned(), riyals(-100)),
            ("3100".to_owned(), riyals(-400)),
        ]
    );

    // A closing entry is not reversed by hand, and a booked year's period
    // does not reopen before the year.
    let by_hand = ledger::reverse_entry(
        &fixture.db,
        &code("closing-2025-SAR-1"),
        &code("by-hand"),
        on("2025-12-31"),
        "undo",
        &metadata,
    )
    .await
    .expect_err("refused");
    assert!(
        matches!(rejection(&by_hand), Some(LedgerError::ClosingEntry(_))),
        "{by_hand:?}"
    );
    assert!(matches!(
        reopen_period_in(&mut conn, "2025-P12", None).await,
        Err(CloseError::YearBooked { year: 2025, .. })
    ));

    // Reopen the year: the entries reverse, the periods stay closed.
    let books = reopen_year_in(&mut conn, 2025, "Reopening 2025", &metadata, None)
        .await
        .expect("reopens");
    assert!(!books.is_booked(2025));
    assert_eq!(books.closed_before, Some(clock.start_of(day("2026-01-01"))));
    drop(conn);
    fixture.project().await;
    assert_eq!(fixture.balance("4000").await, riyals(-550));
    assert_eq!(fixture.balance("3100").await, riyals(0));
    let mut conn = fixture.db.acquire().await.expect("connection");
    let reversal = journal_entry(&mut conn, "closing-2025-SAR-1-reversal")
        .await
        .expect("reads")
        .expect("posted");
    assert!(reversal.closing, "the reversal is a closing entry too");

    // Only the latest closed period reopens.
    assert!(matches!(
        reopen_period_in(&mut conn, "2025-P10", None).await,
        Err(CloseError::NotLatest { period, latest }) if period == "2025-P10" && latest == "2025-P12"
    ));
    let books = reopen_period_in(&mut conn, "2025-P12", None)
        .await
        .expect("reopens December");
    assert_eq!(books.closed_before, Some(clock.start_of(day("2025-12-01"))));
    let open = reopen_period_in(&mut conn, "2026-P03", None)
        .await
        .expect("an open period is a no-op");
    assert_eq!(open, books);

    // Closed and booked again: fresh entries, not a silent repeat.
    close_period_in(&mut conn, "2025-P12", None)
        .await
        .expect("closes December again");
    let books = close_year_in(&mut conn, 2025, "Closing 2025", &metadata, None)
        .await
        .expect("books 2025 again");
    assert_eq!(
        books.years[&2025].entries,
        ["closing-2025-SAR-2", "closing-2025-USD-2"]
    );

    // A later booked year holds an earlier one closed.
    for index in 1..=12 {
        close_period_in(&mut conn, &format!("2026-P{index:02}"), None)
            .await
            .expect("closes a month of 2026");
    }
    drop(conn);
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    close_year_in(&mut conn, 2026, "Closing 2026", &metadata, None)
        .await
        .expect("books 2026");
    assert!(matches!(
        reopen_year_in(&mut conn, 2025, "reopen", &metadata, None).await,
        Err(CloseError::LaterYearBooked {
            year: 2025,
            later: 2026
        })
    ));

    // The figures come from the read model, which must be at the head.
    drop(conn);
    post("sale-2027", "2027-02-01", "1000", "4000", riyals(70)).await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    for index in 1..=12 {
        close_period_in(&mut conn, &format!("2027-P{index:02}"), None)
            .await
            .expect("closes a month of 2027");
    }
    assert!(matches!(
        close_year_in(&mut conn, 2027, "close", &metadata, None).await,
        Err(CloseError::ReadModelBehind { behind }) if behind > 0
    ));
    drop(conn);
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let books = close_year_in(&mut conn, 2027, "Closing 2027", &metadata, None)
        .await
        .expect("books 2027 once the read model has caught up");
    assert_eq!(books.years[&2027].entries, ["closing-2027-SAR-1"]);
    drop(conn);

    fixture.cleanup().await;
}

fn day(text: &str) -> chrono::NaiveDate {
    text.parse().expect("a date")
}
