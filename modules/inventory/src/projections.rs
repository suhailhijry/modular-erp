//! What stock reads as.
//!
//! **Nothing here decides anything.** What is on hand, which lot a movement
//! comes off and what it costs are aggregate state, rehydrated from the log
//! inside a command (L3, L7); these tables are what a screen reads and what a
//! manager prints. A stock figure taken from here and used to refuse a sale
//! would be a number from whenever the worker last ran.
//!
//! # Why a movement carries a signed delta and not a running total
//!
//! Because a running total is a read, and a projection may not read while it is
//! applying (L2). Each event already says what it did — plus on a receipt,
//! minus on each portion of a write-off or a consumption, and the variance
//! itself on a count — so the sum of the movements *is* what is on hand. That is the canary this
//! module is built around, and it holds **lot by lot**: one movement row per
//! portion, naming the lot it came off, so `lot.remaining` is the sum of the
//! rows against it and nothing reconciles the two.
//!
//! # Why one event can be several rows
//!
//! A write-off of twelve off two lots is two portions, costed on their own
//! lots, and flattening them into one row would lose exactly the fact this
//! module exists to keep. `seq` orders them inside the event and completes the
//! movement key.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Cursor, Money, Page, Timestamp};
use sqlx::PgConnection;

use crate::picking::Portion;
use crate::product::ProductEvent;
use crate::stock::{StockEvent, parts};

#[derive(Debug)]
pub struct Inventory;

impl ProjectionGroup for Inventory {
    const NAME: &'static str = "inventory";
    const SCHEMA: &'static str = "proj_inventory";
    /// 2: a serial a count did not find is `missing` (§75). 3: a return opens
    /// a `lot` row for units a count had cleared (§75, review). 4: a return
    /// that reopens an emptied lot moves its `recorded_at` (§77, review).
    const VERSION: i16 = 4;
}

#[derive(Debug)]
pub struct Shelves;

#[async_trait::async_trait]
impl Projection for Shelves {
    type Group = Inventory;

    fn name(&self) -> &'static str {
        "shelves"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        let event_name = envelope.event_name.as_str();
        if ProductEvent::NAMES.contains(&event_name) {
            return declared(ctx, envelope, conn).await;
        }
        if StockEvent::NAMES.contains(&event_name) {
            return moved(ctx, envelope, conn).await;
        }
        // An event this build does not recognise is skipped, not refused:
        // refusing would stop the group for every tenant.
        Ok(())
    }
}

