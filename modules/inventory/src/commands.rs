//! Declaring a product, and moving stock of it.
//!
//! # What is decided from history here
//!
//! **Which lots a movement comes out of and what each portion costs.** The open
//! lots are aggregate state rehydrated from the stream, because a lot's
//! remaining quantity and remaining value are facts about every movement that
//! has touched it and no read model may be asked for them (L3, L7). **Whether
//! this movement has already happened**: the shelf remembers a bounded window of
//! the movements it has recorded, so a retried request writes nothing rather
//! than emptying a lot twice. **What a return is undoing** is not asked of that
//! window: a return follows the consumption through the whole stream
//! ([`crate::stock::Returning`]), because a sale stays returnable after its
//! shelf has moved on. And **how the product is tracked**, which decides
//! whether a movement is a quantity or a list of names.
//!
//! # Why a product's existence is asked of the log
//!
//! [`accepts_movements`] loads the `Product` aggregate rather than reading
//! `proj_inventory.product`, for the reason `crm::accepts_documents` does: the
//! read model is driven by a worker and lags, so a product declared a moment ago
//! is not in it yet and validating against it would tell somebody the product
//! they just created does not exist. It is public because `purchases` asks
//! the same question the same way when a bill line names a product.
//!
//! Asked on its own connection rather than inside the movement's transaction,
//! because the answer is **monotonic**: a product is declared once and there is
//! no verb that un-declares one, so a check that passed cannot stop being true
//! while the movement commits. A product declared in the same instant is
//! refused and the caller retries, which is the direction that cannot be wrong.
//!
//! # What posts, and in which transaction
//!
//! Everything that moves value does it **in the transaction that writes the
//! movement**, through `ledger::post_entry_in` — so a shelf that came down
//! without its entry is not a state this system can reach, and nothing has to
//! sweep for one afterwards. That is `sales::issue_in`'s pattern and the reason
//! for it is the same.
//!
//! **A receipt is not an exception.** It debits the asset and credits goods
//! received not invoiced, in its own transaction, because stock is in the books
//! the moment it lands; the supplier's bill debits the holding account back on
//! the line that names the product.
//!
//! The accounts are **resolved inside that transaction** and the generation
//! they came from is stamped on the metadata (L5), for the reason
//! `sales::resolve_accounts` does both: an entry posted to codes that were
//! never current is a reconciliation nobody can explain a year later.
//!
//! # What a document does to a shelf
//!
//! [`consume_in`] is called by `sales::issue_in`, in the invoice's own
//! transaction, for every line that names a product — so a till sale, a booking
//! bill and a `/v1/sales` invoice all deplete through one path (decision 2).
//! [`restore_in`] is the other direction: a credit note's returned line, put
//! back on the lots it left and at what that consumption froze — and what a
//! count had cleared of what no lot covered, on a lot of its own.
//!
//! **What a shelf cannot cover splits by how the product is tracked** (R1). A
//! lot- or serial-tracked one refuses and takes the document with it; a plain
//! one sells and records a shortfall at the last unit cost it saw. That branch
//! is [`consumed`], and it is the only place the three tracking modes differ on
//! the way out.

use std::collections::BTreeSet;

use erp_eventlog::{
    Committed, Decision, ExecuteError, Loaded, MAX_ATTEMPTS, Metadata, try_execute,
    try_execute_from,
};
use erp_i18n::{Localize, Message, MessageArg};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, Money, StreamId, Timestamp};

use crate::picking::{PickError, Wanted, pick};
use crate::product::{Product, ProductEvent, Tracking};
use crate::stock::{Reason, Returning, Stock, StockEvent, stock_id};

