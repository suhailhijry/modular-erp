-- Payroll runs, and the payslips in them.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_payroll` during provisioning and into a staging schema
-- during `rebuild_swap`.

-- One month's pay.
CREATE TABLE IF NOT EXISTS run (
    id            TEXT PRIMARY KEY,

    -- `YYYY-MM`. A month and not a date range: payroll is monthly everywhere
    -- this system will run, and a range would invite two runs overlapping by a
    -- day.
    period        TEXT NOT NULL,

    -- All in minor units, all in one currency: a run is paid in one.
    gross         BIGINT NOT NULL,
    deductions    BIGINT NOT NULL,
    net           BIGINT NOT NULL,
    currency      TEXT NOT NULL,

    people        INTEGER NOT NULL DEFAULT 0,

    drafted_at    TIMESTAMPTZ NOT NULL,
    -- Set when it posted. **An approved run cannot be redrafted**: the entry is
    -- in the books and the payslips are what people were told.
    approved_at   TIMESTAMPTZ,
    entry         TEXT,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- "This year's runs", which is the whole screen.
CREATE INDEX IF NOT EXISTS run_by_period_idx ON run (period DESC, id DESC);

-- What one person was paid in one run.
--
-- **Frozen at drafting**, name included: a payslip says who it was for, and
-- somebody who marries next month does not get a new copy of last month's.
CREATE TABLE IF NOT EXISTS payslip (
    run           TEXT NOT NULL REFERENCES run (id) ON DELETE CASCADE,
    employee      TEXT NOT NULL,

    -- As it was when the run was made.
    name          TEXT NOT NULL,

    basic         BIGINT NOT NULL,
    gross         BIGINT NOT NULL,
    deductions    BIGINT NOT NULL,
    net           BIGINT NOT NULL,
    currency      TEXT NOT NULL,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    PRIMARY KEY (run, employee)
);

-- "Everything this person has been paid", which is the question an
-- end-of-service calculation and a loan application both ask.
CREATE INDEX IF NOT EXISTS payslip_by_employee_idx ON payslip (employee, run);
