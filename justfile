# Task runner. See docs/DATABASE_SETUP.md for the database layout.

# `.env` is loaded here for the same reason `erp-testkit` loads it: a developer
# whose Postgres wants a password has it in `.env`, and neither cargo nor just
# reads that file by default. Without this, `just prepare` fails with
# `no password supplied` on a checkout where `cargo test` works fine.
set dotenv-load := true

# Both databases are derived from `DATABASE_URL` by swapping the database name,
# so there is one place to configure credentials. Override either directly if
# your server needs something the substitution cannot express.
base_url := env("DATABASE_URL", "postgres://postgres@localhost/postgres")
typecheck_url := env("TYPECHECK_DATABASE_URL", replace_regex(base_url, "/[^/]*$", "/erp_typecheck"))
admin_url := env("ADMIN_DATABASE_URL", replace_regex(base_url, "/[^/]*$", "/postgres"))

default:
    @just --list

# Everything CI runs, in the order that fails fastest.
check: fmt-check lint test

# `nextest` runs each test in its own process, which is both faster here and
# easier to read when one fails: the summary names the failures instead of
# burying them in the scrollback.
#
# `--no-fail-fast` because the default stops the whole run on the first failure,
# which `cargo test` never did: one broken guard would hide four hundred other
# results and turn a full picture into a bisect.
#
# **The second line is not redundant.** `nextest` does not run doctests at all,
# and this workspace has three — two of them compile-checks on `erp-testkit`'s
# public examples, which is exactly the kind of thing that rots unnoticed.
# Dropping them would be a silent loss of coverage, which is what L6 is about.
test:
    cargo nextest run --workspace --no-fail-fast
    cargo test --workspace --doc

# Redis for the test suite, on the port the tests actually default to.
#
# `crates/erp-control/tests/shared.rs` **refuses** rather than skipping when
# there is no Redis — that is law L6, and it is the right call: a suite that
# quietly covers three fewer things than it claims is worse than one that stops.
#
# The port matters. `compose.yaml` publishes Redis on 56379 so a full stack does
# not collide with a Redis someone already runs, but the tests default to
# `redis://127.0.0.1/`, which is 6379. `docker compose up -d redis` therefore
# leaves four tests failing, which is a confusing way to learn about a port
# number. This recipe is the one that matches.
redis:
    REDIS_PORT=6379 docker compose up -d redis

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Rebuild the type-check database and regenerate offline query data.
# Run after any migration change, and commit the `.sqlx/` diff.
prepare:
    psql "{{admin_url}}" -q -c "DROP DATABASE IF EXISTS erp_typecheck WITH (FORCE)"
    psql "{{admin_url}}" -q -c "CREATE DATABASE erp_typecheck"
    # Both schemas live in one type-check database, and sqlx validates every
    # query against a single connection.
    #
    # **Tenant first**, and it matters for exactly one table: `outbox` exists in
    # both planes and is deliberately the same table, so whichever chain runs
    # first is the definition sqlx checks against. The tenant one is the
    # original and the one with `CREATE TABLE` rather than
    # `CREATE TABLE IF NOT EXISTS`, so running it second would fail outright —
    # which is a loud way to be reminded of this, but a slow one.
    for f in migrations/tenant/*.sql migrations/control/*.sql; do psql "{{typecheck_url}}" -q -v ON_ERROR_STOP=1 -f "$f"; done
    # A module's install SQL is schema-relative — it says `invoice`, not
    # `proj_sales.invoice`, which is what lets a rebuild aim it at a staging
    # schema. So the schema has to be created and pointed at here too, the same
    # way `install_schema` does it.
    #
    # The schema is guessed from the crate directory, hyphens to underscores.
    # `a_modules_schema_is_named_after_its_crate` in `crates/erp-api/src/modules.rs`
    # is what stops that guess drifting from what the modules declare.
    #
    # `install.sql` only, deliberately: a module's `seed.sql` writes a tenant's
    # data, and this database has no tenant. That distinction is the whole
    # reason `ModuleSetup::seed_sql` is separate from `install_sql`.
    for f in modules/*/schema/install.sql; do \
      m=$(basename $(dirname $(dirname "$f")) | tr '-' '_'); \
      psql "{{typecheck_url}}" -q -v ON_ERROR_STOP=1 \
        -c "CREATE SCHEMA IF NOT EXISTS proj_$m" \
        -c "SET search_path TO proj_$m, public" -f "$f"; \
    done
    DATABASE_URL="{{typecheck_url}}" SQLX_OFFLINE=false cargo sqlx prepare --workspace -- --all-targets

