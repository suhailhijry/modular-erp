//! Stock, against a real tenant.
//!
//! The test that carries this file is
//! [`a_quantity_on_hand_agrees_with_the_movements`] — the canary this module is
//! built around, and it holds **lot by lot**. What a shelf holds is aggregate
//! state rehydrated from the log, what a screen reads is a projection of the
//! same log, and what a manager is shown as the explanation is a third thing.
//! Nothing reconciles the three; they are three readings of one stream, and
//! this is where that stops being a claim about code nobody can check by
//! reading it.
//!
//! **Nothing here reaches a state the product cannot produce.** Every fixture
//! is built through this module's own commands; the only raw SQL is the
//! `TRUNCATE` and checkpoint rewind in `Fixture::rebuild`, which is what the
//! migrator does to a group and is the one thing no command can express.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use inventory::stock::Reason;
use inventory::{
    Consumption, Count, InventoryError, Receipt, Restoration, Shortfall, Stock, Tracking, WriteOff,
};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

const BEANS: &str = "f81d4fae-7dec-11d0-a765-00a0c91e6bf6";
const MILK: &str = "9f2a6d0c-11d0-7dec-a765-00a0c91e6bf7";
const GRINDER: &str = "3c1b7e55-0a44-4f2d-8b9c-2d5a1e6f7a88";
const SUGAR: &str = "6b0e2f4a-5c3d-4e1f-9a8b-7c6d5e4f3a21";
const OLAYA: &str = "BRANCH-OLAYA";
const MALAZ: &str = "BRANCH-MALAZ";

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn sar(minor: i64) -> Money {
    Money::from_minor(minor, CurrencyCode::new("SAR").expect("a currency"))
}

fn usd(minor: i64) -> Money {
    Money::from_minor(minor, CurrencyCode::new("USD").expect("a currency"))
}

fn at(hour: &str) -> Timestamp {
    format!("2026-04-01T{hour}:00:00Z")
        .parse()
        .expect("a valid instant")
}

fn day(literal: &str) -> chrono::NaiveDate {
    literal.parse().expect("a date")
}

/// A request from a branch, which is what makes stock per-branch without any
/// module threading a field through. See `erp_web::Allowed::branch`.
fn from(branch: Option<&str>) -> Metadata {
    branch.map_or_else(Metadata::default, |branch| {
        Metadata::default().at_branch(branch)
    })
}

/// What a delivery looks like when nothing about it is being tested.
fn plain(quantity: i64, minor: i64, reference: &str) -> Receipt {
    Receipt {
        quantity,
        value: sar(minor),
        code: None,
        expires_on: None,
        serials: Vec::new(),
        reference: reference.to_owned(),
        at: at("06"),
    }
}

/// The lot a delivery makes: named after the shelf it lands on **and** the
/// request's own key. See `inventory::lot_of`.
fn lot_id(product: &str, branch: &str, reference: &str) -> String {
    inventory::lot_of(
        &inventory::stock_id(&code(product), Some(branch)).expect("a key"),
        reference,
    )
}

/// A document line taking so many units, with nothing else to say about it.
fn sold(quantity: i64, reference: &str) -> Consumption {
    Consumption {
        quantity: Some(quantity),
        lot: None,
        serials: Vec::new(),
        reference: reference.to_owned(),
        at: at("14"),
    }
}

/// What somebody found on the whole shelf, naming no lot.
fn shelf_count(declared: i64, reference: &str) -> Count {
    Count {
        lot: None,
        declared,
        serials: Vec::new(),
        reference: reference.to_owned(),
        at: at("16"),
    }
}

/// The units somebody found on a serial-tracked shelf, by name.
fn named(declared: i64, serials: &[&str], reference: &str) -> Count {
    Count {
        serials: serials.iter().map(|s| (*s).to_owned()).collect(),
        ..shelf_count(declared, reference)
    }
}

fn scrapped(reason: Reason, quantity: i64, reference: &str) -> WriteOff {
    WriteOff {
        reason,
        quantity: Some(quantity),
        lot: None,
        serials: Vec::new(),
        reference: reference.to_owned(),
        at: at("12"),
    }
}