#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("there is no product {0}")]
    NoSuchProduct(String),
    #[error("a quantity is a whole number of the product's own unit, and more than nothing")]
    NotAQuantity,
    #[error("what a delivery cost is an amount of money, and more than nothing")]
    NotAValue,
    /// **One shelf, one currency — and it is the inventory account's.** Carries
    /// what disagreed and what it is kept in: the shelf and its own currency
    /// when a delivery does not match the stock already there, the inventory
    /// account and the chart's when it does not match the books.
    #[error("{id} is kept in {kept} and this delivery is not")]
    WrongCurrency { id: String, kept: String },
    #[error("a product needs a name and the unit it is counted in")]
    NeedsANameAndAUnit,
    #[error("{0} cannot be a product")]
    NotAProductId(String),
    /// A lot-tracked delivery with no batch code on it.
    #[error("a lot-tracked delivery names the batch it came in")]
    NeedsALotCode,
    /// A batch code or an expiry date on a product nobody tracks batches of.
    #[error("{0} is not lot-tracked")]
    NotALotProduct(String),
    /// A serial-tracked movement whose names do not match its units.
    #[error("a serial-tracked product names one serial per unit: {units} units, {named} named")]
    NeedsSerials { units: i64, named: i64 },
    #[error("{0} is not serial-tracked")]
    NotASerialProduct(String),
    #[error("{0} is already on the shelf")]
    SerialAlreadyHeld(String),
    #[error("{0} is not on the shelf")]
    NoSuchSerial(String),
    #[error("there is no open lot {0}")]
    NoSuchLot(String),
    #[error("lot {lot} holds {held} and {wanted} were asked for")]
    LotIsShort { lot: String, held: i64, wanted: i64 },
    #[error("there are {held} on the shelf and {wanted} were asked for")]
    NotEnoughStock { held: i64, wanted: i64 },
    /// A return naming a movement this shelf never recorded — a sale of
    /// something else, or a sale at another branch's shelf.
    ///
    /// **Refused rather than restored anyway** (L6). What a return puts back
    /// and what it is worth both come out of the consumption; without it there
    /// is nothing to put back and no cost to put it back at, and inventing
    /// either is how a shelf grows stock nobody ever bought.
    #[error("this shelf has no record of {0} going out")]
    NotConsumed(String),
    /// More than that movement still has out — a return of four off a sale of
    /// three, or a second credit note for units the first one already brought
    /// back. `taken` is what is **still out**, which shrinks as returns land.
    #[error("{taken} of that movement is still out and {wanted} are coming back")]
    MoreThanWasTaken { taken: i64, wanted: i64 },
    /// Part of a movement whose units have names, asked for by quantity. Which
    /// of them came back is not something to guess (decision 17): the return
    /// names them.
    #[error("{taken} named units went out together and part of them cannot be told apart")]
    NamedUnitsComeBackWhole { taken: i64 },
    /// A return naming a unit that is not out on the sale it undoes — one that
    /// sale never took, or one that has already come back against it.
    ///
    /// **Decided from that sale's own movement, followed through the whole
    /// stream** ([`crate::stock::Returning`]), and not from the shelf: a unit
    /// sold again since is off the shelf too, and a shelf check would let it
    /// come back twice.
    #[error("{0} is not out on that sale")]
    NotOut(String),
    /// A count of the shelf that found more than its lots hold, with no lot
    /// open for the extra to join. **Refused rather than landed on a lot the
    /// count would have to invent**: a found carton has no delivery behind it
    /// to say what it cost, and on a lot-tracked product no batch.
    #[error("{found} more were counted than the lots hold, and no lot is open to add them to")]
    NoLotToJoin { found: i64 },
    /// A shelf and a movement reference that will not fit in one journal entry
    /// name. Refused rather than truncated: two movements sharing a truncated
    /// name would share an entry, and the second would post nothing.
    ///
    /// **Both halves, because either can be the long one.** A branch id may eat
    /// the whole budget on its own, and telling the caller to shorten a key
    /// that is already one character is advice that cannot be followed.
    #[error("{shelf} and {reference} cannot name the entry this movement posts")]
    NotAReference { shelf: String, reference: String },
    #[error(transparent)]
    Money(#[from] erp_types::MoneyError),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
    /// The ledger refused the entry — a closed period, a branch that does not
    /// exist, an account this tenant has closed or never opened. **Carried
    /// whole** rather than flattened into "could not post", because the message
    /// names the account and that is the one thing the person fixing it needs.
    #[error(transparent)]
    Ledger(#[from] ledger::LedgerError),
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
}

impl From<PickError> for InventoryError {
    fn from(error: PickError) -> Self {
        match error {
            PickError::NotAQuantity => Self::NotAQuantity,
            PickError::NoSuchLot(lot) => Self::NoSuchLot(lot),
            PickError::LotIsShort { lot, held, wanted } => Self::LotIsShort { lot, held, wanted },
            PickError::NotOnHand(serial) => Self::NoSuchSerial(serial),
            PickError::Money(e) => Self::Money(e),
        }
    }
}

impl Localize for InventoryError {
    fn message(&self) -> Message {
        use crate::messages as m;
        match self {
            Self::NoSuchProduct(id) => {
                Message::new(m::NO_SUCH_PRODUCT).with("id", MessageArg::text(id))
            }
            Self::NotAQuantity => Message::new(m::NOT_A_QUANTITY),
            Self::NotAValue => Message::new(m::NOT_A_VALUE),
            Self::WrongCurrency { id, kept } => Message::new(m::WRONG_CURRENCY)
                .with("id", MessageArg::text(id))
                .with("kept", MessageArg::text(kept)),
            Self::NeedsANameAndAUnit => Message::new(m::NEEDS_A_NAME_AND_A_UNIT),
            Self::NotAProductId(id) => {
                Message::new(m::NOT_A_PRODUCT_ID).with("id", MessageArg::text(id))
            }
            Self::NeedsALotCode => Message::new(m::NEEDS_A_LOT_CODE),
            Self::NotALotProduct(id) => {
                Message::new(m::NOT_A_LOT_PRODUCT).with("id", MessageArg::text(id))
            }
            Self::NeedsSerials { units, named } => Message::new(m::NEEDS_SERIALS)
                .with("units", MessageArg::Int(*units))
                .with("named", MessageArg::Int(*named)),
            Self::NotASerialProduct(id) => {
                Message::new(m::NOT_A_SERIAL_PRODUCT).with("id", MessageArg::text(id))
            }
            Self::SerialAlreadyHeld(serial) => {
                Message::new(m::SERIAL_ALREADY_HELD).with("serial", MessageArg::text(serial))
            }
            Self::NoSuchSerial(serial) => {
                Message::new(m::NO_SUCH_SERIAL).with("serial", MessageArg::text(serial))
            }
            Self::NoSuchLot(lot) => Message::new(m::NO_SUCH_LOT).with("lot", MessageArg::text(lot)),
            Self::LotIsShort { lot, held, wanted } => Message::new(m::LOT_IS_SHORT)
                .with("lot", MessageArg::text(lot))
                .with("held", MessageArg::Int(*held))
                .with("wanted", MessageArg::Int(*wanted)),
            Self::NotEnoughStock { held, wanted } => Message::new(m::NOT_ENOUGH_STOCK)
                .with("held", MessageArg::Int(*held))
                .with("wanted", MessageArg::Int(*wanted)),
            Self::NotConsumed(reference) => {
                Message::new(m::NOT_CONSUMED).with("reference", MessageArg::text(reference))
            }
            Self::MoreThanWasTaken { taken, wanted } => Message::new(m::MORE_THAN_WAS_TAKEN)
                .with("taken", MessageArg::Int(*taken))
                .with("wanted", MessageArg::Int(*wanted)),
            Self::NamedUnitsComeBackWhole { taken } => {
                Message::new(m::NAMED_UNITS_COME_BACK_WHOLE).with("taken", MessageArg::Int(*taken))
            }
            Self::NotOut(serial) => {
                Message::new(m::NOT_OUT).with("serial", MessageArg::text(serial))
            }
            Self::NoLotToJoin { found } => {
                Message::new(m::NO_LOT_TO_JOIN).with("found", MessageArg::Int(*found))
            }
            Self::NotAReference { shelf, reference } => Message::new(m::NOT_A_REFERENCE)
                .with("shelf", MessageArg::text(shelf))
                .with("reference", MessageArg::text(reference)),
            Self::Money(_) | Self::Unbalanced(_) => Message::new(m::AMOUNT_OUT_OF_RANGE),
            // Already say the right thing in both languages.
            Self::Config(e) => e.message(),
            Self::Ledger(e) => e.message(),
        }
    }
}

impl InventoryError {
    /// Whether this refusal is about the request rather than the state of the
    /// world — which is the 400/422 split every module's `refused` makes.
    ///
    /// **A serial that is not on the shelf is a 422 and not a 400.** The request
    /// is well formed and names a unit that was never received, has already gone
    /// or was named twice here; which of the four it is depends on the state of
    /// the world, not on the shape of the message.
    #[must_use]
    pub const fn is_malformed(&self) -> bool {
        matches!(
            self,
            Self::NotAQuantity
                | Self::NotAValue
                | Self::NeedsANameAndAUnit
                | Self::NotAProductId(_)
                | Self::NeedsALotCode
                | Self::NeedsSerials { .. }
                | Self::NotAReference { .. }
                | Self::Money(_)
        )
    }
}

type Refusal = CommandError<InventoryError>;
type Moved = Result<Committed<StockEvent>, Refusal>;

/// The longest a product's name, unit, batch code or serial may be.
///
/// Generous for all four, and far short of what would make a listing
/// unreadable.
pub const MAX_LABEL: usize = 200;

/// What arrived. **Every receipt is a lot**, whatever the tracking mode.
#[derive(Debug, Clone)]
pub struct Receipt {
    pub quantity: i64,
    /// What the whole delivery cost — see [`StockEvent::Received`].
    pub value: Money,
    /// The tenant's own batch code. Required on a lot-tracked product and
    /// refused on any other.
    pub code: Option<String>,
    /// Only on a lot-tracked product, and only when the batch has a shelf life
    /// — an undated lot goes out after every dated one, oldest first.
    pub expires_on: Option<chrono::NaiveDate>,
    /// One per unit, on a serial-tracked product. **From the caller** (L8).
    pub serials: Vec<String>,
    /// What makes receiving it twice nothing, and what the lot is named after.
    /// The request's own key.
    pub reference: String,
    pub at: Timestamp,
}

/// What somebody threw away, and why.
#[derive(Debug, Clone)]
pub struct WriteOff {
    pub reason: Reason,
    /// How many, on a product tracked by quantity.
    pub quantity: Option<i64>,
    /// Which lot, when the picking rule is being overridden — a recall, a
    /// scanned batch.
    pub lot: Option<String>,
    /// Which units, on a serial-tracked product.
    pub serials: Vec<String>,
    pub reference: String,
    pub at: Timestamp,
}

/// What a document took off the shelf.
///
/// The same three ways of naming units a [`WriteOff`] has, and no reason: a
/// sale is not a loss. See [`consume_in`].
#[derive(Debug, Clone)]
pub struct Consumption {
    /// How many, on a product tracked by quantity.
    pub quantity: Option<i64>,
    /// Which lot, when the picking rule is being overridden — a scanned batch,
    /// stock promised to this customer.
    pub lot: Option<String>,
    /// Which units, on a serial-tracked product.
    pub serials: Vec<String>,
    /// **The document and the line this went out on**, derived by the caller
    /// and never minted here (L8). What makes consuming it twice nothing, and
    /// what names the journal entry — see [`cost_entry_of`].
    pub reference: String,
    pub at: Timestamp,
}

/// What a credit note is putting back onto the shelf.
///
/// **Two references, and they are different things.** `taken_on` names the
/// consumption being undone — that is where the lots and their costs come from.
/// `reference` is this return's own key: keyed on the consumption's, the shelf
/// would hear a retry of the *sale* and record nothing.
#[derive(Debug, Clone)]
pub struct Restoration {
    /// The movement reference the sale recorded, derived by the caller from the
    /// document and the line the way it derived the consumption's (L8).
    pub taken_on: String,
    /// **The branch the sale took it from**, which is the shelf it goes back
    /// on — not the branch this request came from. A cancellation raised at
    /// head office for a sale rung at Olaya puts the goods back at Olaya, and
    /// asked of the requesting branch it would look for the sale on a shelf
    /// that never saw it. `None` for a sale made without one.
    pub branch: Option<String>,
    /// How many units come back. `None` is all of them — a whole cancellation.
    pub quantity: Option<i64>,
    /// **Which units come back**, on a movement whose units have names. Each
    /// has to be still out on the sale `taken_on` names. Empty returns by
    /// quantity — which, for named units, only the whole of what is out can.
    pub serials: Vec<String>,
    /// This return's own key. What makes putting it back twice nothing.
    pub reference: String,
    pub at: Timestamp,
}

/// What somebody counted: **the shelf, or one lot on it**.
#[derive(Debug, Clone)]
pub struct Count {
    /// Which lot, for someone counting batches. `None` counts the shelf.
    pub lot: Option<String>,
    /// How many were there. On a serial-tracked product, how many units are
    /// named below — the two have to agree, as a delivery's do.
    pub declared: i64,
    /// **The units found**, by name, on a serial-tracked product. What is
    /// missing is what was on hand and not named.
    pub serials: Vec<String>,
    pub reference: String,
    pub at: Timestamp,
}

/// Declares a product, with the unit it is counted in and the way it is tracked
/// **for ever**.
///
/// A create: the id is the caller's key and nothing is minted here (L8), and a
/// second request under the same key is the same declaration rather than a
/// second product. Neither the unit nor the tracking mode can be amended
/// afterwards and there is no route that would — see `crate::product`.
///
/// # Errors
/// A name or a unit that is empty or absurdly long.
pub async fn declare(
    db: &TenantDb,
    product: &AggregateId,
    name: &str,
    unit: &str,
    tracking: Tracking,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<ProductEvent>, Refusal> {
    let name = label(name).map_err(rejected)?;
    let unit = label(unit).map_err(rejected)?;

    db.create::<Product, _, InventoryError>(product, crate::upcasters(), metadata, move |_| {
        Ok(Decision::one(ProductEvent::Declared {
            name: name.clone(),
            unit: unit.clone(),
            tracking,
            at,
        }))
    })
    .await
}

/// Records a delivery onto one shelf, as a lot, **and books it**.
///
/// `Dr` inventory, `Cr` goods received not invoiced, at what the delivery cost,
/// in the transaction that writes the movement. The goods are an asset the
/// moment they land and what they owe is not yet accounts payable; the
/// supplier's bill debits the holding account back on the line that names the
/// product. See [`crate::posting`].
///
/// # Errors
/// A quantity or a value that is not one, a delivery whose batch and serial
/// details do not match how the product is tracked, a currency the shelf or the
/// inventory account is not kept in, a branch nobody opened, a shelf and a key
/// too long to name an entry, a serial already on the shelf, or a ledger that
/// refuses the entry.
pub async fn receive(
    db: &TenantDb,
    product: &AggregateId,
    receipt: &Receipt,
    metadata: &Metadata,
) -> Moved {
    if receipt.quantity <= 0 {
        return Err(rejected(InventoryError::NotAQuantity));
    }
    // **What a delivery cost is money, and money here is positive.** Nothing
    // downstream would catch a negative one: the shelf would carry a negative
    // asset, every portion drawn off the lot would be a credit, and the margin
    // on every sale off it would read better for it. The same guard
    // `hr` puts on every part of a salary, for the same reason.
    if !receipt.value.is_positive() {
        return Err(rejected(InventoryError::NotAValue));
    }

    let shelf = shelf_of(product, metadata).map_err(rejected)?;
    let mut conn = db.acquire().await?;
    let tracking = tracking_in(&mut conn, product).await?;
    usable_shelf(&mut conn, &shelf, receipt, metadata).await?;
    drop(conn);

    let receipt = delivery(product, tracking, receipt).map_err(rejected)?;
    let lot = lot_of(&shelf, &receipt.reference);
    let entry = entry_id("ir", &shelf, &receipt.reference).map_err(rejected)?;
    let memo = format!("Stock received · {product}");

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

            let committed = try_execute::<Stock, _, InventoryError>(
                &mut *conn,
                &shelf,
                crate::upcasters(),
                &metadata,
                |loaded: &Loaded<Stock>| decide_receipt(product, &lot, &receipt, loaded),
            )
            .await?;

            if let Some(value) = delivered(&committed.events) {
                let lines = crate::posting::entry_for_receipt(value, &accounts)
                    .map_err(|e| ExecuteError::Rejected(InventoryError::Unbalanced(e)))?;
                post(&mut *conn, &entry, receipt.at, &memo, lines, &metadata).await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(&shelf)
}

/// What a delivery writes, or nothing when it has already been recorded.
///
/// Its own function only because [`receive`]'s retry loop borrows the receipt
/// on every attempt, and a closure that moved it could run once.
fn decide_receipt(
    product: &AggregateId,
    lot: &str,
    receipt: &Receipt,
    loaded: &Loaded<Stock>,
) -> Result<Decision<StockEvent>, InventoryError> {
    // **Before anything else**, so a retried delivery is the one that
    // already landed rather than a second lot of the same goods.
    if loaded.aggregate.has_heard(&receipt.reference) {
        return Ok(Decision::nothing());
    }
    // **`apply` cannot fail**, so a delivery in a currency the shelf is not
    // carried in has to be refused here or the aggregate would hold lots
    // that cannot be summed while the projection added the minor units
    // anyway (L6). The same guard `prepaid` puts on a card whose scheme
    // changed currency under it.
    //
    // [`usable_shelf`] has already refused anything the inventory account
    // is not kept in, which is the only way a shelf gets its currency — so
    // what is left for this to catch is a tenant who re-pointed the config
    // at an account in another currency after this shelf was started.
    if let Some(held) = loaded
        .aggregate
        .currency()
        .filter(|held| *held != receipt.value.currency())
    {
        return Err(InventoryError::WrongCurrency {
            id: product.to_string(),
            kept: held.to_string(),
        });
    }
    // **A serial is an identity**, and this is the half of it a shelf can
    // answer: a name it is already holding. The other half — one name on
    // two units of the same delivery — is refused in `delivery`, before
    // this runs, because no shelf can see inside the incoming list.
    lands(&loaded.aggregate, &receipt.serials)?;

    Ok(Decision::one(StockEvent::Received {
        lot: lot.to_owned(),
        code: receipt.code.clone(),
        expires_on: receipt.expires_on,
        quantity: receipt.quantity,
        value: receipt.value,
        serials: receipt.serials.clone(),
        reference: receipt.reference.clone(),
        at: receipt.at,
    }))
}

/// Takes stock off the shelf, says why it went, **and books the loss**.
///
/// **The picking rule decides which lots give it up** — earliest expiry first,
/// unless a lot is named — and each portion is costed on the lot it came off.
/// The entry is `Dr` waste, `Cr` inventory at the sum of those portions, in
/// this transaction: a shelf that came down without its entry is not a state
/// this system can reach.
///
/// **This is not a sale.** A person holding the goods who asks to write off more
/// than the shelf holds is refused (L6): decision 7's *"never refuse for
/// stock"* is about a till that must not stop, and nothing here is a till.
///
/// # Errors
/// A movement whose shape does not match how the product is tracked, a lot or a
/// serial that is not on the shelf, more than the shelf holds, or a ledger that
/// refuses the entry.
pub async fn write_off(
    db: &TenantDb,
    product: &AggregateId,
    write_off: &WriteOff,
    metadata: &Metadata,
) -> Moved {
    let tracking = tracking_of(db, product).await?;
    let shelf = shelf_of(product, metadata).map_err(rejected)?;
    let taking = leaving(
        product,
        tracking,
        write_off.quantity,
        write_off.lot.as_deref(),
        &write_off.serials,
    )
    .map_err(rejected)?;
    let entry = entry_id("iw", &shelf, &write_off.reference).map_err(rejected)?;
    let memo = format!(
        "Stock written off ({}) · {product}",
        write_off.reason.as_str()
    );

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

            let committed = try_execute::<Stock, _, InventoryError>(
                &mut *conn,
                &shelf,
                crate::upcasters(),
                &metadata,
                |loaded: &Loaded<Stock>| {
                    if loaded.aggregate.has_heard(&write_off.reference) {
                        return Ok(Decision::nothing());
                    }
                    let picked = taken(&loaded.aggregate, taking.wanted())?;

                    Ok(Decision::one(StockEvent::WrittenOff {
                        reason: write_off.reason,
                        portions: picked.portions,
                        reference: write_off.reference.clone(),
                        at: write_off.at,
                    }))
                },
            )
            .await?;

            if let Some(cost) = cost_of(&committed.events)? {
                let lines = crate::posting::entry_for_write_off(cost, &accounts)
                    .map_err(|e| ExecuteError::Rejected(InventoryError::Unbalanced(e)))?;
                post(&mut *conn, &entry, write_off.at, &memo, lines, &metadata).await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(&shelf)
}

/// **What a document took off the shelf, and what it cost.**
///
/// Posts `Dr` cost of goods sold, `Cr` inventory at the sum of what the lots it
/// took were carried at — lot by lot, never an average across the shelf.
///
/// # Its one caller is `sales::issue_in`
///
/// It takes the caller's connection rather than a [`TenantDb`] because the sale
/// owns the transaction its invoice commits in, exactly as `pos` composes
/// `sales::issue_in`. So a till sale, a booking bill and a `/v1/sales` invoice
/// all deplete through one path (decision 2), and an invoice that exists
/// without the movement that supplied it is not a state this system can reach.
///
/// # What it refuses, and what it lets through
///
/// **Revision R1, in one branch.** A lot- or serial-tracked product refuses
/// what the shelf cannot cover: that stock is meant to be known exactly, a
/// phantom unit has no batch and no expiry, and a named unit that is not there
/// was never there. A **plain** product goes below zero instead — the till does
/// not stop for a bad count — and what the lots could not cover is recorded as
/// a shortfall at the shelf's last known unit cost (decision 16), which a count
/// corrects.
///
/// A named lot and a named serial are refused for either, because both are
/// claims about *that* stock rather than about a quantity; `pick` refuses them
/// before this is asked.
///
/// **The refusal is inside the decision and after the retry check**, which is
/// the placement `sales::cancel_in` gives its claim and its limit: a sale the
/// shelf has already recorded is that sale, not a fresh one to judge against a
/// shelf it has already emptied.
///
/// # Errors
/// A movement whose shape does not match how the product is tracked, a lot or a
/// serial that is not on the shelf, more than a tracked shelf holds, or a
/// ledger that refuses the entry.
pub async fn consume_in(
    conn: &mut sqlx::PgConnection,
    product: &AggregateId,
    taking: &Consumption,
    metadata: &Metadata,
) -> Result<Committed<StockEvent>, ExecuteError<InventoryError>> {
    let tracking = tracking_in(&mut *conn, product).await?;
    let shelf = shelf_of(product, metadata).map_err(rejection)?;
    let leaving = leaving(
        product,
        tracking,
        taking.quantity,
        taking.lot.as_deref(),
        &taking.serials,
    )
    .map_err(rejection)?;
    let entry = entry_id(COST, &shelf, &taking.reference).map_err(rejection)?;
    let memo = format!("Cost of goods sold · {}", taking.reference);

    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

    let committed = try_execute::<Stock, _, InventoryError>(
        &mut *conn,
        &shelf,
        crate::upcasters(),
        &metadata,
        |loaded: &Loaded<Stock>| {
            // **Before the shelf is judged**, so a retried sale answers with
            // the movement it already made instead of being refused for stock
            // it has already taken.
            if loaded.aggregate.has_heard(&taking.reference) {
                return Ok(Decision::nothing());
            }
            let picked = consumed(&loaded.aggregate, leaving.wanted(), tracking)?;

            Ok(Decision::one(StockEvent::Consumed {
                portions: picked.portions,
                shortfall: picked.shortfall,
                reference: taking.reference.clone(),
                at: taking.at,
            }))
        },
    )
    .await?;

    if let Some(cost) = cost_of(&committed.events)? {
        let lines = crate::posting::entry_for_consumption(cost, &accounts)
            .map_err(|e| ExecuteError::Rejected(InventoryError::Unbalanced(e)))?;
        post(&mut *conn, &entry, taking.at, &memo, lines, &metadata).await?;
    }
    Ok(committed)
}

/// **What a credit note put back, and what it is worth.**
///
/// Posts `Dr` inventory, `Cr` cost of goods sold — the consumption undone.
///
/// # Where the stock lands, and at what
///
/// **On the lots it left, at the cost it left at**, read out of the
/// consumption this shelf recorded — followed through the whole stream by
/// [`crate::stock::Returning`], however many movements ago it was. Not
/// from `proj_inventory`, which is a projection and may not be decided from
/// (L3); not at today's cost, which would restate a margin that has been
/// reported; and never at a proportion of what was credited (decision 12) —
/// the division does not always land, and L6 says refuse rather than guess. A
/// lot that closed in between **reopens as itself**, with its own batch code and
/// expiry date, because the portion froze them at consumption.
///
/// Of what the sale could not cover, **what the shelf still owes is settled
/// first**: a phantom unit is not stock, and paying it off before refilling a
/// real batch is what keeps the count and the books agreeing. **What a count of
/// the shelf has since cleared is not owed any more**, so it comes back as
/// stock — on a lot of its own, [`returned_lot_of`], at what the sale charged it
/// out at (see [`crate::stock::WentOut::counted`]). Then the lots, in the order
/// the sale took them, which is the order they will go out in again.
///
/// Units nothing was ever paid for come back at nothing, in the shelf's
/// currency — or, on a shelf that has never been received onto, the one the
/// inventory account is kept in, which is the only one `receive` would let a
/// delivery land in.
///
/// # What it refuses
///
/// A reference this shelf never recorded going out — there is nothing to put
/// back and no cost to put it back at. More than went out under that reference.
/// **Part** of a movement whose units have names, asked for by quantity: which
/// of them came back is not a thing to guess (decision 17), so such a return
/// names them — and a name that is not still out on that sale, because the sale
/// never took it or it has already come back, is refused
/// ([`InventoryError::NotOut`]). And a named unit the shelf is already holding
/// again — see [`lands`].
///
/// # Errors
/// Any of those, or a ledger that refuses the entry.
pub async fn restore_in(
    conn: &mut sqlx::PgConnection,
    product: &AggregateId,
    back: &Restoration,
    metadata: &Metadata,
) -> Result<Committed<StockEvent>, ExecuteError<InventoryError>> {
    let shelf = stock_id(product, back.branch.as_deref())
        .map_err(|_| rejection(InventoryError::NotAProductId(product.to_string())))?;
    let entry = entry_id(BACK, &shelf, &back.reference).map_err(rejection)?;
    let memo = format!("Stock returned · {}", back.reference);
    // Trimmed the way they were when they went out, so a name matches itself.
    let serials = back
        .serials
        .iter()
        .map(|serial| label(serial))
        .collect::<Result<Vec<_>, _>>()
        .map_err(rejection)?;

    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;
    let kept = ledger::posting_currency(&mut *conn, &accounts.inventory)
        .await
        .map_err(ExecuteError::Load)?;

    let committed = try_execute_from::<Returning, _, InventoryError>(
        &mut *conn,
        &shelf,
        crate::upcasters(),
        &metadata,
        Returning::of(&back.taken_on, returned_lot_of(&shelf, &back.taken_on)),
        |loaded: &Loaded<Returning>| {
            // First, as everywhere here: a retried credit note puts the goods
            // back once.
            if loaded.aggregate.stock.has_heard(&back.reference) {
                return Ok(Decision::nothing());
            }
            let went_out = loaded
                .aggregate
                .went_out
                .as_ref()
                .ok_or_else(|| InventoryError::NotConsumed(back.taken_on.clone()))?;
            let unpaid = loaded
                .aggregate
                .stock
                .currency()
                .or(kept)
                .map(Money::zero)
                .ok_or_else(|| {
                    InventoryError::Ledger(ledger::LedgerError::NoSuchAccount(
                        accounts.inventory.to_string(),
                    ))
                });
            let (portions, settles) = coming_back(
                went_out,
                back.quantity,
                &serials,
                &loaded.aggregate.lands_on,
                unpaid,
            )?;
            // **A unit that is back on the shelf already cannot come back
            // again**: sold, then received a second time under the same name,
            // then this sale's credit note — two copies of one identity.
            lands(
                &loaded.aggregate.stock,
                portions.iter().flat_map(|portion| &portion.serials),
            )?;

            Ok(Decision::one(StockEvent::Restored {
                taken_on: back.taken_on.clone(),
                portions,
                settles,
                reference: back.reference.clone(),
                at: back.at,
            }))
        },
    )
    .await?;

    if let Some(cost) = cost_of(&committed.events)? {
        let lines = crate::posting::entry_for_restoration(cost, &accounts)
            .map_err(|e| ExecuteError::Rejected(InventoryError::Unbalanced(e)))?;
        post(&mut *conn, &entry, back.at, &memo, lines, &metadata).await?;
    }
    Ok(committed)
}

/// **What of a consumption comes back**, taken off the debt first, then onto
/// `lands_on` for what a count cleared, and then off the lots in the order the
/// sale took them. A consumption owes or has been counted, never both: a count
/// clears all of it.
///
/// `went_out` is what that movement **still has out**: the shelf takes each
/// return off it as it lands, so a second credit note against one sale can only
/// put back what the first left.
///
/// Every cost here is `Money::apportioned` of what the consumption froze, so a
/// whole return puts back exactly what went out and the last unit takes the
/// remainder — the same exactness a lot closing on zero relies on. `unpaid` is
/// what a cleared unit nothing was ever paid for comes back at, and its refusal
/// is only raised if one is. Named units come back by name, through
/// [`named_back`].
fn coming_back(
    went_out: &crate::stock::WentOut,
    wanted: Option<i64>,
    serials: &[String],
    lands_on: &str,
    unpaid: Result<Money, InventoryError>,
) -> Result<
    (
        Vec<crate::picking::Portion>,
        Option<crate::picking::Shortfall>,
    ),
    InventoryError,
> {
    if !serials.is_empty() {
        return Ok((named_back(went_out, wanted, serials)?, None));
    }
    let owed = went_out.shortfall.unwrap_or_default();
    let cleared = went_out.counted.unwrap_or_default();
    let took: i64 = went_out
        .portions
        .iter()
        .map(|portion| portion.quantity)
        .sum::<i64>()
        + owed.quantity
        + cleared.quantity;
    let mut left = wanted.unwrap_or(took);
    if left <= 0 {
        return Err(InventoryError::NotAQuantity);
    }
    if left > took {
        return Err(InventoryError::MoreThanWasTaken {
            taken: took,
            wanted: left,
        });
    }
    // **Named units come back by name, or whole.** A movement that took three
    // phones took three identities, and a return of one of them that did not
    // say which would put an arbitrary name back on the shelf.
    if left < took
        && went_out
            .portions
            .iter()
            .any(|portion| !portion.serials.is_empty())
    {
        return Err(InventoryError::NamedUnitsComeBackWhole { taken: took });
    }

    let settled = left.min(owed.quantity);
    let settles = (settled > 0)
        .then(|| {
            Ok::<_, erp_types::MoneyError>(crate::picking::Shortfall {
                quantity: settled,
                cost: owed
                    .cost
                    .map(|cost| cost.apportioned(settled, owed.quantity))
                    .transpose()?,
            })
        })
        .transpose()?;
    left -= settled;

    let mut portions = Vec::new();
    let found = left.min(cleared.quantity);
    if found > 0 {
        portions.push(crate::picking::Portion {
            lot: lands_on.to_owned(),
            quantity: found,
            cost: match cleared.cost {
                Some(cost) => cost.apportioned(found, cleared.quantity)?,
                None => unpaid?,
            },
            serials: Vec::new(),
            code: None,
            expires_on: None,
        });
        left -= found;
    }
    for portion in &went_out.portions {
        if left == 0 {
            break;
        }
        let back = left.min(portion.quantity);
        portions.push(crate::picking::Portion {
            quantity: back,
            cost: portion.cost.apportioned(back, portion.quantity)?,
            ..portion.clone()
        });
        left -= back;
    }
    Ok((portions, settles))
}

/// **The named units coming back**, each off the portion of the sale it went
/// out on, at that portion's share of what the sale froze.
///
/// Every name has to be **still out on that sale**. `went_out` is the
/// consumption followed through the whole stream less every return already
/// landed against it (`WentOut::give_back` takes the names off), so a unit the
/// sale never took and a unit that has already come back are one refusal. Not
/// asked of the shelf: a unit sold again since is off the shelf as well, and
/// would pass. A named-unit movement never owes, because a tracked product that
/// the shelf cannot cover refuses (R1), so nothing here settles a debt.
fn named_back(
    went_out: &crate::stock::WentOut,
    wanted: Option<i64>,
    serials: &[String],
) -> Result<Vec<crate::picking::Portion>, InventoryError> {
    let named = i64::try_from(serials.len()).unwrap_or(i64::MAX);
    if let Some(units) = wanted.filter(|units| *units != named) {
        return Err(InventoryError::NeedsSerials { units, named });
    }
    let mut left: BTreeSet<&str> = BTreeSet::new();
    if let Some(twice) = serials.iter().find(|serial| !left.insert(serial.as_str())) {
        return Err(InventoryError::NotOut(twice.clone()));
    }

    let mut portions = Vec::new();
    for portion in &went_out.portions {
        let back: Vec<String> = portion
            .serials
            .iter()
            .filter(|held| left.remove(held.as_str()))
            .cloned()
            .collect();
        if back.is_empty() {
            continue;
        }
        let units = i64::try_from(back.len()).unwrap_or(i64::MAX);
        portions.push(crate::picking::Portion {
            quantity: units,
            cost: portion.cost.apportioned(units, portion.quantity)?,
            serials: back,
            ..portion.clone()
        });
    }
    match serials.iter().find(|serial| left.contains(serial.as_str())) {
        Some(stray) => Err(InventoryError::NotOut(stray.clone())),
        None => Ok(portions),
    }
}

/// Records what somebody counted — **the shelf, or one lot on it** — and books
/// what the books disagreed by.
///
/// # Where the difference lands (R2)
///
/// **A shortage comes off the lots in picking order**, through [`pick`] — the
/// rule a sale takes stock by, not a second one — each portion at its own lot's
/// cost. **An overage joins the lot that goes out last** — the far end of that
/// same order, so the latest expiry, or on undated stock the last received or
/// put back — at that lot's own unit cost: joined
/// at any other price the lot would stop costing what it cost and become an
/// average of two, which is the thing lots exist not to be. A count that finds
/// more than the lots hold with no lot open is refused
/// ([`InventoryError::NoLotToJoin`]) rather than landed on a lot it invents.
///
/// **Counting one lot stays available**, for someone counting batches: the same
/// rule with that lot as the whole of what was counted, so the difference lands
/// on it.
///
/// # A serial-tracked product names what it found
///
/// What is missing is what was on hand and not named, and each missing unit
/// leaves at its own lot's cost. A named unit that is not on hand is **refused**
/// (decision 17): a count corrects a quantity, and nothing corrects an identity.
///
/// # What a count does to what the shelf owes
///
/// A plain product sold short owes units, charged out at the last unit cost
/// the sale knew. **A count of the shelf clears the debt** — what is on the
/// shelf is what was counted, and a shelf cannot hold less than nothing — so the
/// debt comes back at what it was charged out at and the lots are taken to what
/// was counted at theirs. What posts is the difference: the units the count
/// found or did not, and, where a delivery had covered the debt, the gap between
/// the guess the sale was costed at and what those units actually cost. Nothing
/// else could book that gap without a price-variance account, which is why a
/// receipt leaves the debt alone. A count of one lot leaves it alone too: the
/// debt is on no lot.
///
/// The variance is `declared - expected`, **frozen into the event with where it
/// landed** (L5), and so is what it was worth.
///
/// # A discrepancy posts, and that is why a count exists
///
/// A short count books the loss and credits the asset; an over count is the
/// same entry the other way round. **A count that moved no value posts
/// nothing** — `posting::entry_for_variance` returns no lines rather than a
/// pair of zeroes, because an entry that moves nothing is not an entry. A shelf
/// that records a shortage and does not book it leaves the ledger saying the
/// business holds what it does not, for ever, which is `pos`'s argument about a
/// till drawer and the same argument here.
///
/// # Errors
/// A negative count, serials that do not agree with it or on a product that has
/// none, a lot or a serial that is not on the shelf, more found than the lots
/// hold with no lot open to join, or a ledger that refuses the entry.
pub async fn count(
    db: &TenantDb,
    product: &AggregateId,
    count: &Count,
    metadata: &Metadata,
) -> Moved {
    if count.declared < 0 {
        return Err(rejected(InventoryError::NotAQuantity));
    }
    let tracking = tracking_of(db, product).await?;
    let count = tally(product, tracking, count).map_err(rejected)?;
    let shelf = shelf_of(product, metadata).map_err(rejected)?;
    let entry = entry_id("iv", &shelf, &count.reference).map_err(rejected)?;
    let memo = format!("Stock count variance · {product}");

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

            let committed = try_execute::<Stock, _, InventoryError>(
                &mut *conn,
                &shelf,
                crate::upcasters(),
                &metadata,
                |loaded: &Loaded<Stock>| {
                    // **Before anything else**, so a retried count is the count
                    // that already happened rather than a second one finding a
                    // variance of zero.
                    if loaded.aggregate.has_heard(&count.reference) {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(counted(&loaded.aggregate, tracking, &count)?))
                },
            )
            .await?;

            if let Some(value) = found(&committed.events) {
                let lines = crate::posting::entry_for_variance(value, &accounts)
                    .map_err(|e| ExecuteError::Rejected(InventoryError::Unbalanced(e)))?;
                post(&mut *conn, &entry, count.at, &memo, lines, &metadata).await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(&shelf)
}

/// **The lot a receipt creates**, derived from the shelf it lands on and the
/// receipt that made it — never minted (L8).
///
/// Public for the reason `sales::credit_entry_of` is, and takes two arguments
/// for the same reason it does: a caller that has just received stock has to be
/// able to name the lot it made — to count it, to write it off, to print it on
/// a label — and the alternative is a second copy of the prefix in another
/// crate.
///
/// # Why the shelf is in it
///
/// `lot.id` is unique across the tenant, and the reference is the caller's own
/// idempotency key. **A key is only promised to be unique to the client that
/// sent it**, and one that keys a retry loop on a batch rather than on a row
/// sends the same key for two rows: two products, or one product at two
/// branches. Each lands on a different shelf and neither shelf has heard the
/// other's reference, so both are recorded — and derived from the reference
/// alone they would be one id for two lots, which the read model resolves by
/// dropping the second and then drawing the wrong one down. The shelf is what
/// tells the two apart; within one shelf the reference already does, and the
/// window of references already heard is what makes a repeat a retry.
#[must_use]
pub fn lot_of(shelf: &AggregateId, reference: &str) -> String {
    format!("lot.{shelf}.{reference}")
}

/// **The lot a sale's units come back on when a count had cleared what that
/// sale owed** — derived from the shelf and the consumption, never minted (L8).
///
/// One per consumption, so two credit notes against one sale land on one lot at
/// one unit cost, and two sales' units never blend into an average. A prefix of
/// its own rather than [`lot_of`]'s, so no receipt reference can ever name it.
#[must_use]
pub fn returned_lot_of(shelf: &AggregateId, taken_on: &str) -> String {
    format!("back.{shelf}.{taken_on}")
}

/// Whether a product exists and stock of it may be moved **right now**.
///
/// Asked of the log rather than of `proj_inventory.product` — see the module
/// doc. This is the function a document that depletes stock calls, in its own
/// transaction, the way `sales` calls `crm::accepts_documents`.
///
/// # Errors
/// The log could not be read.
pub async fn accepts_movements(
    conn: &mut sqlx::PgConnection,
    product: &AggregateId,
) -> Result<bool, erp_eventlog::LoadError> {
    let loaded = erp_eventlog::load::<Product>(conn, product, crate::upcasters()).await?;
    Ok(loaded.aggregate.exists())
}

/// How this product is tracked, refusing one that does not exist.
async fn tracking_of(db: &TenantDb, product: &AggregateId) -> Result<Tracking, Refusal> {
    let mut conn = db.acquire().await?;
    let tracking = tracking_in(&mut conn, product).await;
    drop(conn);
    tracking.map_err(Into::into)
}

/// The same question on the caller's connection, for [`consume_in`].
async fn tracking_in(
    conn: &mut sqlx::PgConnection,
    product: &AggregateId,
) -> Result<Tracking, ExecuteError<InventoryError>> {
    let loaded = erp_eventlog::load::<Product>(conn, product, crate::upcasters())
        .await
        .map_err(ExecuteError::Load)?;

    if loaded.aggregate.exists() {
        Ok(loaded.aggregate.tracking())
    } else {
        Err(rejection(InventoryError::NoSuchProduct(
            product.to_string(),
        )))
    }
}

/// **The journal entry a consumption posts under.**
///
/// Public for the reason `sales::issue_entry_of` is, and only this one of the
/// three: a consumption belongs to a *document*, so a report asked *which
/// postings did this invoice line make* has to be able to name the entry
/// without reimplementing the prefix. A write-off and a count belong to nothing
/// outside this module, and publishing names nobody needs is how a prefix
/// becomes impossible to change.
///
/// `shelf` is the stock stream — the product **and** the branch — because the
/// reference is only promised to be unique to the client that sent it, and one
/// key used for a batch of lines can reach two shelves. See [`lot_of`].
#[must_use]
pub fn cost_entry_of(shelf: &AggregateId, reference: &str) -> String {
    format!("{COST}.{shelf}.{reference}")
}

/// The prefix a consumption's entry is named under, shared by [`cost_entry_of`]
/// and [`consume_in`] so the name a caller can predict is the name that posts.
const COST: &str = "ic";

/// The prefix a restoration's entry is named under. Its own, and not the
/// consumption's: a return that posted under the sale's entry id would be
/// absorbed silently, because posting an existing entry is a no-op.
const BACK: &str = "ib";

/// The entry one of this module's own movements posts under.
///
/// **Derived, never minted** (L8): the shelf and the movement's own reference,
/// which is what makes re-posting a retried movement a no-op in the ledger as
/// well as here. Refused rather than truncated when the two will not fit in one
/// `AggregateId` — see [`InventoryError::NotAReference`].
fn entry_id(
    prefix: &str,
    shelf: &AggregateId,
    reference: &str,
) -> Result<AggregateId, InventoryError> {
    AggregateId::new(format!("{prefix}.{shelf}.{reference}")).map_err(|_| {
        InventoryError::NotAReference {
            shelf: shelf.to_string(),
            reference: reference.to_owned(),
        }
    })
}

/// The accounts this movement posts to, **and the generation they came from**.
///
/// Read inside the movement's transaction, and stamped onto the metadata so the
/// events and the entry both record which configuration decided them (L5). The
/// shape, and the reason, are `sales::resolve_accounts`'s.
async fn resolve_accounts(
    conn: &mut sqlx::PgConnection,
    metadata: &Metadata,
) -> Result<(crate::PostingAccounts, Metadata), ExecuteError<InventoryError>> {
    let accounts = crate::PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(InventoryError::Config(e)))?;
    let version = erp_eventlog::configuration::version(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(InventoryError::Config(e)))?;

    Ok((
        accounts,
        Metadata {
            config_version: Some(version),
            ..metadata.clone()
        },
    ))
}

/// What a movement across the shelf's edge was worth, from the event it wrote —
/// or nothing, when it wrote none because it had already been recorded.
///
/// **The shortfall counts.** A plain product sold off an empty shelf moved no
/// lot and still cost the business what it last paid for one, and the entry has
/// to say so or the asset never comes down for it. A shortfall the shelf could
/// not cost — nothing was ever received onto it — is worth nothing, which is
/// the honest number rather than a zero in a currency nobody has named.
fn cost_of(events: &[StockEvent]) -> Result<Option<Money>, ExecuteError<InventoryError>> {
    let Some((portions, uncovered)) = events.iter().find_map(|event| match event {
        StockEvent::Consumed {
            portions,
            shortfall,
            ..
        } => Some((portions, shortfall.and_then(|short| short.cost))),
        StockEvent::Restored {
            portions, settles, ..
        } => Some((portions, settles.and_then(|short| short.cost))),
        StockEvent::WrittenOff { portions, .. } => Some((portions, None)),
        _ => None,
    }) else {
        return Ok(None);
    };
    let Some(currency) = portions
        .first()
        .map(|portion| portion.cost.currency())
        .or_else(|| uncovered.map(Money::currency))
    else {
        return Ok(None);
    };
    Money::checked_sum(
        portions.iter().map(|portion| portion.cost).chain(uncovered),
        currency,
    )
    .map(Some)
    .map_err(|e| ExecuteError::Rejected(InventoryError::Money(e)))
}

/// What a delivery landed, from the event it wrote — or nothing, when it wrote
/// none because it had already been recorded.
fn delivered(events: &[StockEvent]) -> Option<Money> {
    events.iter().find_map(|event| match event {
        StockEvent::Received { value, .. } => Some(*value),
        _ => None,
    })
}

/// What a count disagreed by, from the event it wrote.
fn found(events: &[StockEvent]) -> Option<Money> {
    events.iter().find_map(|event| match event {
        StockEvent::Counted { value, .. } => *value,
        _ => None,
    })
}

/// Posts the entry a movement makes, or nothing when it moved no value.
///
/// In the caller's transaction, so a refusal here — a closed period, an account
/// this tenant has closed, a branch that does not exist — takes the movement
/// with it. That is L6 in its concrete form: the stock does not come off the
/// shelf into books that were never told.
async fn post(
    conn: &mut sqlx::PgConnection,
    entry: &AggregateId,
    at: Timestamp,
    memo: &str,
    lines: Option<ledger::BalancedLines>,
    metadata: &Metadata,
) -> Result<(), ExecuteError<InventoryError>> {
    let Some(lines) = lines else {
        return Ok(());
    };
    ledger::post_entry_in(conn, entry, at, memo, &lines, metadata)
        .await
        .map(|_| ())
        .map_err(lift)
}

/// Carries a ledger failure into this module's error without flattening what
/// kind of failure it was — a rejection stays a rejection, a conflict stays a
/// conflict, so the retry loops above still recognise it. The same shape
/// `sales::lift` and `pos::lift` have.
fn lift(error: ExecuteError<ledger::LedgerError>) -> ExecuteError<InventoryError> {
    match error {
        ExecuteError::Rejected(e) => ExecuteError::Rejected(InventoryError::Ledger(e)),
        ExecuteError::Load(e) => ExecuteError::Load(e),
        ExecuteError::Append(e) => ExecuteError::Append(e),
        ExecuteError::Enqueue(e) => ExecuteError::Enqueue(e),
        ExecuteError::Database(e) => ExecuteError::Database(e),
        ExecuteError::Contended { stream, attempts } => {
            ExecuteError::Contended { stream, attempts }
        }
        ExecuteError::AlreadyExists { stream } => ExecuteError::AlreadyExists { stream },
    }
}

/// Commits, retries, or gives up — the shape `pos::settle` has, and for the
/// same reason: the transaction spans a module's own event and the ledger's, so
/// the retry cannot live inside either.
async fn settle<T>(
    tx: erp_tenant::Tx,
    outcome: Result<T, ExecuteError<InventoryError>>,
) -> Result<Option<T>, Refusal> {
    match outcome {
        Ok(done) => {
            tx.commit().await.map_err(ExecuteError::from)?;
            Ok(Some(done))
        }
        Err(e) if e.is_conflict() => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Ok(None)
        }
        Err(e) => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Err(e.into())
        }
    }
}

fn contended<T>(shelf: &AggregateId) -> Result<T, Refusal> {
    Err(CommandError::Execute(ExecuteError::Contended {
        stream: StreamId::new(<Stock as erp_eventlog::Aggregate>::domain(), shelf.clone()),
        attempts: MAX_ATTEMPTS,
    }))
}

/// **A delivery has to look like the product it is of.**
///
/// One place, checked before the shelf is loaded, because none of it is a fact
/// about the shelf: a batch code on a product nobody tracks batches of is a
/// wrong request whatever is on hand. Refused rather than dropped — silently
/// ignoring a serial somebody sent is how a phone ends up on a shelf with no
/// name (L6).
fn delivery(
    product: &AggregateId,
    tracking: Tracking,
    receipt: &Receipt,
) -> Result<Receipt, InventoryError> {
    let code = receipt
        .code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    match tracking {
        Tracking::Lot if code.is_none() => return Err(InventoryError::NeedsALotCode),
        Tracking::Lot => {}
        _ if code.is_some() || receipt.expires_on.is_some() => {
            return Err(InventoryError::NotALotProduct(product.to_string()));
        }
        _ => {}
    }

    // **Counted distinct**, because a serial is an identity: a delivery that
    // gives two units one name has named one unit, and saying so with the
    // refusal that already exists — two units, one named — needs no second
    // message. Here rather than in `receive`'s guard on the shelf below,
    // because a name repeated inside the list is a fact about the request: no
    // shelf can see it, and the shelf guard only looks for names already on it.
    // Trimmed the way [`label`] trims, so two spellings of one name are one.
    let named = i64::try_from(
        receipt
            .serials
            .iter()
            .map(|serial| serial.trim())
            .collect::<BTreeSet<_>>()
            .len(),
    )
    .unwrap_or(i64::MAX);
    match tracking {
        Tracking::Serial if named != receipt.quantity => {
            return Err(InventoryError::NeedsSerials {
                units: receipt.quantity,
                named,
            });
        }
        Tracking::Serial => {}
        _ if !receipt.serials.is_empty() => {
            return Err(InventoryError::NotASerialProduct(product.to_string()));
        }
        _ => {}
    }

    Ok(Receipt {
        code: code.map(label).transpose()?,
        serials: receipt
            .serials
            .iter()
            .map(|serial| label(serial))
            .collect::<Result<Vec<_>, _>>()?,
        ..receipt.clone()
    })
}

/// A movement out, normalised against how the product is tracked.
///
/// One shape for both ways stock leaves, because a write-off and a consumption
/// name their units identically and only the reason differs — and a second copy
/// of these rules is a second place for them to drift.
struct Leaving {
    quantity: i64,
    lot: Option<String>,
    serials: Vec<String>,
}

impl Leaving {
    /// What to ask [`pick`] for.
    fn wanted(&self) -> Wanted<'_> {
        if !self.serials.is_empty() {
            return Wanted::Serials(&self.serials);
        }
        self.lot
            .as_deref()
            .map_or(Wanted::Quantity(self.quantity), |lot| Wanted::From {
                lot,
                quantity: self.quantity,
            })
    }
}

/// **And so does a movement out.** See [`delivery`].
fn leaving(
    product: &AggregateId,
    tracking: Tracking,
    quantity: Option<i64>,
    lot: Option<&str>,
    serials: &[String],
) -> Result<Leaving, InventoryError> {
    let named = i64::try_from(serials.len()).unwrap_or(i64::MAX);
    if tracking == Tracking::Serial {
        // A serial-tracked movement names its units and nothing else: a
        // quantity beside them would be a second answer to the same question.
        if named == 0 || quantity.is_some() || lot.is_some() {
            return Err(InventoryError::NeedsSerials {
                units: quantity.unwrap_or(0),
                named,
            });
        }
        return Ok(Leaving {
            quantity: named,
            lot: None,
            serials: serials
                .iter()
                .map(|serial| label(serial))
                .collect::<Result<Vec<_>, _>>()?,
        });
    }

    if named > 0 {
        return Err(InventoryError::NotASerialProduct(product.to_string()));
    }
    match quantity {
        Some(quantity) if quantity > 0 => Ok(Leaving {
            quantity,
            lot: lot.map(ToOwned::to_owned),
            serials: Vec::new(),
        }),
        _ => Err(InventoryError::NotAQuantity),
    }
}

/// **Which lots a write-off comes off, refusing what they cannot cover.**
///
/// A person standing in front of the goods who asks to throw away more than is
/// on the shelf is wrong about the goods (L6). Decision 7's *"never refuse for
/// stock"* — narrowed by R1 — governs a till that must not stop, and a
/// write-off is not a till.
fn taken(stock: &Stock, wanted: Wanted<'_>) -> Result<crate::picking::Picked, InventoryError> {
    let picked = pick(&stock.lots, wanted, stock.last_unit_cost)?;
    if picked.shortfall.is_some() {
        return Err(InventoryError::NotEnoughStock {
            held: stock.on_hand(),
            wanted: picked.quantity(),
        });
    }
    Ok(picked)
}

/// **Which lots a document's line comes off, and whether what they cannot cover
/// is a refusal or a shortfall.**
///
/// Revision R1: a lot- or serial-tracked shelf refuses, a plain one goes
/// negative. The whole of the difference is this one branch — everything else
/// about taking stock off a shelf is the same for all three — and putting it
/// here rather than at the call site is what keeps a till, a booking bill and a
/// `/v1/sales` invoice answering identically.
fn consumed(
    stock: &Stock,
    wanted: Wanted<'_>,
    tracking: Tracking,
) -> Result<crate::picking::Picked, InventoryError> {
    let picked = pick(&stock.lots, wanted, stock.last_unit_cost)?;
    if picked.shortfall.is_some() && tracking != Tracking::None {
        return Err(InventoryError::NotEnoughStock {
            held: stock.on_hand(),
            wanted: picked.quantity(),
        });
    }
    Ok(picked)
}

/// **A count has to look like the product it is of** — the rule [`delivery`]
/// applies to a receipt: serials only on a serial-tracked product, and there as
/// many distinct names as the count declares. Before the shelf is loaded,
/// because neither is a fact about the shelf.
fn tally(
    product: &AggregateId,
    tracking: Tracking,
    count: &Count,
) -> Result<Count, InventoryError> {
    if tracking != Tracking::Serial {
        if !count.serials.is_empty() {
            return Err(InventoryError::NotASerialProduct(product.to_string()));
        }
        return Ok(count.clone());
    }
    let named = count
        .serials
        .iter()
        .map(|serial| label(serial))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let units = i64::try_from(named.len()).unwrap_or(i64::MAX);
    if units != count.declared {
        return Err(InventoryError::NeedsSerials {
            units: count.declared,
            named: units,
        });
    }
    Ok(Count {
        serials: named.into_iter().collect(),
        ..count.clone()
    })
}

/// **What a count found, and where the difference lands** — see [`count`].
///
/// One rule for a shelf and for a lot: what was counted is the lots in scope,
/// and they are taken to `declared` — a shortage through [`pick`], an overage
/// onto the one that goes out last. Only a count of the whole shelf settles what it
/// owes. A serial-tracked count takes off the units in scope it did not name.
fn counted(stock: &Stock, tracking: Tracking, count: &Count) -> Result<StockEvent, InventoryError> {
    let scope = match count.lot.as_deref() {
        Some(id) => std::slice::from_ref(
            stock
                .lot(id)
                .ok_or_else(|| InventoryError::NoSuchLot(id.to_owned()))?,
        ),
        None => stock.lots.as_slice(),
    };
    let on_lots: i64 = scope.iter().map(|lot| lot.quantity).sum();
    let settles = count.lot.is_none().then_some(stock.owed).flatten();
    let expected = on_lots - settles.map_or(0, |owed| owed.quantity);

    let (taken, joined) = if tracking == Tracking::Serial {
        if let Some(stranger) = count
            .serials
            .iter()
            .find(|named| !scope.iter().any(|lot| lot.serials.contains(named)))
        {
            return Err(InventoryError::NoSuchSerial(stranger.clone()));
        }
        let missing: Vec<String> = scope
            .iter()
            .flat_map(|lot| &lot.serials)
            .filter(|held| !count.serials.contains(held))
            .cloned()
            .collect();
        let taken = if missing.is_empty() {
            Vec::new()
        } else {
            pick(scope, Wanted::Serials(&missing), None)?.portions
        };
        (taken, None)
    } else {
        match count
            .declared
            .checked_sub(on_lots)
            .ok_or(InventoryError::NotAQuantity)?
        {
            short if short < 0 => (pick(scope, Wanted::Quantity(-short), None)?.portions, None),
            over if over > 0 => {
                let last_out = crate::picking::earliest_first(scope)
                    .pop()
                    .ok_or(InventoryError::NoLotToJoin { found: over })?;
                (Vec::new(), Some(last_out.gives(over, Vec::new())?))
            }
            _ => (Vec::new(), None),
        }
    };

    // What came back onto the books less what left them, in the shelf's own
    // currency — `None` only on a shelf nothing was ever received onto.
    let value = stock
        .currency()
        .map(|currency| {
            let back = Money::checked_sum(
                joined
                    .iter()
                    .map(|portion| portion.cost)
                    .chain(settles.and_then(|owed| owed.cost)),
                currency,
            )?;
            back.checked_sub(Money::checked_sum(
                taken.iter().map(|portion| portion.cost),
                currency,
            )?)
        })
        .transpose()?;

    Ok(StockEvent::Counted {
        lot: count.lot.clone(),
        expected,
        declared: count.declared,
        variance: count
            .declared
            .checked_sub(expected)
            .ok_or(InventoryError::NotAQuantity)?,
        value,
        taken,
        joined,
        settles,
        reference: count.reference.clone(),
        at: count.at,
    })
}

/// **One name is on a shelf once.** The one place a named unit arriving is
/// checked against what the shelf is already holding, and both ways a unit
/// arrives ask it: a delivery, and a return putting a sold unit back. The
/// second is the one that was missed — a phone sold, received again under the
/// same serial, and then its sale's credit note would have put a second copy
/// of it on the shelf.
fn lands<'a>(
    stock: &Stock,
    serials: impl IntoIterator<Item = &'a String>,
) -> Result<(), InventoryError> {
    match serials
        .into_iter()
        .find(|serial| stock.lot_holding(serial).is_some())
    {
        Some(held) => Err(InventoryError::SerialAlreadyHeld(held.clone())),
        None => Ok(()),
    }
}

