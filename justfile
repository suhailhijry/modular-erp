# Task runner. See docs/DATABASE_SETUP.md for the database layout.

# `.env` is loaded here for the same reason `spa-testkit` loads it: a developer
# whose Postgres wants a password has it in `.env`, and neither cargo nor just
# reads that file by default. Without this, `just prepare` fails with
# `no password supplied` on a checkout where `cargo test` works fine.
set dotenv-load := true

# Both databases are derived from `DATABASE_URL` by swapping the database name,
# so there is one place to configure credentials. Override either directly if
# your server needs something the substitution cannot express.
base_url := env("DATABASE_URL", "postgres://postgres@localhost/postgres")
typecheck_url := env("TYPECHECK_DATABASE_URL", replace_regex(base_url, "/[^/]*$", "/spa_typecheck"))
admin_url := env("ADMIN_DATABASE_URL", replace_regex(base_url, "/[^/]*$", "/postgres"))

default:
    @just --list

# Everything CI runs, in the order that fails fastest.
check: fmt-check lint test

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Rebuild the type-check database and regenerate offline query data.
# Run after any migration change, and commit the `.sqlx/` diff.
prepare:
    psql "{{admin_url}}" -q -c "DROP DATABASE IF EXISTS spa_typecheck WITH (FORCE)"
    psql "{{admin_url}}" -q -c "CREATE DATABASE spa_typecheck"
    # Both schemas live in one type-check database. Table names do not collide,
    # and sqlx validates every query against a single connection.
    for f in migrations/control/*.sql migrations/tenant/*.sql; do psql "{{typecheck_url}}" -q -v ON_ERROR_STOP=1 -f "$f"; done
    # A module's install SQL is schema-relative — it says `invoice`, not
    # `proj_sales.invoice`, which is what lets a rebuild aim it at a staging
    # schema. So the schema has to be created and pointed at here too, the same
    # way `install_schema` does it.
    #
    # The schema is guessed from the crate directory, hyphens to underscores.
    # `a_modules_schema_is_named_after_its_crate` in `crates/spa-api/src/modules.rs`
    # is what stops that guess drifting from what the modules declare.
    for f in modules/*/schema/*.sql; do \
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
    REGENERATE_DOCS=1 cargo test --quiet -p spa-api --test errors

# Regenerate the OpenAPI document from the router that serves the requests.
# `just check` fails when `docs/openapi.json` no longer matches.
openapi:
    REGENERATE_DOCS=1 cargo test --quiet -p spa-api --test openapi

# Drop every database this project creates. Does not touch anything else.
#
# Includes `spa_tenant_%`: a soak test that fails an assertion panics before its
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
    psql "{{admin_url}}" -tAc "SELECT datname FROM pg_database WHERE datname LIKE 'spa_test_%' OR datname LIKE 'spa_tmpl_%' OR datname LIKE 'spa_tenant_%'" \
      | xargs -r -I{} psql "{{admin_url}}" -q -c 'DROP DATABASE IF EXISTS "{}" WITH (FORCE)'
    psql "{{base_url}}" -q -c "DELETE FROM tenant WHERE database_name NOT IN (SELECT datname FROM pg_database)" 2>/dev/null || true
    psql "{{base_url}}" -q -c "TRUNCATE audit_entry" -c "DELETE FROM identity WHERE id NOT IN (SELECT identity_id FROM membership)" 2>/dev/null || true
