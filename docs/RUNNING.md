# Running it

Everything you need to bring the API and the workers up — in containers or by
hand — poke at them, and see what they did.

## The whole thing, in containers

```bash
docker compose up --build -d
```

One image, five binaries: `api`, `worker`, `migrator`, `reaper` and `demo` are
the same build and differ only in which `main` runs. Five images would be five
things to keep at one version, and "the worker is a deploy behind the API" is a
failure this system has a pre-deploy gate for.

What comes up is deliberately **not** one of everything:

| | | why more than one |
|---|---|---|
| `api` | ×2 | one process never disagrees with itself, which is what `REDIS_URL` is for |
| `worker` | ×2 | tenant leases are load-bearing or they are decorative |
| `pg-primary` + `pg-standby` | streaming | `TenantDb::read` has routed to a replica since Phase 1 with nothing attached |
| `redis` | ×1 | the shared session cache and the invalidation broadcast |
| `mailpit` | ×1 | catches every email; read them at <http://localhost:8025> |

`migrate` runs first and everything else waits for it to exit 0, so `up` is one
command. Then:

```bash
docker compose run --rm demo                       # a tenant to sign into
curl -H 'Host: demo.localhost' http://localhost:8080/v1/tenant
docker compose logs -f api worker
docker compose down -v                             # and the volumes with it
```

**This is a model, not a deployment.** The passwords are `postgres`, nothing is
encrypted in transit, and there is no backup.

### Checking the replica is really streaming

```bash
docker compose exec pg-primary psql -U postgres -tAc \
  "SELECT application_name, state, sync_state FROM pg_stat_replication"
docker compose exec pg-standby psql -U postgres -tAc "SELECT pg_is_in_recovery()"
```

The first should name a walreceiver in `streaming`; the second should be `t`.
Reads that tolerate lag go there — a VAT return, a list of invoices. Anything
that must see its own write uses the primary, which is what
`?consistent_after=` is for.

## Before anything (running it by hand)

```bash
just prepare
```

Rebuilds the type-check database and the offline query cache. Run it after any
migration or module-schema change, or `cargo` fails with
`SQLX_OFFLINE=true but there is no cached data for this query`.

Databases and credentials come from `.env` — see [DATABASE_SETUP.md](DATABASE_SETUP.md).

```bash
just redis
```

**The test suite needs a Redis.** `crates/erp-control/tests/shared.rs` refuses
rather than skipping when there is none — that is law L6, and it is deliberate: a
suite that quietly covers three fewer things than it claims is worse than one
that stops and says so.

Note the port. `compose.yaml` publishes Redis on **56379**, so a full stack does
not collide with a Redis already running on the machine, but the tests default to
`redis://127.0.0.1/` — which is **6379**. So `docker compose up -d redis` leaves
four tests failing, and `just redis` is the one that matches. Either that or set
`REDIS_URL` to whatever you are running.

## The three processes

| | what it does | needs |
|---|---|---|
| `bin/api` | serves HTTP | control DB, cluster URL |
| `bin/worker` | projections, both outboxes, health checks, the ZATCA sweeps | the same, plus `SEALING_KEY` for ZATCA and `SMTP_URL` for email |
| `bin/migrator` | brings tenant schemas up to this build; **run before a deploy** | the same |

`bin/reaper` destroys expired demo tenants. Schedule it; it exits when done.

## Environment

```bash
CONTROL_DATABASE_URL   # the control plane
PRIMARY_CLUSTER_URL    # the tenant cluster named `primary` in the control plane
PUBLIC_DOMAIN          # tenants are subdomains of this; defaults to `localhost`
BIND                   # the API's address; defaults to 0.0.0.0:8080
SEALING_KEY            # <id>:<64 hex chars> — see below
PRIMARY_REPLICA_URL    # reads that tolerate lag; blank or unset means no replica
PRIMARY_DIRECT_URL     # the route that bypasses a connection pooler — see below
REDIS_URL              # the shared session cache and invalidation — see below
SMTP_URL               # the relay; without it nothing sends mail — see below
SMTP_FROM              # e.g. "ERP <noreply@erp.com>"; required when SMTP_URL is set
S3_BUCKET              # where files go; without it, and without FILE_ROOT,
                       #   the file routes refuse — see below
FILE_ROOT              # a directory, for a tenant who keeps their own documents
TAQNYAT_TOKEN          # SMS; needs TAQNYAT_SENDER too — see below
TAQNYAT_SENDER         # the sender name registered on the Taqnyat account
FCM_SERVICE_ACCOUNT_FILE   # push; the Firebase service account JSON — see below
SMS_RELAY_URL          # any channel with no gateway can use a relay instead
PUSH_RELAY_URL         #   `<CHANNEL>_RELAY_URL` + `<CHANNEL>_RELAY_TOKEN`
WHATSAPP_RELAY_URL     #   CHANNEL is SMS, PUSH or WHATSAPP
WORKER_NAME            # for logs; defaults to $HOSTNAME
RUST_LOG               # e.g. info,erp_worker=debug
```