async fn declared(
    ctx: &ProjectionCtx<'_>,
    envelope: &Envelope,
    conn: &mut PgConnection,
) -> Result<(), ProjectionError> {
    let ProductEvent::Declared {
        name,
        unit,
        tracking,
        at,
    } = ctx
        .decode::<ProductEvent>(envelope)
        .map_err(|source| decode(envelope, source))?;

    sqlx::query(
        "INSERT INTO product (id, name, unit, tracking, declared_at, recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(envelope.stream.id.as_str())
    .bind(&name)
    .bind(&unit)
    .bind(tracking.as_str())
    .bind(at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The lots, the serials, the movements and the shelf — in that order, because
/// each row names the one before it.
async fn moved(
    ctx: &ProjectionCtx<'_>,
    envelope: &Envelope,
    conn: &mut PgConnection,
) -> Result<(), ProjectionError> {
    // **The product and the branch come out of the stream's own name**, so no
    // event repeats what its key already says. See `stock::stock_id`.
    let stream = envelope.stream.id.as_str();
    let (product, branch) = parts(stream);
    let shelf = Shelf {
        stream,
        product,
        branch,
    };
    let event = ctx
        .decode::<StockEvent>(envelope)
        .map_err(|source| decode(envelope, source))?;

    match event {
        StockEvent::Received { .. } => received(ctx, conn, &shelf, event).await,
        StockEvent::Consumed { .. } | StockEvent::WrittenOff { .. } => {
            taken(ctx, conn, &shelf, event).await
        }
        StockEvent::Restored { .. } => restored(ctx, conn, &shelf, event).await,
        StockEvent::Counted { .. } => counted(ctx, conn, &shelf, event).await,
    }
}

async fn received(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    event: StockEvent,
) -> Result<(), ProjectionError> {
    let StockEvent::Received {
        lot,
        code,
        expires_on,
        quantity,
        value,
        serials,
        reference,
        at,
    } = event
    else {
        return Ok(());
    };

    sqlx::query(
        "INSERT INTO lot
             (id, stock, product, branch, code, expires_on, quantity, remaining,
              value, currency, received_at, recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$7,$8,$9,$10,$11,$12)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&lot)
    .bind(shelf.stream)
    .bind(shelf.product)
    .bind(shelf.branch)
    .bind(code.as_deref())
    .bind(expires_on)
    .bind(quantity)
    .bind(value.minor())
    .bind(value.currency().as_str())
    .bind(at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;

    for serial in &serials {
        // **A receipt is the newest fact about a named unit**, so it overwrites
        // what the row said rather than losing to it. A serial that has left
        // the shelf may be received again — `receive` refuses only one it is
        // still holding — and first-write-wins would leave the screen saying
        // `written_off` on a closed lot while the log says the unit is on hand
        // in a new one. A replay re-applies the events in order, so the last
        // receipt still wins.
        sqlx::query(
            "INSERT INTO serial
                 (product, serial, lot, stock, branch, state, received_at,
                  recorded_at, position)
             VALUES ($1,$2,$3,$4,$5,'on_hand',$6,$7,$8)
             ON CONFLICT (product, stock, serial) DO UPDATE
                 SET lot = EXCLUDED.lot,
                     branch = EXCLUDED.branch,
                     state = 'on_hand',
                     left_at = NULL,
                     received_at = EXCLUDED.received_at,
                     recorded_at = EXCLUDED.recorded_at,
                     position = EXCLUDED.position",
        )
        .bind(shelf.product)
        .bind(serial)
        .bind(&lot)
        .bind(shelf.stream)
        .bind(shelf.branch)
        .bind(at)
        .bind(ctx.event_time())
        .bind(ctx.position().get())
        .execute(&mut *conn)
        .await?;
    }

    movement(
        ctx,
        conn,
        shelf,
        0,
        &Moved {
            kind: "received",
            reason: None,
            lot: Some(&lot),
            quantity,
            value: value.minor(),
            currency: Some(value.currency()),
            expected: None,
            declared: None,
            reference: &reference,
            at,
        },
    )
    .await?;

    on_shelf(
        ctx,
        conn,
        shelf,
        quantity,
        value.minor(),
        Some(value.currency()),
        at,
    )
    .await
}

/// **Everything that leaves the shelf**, whichever way it left.
///
/// One function for both, because a consumption and a write-off differ in
/// exactly two things a reader can see here — the `kind` on the movement and
/// the state a named unit lands in — and nothing else about drawing a lot down
/// changes with the reason. A second copy of this would be a second place for
/// the signed deltas to drift.
async fn taken(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    event: StockEvent,
) -> Result<(), ProjectionError> {
    let (kind, state, reason, portions, shortfall, reference, at) = match event {
        StockEvent::Consumed {
            portions,
            shortfall,
            reference,
            at,
        } => ("consumed", "sold", None, portions, shortfall, reference, at),
        StockEvent::WrittenOff {
            reason,
            portions,
            reference,
            at,
        } => (
            "written_off",
            "written_off",
            Some(reason.as_str()),
            portions,
            None,
            reference,
            at,
        ),
        _ => return Ok(()),
    };

    let currency = portions
        .first()
        .map(|portion| portion.cost.currency())
        .or_else(|| shortfall.and_then(|short| short.cost).map(Money::currency));
    if portions.is_empty() && shortfall.is_none() {
        // A movement of nothing, which no command writes. Skipped rather than
        // refused, for the reason an unknown event is.
        return Ok(());
    }

    let mut quantity = 0_i64;
    let mut value = 0_i64;
    for (seq, portion) in portions.iter().enumerate() {
        drawn_down(conn, portion).await?;
        gone(ctx, conn, shelf, portion, state, at).await?;
        movement(
            ctx,
            conn,
            shelf,
            i32::try_from(seq).unwrap_or(i32::MAX),
            &Moved {
                kind,
                reason,
                lot: Some(&portion.lot),
                quantity: -portion.quantity,
                value: -portion.cost.minor(),
                currency,
                expected: None,
                declared: None,
                reference: &reference,
                at,
            },
        )
        .await?;
        quantity -= portion.quantity;
        value -= portion.cost.minor();
    }

    // **What no lot could cover, on a row of its own with no lot on it.** A
    // plain product's sale may go below zero (R1); the row is what a count is
    // read against, and folding it into a portion would claim a lot gave up
    // units it never had.
    if let Some(short) = shortfall {
        movement(
            ctx,
            conn,
            shelf,
            i32::try_from(portions.len()).unwrap_or(i32::MAX),
            &Moved {
                kind,
                reason,
                lot: None,
                quantity: -short.quantity,
                value: -short.cost.map_or(0, Money::minor),
                currency,
                expected: None,
                declared: None,
                reference: &reference,
                at,
            },
        )
        .await?;
        quantity -= short.quantity;
        value -= short.cost.map_or(0, Money::minor);
    }

    on_shelf(ctx, conn, shelf, quantity, value, currency, at).await
}

/// **What a credit note put back**, onto the lots it left.
///
/// `received`, because that is what the column means — bought in, or put back —
/// and a screen showing a shelf going up wants one word for both. The lot rows
/// are the ones the sale drew down, including ones it closed: this table keeps
/// every lot and only the aggregate drops the empty ones.
async fn restored(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    event: StockEvent,
) -> Result<(), ProjectionError> {
    let StockEvent::Restored {
        portions,
        settles,
        reference,
        at,
        ..
    } = event
    else {
        return Ok(());
    };

    let currency = portions
        .first()
        .map(|portion| portion.cost.currency())
        .or_else(|| settles.and_then(|short| short.cost).map(Money::currency));

    let mut quantity = 0_i64;
    let mut value = 0_i64;
    for (seq, portion) in portions.iter().enumerate() {
        put_back(ctx, conn, shelf, portion, at).await?;
        gone(ctx, conn, shelf, portion, "on_hand", at).await?;
        movement(
            ctx,
            conn,
            shelf,
            i32::try_from(seq).unwrap_or(i32::MAX),
            &Moved {
                kind: "received",
                reason: None,
                lot: Some(&portion.lot),
                quantity: portion.quantity,
                value: portion.cost.minor(),
                currency,
                expected: None,
                declared: None,
                reference: &reference,
                at,
            },
        )
        .await?;
        quantity += portion.quantity;
        value += portion.cost.minor();
    }

    // What the return settled of what the shelf owed — no lot, for the reason
    // the shortfall row above has none.
    if let Some(settled) = settles {
        movement(
            ctx,
            conn,
            shelf,
            i32::try_from(portions.len()).unwrap_or(i32::MAX),
            &Moved {
                kind: "received",
                reason: None,
                lot: None,
                quantity: settled.quantity,
                value: settled.cost.map_or(0, Money::minor),
                currency,
                expected: None,
                declared: None,
                reference: &reference,
                at,
            },
        )
        .await?;
        quantity += settled.quantity;
        value += settled.cost.map_or(0, Money::minor);
    }

    if quantity == 0 && value == 0 {
        return Ok(());
    }
    on_shelf(ctx, conn, shelf, quantity, value, currency, at).await
}

/// **What a count found**, one row per lot the difference landed on.
///
/// A shortage's portions come off their lots and a named unit nobody found is
/// marked `missing`; an overage goes onto the lot it joined; what the count
/// settled of the shelf's debt is a row with no lot, for the reason a
/// shortfall's has none. **A count that moved nothing still leaves a row**,
/// because somebody counting and finding the books right is a fact worth
/// keeping. Every row carries the count's own `expected` and `declared`.
async fn counted(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    event: StockEvent,
) -> Result<(), ProjectionError> {
    let StockEvent::Counted {
        lot,
        expected,
        declared,
        variance,
        value,
        taken,
        joined,
        settles,
        reference,
        at,
    } = event
    else {
        return Ok(());
    };

    let row = Moved {
        kind: "counted",
        reason: None,
        lot: lot.as_deref(),
        quantity: 0,
        value: 0,
        currency: value.map(Money::currency),
        expected: Some(expected),
        declared: Some(declared),
        reference: &reference,
        at,
    };
    let mut rows = Vec::new();
    for portion in &taken {
        drawn_down(conn, portion).await?;
        gone(ctx, conn, shelf, portion, "missing", at).await?;
        rows.push(Moved {
            lot: Some(&portion.lot),
            quantity: -portion.quantity,
            value: -portion.cost.minor(),
            ..row
        });
    }
    // **`position` is left alone** on the lot an overage joins: it is the
    // receipt's position and therefore the received order, which is what the
    // listing sorts by. See `schema/install.sql`.
    if let Some(portion) = &joined {
        put_back(ctx, conn, shelf, portion, at).await?;
        rows.push(Moved {
            lot: Some(&portion.lot),
            quantity: portion.quantity,
            value: portion.cost.minor(),
            ..row
        });
    }
    if let Some(settled) = settles {
        rows.push(Moved {
            lot: None,
            quantity: settled.quantity,
            value: settled.cost.map_or(0, Money::minor),
            ..row
        });
    }
    if rows.is_empty() {
        rows.push(row);
    }
    for (seq, moved) in rows.iter().enumerate() {
        movement(
            ctx,
            conn,
            shelf,
            i32::try_from(seq).unwrap_or(i32::MAX),
            moved,
        )
        .await?;
    }

    on_shelf(
        ctx,
        conn,
        shelf,
        variance,
        value.map_or(0, Money::minor),
        value.map(Money::currency),
        at,
    )
    .await
}

/// Which shelf a row belongs to, so three arguments do not travel separately.
struct Shelf<'a> {
    stream: &'a str,
    product: &'a str,
    branch: Option<&'a str>,
}

/// One movement row: **what this did**, never what the total became.
struct Moved<'a> {
    kind: &'static str,
    reason: Option<&'static str>,
    lot: Option<&'a str>,
    quantity: i64,
    value: i64,
    /// **Null only when nothing has ever been received onto the shelf** and the
    /// movement is a shortfall worth nothing: there is no purchase behind those
    /// units, so there is no currency to state their zero in.
    currency: Option<CurrencyCode>,
    expected: Option<i64>,
    declared: Option<i64>,
    reference: &'a str,
    at: Timestamp,
}

async fn movement(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    seq: i32,
    moved: &Moved<'_>,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO stock_movement
             (stock, position, seq, product, branch, lot, kind, reason, quantity,
              value, currency, expected, declared, reference, moved_at, recorded_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
         ON CONFLICT (stock, position, seq) DO NOTHING",
    )
    .bind(shelf.stream)
    .bind(ctx.position().get())
    .bind(seq)
    .bind(shelf.product)
    .bind(shelf.branch)
    .bind(moved.lot)
    .bind(moved.kind)
    .bind(moved.reason)
    .bind(moved.quantity)
    .bind(moved.value)
    .bind(moved.currency.as_ref().map(CurrencyCode::as_str))
    .bind(moved.expected)
    .bind(moved.declared)
    .bind(moved.reference)
    .bind(moved.at)
    .bind(ctx.event_time())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The shelf itself: the movements above, added up as they arrive.
async fn on_shelf(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    quantity: i64,
    value: i64,
    currency: Option<CurrencyCode>,
    at: Timestamp,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO stock_item
             (stock, product, branch, on_hand, value, currency,
              last_at, recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (stock) DO UPDATE
             SET on_hand = stock_item.on_hand + EXCLUDED.on_hand,
                 value = stock_item.value + EXCLUDED.value,
                 currency = COALESCE(stock_item.currency, EXCLUDED.currency),
                 last_at = GREATEST(stock_item.last_at, EXCLUDED.last_at),
                 recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(shelf.stream)
    .bind(shelf.product)
    .bind(shelf.branch)
    .bind(quantity)
    .bind(value)
    .bind(currency.as_ref().map(CurrencyCode::as_str))
    .bind(at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One lot, less what a portion took off it.
async fn drawn_down(conn: &mut PgConnection, portion: &Portion) -> Result<(), ProjectionError> {
    sqlx::query(
        "UPDATE lot
            SET remaining = remaining - $2,
                value = value - $3
          WHERE id = $1",
    )
    .bind(&portion.lot)
    .bind(portion.quantity)
    .bind(portion.cost.minor())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One lot, plus what a return or a count put back on it — including a lot the
/// sale closed, which this table still has a row for.
///
/// **And a lot it has never seen**, which a return opens for units a count had
/// cleared (`inventory::returned_lot_of`): no receipt made that row, so this is
/// where it is made, positioned at the return. An `UPDATE` alone left the lot on
/// the shelf and off the screen. A lot that exists keeps its `quantity` and its
/// `position`, which are the receipt's — and its `recorded_at` while it is still
/// open. **One that had emptied takes the return's**, because that is when it
/// came back onto the shelf: the worker's check that the bell rang for a lot
/// going off counts from it.
async fn put_back(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    portion: &Portion,
    at: Timestamp,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO lot
             (id, stock, product, branch, code, expires_on, quantity, remaining,
              value, currency, received_at, recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$7,$8,$9,$10,$11,$12)
         ON CONFLICT (id) DO UPDATE
             SET remaining = lot.remaining + EXCLUDED.remaining,
                 value = lot.value + EXCLUDED.value,
                 -- **A lot that had emptied is back on the shelf now**, and
                 -- nothing could have said anything about it before.
                 recorded_at = CASE WHEN lot.remaining > 0 THEN lot.recorded_at
                                    ELSE EXCLUDED.recorded_at END",
    )
    .bind(&portion.lot)
    .bind(shelf.stream)
    .bind(shelf.product)
    .bind(shelf.branch)
    .bind(portion.code.as_deref())
    .bind(portion.expires_on)
    .bind(portion.quantity)
    .bind(portion.cost.minor())
    .bind(portion.cost.currency().as_str())
    .bind(at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The named units a portion took, marked as gone — **on the shelf they left**,
/// and **saying which way they went**.
///
/// Scoped by the stream and not by the product alone, because the same name may
/// be on hand at two branches: the write side keeps one `Stock` per branch and
/// neither sees the other's serials. Olaya throwing a machine away does not make
/// Malaz's disappear off the screen.
///
/// `state` is `written_off`, `sold` or — when a credit note puts the unit back
/// — `on_hand` again, which is the whole of what the caller changes: a machine
/// thrown away and a machine sold are both off the shelf and only one of them
/// is a loss, and the column is what a screen reads to tell them apart.
async fn gone(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    shelf: &Shelf<'_>,
    portion: &Portion,
    state: &str,
    at: Timestamp,
) -> Result<(), ProjectionError> {
    if portion.serials.is_empty() {
        return Ok(());
    }
    sqlx::query(
        // **`left_at` is cleared when the unit comes back**, because the column
        // says when it stopped being on hand and it has not. Written here
        // rather than in a second statement so the one place a serial's state
        // moves stays one place.
        "UPDATE serial
            SET state = $4,
                left_at = CASE WHEN $4 = 'on_hand' THEN NULL ELSE $5 END,
                recorded_at = $6,
                position = $7
          WHERE product = $1 AND stock = $2 AND serial = ANY($3)",
    )
    .bind(shelf.product)
    .bind(shelf.stream)
    .bind(&portion.serials)
    .bind(state)
    .bind(at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn decode(envelope: &Envelope, source: erp_eventlog::UpcastError) -> ProjectionError {
    ProjectionError::Decode {
        event_name: envelope.event_name.as_str().to_owned(),
        position: envelope.position,
        source,
    }
}

#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Inventory>>> {
    vec![std::sync::Arc::new(Shelves)]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// The day an undated lot sorts as, so "undated last" survives a keyset page.
///
/// `NULLS LAST` cannot be expressed in a `(a, b) > (a, b)` comparison, and the
/// index in `install.sql` is built on this same expression. Both ends are days
/// Postgres can hold: `NaiveDate::MIN` is year -262143 and `DATE` refuses it.
const NEVER: &str = "9999-12-31";
const BEFORE_ANY: &str = "0001-01-01";

/// A literal in this crate, which a build that shipped a bad one would fail on
/// its first listing rather than at a customer.
fn sentinel(literal: &str) -> chrono::NaiveDate {
    literal
        .parse()
        .unwrap_or_else(|_| unreachable!("a date literal in this crate"))
}

/// A product, as a list of them shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductRow {
    pub id: String,
    pub name: String,
    /// What one of these is, in the business's own words. Frozen.
    pub unit: String,
    /// `none`, `lot` or `serial`. Frozen.
    pub tracking: String,
    pub declared_at: Timestamp,
}

/// What the business keeps, by name.
///
/// # Errors
/// The database.
pub async fn products(
    conn: &mut PgConnection,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<ProductRow>, sqlx::Error> {
    let since = cursor_text(after, 0);

    let rows = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", unit as "unit!",
                  tracking as "tracking!", declared_at as "declared_at!"
             FROM proj_inventory.product
            WHERE id > $1
            ORDER BY id
            LIMIT $2"#,
        since,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| ProductRow {
                id: r.id,
                name: r.name,
                unit: r.unit,
                tracking: r.tracking,
                declared_at: r.declared_at,
            })
            .collect(),
        limit,
        |row: &ProductRow| Cursor::over(&[&row.id]),
    ))
}

/// One shelf: a product at a branch, what is on it and what it is carried at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockRow {
    pub stock: String,
    pub product: String,
    /// The product's name, joined from `product` at read time. `None` when
    /// this read model holds no declaration for it — a row is not dropped, and
    /// a listing does not fail, over a name.
    pub name: Option<String>,
    pub branch: Option<String>,
    pub on_hand: i64,
    pub value: i64,
    /// `None` until something has been received onto this shelf.
    pub currency: Option<String>,
    pub last_at: Timestamp,
}

/// What is on hand, by product and branch — one product, one branch, or both
/// when asked.
///
/// # Errors
/// The database.
pub async fn stock(
    conn: &mut PgConnection,
    product: Option<&str>,
    branch: Option<&str>,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<StockRow>, sqlx::Error> {
    let since = cursor_text(after, 0);

    let rows = sqlx::query!(
        r#"SELECT s.stock as "stock!", s.product as "product!", p.name as "name?",
                  s.branch, s.on_hand as "on_hand!", s.value as "value!", s.currency,
                  s.last_at as "last_at!"
             FROM proj_inventory.stock_item s
             LEFT JOIN proj_inventory.product p ON p.id = s.product
            WHERE s.stock > $1 AND ($2::TEXT IS NULL OR s.product = $2)
              AND ($3::TEXT IS NULL OR s.branch = $3)
            ORDER BY s.stock
            LIMIT $4"#,
        since,
        product,
        branch,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| StockRow {
                stock: r.stock,
                product: r.product,
                name: r.name,
                branch: r.branch,
                on_hand: r.on_hand,
                value: r.value,
                currency: r.currency,
                last_at: r.last_at,
            })
            .collect(),
        limit,
        |row: &StockRow| Cursor::over(&[&row.stock]),
    ))
}

/// One open lot, and what is still on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LotRow {
    pub id: String,
    pub product: String,
    /// The product's name, as [`StockRow::name`] has it.
    pub name: Option<String>,
    pub branch: Option<String>,
    /// The tenant's own batch code, on a lot-tracked product.
    pub code: Option<String>,
    /// `None` on a lot of something that does not spoil.
    pub expires_on: Option<chrono::NaiveDate>,
    /// What arrived.
    pub quantity: i64,
    /// What is still here.
    pub remaining: i64,
    pub value: i64,
    pub currency: String,
    pub received_at: Timestamp,
    /// **When this lot came onto the shelf for anything reading the log** —
    /// its receipt written, or a return putting units back once it had emptied
    /// — which a delivery dated in the past is not.
    pub recorded_at: Timestamp,
    /// **The receipt's log position**, which is the received order this listing
    /// breaks ties by — and the cursor's second half.
    pub position: i64,
    /// The units still on this lot, by name, on a serial-tracked product.
    pub serials: Vec<String>,
}

/// **Open lots, in the order stock will go out of them.**
///
/// Earliest expiry first, undated last, oldest received first among equals —
/// the same sort `crate::picking::pick` follows, so the screen a manager reads
/// and the rule the system obeys cannot drift apart.
///
/// Closed lots are not here: a lot with nothing on it cannot expire into
/// anything, and what it did is in `movements`.
///
/// # Errors
/// The database.
pub async fn lots(
    conn: &mut PgConnection,
    product: Option<&str>,
    branch: Option<&str>,
    expiring_before: Option<chrono::NaiveDate>,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<LotRow>, sqlx::Error> {
    let first = sentinel(BEFORE_ANY);
    let since_day = match cursor_text(after, 0) {
        empty if empty.is_empty() => first,
        day => day.parse().unwrap_or(first),
    };
    let since_position = cursor_text(after, 1).parse::<i64>().unwrap_or(0);
    let never = sentinel(NEVER);

    let rows = sqlx::query!(
        r#"SELECT l.id as "id!", l.product as "product!", p.name as "name?", l.branch,
                  l.code, l.expires_on,
                  l.quantity as "quantity!", l.remaining as "remaining!",
                  l.value as "value!", l.currency as "currency!",
                  l.received_at as "received_at!", l.recorded_at as "recorded_at!",
                  l.position as "position!",
                  COALESCE(
                      ARRAY(SELECT s.serial FROM proj_inventory.serial s
                             WHERE s.lot = l.id AND s.state = 'on_hand'
                             ORDER BY s.serial),
                      '{}'
                  ) as "serials!: Vec<String>"
             FROM proj_inventory.lot l
             LEFT JOIN proj_inventory.product p ON p.id = l.product
            WHERE l.remaining > 0
              AND (COALESCE(l.expires_on, $5::DATE), l.position) > ($1::DATE, $2)
              AND ($3::TEXT IS NULL OR l.product = $3)
              -- Strictly before, which is what `expiring_before` says: a batch
              -- dated the day asked for is still good that day.
              AND ($4::DATE IS NULL OR l.expires_on < $4)
              AND ($7::TEXT IS NULL OR l.branch = $7)
            ORDER BY COALESCE(l.expires_on, $5::DATE), l.position
            LIMIT $6"#,
        since_day,
        since_position,
        product,
        expiring_before,
        never,
        limit,
        branch,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| LotRow {
                id: r.id,
                product: r.product,
                name: r.name,
                branch: r.branch,
                code: r.code,
                expires_on: r.expires_on,
                quantity: r.quantity,
                remaining: r.remaining,
                value: r.value,
                currency: r.currency,
                received_at: r.received_at,
                recorded_at: r.recorded_at,
                position: r.position,
                serials: r.serials,
            })
            .collect(),
        limit,
        |row: &LotRow| {
            Cursor::over(&[
                &row.expires_on
                    .map_or_else(|| NEVER.to_owned(), |day| day.to_string()),
                &row.position.to_string(),
            ])
        },
    ))
}

/// **One lot, open or emptied** — what a message about it says: its batch,
/// its date, and where it is.
///
/// Not `lots` with a filter: that listing is the shelf, and a lot that emptied
/// after somebody was told about it is still the lot they were told about.
///
/// # Errors
/// The database.
pub async fn lot(conn: &mut PgConnection, id: &str) -> Result<Option<LotRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT l.id as "id!", l.product as "product!", p.name as "name?", l.branch,
                  l.code, l.expires_on,
                  l.quantity as "quantity!", l.remaining as "remaining!",
                  l.value as "value!", l.currency as "currency!",
                  l.received_at as "received_at!", l.recorded_at as "recorded_at!",
                  l.position as "position!",
                  COALESCE(
                      ARRAY(SELECT s.serial FROM proj_inventory.serial s
                             WHERE s.lot = l.id AND s.state = 'on_hand'
                             ORDER BY s.serial),
                      '{}'
                  ) as "serials!: Vec<String>"
             FROM proj_inventory.lot l
             LEFT JOIN proj_inventory.product p ON p.id = l.product
            WHERE l.id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| LotRow {
        id: r.id,
        product: r.product,
        name: r.name,
        branch: r.branch,
        code: r.code,
        expires_on: r.expires_on,
        quantity: r.quantity,
        remaining: r.remaining,
        value: r.value,
        currency: r.currency,
        received_at: r.received_at,
        recorded_at: r.recorded_at,
        position: r.position,
        serials: r.serials,
    }))
}

