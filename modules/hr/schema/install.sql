-- The org chart, as a screen shows it.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_hr` during provisioning and into a staging schema during
-- `rebuild_swap`.
--
-- # What is deliberately not here
--
-- **The claims.** They live in `migrations/tenant/0008_org_claims.sql`, as
-- write-side state, because a command deciding "may this person approve this"
-- cannot read a table that may be a second behind. This schema is for the
-- screen that *displays* the chart; it is not what any check reads.

-- A person, and where they sit.
CREATE TABLE IF NOT EXISTS employee (
    id            TEXT PRIMARY KEY,

    name          TEXT NOT NULL,
    name_latin    TEXT,
    national_id   TEXT,
    email         TEXT,
    phone         TEXT,

    -- Who they report to. Null for the root, of which a tenant has one.
    reports_to    TEXT,

    -- **Where this person works**, which is not where a request happened. See
    -- the module docs; conflating them is the bug this phase invites.
    branch        TEXT,

    hired_on      TIMESTAMPTZ NOT NULL,
    -- Set when they leave. The record stays: they are on last year's payroll
    -- and whatever they approved.
    left_at       TIMESTAMPTZ,
    left_why      TEXT,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- The chart, drawn top-down. Partial on `reports_to` because the root has none
-- and a scan for it would be one row in a thousand.
CREATE INDEX IF NOT EXISTS employee_by_manager_idx ON employee (reports_to)
    WHERE reports_to IS NOT NULL;

-- "Who works at Olaya", which is the list a branch manager opens.
CREATE INDEX IF NOT EXISTS employee_by_branch_idx ON employee (branch, name)
    WHERE branch IS NOT NULL;

-- Who is currently on the books, which is most screens. Partial, because a
-- business accumulates leavers and reads about the people who are here.
CREATE INDEX IF NOT EXISTS employee_current_idx ON employee (name, id)
    WHERE left_at IS NULL;

-- What somebody holds, one row per kind.
--
-- **The current one**, because a renewal replaces: nothing here asks what an
-- expired document used to say, and the log keeps that history anyway.
--
-- This is the table the expiry screen and the health check read. It is **not**
-- what `may_work_on` reads — that is the aggregate, inside the command that
-- decides, for the same reason a claim check does not read a projection: a
-- document recorded a moment ago must already count.
CREATE TABLE IF NOT EXISTS employee_document (
    employee      TEXT NOT NULL REFERENCES employee (id) ON DELETE CASCADE,
    kind          TEXT NOT NULL
                  CHECK (kind IN ('identity', 'work_permit', 'medical', 'licence')),

    number        TEXT NOT NULL,
    -- **A date, not an instant.** An iqama expires on a day in Riyadh, not at
    -- an hour in UTC, and storing an instant would make the answer depend on
    -- which side of midnight somebody asked.
    expires_on    DATE NOT NULL,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    PRIMARY KEY (employee, kind)
);

-- "What expires in the next sixty days", which is the whole screen.
CREATE INDEX IF NOT EXISTS employee_document_by_expiry_idx
    ON employee_document (expires_on, employee);
