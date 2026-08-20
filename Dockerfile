# syntax=docker/dockerfile:1
#
# One image, five binaries.
#
# `api`, `worker`, `migrator`, `reaper` and `demo` are the same build of the same
# workspace and differ only in which `main` runs. Five images would be five
# things to keep at the same version, and "the worker is one deploy behind the
# API" is a class of bug this system takes seriously enough to have a pre-deploy
# gate for it (`just migrate-fleet versions`). One image makes it impossible.
#
#     docker run … erp                 # the API, by default
#     docker run … erp worker
#     docker run … erp migrator check
#
# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
FROM rust:1.97.1-trixie AS build

WORKDIR /src
COPY . .

# **`SQLX_OFFLINE` is what makes this buildable with no database.** `.sqlx/` is
# committed for exactly this reason; without it every `query!` in the workspace
# would want a live Postgres at compile time, which a build stage does not have
# and should not need.
ENV SQLX_OFFLINE=true

# Cache mounts rather than the usual dummy-sources-then-real-sources dance.
# That trick works by making cargo believe the dependency layer is current, and
# it goes wrong quietly: a stale `liberp_control.rlib` built from an empty
# `lib.rs` links cleanly and contains none of the code. This caches the same
# thing without lying to cargo about anything.
#
# The binaries are copied out inside the same `RUN`, because a cache mount is not
# part of the image and `COPY --from` cannot see into one.
RUN --mount=type=cache,target=/src/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    set -eux; \
    cargo build --release --workspace \
      --bin api --bin worker --bin migrator --bin reaper --bin demo; \
    mkdir -p /out; \
    cp target/release/api target/release/worker target/release/migrator \
       target/release/reaper target/release/demo /out/

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------
FROM debian:trixie-slim AS runtime

# `ca-certificates` is not optional: this build talks TLS to ZATCA, to an SMTP
# relay, and possibly to Postgres and Redis. A slim image ships no root store,
# and the failure is a handshake error that reads like the remote is broken.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates libssl3 curl; \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --uid 10001 erp
USER erp
WORKDIR /home/erp

COPY --from=build /out/api      /usr/local/bin/
COPY --from=build /out/worker   /usr/local/bin/
COPY --from=build /out/migrator /usr/local/bin/
COPY --from=build /out/reaper   /usr/local/bin/
COPY --from=build /out/demo     /usr/local/bin/

ENV BIND=0.0.0.0:8080
EXPOSE 8080

# The health endpoint deliberately does not touch the database — a check that
# fails when a query is slow takes the fleet out during a slow query. Only
# meaningful for the `api` command; compose overrides it for the others.
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=6 \
    CMD curl -fsS "http://127.0.0.1:8080/v1/health" || exit 1

# **No `ENTRYPOINT`**, so `docker run erp worker` runs the worker as PID 1 and
# receives SIGTERM directly. That matters: the API's graceful shutdown and the
# worker's lease-releasing drain are both written against the signal arriving at
# the process, not at a shell that forwards it if it feels like it.
CMD ["api"]