/// One product, by the key it was declared under.
///
/// # Errors
/// The database.
pub async fn product(conn: &mut PgConnection, id: &str) -> Result<Option<ProductRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", unit as "unit!",
                  tracking as "tracking!", declared_at as "declared_at!"
             FROM proj_inventory.product
            WHERE id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| ProductRow {
        id: r.id,
        name: r.name,
        unit: r.unit,
        tracking: r.tracking,
        declared_at: r.declared_at,
    }))
}

/// One movement against one lot: why the number above is what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovementRow {
    pub product: String,
    pub branch: Option<String>,
    /// Null only where no lot was involved: a plain product's shortfall, the
    /// part of a return or a count that settles that debt, and the row a count
    /// of the shelf leaves when it moved nothing.
    pub lot: Option<String>,
    /// `received`, `consumed`, `written_off` or `counted`.
    pub kind: String,
    /// `expired` or `damaged`, on a write-off.
    pub reason: Option<String>,
    /// Signed: what this did to what is on hand.
    pub quantity: i64,
    pub value: i64,
    pub currency: Option<String>,
    /// Set on a count — the lot's, or the shelf's on a count of the shelf —
    /// and frozen as it was found (L5).
    pub expected: Option<i64>,
    pub declared: Option<i64>,
    pub reference: String,
    pub moved_at: Timestamp,
    pub position: i64,
    pub seq: i32,
}

