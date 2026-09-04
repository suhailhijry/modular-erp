-- What was collected, and how it went.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_payments` during provisioning and into a staging schema
-- during `rebuild_swap`. See `modules/ledger/schema/install.sql` for the full
-- argument.
--
-- `IF NOT EXISTS` throughout, because everything here is derived from the event
-- log. A changed read model is answered by dropping the schema and replaying,
-- never by a migration.

CREATE TABLE IF NOT EXISTS payment (
    -- This system's own id for the attempt.
    id            TEXT PRIMARY KEY,

    provider      TEXT NOT NULL,
    -- **The gateway's own id.** What every callback names, and what a
    -- reconciliation against a payout report matches on.
    gateway_id    TEXT NOT NULL,
    invoice       TEXT NOT NULL,

    -- Minor units, and the currency beside them. Never a float: see
    -- `erp_payments::decimal` for what that costs.
    amount_minor  BIGINT NOT NULL,
    currency      TEXT NOT NULL CHECK (length(currency) = 3),

    stage         TEXT NOT NULL
                  CHECK (stage IN ('pending', 'settled', 'failed', 'refunded', 'voided')),

    -- What the gateway kept. Null until it says, which for most providers is
    -- not until the payout.
    fee_minor     BIGINT,
    -- What has gone back so far.
    refunded_minor BIGINT NOT NULL DEFAULT 0,

    -- In the gateway's words, when it refused. For a person to read.
    failed_why    TEXT,

    -- **Which payout paid this over**, when one has. Null is the ordinary
    -- state for a payment that settled today: the gateway batches, and the
    -- transfer is days away. What is null here is what the clearing account
    -- is still holding.
    paid_out_in   TEXT,

    started_at    TIMESTAMPTZ NOT NULL,
    settled_at    TIMESTAMPTZ,

    -- Where in the log this row is true as of.
    position      BIGINT NOT NULL
);

-- **A gateway id is how a callback finds its payment**, and it is the only
-- lookup on the hot path.
CREATE UNIQUE INDEX IF NOT EXISTS payment_by_gateway_id
    ON payment (provider, gateway_id);

-- "What is still owed on this invoice, and what has been tried."
CREATE INDEX IF NOT EXISTS payment_by_invoice ON payment (invoice, started_at DESC);

-- "What has not resolved", which is the list somebody actually chases.
CREATE INDEX IF NOT EXISTS payment_pending
    ON payment (started_at DESC) WHERE stage = 'pending';

-- "What has the gateway settled and not yet paid over" — the balance the
-- clearing account should agree with, and the list a payout reconciles against.
CREATE INDEX IF NOT EXISTS payment_awaiting_payout
    ON payment (provider, settled_at) WHERE stage = 'settled' AND paid_out_in IS NULL;

-- What a gateway actually sent, against what it owed.
CREATE TABLE IF NOT EXISTS payout (
    id             TEXT PRIMARY KEY,
    provider       TEXT NOT NULL,
    -- The gateway's own id for the transfer.
    reference      TEXT NOT NULL,

    -- What arrived. The number on the bank statement.
    amount_minor   BIGINT NOT NULL,
    -- What the covered payments say should have arrived. Equal to the amount
    -- when nothing was named, so the difference is zero and honest rather than
    -- invented.
    expected_minor BIGINT NOT NULL,
    currency       TEXT NOT NULL CHECK (length(currency) = 3),

    -- How many payments it reconciles against. **Zero means it reconciles
    -- nothing**, which is not the same as agreeing.
    covered        INTEGER NOT NULL CHECK (covered >= 0),

    into_account   TEXT NOT NULL,
    received_on    TIMESTAMPTZ NOT NULL,
    position       BIGINT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS payout_by_reference ON payout (provider, reference);

-- "Which payouts did not add up" — the worklist somebody actually works.
CREATE INDEX IF NOT EXISTS payout_disagreed
    ON payout (received_on DESC) WHERE amount_minor <> expected_minor;
