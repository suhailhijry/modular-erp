CREATE TYPE accounting_side AS ENUM ('debit', 'credit');
CREATE TYPE account_type AS ENUM ('asset', 'liability', 'equity', 'revenue', 'expense');

CREATE TABLE IF NOT EXISTS ledger_accounts (
    account_code TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    account_type ACCOUNT_TYPE NOT NULL,
    normal ACCOUNTING_SIDE NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT true,
    parent_code TEXT DEFAULT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS general_ledger (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_code TEXT NOT NULL,
    journal_entry_id TEXT NOT NULL,
    line_index INT NOT NULL,
    global_position BIGINT NOT NULL,
    side ACCOUNTING_SIDE NOT NULL,
    amount BIGINT NOT NULL,
    signed BIGINT NOT NULL,
    currency TEXT NOT NULL,
    entry_date DATE NOT NULL,

    CONSTRAINT const_general_ledger_natural_key UNIQUE (account_code, journal_entry_id, line_index)
);

CREATE INDEX IF NOT EXISTS idx_general_ledger_account ON general_ledger (account_code, currency, id);
CREATE INDEX IF NOT EXISTS idx_general_ledger_balance_scan ON general_ledger (account_code, currency, global_position);
CREATE INDEX IF NOT EXISTS idx_general_ledger_by_date ON general_ledger (entry_date);

CREATE OR REPLACE VIEW general_ledger_with_balance AS
SELECT gl.*,
       SUM(gl.signed) OVER (
           PARTITION BY gl.account_code, gl.currency
           ORDER BY gl.global_position
       )::BIGINT AS running_balance
FROM general_ledger gl;

CREATE TABLE IF NOT EXISTS trial_balance (
    account_code TEXT NOT NULL,
    currency TEXT NOT NULL,
    debit_total BIGINT NOT NULL DEFAULT 0,
    credit_total BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (account_code, currency)
);