/// What moved, newest first.
///
/// # Errors
/// The database.
pub async fn movements(
    conn: &mut PgConnection,
    product: Option<&str>,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<MovementRow>, sqlx::Error> {
    let (since, since_seq) = match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (
            parts[0].parse::<i64>().unwrap_or(i64::MAX),
            parts[1].parse::<i32>().unwrap_or(i32::MAX),
        ),
        _ => (i64::MAX, i32::MAX),
    };

    let rows = sqlx::query!(
        r#"SELECT product as "product!", branch, lot, kind as "kind!", reason,
                  quantity as "quantity!", value as "value!", currency,
                  expected, declared, reference as "reference!",
                  moved_at as "moved_at!", position as "position!", seq as "seq!"
             FROM proj_inventory.stock_movement
            WHERE (position, seq) < ($1, $2)
              AND ($3::TEXT IS NULL OR product = $3)
            ORDER BY position DESC, seq DESC
            LIMIT $4"#,
        since,
        since_seq,
        product,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| MovementRow {
                product: r.product,
                branch: r.branch,
                lot: r.lot,
                kind: r.kind,
                reason: r.reason,
                quantity: r.quantity,
                value: r.value,
                currency: r.currency,
                expected: r.expected,
                declared: r.declared,
                reference: r.reference,
                moved_at: r.moved_at,
                position: r.position,
                seq: r.seq,
            })
            .collect(),
        limit,
        |row: &MovementRow| Cursor::over(&[&row.position.to_string(), &row.seq.to_string()]),
    ))
}