**`PRIMARY_DIRECT_URL` is only needed once there is a pooler.** Leave it unset
or blank and it *is* the primary, which is right for every deployment that talks
to Postgres directly.

It exists because a transaction pooler — Supavisor, PgBouncer — hands out a
different backend for each transaction. `CREATE DATABASE` cannot run inside a
transaction, and installing a module's schema is a sequence whose steps share a
`search_path`; neither survives that. So provisioning, fleet migration and
schema rebuilds ask for this route and everything else goes through the pooler:

```bash
PRIMARY_CLUSTER_URL="postgres://user:pass@pooler:6543/postgres"   # request traffic
PRIMARY_DIRECT_URL="postgres://user:pass@primary:5432/postgres"   # DDL only
```

Set `POOL_STATEMENT_CACHE=0` alongside it unless the pooler is configured to
handle prepared statements — sqlx prepares by default, and a cached handle
refers to a statement the next backend never parsed.

`crates/erp-control/tests/pooler.rs` is what keeps this true: it fails the build
if a session-scoped `SET`, a session advisory lock, or a `LISTEN` appears
anywhere outside the DDL paths.

**`REDIS_URL` is what makes more than one API process correct.**

Without it everything still works and two things get worse, both of which this
system documented before Redis existed:

- Every authenticated request reads its session from the control database. That
  is the busiest lookup in the system and the one that was deliberately never
  cached, because an *in-process* cache would make a logout take effect on the
  node that served it and nowhere else. Shared, it can be cached, and a logout
  deletes it for everybody at once.
- A role change invalidates the cache on the node that made it. The others wait
  out their five-second TTL. With one API process there are no others.

So: one process, skip it. More than one, set it.

```bash
REDIS_URL="redis://localhost:6379/"
REDIS_URL="rediss://:password@redis.internal:6379/"   # TLS, via the OpenSSL already linked
```

Redis being unreachable degrades to exactly the behaviour above and says so in
the log. The one exception is stated in `erp_control::shared`: a logout that
cannot reach Redis leaves that token usable until the cached entry expires,
which is why `SESSION_TTL` is one minute and not an hour.

**`SMTP_URL` is what makes invitations arrive.** Without it the worker registers
no email handler, and an effect whose kind has no handler is **not claimed** — so
an invitation email waits in the control plane's outbox as an undelivered promise
rather than being attempted and given up on. Configure a relay later and
everything already promised goes out.

lettre's URL form, and **`tls=required` is not optional**: without it `smtp://`
will continue in the clear when a relay does not offer STARTTLS, and an
invitation link is a credential.

```bash
SMTP_URL="smtps://user:pass@smtp.example.com:465"                 # implicit TLS
SMTP_URL="smtp://user:pass@smtp.example.com:587?tls=required"     # STARTTLS
SMTP_FROM="ERP <noreply@erp.com>"
```

Any relay that speaks SMTP works — a provider, or a Postfix of your own. There is
one sender for the whole platform; a per-tenant `From` needs domain verification
first, or mail claiming a tenant's domain fails SPF at most receivers.

To watch it locally without sending anything, point it at a catcher:

```bash
docker run --rm -p 1025:1025 -p 8025:8025 axllent/mailpit
```

then `SMTP_URL="smtp://localhost:1025"` and read the mail at
`http://localhost:8025`. **No `?tls=` at all** is how lettre spells plain SMTP —
`tls=none` is not a value it knows and is refused at start-up.

**`S3_BUCKET` is what makes file uploads land anywhere.** Without it, and
without `FILE_ROOT`, the API refuses every upload (503, `files.no_storage`)
rather than dropping the bytes. That is deliberate: a customer told their
contract uploaded when it went nowhere is worse served than one told it did not.

The engine is S3-compatible, not Amazon-only — the endpoint is configuration:

