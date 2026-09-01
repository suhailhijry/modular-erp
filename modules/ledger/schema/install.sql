-- The ledger module's read models.
--
-- Applied when a tenant enables the module, not with the tenant schema — which
-- is why it lives here and not in `migrations/tenant`.
--
-- Everything below is derived from the event log and can be dropped and rebuilt
-- (architecture L2). Nothing here is a source of truth, which is why this is an
-- idempotent install script rather than a numbered migration chain: there is no
-- data to preserve across a change, only a checkpoint to reset.
--
-- ponytail: a module whose read models change today drops and rebuilds. When one
-- needs a real schema migration, it needs its own version table — sqlx's
-- migrator hard-codes `_sqlx_migrations`, and the tenant schema already owns
-- that one.

CREATE TABLE IF NOT EXISTS account (
    -- The account code, as the tenant chose it: "1000", "4100.02". The
    -- aggregate id, so it is also the natural key.
    code       TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL
               CHECK (kind IN ('asset', 'liability', 'equity', 'revenue', 'expense')),
    currency   CHAR(3) NOT NULL,
    closed     BOOLEAN NOT NULL DEFAULT false,
    opened_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS account_kind_idx ON account (kind) WHERE NOT closed;

-- One row per line of every entry. The ledger's detail, and what everything
-- else is summed from.
CREATE TABLE IF NOT EXISTS posting (
    -- Derived from the event's log position (`ProjectionCtx::derive_id`), so a
    -- rebuild reproduces it exactly.
    id          UUID PRIMARY KEY,
    entry_id    TEXT NOT NULL,
    line_index  INT  NOT NULL CHECK (line_index >= 0),

    account     TEXT NOT NULL,
    -- Signed: positive debits, negative credits. Statements split the two
    -- columns from the sign; storing them separately would allow a row that is
    -- somehow both.
    amount      BIGINT NOT NULL CHECK (amount <> 0),
    currency    CHAR(3) NOT NULL,

    memo        TEXT,

    -- **The reporting dimension.** Null on an entry posted without one, which
    -- is every entry a single-branch business writes and every entry written
    -- before branches existed. No foreign key: `proj_branches` is another
    -- projection group and L3 forbids joining to it — a report that wants a
    -- branch's *name* reads that group and these numbers separately.
    branch      TEXT,

    occurred_on TIMESTAMPTZ NOT NULL,
    -- The event's own timestamp, never `now()` — see architecture L2.
    recorded_at TIMESTAMPTZ NOT NULL,

    CONSTRAINT posting_line_is_unique UNIQUE (entry_id, line_index)
);

CREATE INDEX IF NOT EXISTS posting_by_account_idx ON posting (account, occurred_on);
CREATE INDEX IF NOT EXISTS posting_by_entry_idx ON posting (entry_id);

-- ---------------------------------------------------------------------------
-- Balances are views, not tables.
--
-- A maintained balance table is a second thing that can be wrong, and keeping it
-- in step is the projection code most likely to double-count. Summing is exact
-- and needs no code at all.
--
-- ponytail: a view scans every posting. That is fine to millions of rows and
-- wrong at hundreds of millions; the upgrade is a materialized balance
-- maintained by the projection, which is why `posting` carries everything it
-- would need.
-- ---------------------------------------------------------------------------

-- `sum(BIGINT)` is NUMERIC in Postgres, so every total is cast back. A sum of
-- minor units that will not fit in a BIGINT means the ledger has become
-- nonsense, and the cast raising is the correct outcome (L6).
CREATE OR REPLACE VIEW account_balance AS
SELECT a.code,
       a.name,
       a.kind,
       a.currency,
       a.closed,
       COALESCE(sum(p.amount), 0)::BIGINT AS balance,
       count(p.id)                        AS postings
  FROM account a
  LEFT JOIN posting p ON p.account = a.code
 GROUP BY a.code, a.name, a.kind, a.currency, a.closed;

-- What each branch holds, per account.
--
-- **A branch's own trial balance does not have to be zero**, and this view does
-- not claim it is. Debits equal credits per *currency*, which is the invariant
-- below; moving cash from one branch to another debits one and credits the
-- other, so each side is out by the transfer until inter-branch clearing
-- accounts exist. What this answers is "what did Olaya do", and the branches
-- sum to the whole.
CREATE OR REPLACE VIEW branch_balance AS
SELECT p.branch,
       p.account,
       a.name,
       a.kind,
       p.currency,
       sum(p.amount)::BIGINT AS balance,
       count(*)              AS postings
  FROM posting p
  LEFT JOIN account a ON a.code = p.account
 GROUP BY p.branch, p.account, a.name, a.kind, p.currency;

-- The invariant, as a query.
--
-- Every currency's postings must sum to zero. It holds only if commands,
-- events, projections and replays are all correct, so one number catches an
-- entire class of pipeline bug — which is why the platform health check runs it
-- per tenant (architecture §7).
CREATE OR REPLACE VIEW trial_balance AS
SELECT currency,
       sum(amount)::BIGINT                                          AS difference,
       COALESCE(sum(amount) FILTER (WHERE amount > 0), 0)::BIGINT   AS debits,
       COALESCE(-sum(amount) FILTER (WHERE amount < 0), 0)::BIGINT  AS credits,
       count(*)                                                     AS postings
  FROM posting
 GROUP BY currency;