/// **The number `1300 Inventory` will have to agree with.**
///
/// What every shelf is carried at, per currency, in a stable order.
///
/// # Why this returns a number instead of checking it
///
/// The comparison needs the ledger's account balance, and that lives in
/// `proj_ledger` — a different projection group, which L3 forbids this module
/// from reading. The same half-a-canary `prepaid::outstanding` is, and for the
/// same reason.
///
/// # Why it is worth checking at all
///
/// Because two readings of the same log are only equal while every movement
/// books its entry. A receipt debits `1300`, and every write-off, count
/// variance and consumption credits it, all in the transaction that writes the
/// movement — so a difference here is a posting that did not happen or an entry
/// somebody made against the account by hand, and either is worth being told
/// about. `StockValueAgrees` in `bin/worker` is the other half, and it also
/// catches what this function cannot see: a balance on an account with no stock
/// row at all.
///
/// # Errors
/// The database.
pub async fn value_on_hand(conn: &mut PgConnection) -> Result<Vec<Money>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT currency as "currency!", SUM(value)::BIGINT as "held!"
             FROM proj_inventory.stock_item
            WHERE currency IS NOT NULL
            GROUP BY currency
            ORDER BY currency"#
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|r| {
            let currency =
                CurrencyCode::new(&r.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(Money::from_minor(r.held, currency))
        })
        .collect()
}