```bash
S3_BUCKET="documents"
S3_REGION="fsn1"                                    # Contabo's is `default`
S3_ENDPOINT="https://fsn1.your-objectstorage.com"   # omit for Amazon itself
S3_ACCESS_KEY_ID="…"
S3_SECRET_ACCESS_KEY="…"
S3_VIRTUAL_HOSTED_STYLE="false"   # `bucket.host`; Contabo cannot, AWS prefers it
S3_ALLOW_HTTP="false"             # development only, for a local MinIO
```

Two providers this was written against:

| | `S3_REGION` | `S3_ENDPOINT` | addressing |
|---|---|---|---|
| Hetzner | `fsn1`, `nbg1` or `hel1` | `https://<region>.your-objectstorage.com` | either |
| Contabo | `default` | `https://<region>.contabostorage.com` | path only |

Path-style is the default because it is the one both accept. Set
`S3_VIRTUAL_HOSTED_STYLE=true` for Amazon.

**`FILE_ROOT` is the other half of D15**: a business that keeps its own records
on its own hardware sets a directory instead, and no bucket is involved. It is
read only when `S3_BUCKET` is unset. One caveat that is a deployment fact rather
than a bug: two API processes with two local roots each hold half a tenant's
files and neither knows. Use one filesystem, or use a bucket.

There is a real one to develop against:

```bash
docker compose up -d minio createbucket    # http://localhost:9001
```

**The message gateways are per channel, and each is optional.** A channel with
no transport leaves its messages **in the outbox** rather than dead-lettering
them, which is what makes a staggered rollout safe. A named gateway wins over
the relay for its channel; configuring both means the gateway.

```bash
# SMS through Taqnyat. The sender name is case sensitive and must be one that is
# active on the account — a wrong one is a permanent refusal on every message.
TAQNYAT_TOKEN="…"
TAQNYAT_SENDER="Bassat"

# Push through Firebase. The service account JSON the Firebase console hands
# out. Prefer the file: the key is a multi-line PEM, and an environment
# variable holding one is a variable somebody eventually pastes into a chat.
FCM_SERVICE_ACCOUNT_FILE="/run/secrets/firebase.json"
FCM_SERVICE_ACCOUNT='{"type":"service_account", …}'   # or the JSON itself
```

**There is no WhatsApp gateway**, deliberately. Meta accepts only pre-approved
templates outside a 24-hour window the customer opens, and this system renders a
finished message. `WHATSAPP_RELAY_URL` still works if you have a service that
handles the template mapping.

**`<CHANNEL>_RELAY_URL` is the escape hatch** for a provider with no adapter.
One `POST` per message with a bearer token; `410 Gone` means the address is
dead, any other `4xx` is permanent, `5xx` is retried. The contract is documented
on `messaging::Relay`.

**`SEALING_KEY` is what module secrets are sealed under.** Without it the API
refuses to store a tenant's ZATCA private key (503, `request.no_sealing_key`)
and the worker does not register the ZATCA sweeps at all — invoices are built
and chained but never signed or sent. Generate one:

```bash
echo "$(date +%Y-%m):$(openssl rand -hex 32)"
```

The identifier before the colon is stored beside every row it seals, so a
rotation can find what it has not re-sealed yet. **Both processes must have the
same key**: the API writes the secrets and the worker reads them.

## Seed something to look at

```bash
just demo correct-horse-battery-staple
```

Builds a tenant with every module on, filled through the public API: six
invoices (one credited, one discounted), four bills, three payments, a filed VAT
return, and a colleague with narrower permissions. It prints the slug, the
sign-in address and the tenant id.

## Start the API

```bash
CONTROL_DATABASE_URL=postgresql://postgres:postgres@localhost/erp_backend \
PRIMARY_CLUSTER_URL=postgresql://postgres:postgres@localhost/erp_backend \
PUBLIC_DOMAIN=erp.test \
SEALING_KEY="2026-08:$(openssl rand -hex 32)" \
cargo run --bin api
```

## Start a worker

Same variables, second terminal:

```bash
CONTROL_DATABASE_URL=postgresql://postgres:postgres@localhost/erp_backend \
PRIMARY_CLUSTER_URL=postgresql://postgres:postgres@localhost/erp_backend \
SEALING_KEY="…the same key as the API…" \
RUST_LOG=info \
cargo run --bin worker
```

It visits every tenant in turn: runs each module's projections, dispatches the
outbox, signs ZATCA documents, submits them, and every five minutes checks the
invariants (the trial balance, overpaid invoices and bills, certificate expiry).
Ctrl-C drains — it finishes what it is holding and exits 0. An exit code of 1
means the drain timed out.

Run as many as you like; tenant leases keep two workers off the same tenant.