# Build the demo tenant against the databases in `.env`.
#
# Every module enabled, filled through the public API. `just check` builds the
# same thing on a throwaway database and asserts it works; this one leaves it
# behind so a person can sign in.
demo password:
    CONTROL_DATABASE_URL="{{base_url}}" PRIMARY_CLUSTER_URL="{{base_url}}" \
      DEMO_PASSWORD="{{password}}" cargo run --quiet --bin demo

# Bring every tenant database up to the migrations this build expects.
#
# Two gates, and a deploy runs both before the pods go up:
#   `just migrate-fleet check`    is the fleet's *schema* where this build expects?
#   `just migrate-fleet versions` can this build *read* what is already in the logs?
# Both look without touching and exit non-zero when the answer is no.
migrate-fleet mode="" module="":
    CONTROL_DATABASE_URL="{{base_url}}" PRIMARY_CLUSTER_URL="{{base_url}}" \
      cargo run --quiet --bin migrator -- {{mode}} {{module}}

# Destroy demo tenants whose time is up. Schedule it; it exits when done.
reap:
    CONTROL_DATABASE_URL="{{base_url}}" PRIMARY_CLUSTER_URL="{{base_url}}" \
      cargo run --quiet --bin reaper

# Regenerate the error-code reference from the message catalog.
# `just check` fails when `docs/ERRORS.md` no longer matches.
errors:
    REGENERATE_DOCS=1 cargo test --quiet -p erp-api --test errors

# Regenerate the OpenAPI document from the router that serves the requests.
# `just check` fails when `docs/openapi.json` no longer matches.
openapi:
    REGENERATE_DOCS=1 cargo test --quiet -p erp-api --test openapi

# Drop every database this project creates. Does not touch anything else.
#
# Includes `erp_tenant_%`: a soak test that fails an assertion panics before its
# own cleanup runs, so those leak. Harmless, but they accumulate.
#
# The control plane is cleared of the tenants that went with them. Without that
# the rows outlive their databases, and the next `just demo` fails with
# `slug_taken` against a tenant whose database is gone — which is a confusing
# way to find out this recipe left the two halves disagreeing.
#
# And then the people who belonged to them, for the same reason one table over:
# a membership goes with its tenant, an identity does not, and the next
# `just demo` with a different password fails with `invalid_credentials` against
# an account whose company no longer exists. An identity nobody is a member of
# cannot sign in to anything, so there is nothing here to keep.
#
# `TRUNCATE` on the audit trail, and it has to be: `audit_entry` refuses UPDATE
# and DELETE by trigger, which makes the `ON DELETE SET NULL` on its actor
# columns unreachable — deleting an identity that has ever acted raises
# `audit_entry is append-only`. TRUNCATE fires no row trigger, which is the only
# way past it and is what this recipe means anyway.
clean-databases:
    #!/usr/bin/env bash
    set -euo pipefail
    # **Refuse while a test run is in progress.**
    #
    # `WITH (FORCE)` terminates whatever is connected, which is what makes this
    # recipe able to clear databases a panicking test leaked — and what makes it
    # able to pull a database out from under a *running* one. That failure is
    # ugly: the test does not error, it sees an empty table and fails an
    # assertion somewhere unrelated, which is an afternoon of looking in the
    # wrong place. It has already cost one.
    busy=$(psql "{{admin_url}}" -tAc "
        SELECT count(*) FROM pg_stat_activity
         WHERE datname LIKE 'erp_test_%' OR datname LIKE 'erp_tmpl_%'")
    if [ "${busy:-0}" -gt 0 ]; then
        echo "refusing: ${busy} connection(s) to test databases — a test run is in progress." >&2
        echo "wait for it to finish, or drop them by hand if you are sure." >&2
        exit 1
    fi
    psql "{{admin_url}}" -tAc "SELECT datname FROM pg_database WHERE datname LIKE 'erp_test_%' OR datname LIKE 'erp_tmpl_%' OR datname LIKE 'erp_tenant_%'" \
      | xargs -r -I{} psql "{{admin_url}}" -q -c 'DROP DATABASE IF EXISTS "{}" WITH (FORCE)'
    psql "{{base_url}}" -q -c "DELETE FROM tenant WHERE database_name NOT IN (SELECT datname FROM pg_database)" 2>/dev/null || true
    psql "{{base_url}}" -q -c "TRUNCATE audit_entry" -c "DELETE FROM identity WHERE id NOT IN (SELECT identity_id FROM membership)" 2>/dev/null || true

# Build the rustdoc API reference and open it.
#
# `--document-private-items` on purpose: half the reasoning in this codebase is
# in doc comments on things a caller cannot name, and a reference that hides
# them hides the part worth reading. The book's API chapters are the curated
# view; this is the exhaustive one.
docs:
    cargo doc --workspace --no-deps --document-private-items --open

# Serve the handbook, including the API reference chapters.
book:
    mdbook serve docs/book
