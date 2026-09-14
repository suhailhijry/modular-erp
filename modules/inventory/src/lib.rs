//! What is on the shelf, and which delivery it came in.
//!
//! # What this is, in one sentence
//!
//! Products declared once, stock received into **lots**, and every movement an
//! event — so "how much milk do we have", "which batch is it" and "when does it
//! go off" have one answer, and it is the same answer tomorrow.
//!
//! # A lot, and the lot is the point
//!
//! Every receipt creates a lot: a quantity, what that quantity cost, and — when
//! the product is tracked that way — the tenant's own batch code and the day it
//! spoils. Nothing is pooled. A movement takes from the lot that expires first,
//! and costs that portion at **that lot's** cost.
//!
//! **Reversed, this is wrong.** A weighted average across the shelf is two
//! integers where this is a list, and it is genuinely cheaper — it is what an
//! earlier draft of this module did. It cannot answer *which delivery is this*,
//! and that question is the whole of expiry, the whole of a recall, and the
//! whole of a serial. A business that has to throw away one batch cannot be
//! served by a number that has forgotten which units are old.
//!
//! # Earliest expiry first, and untracked stock is FIFO down the same path
//!
//! [`pick`] is one pure function. A dated lot sorts before an undated one,
//! earlier date first; undated lots go oldest-received first. So a pharmacy's
//! stock leaves in the order it will spoil and a hardware shop's leaves in the
//! order it arrived, through one rule with one set of tests. **There is no
//! second rule for untracked products**, because a second rule is a second
//! thing to get wrong. Naming a lot overrides it — a recall, a scanned batch —
//! and a named lot that cannot cover what was asked is refused rather than
//! topped up from the next.
//!
//! # Open lots only live in the aggregate
//!
//! A lot that empties closes and leaves. A café receiving beans every morning
//! for three years carries the four lots it can still pour from, not the eleven
//! hundred it has ever bought — and the read model keeps the history. The
//! window of references already heard is bounded for the same reason, the way
//! `conversations::Thread` bounds its, and so it answers retries only: a return
//! follows the sale it undoes through the whole stream.
//!
//! # A serial is an identity, and identities are not invented
//!
//! A serial-tracked product names one serial per unit at receipt, **from the
//! caller**, and names the units it takes. A serial that is unknown, already
//! gone, or named twice in one movement is **refused** (L6) — and so is a
//! delivery that gives two units one name, because that is a delivery that has
//! named one unit. That is the one place this module refuses for stock: a
//! quantity that is wrong is corrected by counting, and nothing corrects a unit
//! that was never there.
//!
//! **One name per shelf, not per tenant.** The write side keeps one `Stock` per
//! branch and neither can see the other's serials, so the same number on hand at
//! Olaya and at Malaz is two units — and the read model is keyed to say so
//! rather than to claim a uniqueness no command enforces. A serial that has
//! *left* may be received again: a machine comes back from repair, and the row
//! follows the log onto its new lot.
//!
//! # A count counts the shelf
//!
//! The counter says what is on the shelf for a product at a branch (revision
//! R2). A shortage comes off the lots in [`pick`]'s order — earliest expiry
//! first, undated oldest-first, the rule sales already use rather than a new
//! invention — each portion at its own lot's cost, and an overage joins the
//! lot at the far end of that order at that lot's own unit cost. A serial-tracked product is counted
//! by naming the units found: what is missing is what was on hand and not
//! named, and a name that is not on hand is refused. Counting one lot stays
//! available for someone counting batches.
//!
//! **A count of the shelf settles what the shelf owes**: what is there is what
//! was counted, and a shelf cannot hold less than nothing. Expected, declared,
//! the variance, where it landed and what it was worth are all frozen into the
//! event, exactly as `pos::ShiftEvent::Closed` freezes the drawer's.
//!
//! # A shelf is per branch
//!
//! Stock on hand is a fact about a place: what is at Olaya is at Olaya, and head
//! office writing off spoiled stock sitting in Malaz is the case that makes a
//! tenant-wide number meaningless. The branch already travels on every request
//! in `Metadata`, so the stream is keyed `{product}.{branch}` and a business
//! that sends no `X-Branch` lands on one shelf per product.
//!
//! **Reversed, this is a migration rather than a feature.** Keying per tenant
//! now and per branch later re-keys every stream that exists, which is a rewrite
//! of history and not a column.
//!
//! # A movement that costs money books it, in the same transaction
//!
//! A delivery, a write-off's loss, a count's discrepancy, what a document
//! consumed and what a credit note put back all post through
//! `ledger::post_entry_in` **inside the transaction that writes the movement** — so a shelf that moved without its entry is not
//! a state this system can reach. A count that found exactly what the books
//! said posts nothing, because an entry that moves nothing is not an entry.
//!
//! **A receipt debits the asset and credits `2010 Goods received, not
//! invoiced`.** Stock is in the books the moment it lands, and what it owes is
//! not accounts payable until somebody bills it; the supplier's bill then
//! debits the holding account back on the line that names the product. Between
//! the two, that account is exactly what has arrived and not been invoiced —
//! and a bill that beats its delivery leaves it a debit, which reads as
//! *invoiced, not yet received*. Nothing refuses either order.
//!
//! So this module writes the asset at both ends, and [`value_on_hand`] against
//! `1300` is a comparison of like with like rather than a race with somebody
//! else's paperwork. It is still worth making — a bill line that named the
//! wrong product, a movement nobody expected — and it is still only half of
//! one: the comparison lives in the composition root, where reading
//! `proj_ledger` beside `proj_inventory` is not the cross-group read L3
//! forbids.
//!
//! # A sale depletes the shelf, and a credit note fills it back up
//!
//! `sales::issue_in` calls [`consume_in`] for every invoice line that names a
//! product, in the invoice's own transaction (decision 2) — so a till sale, a
//! booking bill and a `/v1/sales` invoice all deplete through one path, and
//! cost of goods sold is booked lot by lot as the goods leave.
//!
//! **What the shelf cannot cover splits two ways** (revision R1). A lot- or
//! serial-tracked product refuses: that stock is meant to be known exactly, a
//! phantom unit has no batch and no expiry, and a named unit that is not there
//! was never there. A plain one sells anyway and records a
//! [`Shortfall`] at the shelf's last known unit cost, which is the
//! negative number a count corrects. The debt stays until it does: netting the
//! next delivery against it would have to put the difference between the guess
//! and what the delivery actually cost into a price-variance account nobody has
//! opened.
//!
//! A credit note's returned line goes back through [`restore_in`], onto the
//! lots it left and at what the consumption froze — read from this shelf's own
//! stream, never from the read model (L3) and never as a share of what was
//! credited (decision 12, L6). What the sale could not cover settles the debt
//! if the shelf still owes it; once a count has cleared that debt, those units
//! come back as stock on a lot of their own, [`returned_lot_of`].
//!
//! **A line may name the lot it takes from**, overriding the picking rule — a
//! scanned batch, stock promised to a customer — and a lot that is not open on
//! this shelf, or holds fewer than the line takes, refuses. **A return of named
//! units names the ones that came back**, and each has to be still out on the
//! sale it undoes: a unit that sale never took, or one that has already come
//! back, is refused. That is decided from the sale followed through the stream
//! and not from the shelf, because a unit sold again since is off the shelf too.
//!
//! # Expiry is warned about, and nothing else
//!
//! The tenant chooses how far ahead ([`ExpiryWindow`]); the worker reads it with
//! the open lots and tells whoever may write stock off, on their notification
//! bell, about each lot that reaches its date inside the window — and once more
//! when one has passed its date and is still on the shelf. It posts nothing and
//! moves nothing: what leaves the shelf leaves through a write-off somebody
//! enters with a reason (decision 9).
//!
//! **This module does not ring the bell itself.** `messaging` reads it to say
//! what a lot is, so depending on `notifications` would be a cycle — the reason
//! no module announces (§47). Who may write stock off is named once, as the
//! type the write-off route takes ([`http::WritesOff`]).
//!
//! **Without `notifications` nothing is pushed, and that is decided.** A tenant
//! may run this module alone. It is then told nothing about stock going off and
//! the operators' `stock_bell` check stays silent for it, but it still sees every
//! expiring and expired lot in `GET /v1/inventory/summary`, counted against the
//! same window (§77).
//!
//! # What is deliberately not here
//!
//! **Found stock with no lot to join.** A count of the shelf that finds more
//! than its lots hold, with no lot open, is refused: the extra has no delivery
//! behind it to say what it cost, and on a lot-tracked product no batch. It
//! comes in as the receipt it is.
//!
//! **A second loss account in the shipped charts.** [`PostingAccounts`] keeps a
//! count's variance and a write-off's waste in two fields and defaults both to
//! `5900`, so a tenant can split spoilage from shrinkage in one request. Adding
//! a code for the split to charts this system ships to everybody would be a
//! guess about businesses nobody has asked.
//!
//! **A serial unique across the tenant.** See above: two branches are two
//! aggregates, and deciding one from the other would be reading a second stream
//! to take a decision (L3). Refusing a number already on hand somewhere else
//! needs a tenant-wide index nothing writes, or a transfer command.
//!
//! **Unit conversion, recipes and bills of material.** One frozen unit per
//! product, integer quantities in it. A kilo bought and a cup sold is a
//! conversion factor and a rounding policy; a drink made of four things is a
//! recipe. Both are features, not fields.
//!
//! **Reorder points, transfers between branches, valuation reports and
//! reservations.** Every one of them is additive over the three events here.