## Talking to it

**The tenant is the subdomain.** `demo.erp.test` is one company; there is no
tenant in any path. `*.localhost` resolves to loopback in every browser and in
curl with no `/etc/hosts` editing, so `PUBLIC_DOMAIN=localhost` and a `Host:
demo.localhost` header is the least-setup option.

```bash
API=http://127.0.0.1:8080
H="Host: demo.erp.test"

TOKEN=$(curl -s -X POST $API/v1/sessions -H "$H" -H 'content-type: application/json' \
  -d '{"handle":"owner@demo.example","password":"correct-horse-battery-staple"}' \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')
A="Authorization: Bearer $TOKEN"

curl -s $API/v1/tenant            -H "$H" -H "$A"   # who this is
curl -s $API/v1/sales/invoices    -H "$H" -H "$A"   # a page of invoices
curl -s $API/v1/ledger/accounts   -H "$H" -H "$A"
curl -s "$API/v1/tax_sa/vat-return?from=2026-01-01T00:00:00Z&until=2026-04-01T00:00:00Z&currency=SAR" \
     -H "$H" -H "$A"
```

Useful headers and parameters:

- `Accept-Language: ar` — every error and message comes back in Arabic.
- `?after=<next>&limit=50` — lists are paged. `next` absent means the list
  ended; pass it back as `after` to continue. See below.
- `?consistent_after=<position>` — waits for the read model to catch up with a
  write, so a list reflects what you just posted without a sleep.

### Paging

```bash
curl -s "$API/v1/sales/invoices?limit=2" -H "$H" -H "$A"
# {"items":[…],"next":"323032362d…"}
curl -s "$API/v1/sales/invoices?limit=2&after=323032362d…" -H "$H" -H "$A"
```

The cursor is opaque — pass back what you were given. A cursor this build cannot
read is refused (`request.invalid_cursor`) rather than silently starting over.

### The whole API

`docs/openapi.json` is generated from the router that serves the requests, so it
cannot drift. The API serves it too:

```bash
curl -s $API/openapi.json | python3 -m json.tool | head -40
```

`docs/ERRORS.md` lists every `code` the API can answer with. **Branch on the
code, never on `detail`** — the detail is prose in whichever language was asked
for.

## ZATCA, end to end

Onboarding needs a six-digit OTP the taxpayer generates in the Fatoora portal.
The whole flow, from OTP to a tenant that can clear invoices:

```bash
# 1. Who the business is. Every document is stamped with this.
curl -s -X PUT $API/v1/tax_sa/registration -H "$H" -H "$A" -H 'content-type: application/json' -d '{
  "vat_number":"310122393500003","name":"روابي للاستشارات","scheme":"crn",
  "identifier":"1010101010",
  "address":{"street":"طريق الملك فهد","building":"2322","district":"العليا",
             "city":"الرياض","postal_code":"12211","country":"SA"}}'

# 2. Key pair, CSR, OTP, compliance checks, production certificate — four calls
#    to ZATCA, one request.
curl -s -X POST $API/v1/tax_sa/zatca/onboarding/activate -H "$H" -H "$A" -H 'content-type: application/json' -d '{
  "environment":"simulation","otp":"123456","branch":"الفرع الرئيسي",
  "common_name":"EGS1-886431145","serial":"DEV001","industry":"Consulting"}'

# 3. Where it stands.
curl -s $API/v1/tax_sa/zatca/onboarding -H "$H" -H "$A"
curl -s $API/v1/tax_sa/zatca           -H "$H" -H "$A"
curl -s $API/v1/tax_sa/zatca/documents -H "$H" -H "$A"
```

If the automated path fails at any step, the manual one still works: `POST
/v1/tax_sa/zatca/onboarding` returns a CSR to submit by hand, and `PUT
…/onboarding/certificate` takes what ZATCA returns.

With a worker running and a production certificate stored, invoices are signed
and submitted within a visit or two. Watch it:

```bash
curl -s $API/v1/tax_sa/zatca -H "$H" -H "$A"
# {"registered":true,"unsigned":0,"overdue":0,"awaiting_clearance":2,"chain_length":7,…}
```

`unsigned` above zero with a worker running means no production certificate.
`awaiting_clearance` is standard invoices ZATCA has not stamped — **not late**,
but documents the buyer must not have yet. `overdue` is simplified invoices past
their twenty-four hours, which is the number an inspection asks about.

### Against ZATCA with real credentials

```bash
ZATCA_CREDENTIALS=/path/to/credentials \
  cargo test -p tax_sa --test sandbox -- --ignored --nocapture
```