/// One branch's shelves, added up: what `GET /v1/inventory/summary` answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummaryRow {
    /// `None` for the shelves of a business that sends no `X-Branch`.
    pub branch: Option<String>,
    /// What the shelves here are carried at, as `(currency, minor units)`, one
    /// entry per currency in currency order: the sum of [`StockRow::value`]. A
    /// shelf nothing was ever received onto has no currency and adds nothing.
    pub value: Vec<(String, i64)>,
    /// How many products have a shelf here, whatever is on it: the rows
    /// [`stock`] lists for this branch.
    pub products: i64,
    /// Open lots whose date is from `today` to the window's last day.
    pub expiring: i64,
    /// Open lots whose date is before `today`, still on the shelf.
    pub expired: i64,
    /// The shelves here below zero, by product.
    pub below_zero: Vec<BelowZeroRow>,
}

/// A shelf below zero, and what it owes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BelowZeroRow {
    pub product: String,
    /// As [`StockRow::name`] has it.
    pub name: Option<String>,
    /// Negative.
    pub on_hand: i64,
    /// **The shelf's debt**, in minor units: what the units no lot covered were
    /// charged out at, less what returns and counts have settled of it. Not the
    /// shelf's value — a delivery since does not pay the debt, so a shelf may
    /// owe more than it is worth below zero.
    pub owes: i64,
    /// `None` while nothing has ever been received onto the shelf.
    pub currency: Option<String>,
}