/// **What a shelf has to be before a delivery may land on it.**
///
/// Three things an entry is refused for: a branch nobody opened, an amount in a
/// currency the inventory account is not kept in, and a name too long to be an
/// `AggregateId`. A receipt posts now, so two of the three would be refused by
/// [`post_entry_in`](ledger::post_entry_in) a moment later and with the same
/// error — this is the door they are asked at, before the shelf is loaded,
/// because a receipt is the only way stock arrives and a refusal that names the
/// shelf is worth more than one that names a line.
///
/// **The currency is the one the posting cannot phrase.** A delivery in a
/// currency the inventory account is not kept in comes back from the ledger as
/// `MixedCurrencies`, which names neither the shelf nor the cause; here it
/// names the account and what it is kept in.
///
/// **On its own connection rather than in the movement's transaction**, because
/// what it asks is about the *shelf* and not about the instant: a branch id
/// nobody ever opened does not become open while a delivery commits, and an
/// account's currency is frozen when it is opened. A branch closed in that same
/// instant is refused by the posting anyway, in the transaction.
async fn usable_shelf(
    conn: &mut sqlx::PgConnection,
    shelf: &AggregateId,
    receipt: &Receipt,
    metadata: &Metadata,
) -> Result<(), ExecuteError<InventoryError>> {
    // **The ledger's own question, and the ledger's own refusal.** Not a
    // message of this module's: a check that answers differently from the
    // command it is guarding is worse than no check, and what a write-off at
    // this branch would say is exactly what a delivery to it should say.
    if let Some(branch) = metadata.branch() {
        let open = match AggregateId::new(branch) {
            Ok(id) => branches::accepts_documents(&mut *conn, &id)
                .await
                .map_err(ExecuteError::Load)?,
            Err(_) => false,
        };
        if !open {
            return Err(rejection(InventoryError::Ledger(
                ledger::LedgerError::NoSuchBranch(branch.to_owned()),
            )));
        }
    }

    // Every prefix this module posts under is two characters long, so the name
    // a write-off would build for *this* reference is the name this receipt, a
    // count and a consumption would build too: one of them answers for all
    // four. It does not promise that every later key fits — a longer one still
    // will not, and is refused with both halves named.
    entry_id("iw", shelf, &receipt.reference).map_err(rejection)?;

    let accounts = crate::PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| rejection(InventoryError::Config(e)))?;
    if let Some(kept) = ledger::posting_currency(&mut *conn, &accounts.inventory)
        .await
        .map_err(ExecuteError::Load)?
        .filter(|kept| *kept != receipt.value.currency())
    {
        return Err(rejection(InventoryError::WrongCurrency {
            id: accounts.inventory.to_string(),
            kept: kept.to_string(),
        }));
    }
    Ok(())
}

