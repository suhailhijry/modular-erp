# inventory

What is on the shelf, which delivery it came in, and what it cost.

**Depends on:** `ledger`, reads `branches`, plus the core.
**Depended on by:** `sales` and `purchases` — and so `pos`, whose sales are
`sales` invoices.

## What this module is for

Products declared once, stock received into **lots**, and every movement an
event — so *how much milk do we have*, *which batch is it* and *when does it go
off* have one answer, and it is the same answer tomorrow.

Three things decide its shape:

- **Cost is carried per lot, not averaged.** Each delivery keeps its own cost,
  and a unit leaves at the cost of the lot it left. A weighted average is two
  integers where this is a list and it is cheaper; it cannot answer *which
  delivery is this*, and that question is the whole of expiry, the whole of a
  recall and the whole of a serial.
- **A shelf is a product at a branch.** What is at Olaya is at Olaya. The branch
  already travels on every request, so the stream is keyed `{product}.{branch}`
  and a business that names no branch has one shelf per product.
- **Every movement that moves value posts, in its own transaction**, through
  `ledger::post_entry_in`. A shelf that moved without its entry is not a state
  this system can reach.

## The files

| File | What is in it |
|---|---|
| [`product.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/product.rs) | `Product`, `ProductEvent`, `Tracking` |
| [`picking.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/picking.rs) | `pick`, `OpenLot`, `Wanted`, `Portion`, `Shortfall` |
| [`stock.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/stock.rs) | `Stock`, `StockEvent`, `Returning`, `WentOut`, `Reason`, `stock_id` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/commands.rs) | `declare`, `receive`, `write_off`, `count`, `consume_in`, `restore_in` |
| [`posting.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/posting.rs) | `PostingAccounts`, and the entries a movement posts |
| [`expiry.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/expiry.rs) | `ExpiryWindow` |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/projections.rs) | The `Inventory` group: products, shelves, lots, serials, movements |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/inventory/src/http.rs) | The routes |

## A product, and how closely it is watched

```rust
pub enum Tracking { None, Lot, Serial }

pub async fn declare(db: &TenantDb, product: &AggregateId, name: &str, unit: &str,
    tracking: Tracking, at: Timestamp, metadata: &Metadata)
    -> Result<Committed<ProductEvent>, CommandError<InventoryError>>;
```

| Tracking | A delivery says | A movement names |
|---|---|---|
| `none` — flour, bolts | a quantity and what it cost | a quantity |
| `lot` — milk, medicine | also the batch code, and the expiry when it has one | a quantity, or a lot |
| `serial` — phones, machines | also one serial per unit | the units, by name |

**The unit and the tracking mode are frozen at declaration.** Every quantity
ever recorded is an integer in that unit, so changing either afterwards would
restate history; no route does. The mode decides what a delivery must say, not
whether lots exist — every receipt is a lot, whatever the mode.

## Lots, and the order stock leaves in

A lot is one delivery: a quantity, what it cost, and on a lot-tracked product
the tenant's own batch code and expiry date. Its id is derived from the shelf and
the receipt's key, never minted (L8), and the code is data — two deliveries of
`B-2026-04` are two lots:

```rust
pub fn stock_id(product: &AggregateId, branch: Option<&str>) -> Result<AggregateId, InvalidKey>;
pub fn lot_of(shelf: &AggregateId, reference: &str) -> String;   // "lot.{shelf}.{reference}"
```

**`pick` is the one rule everything that leaves a shelf goes through.** Dated
lots before undated ones, the earlier date first, and received order among
equals. A pharmacy's stock leaves in the order it will spoil; a hardware shop's,
whose lots carry no dates, leaves in the order it arrived — **down the same
function**, so there is no second rule for untracked products to drift.

**Naming a lot overrides it.** A scanned batch, a recall, stock promised to a
customer. Naming one is a claim about that lot, so a lot that is not open on
this shelf is refused (`inventory.no_such_lot`) — including a lot at another
branch — and one that holds fewer than was asked is refused
(`inventory.lot_is_short`) rather than topped up from the next.

**A portion costs its share of the lot**, through `Money::apportioned`, which is
exact at `n/n`: the movement that empties a lot takes whatever is left, so the
lot closes on exactly zero.

**Open lots only live in the aggregate.** A lot that empties leaves `Stock`; a
café receiving beans daily for years carries the few it can still pour from, and
the read model keeps every delivery ever made.

## Serials

A serial-tracked delivery names one serial per unit, **from the caller**. A
movement out that names a serial unknown, already gone, or twice is
**refused** — a count corrects a quantity, and nothing corrects a unit that was
never on the shelf. A unit that has left may be received again (back from
repair), and a serial already on the shelf is refused
(`inventory.serial_already_held`) whichever way it arrives, a delivery or a
return.

A name is unique **per shelf, not per tenant**: two branches are two aggregates,
and neither can see the other's names without a second stream read inside the
decision.

## Receiving, and the account it owes to

```rust
pub struct Receipt {
    pub quantity: i64,
    pub value: Money,                    // what the whole delivery cost
    pub code: Option<String>,            // lot-tracked only, and required there
    pub expires_on: Option<NaiveDate>,   // lot-tracked only
    pub serials: Vec<String>,            // serial-tracked only, one per unit
    pub reference: String,               // the request's key, and the lot's name
    pub at: Timestamp,
}

pub async fn receive(db: &TenantDb, product: &AggregateId, receipt: &Receipt,
    metadata: &Metadata) -> Result<Committed<StockEvent>, CommandError<InventoryError>>;
```

**A receipt posts `Dr 1300 Inventory`, `Cr 2010 Goods received, not invoiced`**
at what the delivery cost. The goods are an asset the moment they land; what
they owe is not accounts payable until somebody bills it. The supplier's bill
line that names the product debits `2010` back instead of its own account (see
`purchases`), so between the two documents `2010` is exactly what has arrived
and not been invoiced — and a bill that beats its delivery leaves it a debit,
*invoiced, not yet received*. Nothing refuses either order.

A delivery to a branch nobody opened is refused before the shelf is loaded, and
so is one in a currency the inventory account is not kept in.

## Writing off

```rust
pub enum Reason { Expired, Damaged }
pub async fn write_off(db: &TenantDb, product: &AggregateId, write_off: &WriteOff,
    metadata: &Metadata) -> Result<Committed<StockEvent>, CommandError<InventoryError>>;
```

Somebody standing in front of the goods says they are gone and why. It takes a
quantity through the picking rule, a named lot, or named units, and posts
`Dr waste`, `Cr inventory` at what those portions were carried at. **More than is
there is refused** (`inventory.not_enough_stock`): a person holding the goods who
asks to throw away more than exists is wrong about the goods. The reason stays on
the movement; both reasons post to one account.

## Counting

```rust
pub struct Count {
    pub lot: Option<String>,   // None counts the shelf
    pub declared: i64,
    pub serials: Vec<String>,  // serial-tracked: the units found
    pub reference: String,
    pub at: Timestamp,
}
```

**A count takes the shelf** (or, for someone counting batches, one lot). What
was expected, what was found, the variance and where it landed are frozen into
the event:

- **A shortage comes off the lots through `pick`**, the order a sale takes stock
  in, each portion at its own lot's cost.
- **An overage joins the lot that goes out last** — the far end of the same
  order — at that lot's own unit cost, so the lot does not quietly become an
  average of two prices. Found stock with no lot open to join is refused
  (`inventory.no_lot_to_join`): it comes in as a receipt, at what it cost.
- **A serial-tracked product is counted by naming the units found.** What was on
  hand and not named leaves as `missing`; a name that is not on hand is refused.
- **A count of the shelf clears what a plain product sold short owes**, at what
  the sale charged it out at.

Short posts `Dr variance`, `Cr inventory`; over posts the reverse; a count that
moved no value posts nothing. Counting needs `PostEntries`, because it posts.

## What an invoice takes off the shelf

`sales::issue_in` calls this for every invoice line that names a product, **in
the invoice's own transaction** — so a till sale, a booking bill and a
`/v1/sales` invoice all deplete through one path, and cost of goods sold is booked
lot by lot as the goods leave.

```rust
pub struct Consumption {
    pub quantity: Option<i64>,
    pub lot: Option<String>,     // the line's own override of the picking rule
    pub serials: Vec<String>,
    pub reference: String,       // "{invoice}.{line}", derived by the caller
    pub at: Timestamp,
}

pub async fn consume_in(conn: &mut PgConnection, product: &AggregateId, taking: &Consumption,
    metadata: &Metadata) -> Result<Committed<StockEvent>, ExecuteError<InventoryError>>;
pub fn cost_entry_of(shelf: &AggregateId, reference: &str) -> String;
```

- **A line may name a lot** (`lot` on the sales and till line): the units come
  off that lot, at its cost, and a lot that is not open at the branch the sale
  is at, or holds fewer than the line takes, refuses the invoice — for a plain
  product too, whose shortfall below is for a line that names no lot. That a
  plain product's named lot refuses is the product owner's decision: the line
  asked for that lot, and it is the one way a plain sale is refused for stock. A
  lot named with no product is refused by `sales` (`sales.lot_without_a_product`).
- **A serial-tracked line names its units**, and a name not on the shelf refuses.
- **What the shelf cannot cover splits by tracking.** A lot- or serial-tracked
  product refuses (`inventory.not_enough_stock`) and takes the invoice with it: that
  stock is meant to be known exactly. A plain product sells anyway and records a
  **shortfall** at the shelf's last known unit cost — the negative number a count
  corrects. A later delivery does not pay the debt; a count does.
- **Products are taken in sorted order**, so two tills selling the same two
  products cannot deadlock; and the lines come off the `Issued` event the
  transaction wrote, so a retried invoice takes nothing.

It posts `Dr cost of goods sold`, `Cr inventory`.

## What a credit note puts back

```rust
pub struct Restoration {
    pub taken_on: String,          // the consumption being undone
    pub branch: Option<String>,    // the branch the invoice was issued at
    pub quantity: Option<i64>,     // None: everything still out
    pub serials: Vec<String>,      // which units, when they have names
    pub reference: String,         // "r.{credit note number}.{line}"
    pub at: Timestamp,
}

pub async fn restore_in(conn: &mut PgConnection, product: &AggregateId, back: &Restoration,
    metadata: &Metadata) -> Result<Committed<StockEvent>, ExecuteError<InventoryError>>;
```

**What comes back is what the client says came back** — the quantity on the
credit line — never a share of the amount credited, because the money and the
goods are two statements and the division does not always land. A whole
cancellation puts back everything the invoice sold.

**It is decided from the sale itself, followed through the shelf's whole
stream** (`Returning`), never from the read model and never from the shelf's
bounded window of recent movements — a sale stays returnable however busy its
shelf has been since. The goods go back **onto the lots they left, at what the
sale froze**; a lot the sale emptied reopens as itself, with its batch code and
date. A second credit note can only return what the first left.

**Named units come back by name.** A credit line for serial-tracked stock names
the units that came back (`serials`, as many as its `quantity`), and each has to
be one that sale took and that has not come back already
(`inventory.not_out`). That is the sale's own record and not the shelf's: a unit
sold again since is off the shelf too, and a shelf check would let it come back
twice. A return of named units by quantity alone only works for the whole of
what is still out (`inventory.named_units_come_back_whole`).

What a plain product's sale could not cover settles the shelf's debt while the
shelf still owes it; once a count has cleared that debt, those units come back as
stock on a lot of their own (`returned_lot_of`, `back.{shelf}.{taken_on}`). It
posts `Dr inventory`, `Cr cost of goods sold`.

## The expiry warning

```rust
pub struct ExpiryWindow { pub days: i32 }
impl ExpiryWindow {
    pub const KEY: &'static str = "inventory.expiry_window";
    pub const DEFAULT: Self = Self { days: 30 };
    pub const fn new(days: i32) -> Result<Self, NotAWindow>;   // 0 to 3650
    pub fn warns_until(self, today: NaiveDate) -> Option<NaiveDate>;  // today + days
}
```

**Warn early, write off by hand — on the bell.** A job in `erp-worker`'s
composition root reads the tenant's window, calendar and open lots, and raises a
notification for **the people who may write stock off**: every member the
write-off route would let through at the lot's branch — the role that applies in
`inventory` (owner, accountant or clerk), narrowed by the tenant's permission
limits. Two kinds, each **once per lot**:

| Kind | When | What it says (English; Arabic too) |
|---|---|---|
| `stock_expiring` | the lot reaches its date within the window | *{product}, batch {code} at {branch}, is good until {date}.* |
| `stock_expired` | the date has passed and the lot is still on the shelf | *…was good until {date} and is still on the shelf. Write off what cannot be sold or sent back.* |

A lot at no branch — a business with one shelf — is told the same sentence
without *at {branch}*. A lot dated today is still good today. A lot with no date
is never announced, and neither is an emptied one — sold, counted or written off
to nothing — nor one that empties after it was told. The notification's id is derived from the kind
and the lot, so a second run, or a wider window reaching a lot already told,
raises nothing. `inventory` does not ring the bell itself: no module may, because
`messaging` reads `inventory` to say what a lot is.

**The bell and nothing else.** The people told are logins, and a login has no
email address or phone number, so `PUT /v1/notifications/preferences` refuses
any channel but `in_system` for these two kinds (`notifications.in_system_only`).

**Operators see whether the bell rang, not the stock.** The worker's
`stock_bell` health check reports a lot that has been due its notification, with
none raised, for longer than the longest a quiet tenant waits for a worker visit
plus one health interval — the bell not working, not the stock — and names the
lots by id and nothing else. A tenant without `notifications` has no bell: it is
told nothing about stock going off, and the check says nothing about it. That is
decided, not missed: such a tenant still sees every expiring and expired lot in
`GET /v1/inventory/summary`, which counts them against the same window.

**It posts nothing, writes nothing off and moves nothing.** A date is not a
smell — a batch may go back to its supplier or be sold at a markdown until its
last good day — so what leaves the shelf leaves through a write-off a person
enters with a reason. `GET /v1/inventory/lots?expiring_before=` answers the same
question on demand, for any day.

## Posting

| When | Entry |
|---|---|
| Received | `Dr` inventory, `Cr` goods received not invoiced |
| Consumed on an invoice | `Dr` cost of goods sold, `Cr` inventory — lot by lot, plus any shortfall |
| Restored by a credit note | `Dr` inventory, `Cr` cost of goods sold, at what the sale froze |
| Written off | `Dr` waste, `Cr` inventory |
| Counted short | `Dr` variance, `Cr` inventory |
| Counted over | `Dr` inventory, `Cr` variance |

```rust
pub struct PostingAccounts {
    pub inventory: AggregateId,       // 1300
    pub goods_received: AggregateId,  // 2010
    pub cogs: AggregateId,            // 5010
    pub variance: AggregateId,        // 5900
    pub waste: AggregateId,           // 5900
}
```

The codes ship in all three charts. The accounts are resolved inside the
movement's transaction and the configuration generation they came from is
stamped on the event's metadata (L5). `GET /v1/inventory/posting-accounts` returns them with an `ETag`;
`PUT` checks every code against the tenant's own chart before storing it.

**The value on the shelves is checked against the account.** The worker's
`stock_value` check compares what `proj_inventory` carries every lot at with what
`proj_ledger` says the inventory account holds, from both sides — an account
balance with no shelf behind it is a finding too.

## Read models

```rust
pub struct Inventory;   // group "inventory", schema proj_inventory

pub async fn products(conn, …) -> Result<Page<ProductRow>, sqlx::Error>;
pub async fn stock(conn: &mut PgConnection, product: Option<&str>, branch: Option<&str>,
    limit: i64, after: Option<&Cursor>) -> Result<Page<StockRow>, sqlx::Error>;
pub async fn lots(conn: &mut PgConnection, product: Option<&str>, branch: Option<&str>,
    expiring_before: Option<NaiveDate>, limit: i64, after: Option<&Cursor>)
    -> Result<Page<LotRow>, sqlx::Error>;
pub async fn summary(conn: &mut PgConnection, branch: Option<&str>, today: NaiveDate,
    expiring_through: Option<NaiveDate>) -> Result<Vec<SummaryRow>, sqlx::Error>;
pub async fn movements(conn, …) -> Result<Page<MovementRow>, sqlx::Error>;
pub async fn value_on_hand(conn: &mut PgConnection) -> Result<Vec<Money>, sqlx::Error>;
```

`lots` lists open lots **in the order stock will leave them**, so the screen a
manager reads and the rule the system obeys cannot drift apart. `stock` and
`lots` take a branch, and every row carries the product's name, joined from the
module's own `product` table when it is read — `null` rather than a refusal if
that table has no declaration for it. What a shelf
holds may go negative for a plain product sold short; the negative number is the
report. A serial's state is `on_hand`, `sold`, `written_off` or `missing` (not
found by a count). A movement's kind is `received` (a delivery, or a return),
`consumed`, `written_off` or `counted`, and a movement over several lots is a row
per lot.

**The summary adds them up**, a branch at a time: what the shelves are carried
at, one amount per currency; how many products have a shelf, whatever is on
it; how many open lots are going off — dated from today through
`ExpiryWindow::warns_until` — and how many have gone; and each shelf below zero
with what it **owes**, the debt its units no lot covered were charged out at
(a delivery since does not pay it, a count does). Nothing of it is stored: it is
summed from the rows above when asked, in one `REPEATABLE READ` snapshot. Today
is the tenant's day, on its calendar, worked out by the route; a branch with no
shelf is not in the answer, and a tenant with none gets an empty one.

**Commands never read these** (L3): on hand is rehydrated from the log, and a
product's existence is asked of the log too (`accepts_movements`), because a
product declared a moment ago is not in the read model yet.

## Routes

| Method | Path | Capability |
|---|---|---|
| `GET` | `/v1/inventory/products` | Read |
| `POST` | `/v1/inventory/products` | ManageTenant |
| `POST` | `/v1/inventory/stock/{product}/receipts` | PostEntries |
| `POST` | `/v1/inventory/stock/{product}/counts` | PostEntries |
| `POST` | `/v1/inventory/stock/{product}/write-offs` | PostEntries |
| `GET` | `/v1/inventory/stock` | Read |
| `GET` | `/v1/inventory/lots` | Read |
| `GET` | `/v1/inventory/summary` | Read |
| `GET` | `/v1/inventory/movements` | Read |
| `GET` `PUT` | `/v1/inventory/posting-accounts` | Read / ManageAccounts |
| `GET` `PUT` | `/v1/inventory/expiry-window` | Read / ManageTenant |

What depletes and restores stock has no route here: it is the `lines` of
`POST /v1/sales/invoices` and the till's sale, and the `lines` of a credit note
or a till return.

## What is deliberately not here

- **Moving stock between branches**, or merging two lots. A line depletes the
  shelf at the request's branch, which is right for a till and wrong for a
  warehouse shipping one invoice out of two places; that is a transfer, and it is
  its own command.
- **Found stock with no lot to join.** A count that finds more than its lots hold,
  with no lot open, is refused; the extra comes in as a receipt.
- **A compulsory expiry.** A lot-tracked delivery needs a batch code and may leave
  the date empty, because a batch with no shelf life is a real thing.
- **A serial unique across the tenant.** See *Serials*.
- **A segregated count.** Counting needs `PostEntries`; a separate
  `inventory:approve_count` claim for a tenant that wants the duties split is not
  built.
- **An account per product group**, and a write-off reason that chooses its own
  account. `1300`, `2010` and the loss accounts are one each.
- **Matching a bill line to the receipt it pays for.** The two meet in the
  balance of `2010`, and nowhere else.
- **Unit conversion, recipes, reorder points, reservations and valuation
  reports.** Each is additive over the events here.
