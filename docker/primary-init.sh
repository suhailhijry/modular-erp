#!/usr/bin/env bash
# Let the standby connect for replication.
#
# The Postgres image writes a `pg_hba.conf` covering ordinary connections and
# **not** replication ones, so `pg_basebackup` from another container fails with
#
#     FATAL: no pg_hba.conf entry for replication connection from host …
#
# which reads like a network problem and is an authorization one. There is no
# environment variable for it and it is not an `ALTER SYSTEM` setting, so this
# runs from `/docker-entrypoint-initdb.d` — after `initdb`, before the server is
# opened to the network.
#
# Scoped to the container network's private ranges rather than `all`, so a
# primary that ends up with a published port is not offering replication to
# whatever can reach it.
set -euo pipefail

{
    echo ""
    echo "# Added by docker/primary-init.sh — see the file for why."
    echo "host    replication    all    10.0.0.0/8       scram-sha-256"
    echo "host    replication    all    172.16.0.0/12    scram-sha-256"
    echo "host    replication    all    192.168.0.0/16   scram-sha-256"
} >> "$PGDATA/pg_hba.conf"

echo "replication entries added to pg_hba.conf"