/// The shelf this request is about: the product, at the branch the request
/// carries. See `crate::stock::stock_id`.
fn shelf_of(product: &AggregateId, metadata: &Metadata) -> Result<AggregateId, InventoryError> {
    stock_id(product, metadata.branch())
        .map_err(|_| InventoryError::NotAProductId(product.to_string()))
}

fn label(value: &str) -> Result<String, InventoryError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_LABEL {
        return Err(InventoryError::NeedsANameAndAUnit);
    }
    Ok(value.to_owned())
}

fn rejected(error: InventoryError) -> Refusal {
    CommandError::Execute(ExecuteError::Rejected(error))
}

fn rejection(error: InventoryError) -> ExecuteError<InventoryError> {
    ExecuteError::Rejected(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picking::Portion;
    use crate::stock::WentOut;

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            erp_types::CurrencyCode::new("SAR")
                .unwrap_or_else(|_| unreachable!("SAR is a real code")),
        )
    }

    fn names(serials: &[&str]) -> Vec<String> {
        serials.iter().map(|serial| (*serial).to_owned()).collect()
    }

    /// What one sale still has out: two phones off one lot at 600.00 together,
    /// and one off another at 350.00.
    fn phones() -> WentOut {
        let portion = |lot: &str, quantity, minor, serials: &[&str]| Portion {
            lot: lot.to_owned(),
            quantity,
            cost: sar(minor),
            serials: names(serials),
            code: None,
            expires_on: None,
        };
        WentOut {
            portions: vec![
                portion("l1", 2, 60_000, &["SN-1", "SN-2"]),
                portion("l2", 1, 35_000, &["SN-3"]),
            ],
            shortfall: None,
            counted: None,
        }
    }

    /// **Named units come back by name**, each off the portion it went out on
    /// and at that portion's share of what the sale froze — in the order the
    /// sale took them, not the order they were typed.
    #[test]
    fn named_units_come_back_off_the_portion_they_went_out_on() {
        let back = named_back(&phones(), Some(2), &names(&["SN-3", "SN-2"])).expect("both are out");
        let shape: Vec<_> = back
            .iter()
            .map(|portion| {
                (
                    portion.lot.as_str(),
                    portion.quantity,
                    portion.cost.minor(),
                    portion.serials.clone(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("l1", 1, 30_000, names(&["SN-2"])),
                ("l2", 1, 35_000, names(&["SN-3"])),
            ]
        );
    }

    /// **A name that is not out is refused, and so is one given twice** — one
    /// unit cannot come back two times in one return — and so are names that
    /// disagree with the quantity beside them.
    #[test]
    fn a_name_that_is_not_out_is_refused() {
        assert!(
            matches!(
                named_back(&phones(), Some(1), &names(&["SN-9"])),
                Err(InventoryError::NotOut(serial)) if serial == "SN-9"
            ),
            "never went out on this sale"
        );
        assert!(
            matches!(
                named_back(&phones(), None, &names(&["SN-1", "SN-1"])),
                Err(InventoryError::NotOut(serial)) if serial == "SN-1"
            ),
            "one phone named twice is one phone"
        );
        assert!(
            matches!(
                named_back(&phones(), Some(3), &names(&["SN-1"])),
                Err(InventoryError::NeedsSerials { units: 3, named: 1 })
            ),
            "three coming back and one named"
        );
    }
}
