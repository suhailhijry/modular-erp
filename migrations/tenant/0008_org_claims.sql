-- Who may do what, and who inherits it.
--
-- ===========================================================================
-- Why this table exists at all
-- ===========================================================================
--
-- The org chart is an authorization structure, not a decoration. The rule is
-- one line:
--
--     claims(node) = own(node) ∪ ⋃ claims(child) for each child
--
-- A manager automatically holds everything their reports hold, so nobody has to
-- remember that giving a new clerk a permission also means giving it to their
-- supervisor. Granting *downward* instead is what produces the support ticket
-- "the branch manager cannot approve what her own cashier can".
--
-- That union is a subtree walk, and a command that has to know "may this person
-- approve this" cannot do a subtree walk of loaded aggregates while it is
-- deciding. So the effective set is **maintained** — recomputed in the same
-- transaction as the org event that changed it — and read as rows when needed.
--
-- ===========================================================================
-- Why it is here and not in a projection schema
-- ===========================================================================
--
-- The same argument as `0007_occupancy.sql`, and it is the reason §9c settled
-- the way it did. **A read model may be a second behind; an authorization
-- answer may not.** A claim revoked a moment ago has to bite now, and a command
-- reading `proj_hr` would happily approve something on the strength of a
-- checkpoint that had not caught up.
--
-- It is the same reason `sales` validates a customer against the event log
-- rather than `proj_crm`, one layer along and with more at stake.
--
-- `rebuild_schema` drops and rebuilds `proj_*`. It must never come near this.
--
-- ===========================================================================
-- Why a claim carries a branch
-- ===========================================================================
--
-- Because a regional manager over two branches accumulates authority in both,
-- and a branch manager must not accumulate authority in a branch they have
-- never seen. The union is over `(claim, branch)` pairs, so a claim that
-- travels up from Olaya arrives at the regional manager still saying Olaya.
--
-- `NULL` is company-wide and is not the same as "some branch": payroll and an
-- end-of-service calculation are company-wide by nature, and a claim that had
-- to name a branch could not express them.

-- What one person was granted directly.
--
-- The input to the union, and the only thing a grant writes. Everything in
-- `org_claim_effective` is derived from this table plus the tree.
CREATE TABLE org_claim_granted (
    -- The employee's aggregate id. `hr` owns the meaning; this file does not
    -- know what an employee is, the same way `occupancy_resource` does not know
    -- what a chair is.
    employee    TEXT NOT NULL CHECK (length(employee) BETWEEN 1 AND 128),

    -- What they may do, in the granting module's own vocabulary —
    -- `hr.approve_leave`, `sales.discount_beyond_ten`. Never parsed here.
    claim       TEXT NOT NULL CHECK (length(claim) BETWEEN 1 AND 128),

    -- Where. `NULL` is company-wide; see the header.
    branch      TEXT CHECK (branch IS NULL OR length(branch) BETWEEN 1 AND 128),

    -- **Whether it travels up the tree**, and the reason this column exists is
    -- an audit that would otherwise fail.
    --
    -- The control every accounting system is measured on is that the person who
    -- raises an invoice is not the person who approves its payment. Under a
    -- bottom-up union their shared manager holds both the moment the org chart
    -- says so — automatically, silently, and in a way no one would notice.
    --
    -- So a claim can be marked non-propagating, and the segregation-of-duties
    -- claims are. Without this the design cannot pass a Saudi statutory audit,
    -- and it is far cheaper here than in a customer's first year.
    propagates  BOOLEAN NOT NULL DEFAULT TRUE,

    granted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One grant per (person, claim, branch), in **two** indexes rather than a
-- primary key, and the reason is the whole point of `branch` being nullable: a
-- primary key cannot contain a nullable column, and company-wide is exactly
-- `branch IS NULL`. Declaring `(employee, claim, branch)` as the key would have
-- made every claim have to name a branch — which payroll and an end-of-service
-- calculation cannot.
--
-- Postgres also treats NULLs as distinct in a unique index, so the second index
-- is not redundant with the first: without it, two company-wide grants of the
-- same claim would both be admitted.
CREATE UNIQUE INDEX org_claim_granted_at_a_branch_is_unique
    ON org_claim_granted (employee, claim, branch)
    WHERE branch IS NOT NULL;

CREATE UNIQUE INDEX org_claim_granted_company_wide_is_unique
    ON org_claim_granted (employee, claim)
    WHERE branch IS NULL;

-- What one person effectively holds: their own, plus everything propagating
-- from everyone beneath them.
--
-- Maintained, never rebuilt from a replay. `source` is kept because the first
-- question anybody asks of an inherited permission is "where did this come
-- from" — and an effective set that cannot answer it is one nobody trusts.
CREATE TABLE org_claim_effective (
    employee    TEXT NOT NULL CHECK (length(employee) BETWEEN 1 AND 128),
    claim       TEXT NOT NULL CHECK (length(claim) BETWEEN 1 AND 128),
    branch      TEXT CHECK (branch IS NULL OR length(branch) BETWEEN 1 AND 128),

    -- Who it came from: themselves, or somebody in their subtree. **The reason
    -- the granting screen can name who else just gained something** — a grant
    -- at a leaf is not a local act, and an interface that showed only the
    -- person being granted is what would make this design dangerous rather
    -- than convenient.
    source      TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 128)
);

-- The same two-index shape, and for the same reason.
CREATE UNIQUE INDEX org_claim_effective_at_a_branch_is_unique
    ON org_claim_effective (employee, claim, branch, source)
    WHERE branch IS NOT NULL;

CREATE UNIQUE INDEX org_claim_effective_company_wide_is_unique
    ON org_claim_effective (employee, claim, source)
    WHERE branch IS NULL;

-- The question every check asks: does this person hold this claim, here?
CREATE INDEX org_claim_effective_by_claim_idx
    ON org_claim_effective (employee, claim);

-- The reporting line, as rows rather than as aggregates.
--
-- # Why the tree is here and not only in the event log
--
-- Because maintaining the union needs the ancestors of the node that changed,
-- and walking to the root by loading one aggregate per hop puts an unbounded
-- number of round trips inside a command. This is the same data the log holds,
-- in the shape the recomputation needs — derived, but write-side derived, for
-- the reason at the top of this file.
--
-- One parent per employee, so this is a tree and not a graph. A cycle is
-- refused at the command: not because it is untidy, but because the union
-- above would not terminate.
CREATE TABLE org_reporting_line (
    employee    TEXT PRIMARY KEY CHECK (length(employee) BETWEEN 1 AND 128),

    -- `NULL` for the root. A tenant has one, and it is whoever nobody reports
    -- to — which the tree gives for free rather than a flag anybody maintains.
    reports_to  TEXT CHECK (length(reports_to) BETWEEN 1 AND 128),

    -- Where this person works. **Not the branch on the request** — that is
    -- where a request happened, and these differ legitimately and often: an
    -- Olaya manager visiting Malaz records attendance for a Malaz shift. A
    -- report that read one where it meant the other would be wrong in a way
    -- nobody notices for a quarter.
    branch      TEXT CHECK (branch IS NULL OR length(branch) BETWEEN 1 AND 128)
);

-- Walking down from a node, which is what a recomputation does.
CREATE INDEX org_reporting_line_by_parent_idx
    ON org_reporting_line (reports_to) WHERE reports_to IS NOT NULL;