/// The refusal a command made, or nothing. Anything that is not a refusal —
/// contention, a pool, the database — is a test that did not do what it meant.
fn rejection<T>(
    outcome: Result<T, erp_tenant::CommandError<InventoryError>>,
) -> Result<(), InventoryError> {
    match outcome {
        Ok(_) => Ok(()),
        Err(erp_tenant::CommandError::Execute(erp_eventlog::ExecuteError::Rejected(e))) => Err(e),
        Err(other) => panic!("got {other:?}"),
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
            .register_tenant_on(slug, "Café", "primary", Actor::system())
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
            inventory::install(&mut conn).await.expect("inventory");
            ensure_group_schema::<inventory::Inventory>(&mut conn)
                .await
                .expect("the group's checkpoint");
            // **Because every movement out posts.** `ledger` for the accounts
            // and `branches` because `post_entry_in` checks the request's
            // branch against the log — and every movement here carries one,
            // since a shelf is a fact about a place.
            ledger::install(&mut conn).await.expect("ledger");
            ensure_group_schema::<ledger::Ledger>(&mut conn)
                .await
                .expect("the ledger's checkpoint");
            branches::install(&mut conn).await.expect("branches");
            ensure_group_schema::<branches::Branches>(&mut conn)
                .await
                .expect("the branches' checkpoint");
        }

        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
        let pool = sqlx::PgPool::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects");

        let fixture = Self {
            db,
            pool,
            _control: control,
            _control_db: control_db,
            database: tenant.database_name,
        };

        // The chart the demo installs, and the two places the café trades from.
        ledger::install_chart(
            &fixture.db,
            ledger::chart("services").expect("the services chart ships"),
            CurrencyCode::new("SAR").expect("a currency"),
            erp_i18n::Locale::English,
            &Metadata::default(),
        )
        .await
        .expect("the chart installs");

        for branch in [OLAYA, MALAZ] {
            branches::open_branch(
                &fixture.db,
                &code(branch),
                &branches::Details {
                    name: "فرع".to_owned(),
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
                at("05"),
                &Metadata::default(),
            )
            .await
            .expect("the branch opens");
        }

        fixture
    }

    /// What one account holds, out of the ledger's own read model.
    ///
    /// **The other side of every assertion about a movement.** Two numbers by
    /// two routes that share nothing but the log, which is what makes them
    /// worth comparing.
    async fn balance(&self, account: &str) -> i64 {
        self.project_ledger().await;
        let mut conn = self.pool.acquire().await.expect("connection");
        ledger::account_balances(&mut conn)
            .await
            .expect("reads")
            .into_iter()
            .find(|a| a.code == account)
            .map_or(0, |a| a.balance.minor())
    }

    /// What every shelf is carried at, summed — `inventory`'s half of the
    /// canary. One currency in these tests, so one number.
    async fn value_on_hand(&self) -> i64 {
        self.project().await;
        let mut conn = self.pool.acquire().await.expect("connection");
        inventory::value_on_hand(&mut conn)
            .await
            .expect("reads")
            .first()
            .map_or(0, |held| held.minor())
    }

    async fn project_ledger(&self) {
        let owned = ledger::projections();
        let refs: Vec<&dyn Projection<Group = ledger::Ledger>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<ledger::Ledger>(&self.pool, &refs, ledger::upcasters(), 200)
            .await
            .expect("the ledger projects");
    }

    async fn project(&self) {
        let owned = inventory::projections();
        let refs: Vec<&dyn Projection<Group = inventory::Inventory>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<inventory::Inventory>(&self.pool, &refs, inventory::upcasters(), 200)
            .await
            .expect("projects");
    }

    /// Throws the read models away and builds them again from the log.
    ///
    /// **The one place raw SQL touches state**, and it is what the migrator's
    /// `rebuild_swap` does rather than anything a tenant could type.
    async fn rebuild(&self) {
        sqlx::query(
            "TRUNCATE proj_inventory.product, proj_inventory.lot, proj_inventory.serial, \
             proj_inventory.stock_item, proj_inventory.stock_movement",
        )
        .execute(&self.pool)
        .await
        .expect("empties the read models");
        sqlx::query("UPDATE projection_checkpoint SET position = 0 WHERE group_name = $1")
            .bind(inventory::GROUP_NAME)
            .execute(&self.pool)
            .await
            .expect("rewinds the checkpoint");
        self.project().await;
    }

    /// The shelf as the write side holds it, rehydrated from the log.
    async fn shelf(&self, product: &str, branch: Option<&str>) -> Stock {
        let id = inventory::stock_id(&code(product), branch).expect("a key");
        let mut conn = self.pool.acquire().await.expect("connection");
        erp_eventlog::load::<Stock>(&mut conn, &id, inventory::upcasters())
            .await
            .expect("loads")
            .aggregate
    }

    async fn shelves(&self) -> Vec<inventory::StockRow> {
        let mut conn = self.pool.acquire().await.expect("connection");
        inventory::stock(&mut conn, None, None, 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn lots(&self, product: Option<&str>) -> Vec<inventory::LotRow> {
        let mut conn = self.pool.acquire().await.expect("connection");
        inventory::lots(&mut conn, product, None, None, 50, None)
            .await
            .expect("reads")
            .items
    }

    /// One lot, open or emptied.
    async fn lot(&self, id: &str) -> inventory::LotRow {
        let mut conn = self.pool.acquire().await.expect("connection");
        inventory::lot(&mut conn, id)
            .await
            .expect("reads")
            .expect("the lot is in the read model")
    }

    async fn expiring_before(&self, until: chrono::NaiveDate) -> Vec<inventory::LotRow> {
        let mut conn = self.pool.acquire().await.expect("connection");
        inventory::lots(&mut conn, None, None, Some(until), 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn movements(&self, product: &str) -> Vec<inventory::MovementRow> {
        let mut conn = self.pool.acquire().await.expect("connection");
        inventory::movements(&mut conn, Some(product), 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn declare(&self, product: &str, name: &str, unit: &str, tracking: Tracking) {
        inventory::declare(
            &self.db,
            &code(product),
            name,
            unit,
            tracking,
            at("05"),
            &Metadata::default(),
        )
        .await
        .expect("declares");
    }

    async fn receive(&self, product: &str, receipt: &Receipt) -> String {
        self.try_receive(product, receipt, OLAYA)
            .await
            .expect("receives");
        lot_id(product, OLAYA, &receipt.reference)
    }

    async fn try_receive(
        &self,
        product: &str,
        receipt: &Receipt,
        branch: &str,
    ) -> Result<(), InventoryError> {
        rejection(inventory::receive(&self.db, &code(product), receipt, &from(Some(branch))).await)
    }

    async fn write_off(&self, product: &str, taken: &WriteOff) {
        self.try_write_off(product, taken)
            .await
            .expect("writes off");
    }

    async fn try_write_off(&self, product: &str, taken: &WriteOff) -> Result<(), InventoryError> {
        rejection(inventory::write_off(&self.db, &code(product), taken, &from(Some(OLAYA))).await)
    }

    async fn count(&self, product: &str, lot: &str, declared: i64, reference: &str) {
        self.try_count(product, lot, declared, reference)
            .await
            .expect("counts");
    }

    async fn try_count(
        &self,
        product: &str,
        lot: &str,
        declared: i64,
        reference: &str,
    ) -> Result<(), InventoryError> {
        self.try_counted(
            product,
            &Count {
                lot: Some(lot.to_owned()),
                ..shelf_count(declared, reference)
            },
        )
        .await
    }

    async fn try_counted(&self, product: &str, count: &Count) -> Result<(), InventoryError> {
        rejection(inventory::count(&self.db, &code(product), count, &from(Some(OLAYA))).await)
    }

    /// **What a credit note does to the shelf**, one layer down — the
    /// transaction `sales` would own, as [`Self::consume`] opens for a sale.
    async fn restore(&self, product: &str, back: &Restoration) -> Result<(), InventoryError> {
        let mut tx = self.db.begin().await.expect("a transaction");
        let outcome =
            inventory::restore_in(&mut tx, &code(product), back, &from(Some(OLAYA))).await;
        match outcome {
            Ok(_) => {
                tx.commit().await.expect("commits");
                Ok(())
            }
            Err(erp_eventlog::ExecuteError::Rejected(e)) => {
                tx.rollback().await.expect("rolls back");
                Err(e)
            }
            Err(other) => panic!("got {other:?}"),
        }
    }

    /// **What `sales::issue_in` does**, one layer down.
    ///
    /// `consume_in` takes the caller's connection because the sale that calls
    /// it owns the transaction its invoice commits in — so a test has to open
    /// one, exactly as `pos` does around `sales::issue_in`. The invoice path is
    /// `sales`' own tests; this is the shelf's half on its own.
    async fn consume(&self, product: &str, taking: &Consumption) -> Result<(), InventoryError> {
        let mut tx = self.db.begin().await.expect("a transaction");
        let outcome =
            inventory::consume_in(&mut tx, &code(product), taking, &from(Some(OLAYA))).await;
        match outcome {
            Ok(_) => {
                tx.commit().await.expect("commits");
                Ok(())
            }
            Err(erp_eventlog::ExecuteError::Rejected(e)) => {
                tx.rollback().await.expect("rolls back");
                Err(e)
            }
            Err(other) => panic!("got {other:?}"),
        }
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// **The canary: what is on hand is the sum of what moved, lot by lot — and the
/// read model says the same as the log.**
///
/// Four readings of one stream: the aggregate the write side decides from, the
/// total a screen shows, the lots that total is made of, and the movements a
/// manager is given as the explanation. A business cannot act on a number it
/// cannot explain, and the explanation here is per batch, because that is what
/// this module is for.
///
/// A rebuild at the end, because a read model that is only right the first time
/// is a read model that is wrong after the next deploy.
#[tokio::test]
async fn a_quantity_on_hand_agrees_with_the_movements() {
    let fixture = Fixture::new("inv-canary").await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;

    // Three deliveries expiring out of order, a write-off that comes off
    // whichever goes first, and a count on the one that is left open.
    for (reference, quantity, minor, expires) in [
        ("rcv-1", 24, 12_000, "2026-04-21"),
        ("rcv-2", 12, 7_200, "2026-04-11"),
        ("rcv-3", 24, 14_400, "2026-05-02"),
    ] {
        fixture
            .receive(
                MILK,
                &Receipt {
                    code: Some("B-2026-04".to_owned()),
                    expires_on: Some(day(expires)),
                    ..plain(quantity, minor, reference)
                },
            )
            .await;
    }
    fixture
        .write_off(MILK, &scrapped(Reason::Expired, 18, "wo-1"))
        .await;
    fixture
        .count(MILK, &lot_id(MILK, OLAYA, "rcv-1"), 16, "cnt-1")
        .await;
    fixture.project().await;

    for pass in ["projected", "rebuilt"] {
        let shelf = fixture.shelf(MILK, Some(OLAYA)).await;
        let shelves = fixture.shelves().await;
        let movements = fixture.movements(MILK).await;

        assert_eq!(shelves.len(), 1, "{pass}");
        assert_eq!(
            shelves[0].on_hand,
            shelf.on_hand(),
            "{pass}: the read model and the log disagree about what is on the shelf"
        );
        assert_eq!(
            Some(sar(shelves[0].value)),
            shelf.value().expect("sums"),
            "{pass}: the read model and the log disagree about what it is worth"
        );

        // **The canary itself**: the movements sum to what is on hand.
        assert_eq!(
            movements.iter().map(|m| m.quantity).sum::<i64>(),
            shelves[0].on_hand,
            "{pass}: the shelf holds a quantity its movements do not explain"
        );
        assert_eq!(
            movements.iter().map(|m| m.value).sum::<i64>(),
            shelves[0].value,
            "{pass}: the shelf is worth something its movements do not explain"
        );

        // **And it holds lot by lot**, which is the whole reason for lots: a
        // batch that is short cannot hide behind a batch that is long.
        let lots = fixture.lots(Some(MILK)).await;
        assert_eq!(
            lots.len(),
            2,
            "{pass}: the emptied batch should have closed"
        );
        for lot in lots {
            let moved: i64 = movements
                .iter()
                .filter(|m| m.lot.as_deref() == Some(lot.id.as_str()))
                .map(|m| m.quantity)
                .sum();
            assert_eq!(
                lot.remaining, moved,
                "{pass}: lot {} holds a quantity its movements do not explain",
                lot.id
            );
            assert_eq!(
                Some(lot.remaining),
                shelf.lot(&lot.id).map(|open| open.quantity),
                "{pass}: lot {} reads differently from the log",
                lot.id
            );
        }

        if pass == "projected" {
            fixture.rebuild().await;
        }
    }

    fixture.cleanup().await;
}

/// **Earliest expiry first, and a lot that empties closes.**
///
/// Eighteen bottles off three batches: the whole of the one expiring on the
/// 11th, then six of the 21st, and the May batch untouched. Each portion costed
/// on its own lot, which an average across the shelf could not do.
#[tokio::test]
async fn stock_goes_out_of_the_lot_that_expires_first() {
    let fixture = Fixture::new("inv-expiry").await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;

    for (reference, quantity, minor, expires) in [
        ("rcv-1", 24, 12_000, "2026-04-21"),
        ("rcv-2", 12, 7_200, "2026-04-11"),
        ("rcv-3", 24, 14_400, "2026-05-02"),
    ] {
        fixture
            .receive(
                MILK,
                &Receipt {
                    code: Some(format!("B-{reference}")),
                    expires_on: Some(day(expires)),
                    ..plain(quantity, minor, reference)
                },
            )
            .await;
    }

    fixture
        .write_off(MILK, &scrapped(Reason::Expired, 18, "wo-1"))
        .await;
    fixture.project().await;

    let shelf = fixture.shelf(MILK, Some(OLAYA)).await;
    assert_eq!(shelf.on_hand(), 42);
    assert!(
        shelf.lot(&lot_id(MILK, OLAYA, "rcv-2")).is_none(),
        "the batch that expired first emptied and should have closed"
    );

    let written_off: Vec<_> = fixture
        .movements(MILK)
        .await
        .into_iter()
        .filter(|m| m.kind == "written_off")
        .map(|m| (m.lot.unwrap_or_default(), m.quantity, m.value))
        .collect();
    assert_eq!(
        written_off,
        vec![
            // Newest first, so the second portion then the first.
            (lot_id(MILK, OLAYA, "rcv-1"), -6, -3_000),
            (lot_id(MILK, OLAYA, "rcv-2"), -12, -7_200),
        ],
        "the whole of the April 11th batch at its own cost, then six of the 21st at theirs"
    );

    // **The listing is the order stock goes out in**, which is what a manager
    // reads before deciding what to throw away.
    let open = fixture.lots(Some(MILK)).await;
    assert_eq!(
        open.iter().map(|l| l.expires_on).collect::<Vec<_>>(),
        vec![Some(day("2026-04-21")), Some(day("2026-05-02"))],
    );
    assert_eq!(open[0].remaining, 18, "twenty-four less the six that went");
    assert_eq!(open[0].code.as_deref(), Some("B-rcv-1"));
    assert_eq!(
        fixture.expiring_before(day("2026-04-30")).await.len(),
        1,
        "only the batch that goes off this month"
    );
    assert_eq!(
        fixture.expiring_before(day("2026-04-21")).await.len(),
        0,
        "a batch dated the day asked about is still good that day — `before` is \
         before"
    );

    fixture.cleanup().await;
}

/// **A serial is refused, not invented** — unknown, already gone, or named twice
/// in one movement.
///
/// Decision 7's *"a sale never refuses for stock"* is about quantities: a count
/// corrects a quantity, and nothing corrects a unit that was never on the shelf.
#[tokio::test]
async fn a_serial_that_is_not_on_the_shelf_is_refused() {
    let fixture = Fixture::new("inv-serial").await;
    fixture
        .declare(GRINDER, "مطحنة", "piece", Tracking::Serial)
        .await;

    fixture
        .receive(
            GRINDER,
            &Receipt {
                serials: vec!["SN-1".to_owned(), "SN-2".to_owned(), "SN-3".to_owned()],
                ..plain(3, 90_000, "rcv-1")
            },
        )
        .await;

    let by_serial = |serials: &[&str], reference: &str| WriteOff {
        reason: Reason::Damaged,
        quantity: None,
        lot: None,
        serials: serials.iter().map(|s| (*s).to_owned()).collect(),
        reference: reference.to_owned(),
        at: at("12"),
    };

    // One unit goes, at its own lot's cost.
    fixture
        .write_off(GRINDER, &by_serial(&["SN-2"], "wo-1"))
        .await;

    // The same unit again: already gone.
    assert!(
        matches!(
            fixture
                .try_write_off(GRINDER, &by_serial(&["SN-2"], "wo-2"))
                .await,
            Err(InventoryError::NoSuchSerial(ref s)) if s == "SN-2"
        ),
        "a unit that has already left was written off twice"
    );
    // One that was never received.
    assert!(matches!(
        fixture
            .try_write_off(GRINDER, &by_serial(&["SN-9"], "wo-3"))
            .await,
        Err(InventoryError::NoSuchSerial(_))
    ));
    // One named twice in one movement, which would take two units off a shelf
    // holding one of them.
    assert!(matches!(
        fixture
            .try_write_off(GRINDER, &by_serial(&["SN-1", "SN-1"], "wo-4"))
            .await,
        Err(InventoryError::NoSuchSerial(_))
    ));
    // And a receipt cannot give two units one identity.
    assert!(matches!(
        fixture
            .try_receive(
                GRINDER,
                &Receipt {
                    serials: vec!["SN-3".to_owned()],
                    ..plain(1, 30_000, "rcv-2")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::SerialAlreadyHeld(_))
    ));

    fixture.project().await;
    let shelf = fixture.shelf(GRINDER, Some(OLAYA)).await;
    assert_eq!(shelf.on_hand(), 2);
    let lots = fixture.lots(Some(GRINDER)).await;
    assert_eq!(
        lots[0].serials,
        vec!["SN-1".to_owned(), "SN-3".to_owned()],
        "the read model lists the units still on the lot"
    );
    assert_eq!(lots[0].value, 60_000, "one of three at thirty thousand");

    // **And a serial-tracked product is not counted by a bare quantity**: a
    // number one short cannot say which unit is gone, so a count names them.
    assert!(matches!(
        fixture.try_count(GRINDER, &lots[0].id, 1, "cnt-1").await,
        Err(InventoryError::NeedsSerials { units: 1, named: 0 })
    ));

    fixture.cleanup().await;
}

/// **A named unit that comes back is on hand again**, and one at another branch
/// is not touched by it.
///
/// `receive` refuses a serial the shelf is *holding*, so a machine that went out
/// for repair and came back is a second delivery and not a wrong one. The read
/// model has to say where the unit is now rather than where it was first seen —
/// first-write-wins leaves a screen saying `written_off` on a closed lot while
/// the log says the unit is on hand in a new one.
///
/// And the shelf is part of the row's identity, because the write side keeps one
/// `Stock` per branch and neither sees the other's serials. A read model claiming
/// one name per tenant would be claiming a uniqueness no command enforces.
#[tokio::test]
async fn a_named_unit_that_comes_back_is_on_hand_again() {
    let fixture = Fixture::new("inv-return").await;
    fixture
        .declare(GRINDER, "مطحنة", "piece", Tracking::Serial)
        .await;

    let one = |reference: &str, minor: i64| Receipt {
        serials: vec!["SN-1".to_owned()],
        ..plain(1, minor, reference)
    };

    // The same model number at two branches. Two shelves, and neither can see
    // the other's names.
    fixture
        .try_receive(GRINDER, &one("rcv-1", 30_000), OLAYA)
        .await
        .expect("receives at Olaya");
    fixture
        .try_receive(GRINDER, &one("rcv-2", 40_000), MALAZ)
        .await
        .expect("receives at Malaz");

    // Olaya sends its one for repair and writes it off.
    fixture
        .write_off(
            GRINDER,
            &WriteOff {
                reason: Reason::Damaged,
                quantity: None,
                lot: None,
                serials: vec!["SN-1".to_owned()],
                reference: "wo-1".to_owned(),
                at: at("12"),
            },
        )
        .await;
    fixture.project().await;

    assert_eq!(
        fixture
            .lots(Some(GRINDER))
            .await
            .iter()
            .map(|l| (l.branch.clone(), l.remaining, l.serials.clone()))
            .collect::<Vec<_>>(),
        vec![(Some(MALAZ.to_owned()), 1, vec!["SN-1".to_owned()])],
        "Olaya writing a machine off took Malaz's off the screen with it"
    );

    // It comes back. The shelf is no longer holding it, so the command accepts
    // the delivery — and the row has to follow the log onto the new lot.
    fixture
        .try_receive(GRINDER, &one("rcv-3", 30_000), OLAYA)
        .await
        .expect("a unit that has left may be received again");
    fixture.project().await;

    assert_eq!(
        fixture
            .lots(Some(GRINDER))
            .await
            .iter()
            .map(|l| (l.id.clone(), l.serials.clone()))
            .collect::<Vec<_>>(),
        vec![
            (lot_id(GRINDER, MALAZ, "rcv-2"), vec!["SN-1".to_owned()]),
            (lot_id(GRINDER, OLAYA, "rcv-3"), vec!["SN-1".to_owned()]),
        ],
        "the unit that came back is still shown gone on the lot it left"
    );
    assert_eq!(fixture.shelf(GRINDER, Some(OLAYA)).await.on_hand(), 1);

    fixture.cleanup().await;
}

/// **What a count found is frozen into the event**, so the answer is the same
/// tomorrow, after a delivery at another price has landed.
#[tokio::test]
async fn what_a_count_found_is_frozen() {
    let fixture = Fixture::new("inv-count").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;

    let first = fixture.receive(BEANS, &plain(5_000, 30_000, "rcv-1")).await;
    fixture.receive(BEANS, &plain(5_000, 40_000, "rcv-2")).await;

    // The first sack is two hundred grams short of what the books say.
    fixture.count(BEANS, &first, 4_800, "cnt-1").await;
    fixture.project().await;

    let counted = fixture
        .movements(BEANS)
        .await
        .into_iter()
        .find(|m| m.kind == "counted")
        .expect("the count is a movement");
    assert_eq!(counted.expected, Some(5_000));
    assert_eq!(counted.declared, Some(4_800));
    assert_eq!(counted.quantity, -200);
    assert_eq!(
        counted.value, -1_200,
        "two hundred grams of the sack that cost 300.00 for five kilos — at its \
         own cost, not at an average of both sacks"
    );
    assert_eq!(counted.lot.as_deref(), Some(first.as_str()));
    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 9_800);

    // **Counting a sack does not move it down the queue.** Both sacks are
    // undated, so the listing breaks the tie on the receipt's log position —
    // and a count that bumped it would put the lots on a screen in a different
    // order from the one the picking rule takes them in.
    assert_eq!(
        fixture
            .lots(Some(BEANS))
            .await
            .iter()
            .map(|l| l.id.clone())
            .collect::<Vec<_>>(),
        vec![first.clone(), lot_id(BEANS, OLAYA, "rcv-2")],
        "the counted sack moved behind the one received after it"
    );

    // A third delivery moves what a gram costs, and the count still says what it
    // said.
    fixture.receive(BEANS, &plain(5_000, 90_000, "rcv-3")).await;
    fixture.rebuild().await;
    let after = fixture
        .movements(BEANS)
        .await
        .into_iter()
        .find(|m| m.kind == "counted")
        .expect("still there");
    assert_eq!(after.value, counted.value, "the count was recomputed");

    // A lot nobody opened is refused rather than counted into existence.
    assert!(matches!(
        fixture.try_count(BEANS, "lot.nothing", 5, "cnt-2").await,
        Err(InventoryError::NoSuchLot(_))
    ));

    fixture.cleanup().await;
}

/// **A movement is recorded once however often it is sent**, and the lot a
/// delivery makes is named after the delivery.
///
/// The shelf's window of references already heard is what makes the retry
/// nothing, and it is checked first in the decision — before anything that could
/// refuse — so a retried movement is never refused for stock it has already
/// taken.
#[tokio::test]
async fn a_movement_is_recorded_once_however_often_it_is_sent() {
    let fixture = Fixture::new("inv-retry").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;

    let lot = fixture.receive(BEANS, &plain(5_000, 30_000, "rcv-1")).await;
    assert_eq!(
        lot,
        lot_id(BEANS, OLAYA, "rcv-1"),
        "a lot is derived from the shelf and the receipt, never minted"
    );

    // Every one of them, sent twice.
    fixture.receive(BEANS, &plain(5_000, 30_000, "rcv-1")).await;
    fixture
        .write_off(BEANS, &scrapped(Reason::Damaged, 1_000, "wo-1"))
        .await;
    fixture
        .write_off(BEANS, &scrapped(Reason::Damaged, 1_000, "wo-1"))
        .await;
    fixture.count(BEANS, &lot, 3_900, "cnt-1").await;
    fixture.count(BEANS, &lot, 3_900, "cnt-1").await;
    fixture.project().await;

    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 3_900);
    assert_eq!(
        fixture.movements(BEANS).await.len(),
        3,
        "a retried request wrote a second movement"
    );

    fixture.cleanup().await;
}

/// **A retried sale is not refused for the stock it already took.**
///
/// The shape that makes the order matter: a lot-tracked shelf the sale empties.
/// Judged before the retry check, the second call finds nothing on the shelf
/// and refuses (R1) — for a movement that has already happened. A plain product
/// with stock to spare would pass whichever order the checks were in, which is
/// why a review found the guard for this ordering did not guard it.
///
/// `sales::issue_in` no longer calls this on a retried invoice at all, so this
/// is the guard on `consume_in`'s own promise to any caller that does.
#[tokio::test]
async fn a_retried_consumption_is_not_refused_for_the_stock_it_already_took() {
    let fixture = Fixture::new("inv-retry-tracked").await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;
    fixture
        .receive(
            MILK,
            &Receipt {
                code: Some("B-2026-04".to_owned()),
                expires_on: Some(day("2026-06-01")),
                ..plain(3, 3_000, "rcv-milk")
            },
        )
        .await;

    fixture
        .consume(MILK, &sold(3, "inv-1.0"))
        .await
        .expect("the sale empties the shelf");
    fixture
        .consume(MILK, &sold(3, "inv-1.0"))
        .await
        .expect("the retry is the sale that already happened, not a short one");

    assert_eq!(fixture.shelf(MILK, Some(OLAYA)).await.on_hand(), 0);
    assert_eq!(fixture.value_on_hand().await, 0, "and it was costed once");

    fixture.cleanup().await;
}

/// **A shelf belongs to a branch.** What is at Olaya is at Olaya, and head
/// office writing off stock sitting in Malaz is the case that makes a
/// tenant-wide number meaningless.
#[tokio::test]
async fn a_shelf_belongs_to_a_branch() {
    let fixture = Fixture::new("inv-branch").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;

    fixture
        .try_receive(BEANS, &plain(5_000, 30_000, "rcv-1"), OLAYA)
        .await
        .expect("receives at Olaya");
    // **The same key at both**, which is what a client keying a retry loop on
    // the batch rather than on the row sends. Each shelf hears it for the first
    // time and records a delivery.
    fixture
        .try_receive(BEANS, &plain(1_000, 8_000, "rcv-1"), MALAZ)
        .await
        .expect("receives at Malaz");
    fixture.project().await;

    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 5_000);
    assert_eq!(fixture.shelf(BEANS, Some(MALAZ)).await.on_hand(), 1_000);
    assert_eq!(
        fixture.shelf(BEANS, None).await.on_hand(),
        0,
        "a branchless shelf is a third shelf, not the total"
    );

    assert_eq!(
        fixture
            .shelves()
            .await
            .iter()
            .map(|s| (s.branch.clone(), s.on_hand))
            .collect::<Vec<_>>(),
        vec![
            (Some(MALAZ.to_owned()), 1_000),
            (Some(OLAYA.to_owned()), 5_000),
        ],
    );

    // **And two lots, because a lot id carries its shelf.** Derived from the
    // key alone the two deliveries would share an id, which is a primary key
    // across the tenant: the read model would drop the second lot and the next
    // movement at Malaz would draw Olaya's down.
    assert_eq!(
        fixture
            .lots(Some(BEANS))
            .await
            .iter()
            .map(|l| (l.id.clone(), l.remaining))
            .collect::<Vec<_>>(),
        vec![
            (lot_id(BEANS, OLAYA, "rcv-1"), 5_000),
            (lot_id(BEANS, MALAZ, "rcv-1"), 1_000),
        ],
        "one key at two branches made one lot"
    );

    fixture.cleanup().await;
}

/// **A shelf nothing could ever leave takes no delivery.**
///
/// Every movement books an entry, and an entry names the branch, the currency
/// and the movement — so a branch nobody opened, a currency the inventory
/// account is not kept in, and a shelf too long to name an entry are each a
/// shelf that receives and never releases. A receipt posts too now, so two of
/// the three would be refused a moment later and with the same error;
/// `usable_shelf` is where all three are asked, at the one door onto a shelf,
/// and the currency is the one the ledger cannot phrase — it comes back from a
/// posting as `MixedCurrencies`, naming neither the shelf nor the cause.
///
/// The proof each one matters is the second half: the write-off that would have
/// been refused at the same branch.
#[tokio::test]
async fn a_shelf_no_movement_could_ever_leave_takes_no_delivery() {
    let fixture = Fixture::new("inv-stuck").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;

    // **A branch nobody opened** — a typo in a header, or a warehouse the
    // business never modelled. `ledger::post_entry_in` refuses it, so a receipt
    // refuses it, with the ledger's own answer rather than a second opinion.
    let refused = fixture
        .try_receive(BEANS, &plain(500, 4_000, "rcv-1"), "BRANCH-NOWHERE")
        .await
        .expect_err("a delivery to a branch that was never opened");
    assert!(
        matches!(
            refused,
            InventoryError::Ledger(ledger::LedgerError::NoSuchBranch(ref branch))
                if branch == "BRANCH-NOWHERE"
        ),
        "got {refused:?}",
    );
    assert_eq!(
        fixture.shelf(BEANS, Some("BRANCH-NOWHERE")).await.on_hand(),
        0,
        "nothing landed on a shelf that could never be emptied",
    );

    // **A currency the inventory account is not kept in.** The chart is SAR;
    // `1300` will refuse a USD line for ever, so the USD delivery is refused
    // now, while there is still nothing to strand.
    let refused = fixture
        .try_receive(
            BEANS,
            &Receipt {
                value: usd(10_000),
                ..plain(500, 0, "rcv-2")
            },
            OLAYA,
        )
        .await
        .expect_err("a delivery priced in a currency the books do not keep");
    assert!(
        matches!(
            refused,
            InventoryError::WrongCurrency { ref id, ref kept } if id == "1300" && kept == "SAR"
        ),
        "got {refused:?}",
    );

    // **A branch id that leaves no room to name the entry.** The shelf carries
    // the product and the branch, the entry carries the shelf and the
    // movement's key, and an `AggregateId` holds 128 characters — so a long
    // enough branch makes every write-off and every count at it unpostable
    // whatever key is used. The delivery is what gets refused.
    let long = format!("BRANCH-{}", "W".repeat(81));
    branches::open_branch(
        &fixture.db,
        &code(&long),
        &branches::Details {
            name: "مستودع".to_owned(),
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
        at("05"),
        &Metadata::default(),
    )
    .await
    .expect("the branch opens");

    let refused = fixture
        .try_receive(BEANS, &plain(500, 4_000, "rcv-3"), &long)
        .await
        .expect_err("a delivery onto a shelf no entry could be named for");
    assert!(
        matches!(
            refused,
            InventoryError::NotAReference { ref shelf, ref reference }
                if shelf.ends_with(&long) && reference == "rcv-3"
        ),
        "the refusal has to name the shelf that overflowed, not only the key: {refused:?}",
    );

    fixture.project().await;
    assert!(
        fixture.movements(BEANS).await.is_empty(),
        "three deliveries onto three unusable shelves, none of them recorded",
    );

    fixture.cleanup().await;
}

/// **A delivery has to look like the product it is of.** Silently ignoring a
/// batch code, or a serial, is how a machine ends up on a shelf with no name.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "nine shapes a delivery can be wrong in, on three products — \
              splitting them would split the argument they make together"
)]
async fn a_delivery_has_to_look_like_the_product_it_is_of() {
    let fixture = Fixture::new("inv-shapes").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;
    fixture
        .declare(GRINDER, "مطحنة", "piece", Tracking::Serial)
        .await;

    // A lot-tracked delivery names its batch.
    assert!(matches!(
        fixture
            .try_receive(MILK, &plain(24, 12_000, "rcv-1"), OLAYA)
            .await,
        Err(InventoryError::NeedsALotCode)
    ));
    // And nothing else may carry one, or a date.
    assert!(matches!(
        fixture
            .try_receive(
                BEANS,
                &Receipt {
                    code: Some("B-1".to_owned()),
                    ..plain(100, 1_000, "rcv-2")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::NotALotProduct(_))
    ));
    assert!(matches!(
        fixture
            .try_receive(
                BEANS,
                &Receipt {
                    expires_on: Some(day("2026-06-01")),
                    ..plain(100, 1_000, "rcv-3")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::NotALotProduct(_))
    ));
    // A serial-tracked delivery names one serial per unit, and no more.
    assert!(matches!(
        fixture
            .try_receive(
                GRINDER,
                &Receipt {
                    serials: vec!["SN-1".to_owned()],
                    ..plain(2, 60_000, "rcv-4")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::NeedsSerials { units: 2, named: 1 })
    ));
    assert!(matches!(
        fixture
            .try_receive(
                BEANS,
                &Receipt {
                    serials: vec!["SN-1".to_owned()],
                    ..plain(1, 1_000, "rcv-5")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::NotASerialProduct(_))
    ));
    // **And two units of one delivery cannot share a name.** Accepted, it puts
    // a unit on the shelf with no name at all: the write-off of that name takes
    // both copies off the lot and one from its quantity, and what is left can
    // never be moved by anything — not by name, not by quantity, and a
    // serial-tracked product is not counted.
    assert!(matches!(
        fixture
            .try_receive(
                GRINDER,
                &Receipt {
                    serials: vec!["SN-1".to_owned(), "SN-1".to_owned()],
                    ..plain(2, 60_000, "rcv-10")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::NeedsSerials { units: 2, named: 1 })
    ));

    // Money, and the currency the shelf is already carried in.
    fixture
        .try_receive(BEANS, &plain(5_000, 30_000, "rcv-6"), OLAYA)
        .await
        .expect("receives");
    assert!(matches!(
        fixture
            .try_receive(
                BEANS,
                &Receipt {
                    value: sar(0),
                    ..plain(10, 0, "rcv-7")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::NotAValue)
    ));
    assert!(matches!(
        fixture
            .try_receive(
                BEANS,
                &Receipt {
                    value: usd(10_000),
                    ..plain(10, 0, "rcv-8")
                },
                OLAYA,
            )
            .await,
        Err(InventoryError::WrongCurrency { .. })
    ));

    // And stock of a product nobody declared does not move at all.
    assert!(matches!(
        fixture
            .try_receive(
                "0195f1e2-2c4a-7c31-9f4e-6d1b8a0f3c21",
                &plain(1, 100, "rcv-9"),
                OLAYA,
            )
            .await,
        Err(InventoryError::NoSuchProduct(_))
    ));

    fixture.project().await;
    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 5_000);
    assert_eq!(fixture.lots(None).await.len(), 1, "one delivery landed");

    fixture.cleanup().await;
}

/// **More than is on the shelf is refused**, and so is a named lot that cannot
/// cover what was asked of it.
///
/// This is somebody holding the goods, not a till. A till may not stop for a bad
/// count; a person looking at a shelf may be told the shelf says otherwise.
#[tokio::test]
async fn a_write_off_takes_no_more_than_is_there() {
    let fixture = Fixture::new("inv-short").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;

    let first = fixture.receive(BEANS, &plain(1_000, 5_000, "rcv-1")).await;
    fixture.receive(BEANS, &plain(1_000, 9_000, "rcv-2")).await;

    assert!(
        matches!(
            fixture
                .try_write_off(BEANS, &scrapped(Reason::Damaged, 2_500, "wo-1"))
                .await,
            Err(InventoryError::NotEnoughStock {
                held: 2_000,
                wanted: 2_500
            })
        ),
        "the shelf was taken below zero"
    );

    // **Naming a lot is a claim about that lot**, so it is not topped up from
    // the next one.
    assert!(matches!(
        fixture
            .try_write_off(
                BEANS,
                &WriteOff {
                    lot: Some(first),
                    ..scrapped(Reason::Damaged, 1_200, "wo-2")
                },
            )
            .await,
        Err(InventoryError::LotIsShort { held: 1_000, .. })
    ));
    assert!(matches!(
        fixture
            .try_write_off(
                BEANS,
                &WriteOff {
                    lot: Some("lot.nothing".to_owned()),
                    ..scrapped(Reason::Damaged, 1, "wo-3")
                },
            )
            .await,
        Err(InventoryError::NoSuchLot(_))
    ));

    // And naming it works, which is what the override is for: the second sack
    // goes even though the first one would have.
    fixture
        .write_off(
            BEANS,
            &WriteOff {
                lot: Some(lot_id(BEANS, OLAYA, "rcv-2")),
                ..scrapped(Reason::Expired, 1_000, "wo-4")
            },
        )
        .await;
    fixture.project().await;

    let shelf = fixture.shelf(BEANS, Some(OLAYA)).await;
    assert_eq!(shelf.on_hand(), 1_000);
    assert_eq!(shelf.value().expect("sums"), Some(sar(5_000)));
    assert_eq!(
        fixture.lots(None).await.len(),
        1,
        "the named lot emptied and closed"
    );
    assert_eq!(
        fixture
            .movements(BEANS)
            .await
            .into_iter()
            .find(|m| m.kind == "written_off")
            .and_then(|m| m.reason),
        Some("expired".to_owned()),
    );

    fixture.cleanup().await;
}

/// **A product declared a moment ago is usable**, because existence is asked of
/// the log and not of the read model.
///
/// No projection run between declaring and receiving, on purpose: a check
/// against `proj_inventory.product` would tell somebody the product they just
/// created does not exist. The same argument `crm::accepts_documents` makes.
#[tokio::test]
async fn a_product_is_usable_before_the_worker_has_seen_it() {
    let fixture = Fixture::new("inv-fresh").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;

    let mut conn = fixture.pool.acquire().await.expect("connection");
    assert!(
        inventory::accepts_movements(&mut conn, &code(BEANS))
            .await
            .expect("asks the log")
    );
    assert!(
        !inventory::accepts_movements(&mut conn, &code(MILK))
            .await
            .expect("asks the log")
    );
    drop(conn);

    fixture
        .try_receive(BEANS, &plain(5_000, 30_000, "rcv-1"), OLAYA)
        .await
        .expect("a product declared a moment ago accepts stock");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// What it costs
// ---------------------------------------------------------------------------

/// **A consumption costs exactly the lots it took**, and two lots at two prices
/// are two different numbers rather than one average.
///
/// The whole argument for per-lot costing, in one entry. Six hundred grams off
/// a five-hundred-gram roast at 6.00 the hundred and a thousand-gram roast at
/// 8.00 the hundred is 30.00 + 8.00 = 38.00. A weighted average across the
/// shelf would have charged 36.67 for the same six hundred grams and the margin
/// on the cup would have been wrong in both directions.
///
/// `sales::issue_in` is what reaches `consume_in` in the product; this test
/// takes the invoice out of it so the costing is the only thing on trial.
#[tokio::test]
async fn a_consumption_posts_exactly_what_the_lots_it_took_cost() {
    let fixture = Fixture::new("inv-cogs").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;
    // Cheap beans first out, dear beans after — plain FIFO, undated.
    fixture.receive(BEANS, &plain(500, 3_000, "rcv-1")).await;
    fixture.receive(BEANS, &plain(1_000, 8_000, "rcv-2")).await;

    fixture
        .consume(BEANS, &sold(600, "inv-1.line-1"))
        .await
        .expect("the line depletes the shelf");

    // The entry is named after the shelf and the document line, derived and
    // never minted (L8) — which is why a caller can find it without asking.
    let entry = inventory::cost_entry_of(
        &inventory::stock_id(&code(BEANS), Some(OLAYA)).expect("a key"),
        "inv-1.line-1",
    );
    let lines = ledger::posted_lines(&fixture.db, &code(&entry))
        .await
        .expect("the consumption posted");
    let on = |account: &str| {
        lines
            .as_slice()
            .iter()
            .filter(|l| l.account.as_str() == account)
            .map(|l| l.amount.minor())
            .sum::<i64>()
    };
    assert_eq!(
        on("5010"),
        3_800,
        "500g at 6.00/100 and 100g at 8.00/100 — an average would have said 3,667"
    );
    assert_eq!(on("1300"), -3_800, "off the asset by the same");

    // And the shelf agrees with the books, which is the whole point.
    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 900);
    assert_eq!(fixture.value_on_hand().await, 7_200);

    fixture.cleanup().await;
}

/// **A delivery is in the books the moment it lands** (decision R3).
///
/// `Dr 1300 Inventory`, `Cr 2010 Goods received, not invoiced`, at what the
/// delivery cost, in the transaction that writes the movement. The goods are an
/// asset as soon as they are in the building — they can be sold and they can
/// spoil — and what they owe is not accounts payable until somebody bills it.
///
/// Until 2026-09-13 a receipt posted nothing and the supplier's bill debited
/// the asset, so between the two the books said the shelves were empty and the
/// health check called every delivery a violation.
#[tokio::test]
async fn a_delivery_debits_the_shelf_and_credits_what_is_not_yet_invoiced() {
    let fixture = Fixture::new("inv-received").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;
    fixture.receive(BEANS, &plain(5_000, 30_000, "rcv-1")).await;

    assert_eq!(fixture.balance("1300").await, 30_000, "onto the shelf");
    assert_eq!(
        fixture.balance("2010").await,
        -30_000,
        "owed for, and not to the payable until a bill says so"
    );
    assert_eq!(
        fixture.balance("2000").await,
        0,
        "nobody has been invoiced by anybody"
    );
    assert_eq!(fixture.value_on_hand().await, 30_000);

    // **A retried delivery is one delivery, in the books as well as on the
    // shelf.** The entry is named after the shelf and the movement's own key,
    // so re-posting it is the ledger's no-op too.
    fixture.receive(BEANS, &plain(5_000, 30_000, "rcv-1")).await;
    assert_eq!(fixture.balance("1300").await, 30_000);
    assert_eq!(fixture.balance("2010").await, -30_000);

    fixture.cleanup().await;
}

/// **A delivery with nowhere to put what it owes is refused** (L6).
///
/// The posting is in the transaction that writes the movement, so a tenant who
/// has closed the holding account gets the account named and no stock — rather
/// than goods on a shelf and a liability nobody recorded. The same rule a
/// write-off lives under, now that a receipt posts too.
#[tokio::test]
async fn a_closed_holding_account_refuses_the_delivery() {
    let fixture = Fixture::new("inv-no-grni").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;

    ledger::close_account(&fixture.db, &code("2010"), &Metadata::default())
        .await
        .expect("the accountant closes it");

    let refused = fixture
        .try_receive(BEANS, &plain(5_000, 30_000, "rcv-1"), OLAYA)
        .await
        .expect_err("a delivery with nowhere to put what it owes");
    assert!(
        matches!(
            refused,
            InventoryError::Ledger(ledger::LedgerError::AccountClosed(ref account))
                if account == "2010"
        ),
        "got {refused:?}",
    );

    // **And nothing happened at all** — not the lot, not a debit to `1300` on
    // its own.
    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 0);
    assert_eq!(fixture.balance("1300").await, 0);
    assert_eq!(fixture.value_on_hand().await, 0);

    fixture.cleanup().await;
}

/// **A count books what it found**: short is a loss, over is the same entry
/// the other way round, and level is no entry at all.
///
/// The argument is `pos`'s about a till drawer: a shelf that records a shortage
/// and does not book it leaves the ledger saying the business holds what it does
/// not, and the next reconciliation inherits it.
#[tokio::test]
async fn a_count_books_what_it_found_and_nothing_when_it_found_nothing() {
    let fixture = Fixture::new("inv-variance").await;
    fixture
        .declare(BEANS, "كرواسان", "piece", Tracking::None)
        .await;
    let short = fixture.receive(BEANS, &plain(60, 18_000, "rcv-1")).await;
    let over = fixture.receive(BEANS, &plain(60, 18_000, "rcv-2")).await;
    let level = fixture.receive(BEANS, &plain(60, 18_000, "rcv-3")).await;

    // The three deliveries are already on the asset, at 180.00 each.
    let delivered = 3 * 18_000;
    assert_eq!(fixture.balance("1300").await, delivered);

    // Sixty on the books, fifty-seven on the tray: three at 3.00 each.
    fixture.count(BEANS, &short, 57, "cnt-1").await;
    assert_eq!(fixture.balance("5900").await, 900, "the loss is booked");
    assert_eq!(
        fixture.balance("1300").await,
        delivered - 900,
        "and off the asset"
    );

    // And two more than anybody expected, on the next tray.
    fixture.count(BEANS, &over, 62, "cnt-2").await;
    assert_eq!(
        fixture.balance("5900").await,
        900 - 600,
        "an overage is the same entry the other way round"
    );
    assert_eq!(fixture.balance("1300").await, delivered - 900 + 600);

    // **A count that found exactly what the books said posts nothing** — not a
    // pair of zero lines, and not a refusal.
    let before = fixture.balance("1300").await;
    fixture.count(BEANS, &level, 60, "cnt-3").await;
    assert_eq!(fixture.balance("1300").await, before);

    fixture.project().await;
    assert_eq!(
        fixture
            .movements(BEANS)
            .await
            .iter()
            .filter(|m| m.kind == "counted")
            .count(),
        3,
        "all three counts are recorded; only two of them moved money"
    );

    fixture.cleanup().await;
}

/// **A write-off books its loss where write-offs go**, valued at what the lots
/// it removed were carried at — and the reason stays on the movement, which is
/// where a loss is explained.
///
/// The reason does not choose an account. Expired and damaged are the same
/// expense and a different conversation, and an account code cannot hold four
/// words.
#[tokio::test]
async fn a_write_off_books_its_loss_and_keeps_its_reason() {
    let fixture = Fixture::new("inv-waste").await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;
    fixture
        .receive(
            MILK,
            &Receipt {
                code: Some("B-2026-04-05".to_owned()),
                expires_on: Some(day("2026-04-11")),
                ..plain(24, 12_000, "rcv-1")
            },
        )
        .await;

    fixture
        .write_off(MILK, &scrapped(Reason::Expired, 24, "wof-1"))
        .await;

    assert_eq!(fixture.balance("5900").await, 12_000, "the crate is a loss");
    assert_eq!(
        fixture.balance("1300").await,
        0,
        "the delivery put it on the asset and the write-off took it off again"
    );

    fixture.project().await;
    assert_eq!(
        fixture
            .movements(MILK)
            .await
            .iter()
            .find(|m| m.kind == "written_off")
            .and_then(|m| m.reason.clone()),
        Some("expired".to_owned()),
        "the account says what it cost and the movement says why",
    );

    fixture.cleanup().await;
}

/// **A closed account refuses the movement rather than posting it somewhere
/// else** (L6).
///
/// The posting is in the transaction that writes the movement, so a ledger that
/// refuses takes the stock movement with it: the shelf does not come down into
/// books that were never told. The tenant sees the account named, which is the
/// one thing the person fixing it needs.
#[tokio::test]
async fn a_closed_account_refuses_the_movement_rather_than_posting_elsewhere() {
    let fixture = Fixture::new("inv-closed").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;
    fixture.receive(BEANS, &plain(1_000, 8_000, "rcv-1")).await;

    ledger::close_account(&fixture.db, &code("5900"), &Metadata::default())
        .await
        .expect("the accountant closes it");

    let refused = fixture
        .try_write_off(BEANS, &scrapped(Reason::Damaged, 100, "wof-1"))
        .await
        .expect_err("a write-off with nowhere to book the loss");
    assert!(
        matches!(
            refused,
            InventoryError::Ledger(ledger::LedgerError::AccountClosed(ref account))
                if account == "5900"
        ),
        "got {refused:?}",
    );

    // **And nothing happened at all.** Not the movement, not a posting to some
    // other account, not a half-written transaction.
    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 1_000);
    fixture.project().await;
    assert!(
        fixture
            .movements(BEANS)
            .await
            .iter()
            .all(|m| m.kind == "received")
    );
    assert_eq!(
        fixture.balance("1300").await,
        8_000,
        "the delivery's own entry stands; nothing of the write-off does"
    );

    fixture.cleanup().await;
}

/// **The canary decision R3 makes load-bearing: what the shelves are worth, and
/// what the inventory account says they are worth.**
///
/// Two numbers built by two routes that share nothing but the log — one from
/// `proj_inventory`, one from `proj_ledger`. This module writes `1300` at both
/// ends now: a delivery debits it, and every movement out credits it, each in
/// the transaction that writes the movement. So the two agree **from the first
/// delivery onwards** rather than from the day somebody types the invoice in,
/// and a difference means a movement that did not post or an entry made against
/// the account by hand.
///
/// The supplier's bill is stood in for here by the journal entry `purchases`
/// makes — `Dr 2010`, `Cr 2000` — because that account is the one it touches
/// now, and this test does not have `purchases`.
///
/// The comparison lives in a test and in `bin/worker`'s `StockValueAgrees`
/// rather than in the module, because it needs two projection groups and L3
/// forbids a module from reading across them. `inventory::value_on_hand` is the
/// half that belongs here.
#[tokio::test]
async fn the_value_on_hand_agrees_with_the_inventory_account() {
    let fixture = Fixture::new("inv-agrees").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;
    fixture.receive(BEANS, &plain(500, 3_000, "rcv-1")).await;
    fixture.receive(BEANS, &plain(1_000, 8_000, "rcv-2")).await;

    // **Before any invoice**, which is the half that used to be a false alarm.
    // The deliveries are on the asset and owed for.
    assert_eq!(fixture.balance("1300").await, fixture.value_on_hand().await);
    assert_eq!(
        fixture.balance("2010").await,
        -11_000,
        "received and not invoiced, which is what that account is for"
    );

    // The supplier's bill: what `purchases` posts, said in the ledger's own
    // words because this test does not have `purchases`. It moves what is owed
    // from the holding account to the payable and leaves the asset alone.
    billed(&fixture, 11_000, "ap-1").await;
    assert_eq!(fixture.balance("2010").await, 0, "the bill cleared it");
    assert_eq!(fixture.balance("1300").await, fixture.value_on_hand().await);

    // Everything that can move value: a sale's line, a count, a write-off.
    fixture
        .consume(BEANS, &sold(600, "inv-1.line-1"))
        .await
        .expect("consumes");
    fixture
        .write_off(BEANS, &scrapped(Reason::Damaged, 100, "wof-1"))
        .await;
    let lot = lot_id(BEANS, OLAYA, "rcv-2");
    fixture.count(BEANS, &lot, 700, "cnt-1").await;

    let held = fixture.value_on_hand().await;
    assert!(held > 0, "a shelf worth nothing proves nothing");
    assert_eq!(
        fixture.balance("1300").await,
        held,
        "the books and the shelves disagree about what the stock is worth",
    );

    // **And the check bites.** A debit to the stock account that no delivery
    // put there — a correction typed into the wrong account, made with the
    // ledger's own route — and the two part company by exactly that much.
    by_hand(&fixture, 5_000, "je-2").await;
    assert_eq!(
        fixture.balance("1300").await,
        held + 5_000,
        "a stray debit is exactly what this comparison exists to find",
    );
    assert_ne!(fixture.balance("1300").await, fixture.value_on_hand().await);

    fixture.cleanup().await;
}

/// The count movements on one product, in the order they were written — each
/// as the lot it landed on, what it did, and what the count said it found.
async fn counted_rows(
    fixture: &Fixture,
    product: &str,
) -> Vec<(Option<String>, i64, i64, Option<i64>, Option<i64>)> {
    fixture.project().await;
    let mut rows: Vec<_> = fixture
        .movements(product)
        .await
        .into_iter()
        .filter(|m| m.kind == "counted")
        .collect();
    rows.sort_by_key(|m| (m.position, m.seq));
    rows.into_iter()
        .map(|m| (m.lot, m.quantity, m.value, m.expected, m.declared))
        .collect()
}

/// **A count of the shelf takes a shortage off the lots in picking order** (R2),
/// each portion at its own lot's cost — the rule a sale takes stock by, not a
/// second one — and the same count sent twice counts once.
#[tokio::test]
async fn a_shelf_count_takes_a_shortage_off_the_lots_in_picking_order() {
    let fixture = Fixture::new("inv-shelf-short").await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;
    // Received in one order and expiring in another.
    for (reference, minor, expires) in [
        ("rcv-1", 6_000, "2026-04-21"),
        ("rcv-2", 7_200, "2026-04-11"),
        ("rcv-3", 9_600, "2026-05-02"),
    ] {
        fixture
            .receive(
                MILK,
                &Receipt {
                    code: Some("B-2026-04".to_owned()),
                    expires_on: Some(day(expires)),
                    ..plain(12, minor, reference)
                },
            )
            .await;
    }

    // Thirty-six on the books and twenty in the fridge: sixteen short. The
    // whole of the crate expiring first, then four of the next — and the May
    // crate is not touched.
    fixture
        .try_counted(MILK, &shelf_count(20, "cnt-1"))
        .await
        .expect("counts");
    fixture
        .try_counted(MILK, &shelf_count(20, "cnt-1"))
        .await
        .expect("a retried count is the count that already happened");

    assert_eq!(
        counted_rows(&fixture, MILK).await,
        vec![
            (
                Some(lot_id(MILK, OLAYA, "rcv-2")),
                -12,
                -7_200,
                Some(36),
                Some(20)
            ),
            (
                Some(lot_id(MILK, OLAYA, "rcv-1")),
                -4,
                -2_000,
                Some(36),
                Some(20)
            ),
        ],
        "the shortage comes off the crate expiring first, then the next, each at its own \
         cost — and counted once"
    );
    assert_eq!(
        fixture.balance("5900").await,
        7_200 + 2_000,
        "the loss is what those lots carried the milk at; an average would have said 101.33"
    );
    assert_eq!(fixture.balance("1300").await, fixture.value_on_hand().await);

    let shelf = fixture.shelf(MILK, Some(OLAYA)).await;
    assert_eq!(shelf.on_hand(), 20);
    assert_eq!(
        shelf
            .lot(&lot_id(MILK, OLAYA, "rcv-3"))
            .map(|lot| lot.quantity),
        Some(12),
        "the May crate goes out last, so it is the last a shortage comes off"
    );

    fixture.cleanup().await;
}

/// **An overage joins the newest lot, at that lot's own cost** — and a count
/// that found exactly what the books said posts nothing and is still recorded.
#[tokio::test]
async fn an_overage_joins_the_newest_lot_at_its_own_cost() {
    let fixture = Fixture::new("inv-shelf-over").await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;
    let older = fixture.receive(BEANS, &plain(5_000, 30_000, "rcv-1")).await;
    let newer = fixture.receive(BEANS, &plain(5_000, 40_000, "rcv-2")).await;

    // Two hundred grams more than the books say. They join the sack received
    // last, at what a gram of that sack cost.
    fixture
        .try_counted(BEANS, &shelf_count(10_200, "cnt-1"))
        .await
        .expect("counts");
    assert_eq!(fixture.balance("1300").await, 70_000 + 1_600);
    assert_eq!(
        fixture.balance("5900").await,
        -1_600,
        "a gain, at 0.08 a gram"
    );

    // Found exactly what the books said: nothing posts.
    fixture
        .try_counted(BEANS, &shelf_count(10_200, "cnt-2"))
        .await
        .expect("counts");
    assert_eq!(
        fixture.balance("1300").await,
        70_000 + 1_600,
        "a count that found the books right posted something"
    );
    assert_eq!(fixture.balance("5900").await, -1_600);

    assert_eq!(
        counted_rows(&fixture, BEANS).await,
        vec![
            (Some(newer.clone()), 200, 1_600, Some(10_000), Some(10_200)),
            (None, 0, 0, Some(10_200), Some(10_200)),
        ],
        "the overage on the newest sack, and the level count still a movement"
    );
    let lots = fixture.lots(Some(BEANS)).await;
    assert_eq!(
        lots.iter()
            .map(|l| (l.id.clone(), l.remaining, l.value))
            .collect::<Vec<_>>(),
        vec![(older, 5_000, 30_000), (newer, 5_200, 41_600)],
        "the found grams sit on the newest sack at its own price, and the older is untouched"
    );
    assert_eq!(fixture.value_on_hand().await, fixture.balance("1300").await);

    fixture.cleanup().await;
}

/// **A count of the shelf settles what a plain product owes.**
///
/// A till sold three croissants the shelf did not have, charged out at the last
/// unit cost. The delivery that had covered them lands late, and the count finds
/// the tray holding what the books say: the debt comes back at what it was
/// charged out at and the three come off the real lot at what they cost, so the
/// only thing posted is the gap between the guess and the price. Then the shelf
/// is sold short again and counted empty: the debt comes back and the negative
/// asset with it.
#[tokio::test]
async fn a_count_settles_what_a_plain_product_owes() {
    let fixture = Fixture::new("inv-shelf-owed").await;
    fixture
        .declare(BEANS, "كرواسان", "piece", Tracking::None)
        .await;
    fixture.receive(BEANS, &plain(10, 3_000, "rcv-1")).await;
    fixture
        .consume(BEANS, &sold(13, "inv-1.line-1"))
        .await
        .expect("a till does not stop");
    let shelf = fixture.shelf(BEANS, Some(OLAYA)).await;
    assert_eq!(shelf.on_hand(), -3);
    assert_eq!(
        shelf.owed,
        Some(Shortfall {
            quantity: 3,
            cost: Some(sar(900)),
        })
    );

    // The delivery nobody had typed in: five at 4.00. It does not pay the debt.
    fixture.receive(BEANS, &plain(5, 2_000, "rcv-2")).await;
    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.on_hand(), 2);

    fixture
        .try_counted(BEANS, &shelf_count(2, "cnt-1"))
        .await
        .expect("counts");
    let shelf = fixture.shelf(BEANS, Some(OLAYA)).await;
    assert_eq!(shelf.owed, None, "the count cleared the debt");
    assert_eq!(shelf.on_hand(), 2);
    assert_eq!(
        shelf
            .lots
            .iter()
            .map(|lot| (lot.id.clone(), lot.quantity, lot.value.minor()))
            .collect::<Vec<_>>(),
        vec![(lot_id(BEANS, OLAYA, "rcv-2"), 2, 800)],
        "the three the debt stood for came off the real lot"
    );
    assert_eq!(
        fixture.balance("5900").await,
        1_200 - 900,
        "12.00 of croissants against the 9.00 guess they were sold at"
    );
    assert_eq!(fixture.balance("1300").await, 800);
    assert_eq!(fixture.value_on_hand().await, 800);

    // Sold short again, off the two left, and the tray counted empty.
    fixture
        .consume(BEANS, &sold(5, "inv-2.line-1"))
        .await
        .expect("a till does not stop");
    assert_eq!(fixture.balance("1300").await, 800 - 800 - 1_200);
    fixture
        .try_counted(BEANS, &shelf_count(0, "cnt-2"))
        .await
        .expect("counts");
    let shelf = fixture.shelf(BEANS, Some(OLAYA)).await;
    assert_eq!(shelf.owed, None);
    assert_eq!(shelf.on_hand(), 0);
    assert_eq!(
        fixture.balance("1300").await,
        0,
        "the asset stops being negative"
    );
    assert_eq!(fixture.balance("5900").await, 300 - 1_200);
    assert_eq!(fixture.value_on_hand().await, 0);

    // **A croissant found on an empty tray has no lot to join**, and nothing to
    // say what it cost: it comes in as a delivery, not as a count.
    assert!(matches!(
        fixture.try_counted(BEANS, &shelf_count(1, "cnt-3")).await,
        Err(InventoryError::NoLotToJoin { found: 1 })
    ));

    let rows = counted_rows(&fixture, BEANS).await;
    assert_eq!(
        rows.last(),
        Some(&(None, 3, 1_200, Some(-3), Some(0))),
        "the settled debt is a row with no lot, at what it was charged out at"
    );
    assert_eq!(
        fixture
            .movements(BEANS)
            .await
            .iter()
            .map(|m| m.quantity)
            .sum::<i64>(),
        0,
        "the shelf holds a quantity its movements do not explain"
    );

    fixture.cleanup().await;
}

/// **A sale's units come back as stock once a count has cleared what that sale
/// owed** — never as a debt the shelf no longer has.
///
/// Ten croissants received at 3.00 and thirteen sold: the shelf owes three,
/// charged out at 9.00. The tray is counted empty, which clears the debt. The
/// invoice is then undone in two credit notes. Before review the fold a return
/// decides from never heard the count, so the return "settled" three units the
/// shelf no longer owed and the aggregate lost them while the books and the
/// read model counted them.
///
/// The second product is the bakery that never receives what it bakes: a
/// shortfall with no cost, counted, returned — three units that come back at
/// nothing, in the currency the inventory account is kept in.
#[tokio::test]
async fn a_return_after_a_count_cleared_the_debt_puts_the_units_back() {
    let fixture = Fixture::new("inv-back-after-count").await;
    fixture
        .declare(BEANS, "كرواسان", "piece", Tracking::None)
        .await;
    let rcv = fixture.receive(BEANS, &plain(10, 3_000, "rcv-1")).await;
    fixture
        .consume(BEANS, &sold(13, "inv-1.line-1"))
        .await
        .expect("a till does not stop");
    fixture
        .try_counted(BEANS, &shelf_count(0, "cnt-1"))
        .await
        .expect("counts");
    assert_eq!(fixture.shelf(BEANS, Some(OLAYA)).await.owed, None);

    let back = |taken_on: &str, quantity: Option<i64>, reference: &str| Restoration {
        taken_on: taken_on.to_owned(),
        branch: Some(OLAYA.to_owned()),
        quantity,
        reference: reference.to_owned(),
        serials: Vec::new(),
        at: at("17"),
    };
    let shelf_id = inventory::stock_id(&code(BEANS), Some(OLAYA)).expect("a key");
    let found = inventory::returned_lot_of(&shelf_id, "inv-1.line-1");
    // Two of the three the count cleared, then everything the sale still has
    // out — which is one of those and the ten off the lot.
    fixture
        .restore(BEANS, &back("inv-1.line-1", Some(2), "r.CN-00001.0"))
        .await
        .expect("two come back");
    fixture.project().await;
    let opened = fixture.lot(&found).await.recorded_at;
    let emptied = fixture.lot(&rcv).await.recorded_at;
    fixture
        .restore(BEANS, &back("inv-1.line-1", None, "r.CN-00002.0"))
        .await
        .expect("the rest come back");
    // **When a lot came onto the shelf**, which the worker's check that the
    // bell rang counts from: the receipt's batch the sale emptied came back
    // with this return, and counting from its receipt would call a lot nobody
    // could have been told about a broken bell. The returned lot was open, and
    // one more unit on it is not a new arrival.
    fixture.project().await;
    assert!(
        fixture.lot(&rcv).await.recorded_at > emptied,
        "the reopened batch still says it came onto the shelf with its receipt"
    );
    assert_eq!(
        fixture.lot(&found).await.recorded_at,
        opened,
        "a unit joining a lot still open moved when that lot came onto the shelf"
    );
    let expected = vec![(found.clone(), 3, 900), (rcv.clone(), 10, 3_000)];
    let shelf = fixture.shelf(BEANS, Some(OLAYA)).await;
    assert_eq!(
        shelf
            .lots
            .iter()
            .map(|lot| (lot.id.clone(), lot.quantity, lot.value.minor()))
            .collect::<Vec<_>>(),
        expected,
        "the units the customer brought back vanished from the shelf, or came back twice"
    );
    assert_eq!(shelf.on_hand(), 13);
    assert_eq!(fixture.balance("1300").await, 3_900);
    assert_eq!(
        fixture.balance("5010").await,
        0,
        "the sale is wholly undone"
    );

    fixture.project().await;
    for pass in ["projected", "rebuilt"] {
        let mut lots: Vec<_> = fixture
            .lots(Some(BEANS))
            .await
            .into_iter()
            .map(|l| (l.id, l.remaining, l.value))
            .collect();
        lots.sort();
        let mut sorted = expected.clone();
        sorted.sort();
        assert_eq!(lots, sorted, "{pass}: the read model's lots");
        assert_eq!(fixture.value_on_hand().await, 3_900, "{pass}");
        if pass == "projected" {
            fixture.rebuild().await;
        }
    }

    // Never received: sold, counted, returned at nothing.
    fixture.declare(MILK, "خبز", "piece", Tracking::None).await;
    fixture
        .consume(MILK, &sold(3, "inv-2.line-1"))
        .await
        .expect("a till does not stop");
    fixture
        .try_counted(MILK, &shelf_count(0, "cnt-2"))
        .await
        .expect("counts");
    fixture
        .restore(MILK, &back("inv-2.line-1", None, "r.CN-00003.0"))
        .await
        .expect("three come back at nothing");
    let shelf = fixture.shelf(MILK, Some(OLAYA)).await;
    assert_eq!(shelf.on_hand(), 3);
    assert_eq!(shelf.value().expect("sums"), Some(sar(0)));

    fixture.cleanup().await;
}

/// **An overage joins the lot that goes out last**, which a return reopening an
/// older batch does not change.
///
/// Two batches of milk; a sale empties the one expiring first and a credit note
/// brings one bottle of it back, which reopens that batch at the end of the
/// shelf's list. Before review the overage took the end of the *list*, so the
/// bottle found by the count was dated to the batch expiring first.
#[tokio::test]
async fn an_overage_joins_the_lot_that_goes_out_last_after_a_return() {
    let fixture = Fixture::new("inv-over-reopened").await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;
    let early = fixture
        .receive(
            MILK,
            &Receipt {
                code: Some("A".to_owned()),
                expires_on: Some(day("2026-04-11")),
                ..plain(2, 2_000, "rcv-1")
            },
        )
        .await;
    let late = fixture
        .receive(
            MILK,
            &Receipt {
                code: Some("B".to_owned()),
                expires_on: Some(day("2026-05-02")),
                ..plain(12, 12_000, "rcv-2")
            },
        )
        .await;
    fixture
        .consume(MILK, &sold(2, "inv-1.line-1"))
        .await
        .expect("sells");
    fixture
        .restore(
            MILK,
            &Restoration {
                taken_on: "inv-1.line-1".to_owned(),
                branch: Some(OLAYA.to_owned()),
                quantity: Some(1),
                serials: Vec::new(),
                reference: "r.CN-00001.0".to_owned(),
                at: at("15"),
            },
        )
        .await
        .expect("one comes back");

    fixture
        .try_counted(MILK, &shelf_count(14, "cnt-1"))
        .await
        .expect("counts");
    let shelf = fixture.shelf(MILK, Some(OLAYA)).await;
    assert_eq!(
        (
            shelf.lot(&early).map(|lot| lot.quantity),
            shelf.lot(&late).map(|lot| lot.quantity),
        ),
        (Some(1), Some(13)),
        "the found bottle joined the batch expiring first"
    );
    assert_eq!(
        fixture.balance("5900").await,
        -1_000,
        "at batch B's own cost"
    );

    fixture.cleanup().await;
}

/// **A serial-tracked shelf is counted by naming what was found**, and what was
/// on hand and not named leaves at its own lot's cost. A name that is not on the
/// shelf is refused (decision 17): a count corrects a quantity, never an
/// identity.
#[tokio::test]
async fn a_serial_count_names_what_it_found_and_the_rest_leave() {
    let fixture = Fixture::new("inv-shelf-serial").await;
    fixture
        .declare(GRINDER, "مطحنة", "piece", Tracking::Serial)
        .await;
    fixture
        .receive(
            GRINDER,
            &Receipt {
                serials: vec!["SN-1".to_owned(), "SN-2".to_owned()],
                ..plain(2, 60_000, "rcv-1")
            },
        )
        .await;
    fixture
        .receive(
            GRINDER,
            &Receipt {
                serials: vec!["SN-3".to_owned()],
                ..plain(1, 40_000, "rcv-2")
            },
        )
        .await;

    assert!(
        matches!(
            fixture
                .try_counted(GRINDER, &named(2, &["SN-1", "SN-9"], "cnt-0"))
                .await,
            Err(InventoryError::NoSuchSerial(ref s)) if s == "SN-9"
        ),
        "a unit nobody received was counted into existence"
    );
    assert!(matches!(
        fixture
            .try_counted(GRINDER, &named(2, &["SN-1"], "cnt-0"))
            .await,
        Err(InventoryError::NeedsSerials { units: 2, named: 1 })
    ));

    fixture
        .try_counted(GRINDER, &named(1, &["SN-1"], "cnt-1"))
        .await
        .expect("counts");

    let shelf = fixture.shelf(GRINDER, Some(OLAYA)).await;
    assert_eq!(shelf.on_hand(), 1);
    assert_eq!(
        shelf
            .lots
            .iter()
            .map(|lot| (lot.serials.clone(), lot.value.minor()))
            .collect::<Vec<_>>(),
        vec![(vec!["SN-1".to_owned()], 30_000)]
    );
    assert_eq!(
        fixture.balance("5900").await,
        30_000 + 40_000,
        "each missing machine at what its own delivery cost"
    );
    assert_eq!(fixture.balance("1300").await, fixture.value_on_hand().await);
    assert_eq!(
        counted_rows(&fixture, GRINDER).await,
        vec![
            (
                Some(lot_id(GRINDER, OLAYA, "rcv-1")),
                -1,
                -30_000,
                Some(3),
                Some(1)
            ),
            (
                Some(lot_id(GRINDER, OLAYA, "rcv-2")),
                -1,
                -40_000,
                Some(3),
                Some(1)
            ),
        ]
    );
    assert_eq!(
        fixture
            .lots(Some(GRINDER))
            .await
            .iter()
            .map(|l| l.serials.clone())
            .collect::<Vec<_>>(),
        vec![vec!["SN-1".to_owned()]],
        "the read model still lists a machine the count did not find"
    );

    // And one that has gone cannot be named either.
    assert!(matches!(
        fixture
            .try_counted(GRINDER, &named(1, &["SN-2"], "cnt-2"))
            .await,
        Err(InventoryError::NoSuchSerial(_))
    ));

    fixture.cleanup().await;
}

/// **One serial is on a shelf once, however it got back there.**
///
/// A machine sold, then received again under the same name — repaired, bought
/// back — and then the first sale's credit note. `receive` refused only a name
/// the shelf was holding and a restoration asked nothing, so the credit note
/// put a second copy of `SN-1` on the shelf.
#[tokio::test]
async fn a_restoration_cannot_put_a_serial_on_the_shelf_twice() {
    let fixture = Fixture::new("inv-serial-twice").await;
    fixture
        .declare(GRINDER, "مطحنة", "piece", Tracking::Serial)
        .await;
    let one = |reference: &str| Receipt {
        serials: vec!["SN-1".to_owned()],
        ..plain(1, 30_000, reference)
    };
    fixture.receive(GRINDER, &one("rcv-1")).await;
    fixture
        .consume(
            GRINDER,
            &Consumption {
                quantity: None,
                lot: None,
                serials: vec!["SN-1".to_owned()],
                reference: "inv-1.line-1".to_owned(),
                at: at("14"),
            },
        )
        .await
        .expect("sells");
    fixture.receive(GRINDER, &one("rcv-2")).await;

    assert!(
        matches!(
            fixture
                .restore(
                    GRINDER,
                    &Restoration {
                        taken_on: "inv-1.line-1".to_owned(),
                        branch: Some(OLAYA.to_owned()),
                        quantity: None,
                        serials: Vec::new(),
                        reference: "r.CN-00001.0".to_owned(),
                        at: at("15"),
                    },
                )
                .await,
            Err(InventoryError::SerialAlreadyHeld(ref s)) if s == "SN-1"
        ),
        "a credit note put a second SN-1 on the shelf"
    );
    assert_eq!(fixture.shelf(GRINDER, Some(OLAYA)).await.on_hand(), 1);

    fixture.cleanup().await;
}

/// The supplier's bill, in the ledger's own words: `Dr` goods received not
/// invoiced, `Cr` payables. `purchases::record_bill` posts exactly this from a
/// line that names a stocked product; the test does not have `purchases` and
/// does not need it.
async fn billed(fixture: &Fixture, minor: i64, reference: &str) {
    entry(fixture, reference, "Stock bought", "2010", minor, "2000").await;
}

/// **A debit somebody put on the stock account themselves.** No delivery behind
/// it, so the shelves and the books stop agreeing — which is the whole reason
/// the comparison exists.
async fn by_hand(fixture: &Fixture, minor: i64, reference: &str) {
    entry(fixture, reference, "A correction", "1300", minor, "2000").await;
}

async fn entry(
    fixture: &Fixture,
    reference: &str,
    memo: &str,
    debit: &str,
    minor: i64,
    credit: &str,
) {
    ledger::post_entry(
        &fixture.db,
        &code(reference),
        at("06"),
        memo,
        ledger::BalancedLines::new(vec![
            ledger::Line::new(code(debit), sar(minor)),
            ledger::Line::new(code(credit), sar(-minor)),
        ])
        .expect("balances"),
        &Metadata::default(),
    )
    .await
    .expect("the entry posts");
}

/// The tenant's day in the summary tests. The route works it out through the
/// tenant's calendar; here it is chosen, so a boundary is a boundary.
const TODAY: &str = "2026-04-10";

/// What `GET /v1/inventory/summary` reads, on [`TODAY`], under `window`.
async fn summarised(
    fixture: &Fixture,
    branch: Option<&str>,
    window: inventory::ExpiryWindow,
) -> Vec<inventory::SummaryRow> {
    let today = day(TODAY);
    let mut conn = fixture.pool.acquire().await.expect("connection");
    inventory::summary(&mut conn, branch, today, window.warns_until(today))
        .await
        .expect("reads")
}

/// A batch of a lot-tracked product, dated or not.
fn batch(
    quantity: i64,
    minor: i64,
    code: &str,
    expires_on: Option<&str>,
    reference: &str,
) -> Receipt {
    Receipt {
        code: Some(code.to_owned()),
        expires_on: expires_on.map(day),
        ..plain(quantity, minor, reference)
    }
}

/// **A summary is the lists it summarises, added up** — per branch, per
/// currency — and `branch` narrows all three reads to one branch, every row
/// naming its product.
///
/// Six shelves at two branches and at none, in two currencies: a dated batch at
/// each branch, beans sold three short and then delivered one more, sugar sold
/// to exactly nothing, beans at a business that sent no branch, and a grinder
/// received after the inventory account moved to dollars. A summary that added
/// currencies together, lost the shelves of no branch, dropped a product that
/// sold out, counted an empty shelf as below zero, or read what a shelf owes
/// off what it is worth would disagree with the lists somewhere here.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "six shelves in two currencies, and each list the summary is checked against"
)]
async fn a_summary_is_the_lists_it_summarises_added_up() {
    let fixture = Fixture::new("inv-summary").await;
    let window = inventory::ExpiryWindow::DEFAULT;
    let today = day(TODAY);
    let through = window.warns_until(today).expect("a last day");

    // **Nothing received and nothing sold** is an empty answer, not a refusal.
    fixture.project().await;
    assert!(
        summarised(&fixture, None, window).await.is_empty(),
        "an empty tenant has an empty summary"
    );

    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;
    fixture
        .declare(BEANS, "حبوب إسبريسو", "gram", Tracking::None)
        .await;
    fixture
        .declare(GRINDER, "مطحنة", "piece", Tracking::None)
        .await;
    fixture.declare(SUGAR, "سكر", "bag", Tracking::None).await;

    fixture
        .receive(
            MILK,
            &batch(10, 5_000, "B-SOON", Some("2026-04-20"), "rcv-1"),
        )
        .await;
    fixture
        .try_receive(
            MILK,
            &batch(4, 2_000, "B-GONE", Some("2026-04-01"), "rcv-2"),
            MALAZ,
        )
        .await
        .expect("receives at Malaz");
    // Two bags at 5.00 each and five sold: three no lot could cover, owed at
    // the last unit cost. The bag delivered after does not pay that debt.
    fixture.receive(BEANS, &plain(2, 1_000, "rcv-3")).await;
    fixture
        .consume(BEANS, &sold(5, "inv-1.1"))
        .await
        .expect("a plain product sells short");
    fixture.receive(BEANS, &plain(1, 700, "rcv-4")).await;
    // **Sold out**: still a product Olaya stocks, and not below zero.
    fixture.receive(SUGAR, &plain(2, 800, "rcv-7")).await;
    fixture
        .consume(SUGAR, &sold(2, "inv-2.1"))
        .await
        .expect("sells what is there");
    // **A business that sends no `X-Branch`**: one shelf, at no branch.
    rejection(
        inventory::receive(
            &fixture.db,
            &code(BEANS),
            &plain(6, 1_200, "rcv-5"),
            &from(None),
        )
        .await,
    )
    .expect("receives at no branch");

    // **Dollars**, for what arrives from here on: the inventory account and the
    // one a delivery owes to, both kept in USD — the only way a tenant comes to
    // hold stock in a second currency.
    let dollars = CurrencyCode::new("USD").expect("a currency");
    for (account, name, kind) in [
        ("1350", "Inventory in dollars", ledger::AccountKind::Asset),
        (
            "2050",
            "Dollar goods received, not invoiced",
            ledger::AccountKind::Liability,
        ),
    ] {
        ledger::open_account(
            &fixture.db,
            &code(account),
            name,
            kind,
            dollars,
            &Metadata::default(),
        )
        .await
        .expect("the account opens");
    }
    {
        let mut conn = fixture.pool.acquire().await.expect("connection");
        erp_eventlog::configuration::set(
            &mut conn,
            inventory::PostingAccounts::KEY,
            &inventory::PostingAccounts {
                inventory: code("1350"),
                goods_received: code("2050"),
                ..inventory::PostingAccounts::conventional()
            },
            Some("the-accountant"),
            None,
        )
        .await
        .expect("the accounts are chosen");
    }
    fixture
        .receive(
            GRINDER,
            &Receipt {
                value: usd(30_000),
                ..plain(1, 0, "rcv-6")
            },
        )
        .await;
    fixture.project().await;

    let summary = summarised(&fixture, None, window).await;
    assert_eq!(
        summary
            .iter()
            .map(|row| row.branch.as_deref())
            .collect::<Vec<_>>(),
        [None, Some(MALAZ), Some(OLAYA)],
        "a row per branch with a shelf, the shelves of no branch first"
    );

    // **Added up from the lists**: every shelf, by branch and by currency, and
    // every open lot before today and through the window's last day.
    let shelves = fixture.shelves().await;
    assert_eq!(shelves.len(), 6);
    assert!(
        shelves
            .iter()
            .any(|shelf| shelf.product == SUGAR && shelf.on_hand == 0 && shelf.value == 0),
        "the sugar shelf is listed, empty: {shelves:?}"
    );
    let open = fixture
        .expiring_before(through.succ_opt().expect("a day"))
        .await;
    let gone = fixture.expiring_before(today).await;
    for row in &summary {
        let here: Vec<_> = shelves
            .iter()
            .filter(|shelf| shelf.branch == row.branch)
            .collect();
        assert_eq!(
            row.products,
            i64::try_from(here.len()).unwrap(),
            "{:?}: a product for every shelf the list shows",
            row.branch
        );
        let mut worth = std::collections::BTreeMap::<String, i64>::new();
        for shelf in &here {
            if let Some(currency) = &shelf.currency {
                *worth.entry(currency.clone()).or_default() += shelf.value;
            }
        }
        assert_eq!(
            row.value,
            worth.into_iter().collect::<Vec<_>>(),
            "{:?}: worth what its shelves are, a currency at a time",
            row.branch
        );
        let counted = |lots: &[inventory::LotRow]| {
            i64::try_from(lots.iter().filter(|lot| lot.branch == row.branch).count()).unwrap()
        };
        assert_eq!(row.expired, counted(&gone), "{:?}: gone", row.branch);
        assert_eq!(
            row.expiring,
            counted(&open) - counted(&gone),
            "{:?}: going off",
            row.branch
        );
    }

    // And what those numbers are, so the agreement above is not nothing agreeing
    // with nothing.
    assert_eq!(summary[0].value, [("SAR".to_owned(), 1_200)]);
    assert_eq!((summary[1].expiring, summary[1].expired), (0, 1));
    let olaya = &summary[2];
    assert_eq!(olaya.products, 4, "the sold-out sugar is still stocked");
    assert_eq!(
        olaya.value,
        [
            ("SAR".to_owned(), 5_000 + 700 - 1_500),
            ("USD".to_owned(), 30_000)
        ],
        "two currencies are two amounts, never one number"
    );
    assert_eq!((olaya.expiring, olaya.expired), (1, 0));

    // **Below zero, with what it owes** — the debt, not the shelf's value.
    assert_eq!(
        olaya.below_zero,
        [inventory::BelowZeroRow {
            product: BEANS.to_owned(),
            name: Some("حبوب إسبريسو".to_owned()),
            on_hand: -2,
            owes: 1_500,
            currency: Some("SAR".to_owned()),
        }],
        "three bags no lot covered, at the 5.00 the last delivery cost; the empty sugar shelf owes nothing"
    );
    let beans = shelves
        .iter()
        .find(|shelf| shelf.product == BEANS && shelf.branch.as_deref() == Some(OLAYA))
        .expect("the beans at Olaya");
    assert_eq!(
        beans.value,
        700 - 1_500,
        "the shelf is worth the bag delivered since less the debt, which is not what it owes"
    );
    assert!(summary[0].below_zero.is_empty() && summary[1].below_zero.is_empty());

    // **One branch, when it is asked for** — the summary, the stock and the
    // lots alike.
    assert_eq!(
        summarised(&fixture, Some(MALAZ), window).await,
        [summary[1].clone()]
    );
    assert!(
        summarised(&fixture, Some("BRANCH-NOWHERE"), window)
            .await
            .is_empty(),
        "a branch with no shelf is not in the answer"
    );
    let mut conn = fixture.pool.acquire().await.expect("connection");
    let at_olaya = inventory::stock(&mut conn, None, Some(OLAYA), 50, None)
        .await
        .expect("reads")
        .items;
    assert_eq!(at_olaya.len(), 4);
    assert!(
        at_olaya
            .iter()
            .all(|shelf| shelf.branch.as_deref() == Some(OLAYA)),
        "{at_olaya:?}"
    );
    let at_malaz = inventory::lots(&mut conn, None, Some(MALAZ), None, 50, None)
        .await
        .expect("reads")
        .items;
    assert_eq!(
        at_malaz
            .iter()
            .map(|lot| lot.id.clone())
            .collect::<Vec<_>>(),
        [lot_id(MILK, MALAZ, "rcv-2")]
    );
    drop(conn);

    // **Every row names its product.**
    for (product, name) in [
        (MILK, "حليب طازج"),
        (BEANS, "حبوب إسبريسو"),
        (GRINDER, "مطحنة"),
        (SUGAR, "سكر"),
    ] {
        let shelf = at_olaya
            .iter()
            .find(|shelf| shelf.product == product)
            .expect("a shelf at Olaya");
        assert_eq!(shelf.name.as_deref(), Some(name), "{product}");
    }
    assert_eq!(at_malaz[0].name.as_deref(), Some("حليب طازج"));
    assert!(
        open.iter()
            .all(|lot| lot.name.as_deref() == Some("حليب طازج")),
        "{open:?}"
    );

    fixture.cleanup().await;
}

/// **Going off is from today through the window's last day, and gone is before
/// today** — the rule the bell is rung by, counted.
///
/// Thirty days from the 10th of April: yesterday's batch has gone; today's is
/// still good today, so it is going off and not gone; tomorrow's and the 10th
/// of May's are going off; the 11th of May's is outside. An undated batch is
/// neither however wide the window, and a batch written off to nothing is
/// neither though its date is long past.
#[tokio::test]
async fn a_lot_is_going_off_from_today_through_the_windows_last_day() {
    let fixture = Fixture::new("inv-going-off").await;
    fixture
        .declare(MILK, "حليب طازج", "bottle", Tracking::Lot)
        .await;
    for (code, expires_on, reference) in [
        ("B-EMPTIED", Some("2026-04-01"), "rcv-0"),
        ("B-YESTERDAY", Some("2026-04-09"), "rcv-1"),
        ("B-TODAY", Some("2026-04-10"), "rcv-2"),
        ("B-TOMORROW", Some("2026-04-11"), "rcv-3"),
        ("B-LAST-DAY", Some("2026-05-10"), "rcv-4"),
        ("B-OUTSIDE", Some("2026-05-11"), "rcv-5"),
        ("B-UNDATED", None, "rcv-6"),
    ] {
        fixture
            .receive(MILK, &batch(2, 1_000, code, expires_on, reference))
            .await;
    }
    // Earliest expiry first, so the two bottles thrown away are the first batch.
    fixture
        .write_off(MILK, &scrapped(Reason::Expired, 2, "wo-1"))
        .await;
    fixture.project().await;

    let counts = |rows: &[inventory::SummaryRow]| {
        rows.iter()
            .map(|row| (row.branch.clone(), row.expiring, row.expired))
            .collect::<Vec<_>>()
    };
    let olaya = Some(OLAYA.to_owned());
    assert_eq!(
        counts(&summarised(&fixture, None, inventory::ExpiryWindow::DEFAULT).await),
        [(olaya.clone(), 3, 1)],
        "today, tomorrow and the 10th of May going off; yesterday gone"
    );
    assert_eq!(
        counts(
            &summarised(
                &fixture,
                None,
                inventory::ExpiryWindow::new(0).expect("none")
            )
            .await
        ),
        [(olaya.clone(), 1, 1)],
        "a window of none still counts what goes today, and what has gone"
    );
    let widest = inventory::ExpiryWindow::new(inventory::expiry::MAX_DAYS).expect("a window");
    assert_eq!(
        counts(&summarised(&fixture, None, widest).await),
        [(olaya, 4, 1)],
        "the undated batch and the emptied one are neither, however wide the window"
    );

    fixture.cleanup().await;
}