The directory holds `key.pem`, `cert.pem` and `csid.json`. Nine documents go to
ZATCA's sandbox and it says what it thinks of each.

## Before a deploy

```bash
just migrate-fleet check      # is every tenant's schema where this build expects?
just migrate-fleet versions   # can this build read what is already in the logs?
```

Both look without touching and exit non-zero when the answer is no. Run them
**before** the new pods go up; that is the whole point of them.

```bash
just migrate-fleet                    # apply outstanding migrations
just migrate-fleet refresh sales      # rebuild one module's read models
```

A rebuild replays into a staging schema, catches up under the checkpoint lock,
and swaps — the old read models keep serving until the new ones are complete.

## When something looks wrong

```bash
RUST_LOG=debug cargo run --bin worker            # what it visits and why it skips
psql "$CONTROL_DATABASE_URL" -c "SELECT * FROM audit_entry ORDER BY at DESC LIMIT 20"
psql "$CONTROL_DATABASE_URL" -c "SELECT slug, database_name, status FROM tenant"
```

Inside a tenant's database:

```sql
SELECT group_name, position FROM projection_checkpoint;   -- how far each group is
SELECT count(*) FROM event;                               -- how far the log is
SELECT kind, attempts, dead_at FROM outbox WHERE delivered_at IS NULL;
SELECT id, status, icv, signed_at FROM proj_tax_sa.zatca_document ORDER BY icv;
```

A projection stopped behind the log is a group that hit something it could not
apply — the worker logs it and stops that group rather than skipping the event.

## Backup, and getting a tenant back

**The procedure below is executed by `crates/erp-control/tests/restore.rs` on
every run.** A backup nobody has restored is not a backup, and under D15 a failed
restore happens on infrastructure we cannot reach — so this is a test rather than
a promise.

### What to back up

Two planes, and they are not independent:

| | why |
|---|---|
| The control database | the tenant registry, identities, memberships, entitlements. Without it a tenant database has no route to it. |
| Every tenant database | the event log above all — it is append-only, so nothing else can reconstruct it. |

Projections *could* be skipped, since L2 makes them pure functions of the log and
`migrator refresh <module>` rebuilds them. Don't. A rebuild runs at roughly four
thousand events a second (`erp-projection/tests/rebuild_throughput.rs`), so a
tenant with a few million events costs a quarter of an hour to save backing up
tables `pg_dump` would have compressed anyway. Keep the option for the day a
backup turns out to be corrupt, not for the day it turns out to be large.

```bash
pg_dump --format=custom --no-owner --no-privileges --file control.dump "$CONTROL_DATABASE_URL"
```

Per tenant, with the database name from `SELECT slug, database_name FROM tenant`:

```bash
pg_dump --format=custom --no-owner --no-privileges --file "$SLUG.dump" "$CLUSTER_URL/$DATABASE"
```

### Restoring

`pg_restore` needs the database to exist and be empty:

```bash
psql "$CLUSTER_URL/postgres" -c "CREATE DATABASE \"$DATABASE\""
```

```bash
pg_restore --no-owner --no-privileges --dbname "$CLUSTER_URL/$DATABASE" "$SLUG.dump"
```

### The part that goes wrong

**Restore both planes to the same point, and check the tenant is enterable
afterwards.** A restore that stops at "the database is back" has not finished.

The two planes are backed up separately, so they can be restored to different
points, and *neither direction reports an error*:

- **Control plane older than the tenant.** The database exists, complete and
  intact, and there is no route to it. `enter` refuses with the same message it
  gives for a tenant that never existed, because §1.9 requires that a stranger
  cannot tell those apart — so the log says "no such tenant" about a tenant whose
  data is sitting on disk. Asserted by
  `a_tenant_database_without_its_control_row_is_unreachable`.
- **Control plane newer than the tenant.** The tenant is reachable and has
  silently lost every event after the tenant backup. Projections rebuilt from it
  will be internally consistent and wrong, which is worse than an error.

After any restore:

```bash
cargo run --bin migrator -- survey
```

A tenant that comes back below `MIGRATION_FLOOR` is refused rather than migrated
in one hop — that is D17, and an old backup is exactly the case it exists for.

## Starting over

```bash
just clean-databases
```

Drops every database this project creates and clears the control plane rows that
went with them. **It refuses while a test run is in progress**: it drops with
`FORCE`, which would otherwise pull a database out from under a running test —
and that failure surfaces as an unrelated assertion somewhere else entirely.
