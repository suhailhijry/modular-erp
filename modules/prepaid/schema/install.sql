-- What customers have already paid for.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_prepaid` during provisioning and into a staging schema
-- during `rebuild_swap`.

-- A package, a course, or a deposit.
CREATE TABLE IF NOT EXISTS entitlement (
    id            TEXT PRIMARY KEY,
    customer      TEXT NOT NULL,

    -- What it is for, in the business's own words. Never matched on by any rule
    -- in this module; it is what a customer reads on a statement.
    what          TEXT NOT NULL,

    -- Uses granted and remaining. Null on an entitlement that is only an
    -- amount, which is a deposit rather than a package.
    uses_granted  INTEGER,
    uses_left     INTEGER,

    -- **The liability**, in minor units. `outstanding` is what the ledger's
    -- deferred revenue account has to agree with; see `crate::outstanding`.
    deferred      BIGINT NOT NULL,
    outstanding   BIGINT NOT NULL,
    currency      TEXT NOT NULL,

    reason        TEXT NOT NULL CHECK (reason IN (
                      'bought', 'gifted_by_customer',
                      'granted_by_business', 'free_from_coupon')),

    -- The thing it is held against, when it is held against one. An opaque id:
    -- this module does not know what a booking is, so there is no foreign key
    -- and could not be one — it would point into another projection group.
    against       TEXT,

    expires_at    TIMESTAMPTZ,
    -- How it ended, if it has. Null while it can still be drawn down.
    closed        TEXT CHECK (closed IN ('spent', 'expired', 'revoked')),

    granted_on    TIMESTAMPTZ NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- What a customer holds, which is the screen this exists for.
CREATE INDEX IF NOT EXISTS entitlement_by_customer_idx
    ON entitlement (customer, granted_on DESC, id DESC);

-- What is still owed, which is the number that has to reconcile.
CREATE INDEX IF NOT EXISTS entitlement_live_idx
    ON entitlement (granted_on DESC, id DESC) WHERE closed IS NULL;

-- Sweeping for what has lapsed. Partial, because an entitlement with no expiry
-- is never a candidate and most of them have none.
CREATE INDEX IF NOT EXISTS entitlement_expiring_idx
    ON entitlement (expires_at) WHERE closed IS NULL AND expires_at IS NOT NULL;

-- A term paid for in advance.
CREATE TABLE IF NOT EXISTS subscription (
    id            TEXT PRIMARY KEY,
    customer      TEXT NOT NULL,
    plan          TEXT NOT NULL,

    -- The current term's price, what has been earned of it, and the difference.
    price         BIGINT NOT NULL,
    recognised    BIGINT NOT NULL,
    outstanding   BIGINT NOT NULL,
    currency      TEXT NOT NULL,

    starts_at     TIMESTAMPTZ NOT NULL,
    -- Exclusive, and it **moves**: resuming from a freeze pushes it out by
    -- exactly the time the clock was stopped for.
    ends_at       TIMESTAMPTZ NOT NULL,
    CONSTRAINT subscription_is_a_term CHECK (ends_at > starts_at),

    frozen_since  TIMESTAMPTZ,
    cancelled_at  TIMESTAMPTZ,
    cancelled_why TEXT,

    started_on    TIMESTAMPTZ NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS subscription_by_customer_idx
    ON subscription (customer, starts_at DESC, id DESC);

-- Who is due to be recognised, and who is due to lapse. Both walk this.
CREATE INDEX IF NOT EXISTS subscription_live_idx
    ON subscription (ends_at) WHERE cancelled_at IS NULL;

-- A loyalty card: points, stamps or visits.
--
-- One table for the three mechanics, because they differ in what produces the
-- count and in nothing after that. See `crate::loyalty`.
CREATE TABLE IF NOT EXISTS card (
    id            TEXT PRIMARY KEY,
    customer      TEXT NOT NULL,
    mechanic      TEXT NOT NULL CHECK (mechanic IN ('points', 'stamps', 'visits')),

    -- Counts redeemable now, and every count ever earned. `lifetime` never
    -- decreases — spending points does not cost a rank — and it is what a tier
    -- is read from.
    counts        INTEGER NOT NULL DEFAULT 0,
    lifetime      INTEGER NOT NULL DEFAULT 0,

    -- **The liability**, in minor units: what the sales that awarded these
    -- counts gave up to them under IFRS 15, less what has been honoured.
    -- Null currency until the first earning, which is what sets it.
    deferred      BIGINT NOT NULL DEFAULT 0,
    currency      TEXT,

    opened_on     TIMESTAMPTZ NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- What a customer holds, which is the screen this exists for.
CREATE INDEX IF NOT EXISTS card_by_customer_idx
    ON card (customer, opened_on DESC, id DESC);

-- Cards still owing something, which is the number that has to reconcile.
CREATE INDEX IF NOT EXISTS card_owing_idx
    ON card (opened_on DESC, id DESC) WHERE deferred > 0;