pub mod commands;
pub mod expiry;
pub mod http;
pub mod messages;
pub mod picking;
pub mod posting;
pub mod product;
pub mod projections;
pub mod stock;

pub use commands::{
    Consumption, Count, InventoryError, Receipt, Restoration, WriteOff, accepts_movements,
    consume_in, cost_entry_of, count, declare, lot_of, receive, restore_in, returned_lot_of,
    write_off,
};
pub use expiry::ExpiryWindow;
pub use picking::{OpenLot, PickError, Picked, Portion, Shortfall, Wanted, pick};
pub use posting::PostingAccounts;
pub use product::{Product, ProductEvent, Tracking};
pub use projections::{
    BelowZeroRow, Inventory, LotRow, MovementRow, ProductRow, StockRow, SummaryRow, lot, lots,
    movements, product, products, projections, stock, summary, value_on_hand,
};
pub use stock::{Reason, Returning, Stock, StockEvent, WentOut, stock_id};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Inventory as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str, i16)] = &[(
    <Inventory as erp_projection::ProjectionGroup>::NAME,
    <Inventory as erp_projection::ProjectionGroup>::SCHEMA,
    <Inventory as erp_projection::ProjectionGroup>::VERSION,
)];

/// Creates this module's read models in a tenant database.
///
/// # Errors
/// The database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_inventory; \
         SET search_path TO proj_inventory, public;",
    )
    .execute(&mut *conn)
    .await?;

    sqlx::raw_sql(include_str!("../schema/install.sql"))
        .execute(&mut *conn)
        .await?;

    sqlx::raw_sql("SET search_path TO public")
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// What a tenant enabling this module needs installed.
///
/// **`ledger`.** Every movement out of a shelf books a journal entry in the
/// transaction that writes the movement, so a closed account or a closed period
/// refuses the stock movement itself — and the codes a tenant chooses are
/// checked against their own chart before they can be stored. A business
/// keeping stock it cannot account for is a stocktake, not this.
///
/// **`branches` is read, not required** — the same stance `ledger` takes. A
/// shelf is a fact about a place, the posting refuses a branch nobody opened,
/// and a receipt asks that question of the log before it accepts a delivery
/// (see `crate::commands::receive`); a tenant who runs from one place and names
/// no branch never reaches it.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["ledger"])
    .reading(&["ledger", "branches"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("inventory")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        ProductEvent::NAMES
            .iter()
            .chain(StockEvent::NAMES.iter())
            .fold(erp_eventlog::Upcasters::new(), |u, n| {
                u.declare(&name(n), VERSION_1)
            })
    })
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn name(literal: &'static str) -> EventName {
    EventName::new(literal).expect("event names in this crate are valid literals")
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn domain(literal: &'static str) -> DomainName {
    DomainName::new(literal).expect("domain names in this crate are valid literals")
}