/// **The shelves, a branch at a time**: what they are worth, how many products
/// are on them, how many lots are going off or gone, and which shelves are below
/// zero and owe what.
///
/// `today` is the tenant's day, through its calendar, and `expiring_through`
/// the window's last day from it (`ExpiryWindow::warns_until`): the caller works
/// both out, so nothing here reads a clock. A lot counts as `expired` before
/// `today` and as `expiring` from `today` to `expiring_through`, which is the
/// rule the worker tells people by — a batch dated today is still good today.
/// A lot with no date is neither.
///
/// # Added up at read time, never kept
///
/// Nothing here is a stored total. A projection may not read while it applies
/// (§71), so a running total a projection kept would be a total it could not
/// check; these are sums over the rows the projection already writes, taken
/// when asked. **What a shelf owes is its movement rows with no lot** — a
/// shortfall takes units off with none, and what a return or a count settles
/// puts them back with none — which is the aggregate's `Stock::owed`, read from
/// the rows.
///
/// **One snapshot.** The three reads share a `REPEATABLE READ` transaction, so
/// the value, the lot counts and the shelves below zero are one moment of the
/// read model and not three.
///
/// A branch with no shelf is not in the answer, and a tenant with no shelves
/// gets an empty one.
///
/// # Errors
/// The database.
pub async fn summary(
    conn: &mut PgConnection,
    branch: Option<&str>,
    today: chrono::NaiveDate,
    expiring_through: Option<chrono::NaiveDate>,
) -> Result<Vec<SummaryRow>, sqlx::Error> {
    let mut tx = sqlx::Connection::begin_with(
        &mut *conn,
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
    )
    .await?;

    let held = sqlx::query!(
        r#"SELECT branch, currency, COUNT(*) as "products!", SUM(value)::BIGINT as "value!"
             FROM proj_inventory.stock_item
            WHERE ($1::TEXT IS NULL OR branch = $1)
            GROUP BY branch, currency
            ORDER BY branch, currency"#,
        branch,
    )
    .fetch_all(&mut *tx)
    .await?;

    let dated = sqlx::query!(
        r#"SELECT branch,
                  COUNT(*) FILTER (WHERE expires_on < $2) as "expired!",
                  COUNT(*) FILTER (WHERE expires_on >= $2
                                     AND ($3::DATE IS NULL OR expires_on <= $3)) as "expiring!"
             FROM proj_inventory.lot
            WHERE remaining > 0 AND expires_on IS NOT NULL
              AND ($1::TEXT IS NULL OR branch = $1)
            GROUP BY branch"#,
        branch,
        today,
        expiring_through,
    )
    .fetch_all(&mut *tx)
    .await?;

    // ponytail: a shelf below zero sums every movement it has ever had to find
    // its debt. Only shelves below zero, which a count clears; a column the
    // projection keeps if one ever has years of history below zero.
    let short = sqlx::query!(
        r#"SELECT s.branch, s.product as "product!", p.name as "name?",
                  s.on_hand as "on_hand!", s.currency,
                  COALESCE((SELECT -SUM(m.value)
                              FROM proj_inventory.stock_movement m
                             WHERE m.stock = s.stock AND m.lot IS NULL), 0)::BIGINT as "owes!"
             FROM proj_inventory.stock_item s
             LEFT JOIN proj_inventory.product p ON p.id = s.product
            WHERE s.on_hand < 0 AND ($1::TEXT IS NULL OR s.branch = $1)
            ORDER BY s.product"#,
        branch,
    )
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut rows = std::collections::BTreeMap::new();
    for r in held {
        let row = branch_row(&mut rows, r.branch);
        row.products += r.products;
        if let Some(currency) = r.currency {
            row.value.push((currency, r.value));
        }
    }
    for r in dated {
        let row = branch_row(&mut rows, r.branch);
        row.expired = r.expired;
        row.expiring = r.expiring;
    }
    for r in short {
        branch_row(&mut rows, r.branch)
            .below_zero
            .push(BelowZeroRow {
                product: r.product,
                name: r.name,
                on_hand: r.on_hand,
                owes: r.owes,
                currency: r.currency,
            });
    }
    Ok(rows.into_values().collect())
}

/// The row for `branch`, started empty the first time it is seen. Ordered by
/// branch, with the shelves of no branch first.
fn branch_row(
    rows: &mut std::collections::BTreeMap<Option<String>, SummaryRow>,
    branch: Option<String>,
) -> &mut SummaryRow {
    rows.entry(branch.clone()).or_insert_with(|| SummaryRow {
        branch,
        ..SummaryRow::default()
    })
}

fn cursor_text(after: Option<&Cursor>, part: usize) -> String {
    after
        .map(Cursor::parts)
        .and_then(|parts| parts.get(part).cloned())
        .unwrap_or_default()
}
