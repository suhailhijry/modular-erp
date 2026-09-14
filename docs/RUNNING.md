# Running it

Everything you need to bring the API and the workers up — in containers or by
hand — poke at them, and see what they did.

## The whole thing, in containers

```bash
docker compose up --build -d
```

One image, six binaries: `api`, `worker`, `migrator`, `reaper`, `operator` and
`demo` are the same build and differ only in which `main` runs. Six images would
be six things to keep at one version, and "the worker is a deploy behind the API" is a
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
six tests failing (five in `shared.rs`, the operator's revoke test in
`bin/operator.rs`), and `just redis` is the one that matches. Either that or set
`REDIS_URL` to whatever you are running.

## The three processes

| | what it does | needs |
|---|---|---|
| `bin/api` | serves HTTP | control DB, cluster URL |
| `bin/worker` | projections, both outboxes, health checks, the ZATCA sweeps | the same, plus `SEALING_KEY` for ZATCA and `SMTP_URL` for email |
| `bin/migrator` | brings tenant schemas up to this build; **run before a deploy**. `reseal` moves sealed secrets to a new key | the same; `SEALING_KEY` for `reseal` |

`bin/reaper` destroys expired demo tenants, abandons signups whose build died,
sweeps unanswered signups, expired password-reset and enrolment links and the
control plane's
delivered mail and texts once they are thirty days old, and looks at tenant
databases that no `tenant` row claims. Schedule it **at least hourly**; it exits
when done.

**A signup whose build died** — the process crashed or a deploy killed it
mid-build — leaves its tenant `provisioning`, holding the name. The reaper runs
the compensation the build never got to, once the tenant is older than
`PROVISIONING_GRACE_SECONDS` (15 minutes), so a stuck name is free again within
the grace plus the reaper's schedule. The customer's confirmation link stays
spent; they ask again. A signup whose request timed out, or whose browser went
away, does not end up here: the build carries on without the request and ends in
a company or a clean failure. The reaper refuses a provisioning tenant whose
database has events or a setting somebody chose, and logs it at `error` on every
run. Nothing in the product gets a provisioning tenant there, so it means the
control plane was restored to a point behind that database. Treat it like the
unclaimed case below.

The unclaimed databases are handled with care, deliberately. An unclaimed tenant
database is almost never rubbish: `provision` writes the row before it creates
the database, so a provisioning that dies leaves a row, which the sweep above
abandons, and never a database with no row. What is left over is a control
plane that has *lost* rows — a restore to a point before a tenant existed, a
failover to a stale replica, a mis-pointed `CONTROL_DATABASE_URL` — and those
databases are full of events. See "Control
plane older than the tenant" below: that state is recoverable by putting the row
back, and it stops being recoverable the moment something drops the database.

So the reaper asks the database rather than the control plane. It drops one
only when the database itself says it holds nothing: **no events, and no setting
anybody chose** — a module's own seed does not count, because installing the
module writes it again.

Anything with data in it, or anything it cannot open, **refuses the whole
cluster's sweep** and is logged at `error`. Nothing is dropped that run, empty
ones included. If you see that: check whether the control plane is behind
reality before you restore anything else. A tenant database with events and no
control-plane row is recoverable by putting the row back, and only until
something deletes it.

`bin/operator` makes and unmakes **platform staff**, and is run by hand:

```bash
docker compose run --rm api operator grant-staff noura@erp.example superadmin
docker compose run --rm api operator revoke-staff noura@erp.example
```

It needs `CONTROL_DATABASE_URL`, and `REDIS_URL` wherever the API has one:
`docker compose run` gives it both. Without Redis its cache invalidations reach
no API node, and they keep a revoked role for up to five seconds. Use it once,
for the first superadmin — after that superadmins manage staff over
`/v1/platform/staff`, where the audit trail names who did it. The account has to
exist already and have a second factor (sign in, enrol an authenticator app at
`/v1/sessions/second-factor`, then grant). `revoke-staff` is the break-glass
path: it will remove the **last** superadmin, which the HTTP route refuses, and
it ends every session that account holds. Reach for it when that account is
compromised and grant a replacement right after. Anything else it is given
prints the usage and exits 2.

**Setting a company up** is billing's or a superadmin's, over
`POST /v1/platform/tenants` with the slug, the company name, the owner's email
and the modules it asked for. Signup is closed to the public unless the
deployment sets `SIGNUP=open` — a production deployment does not, because a
tenant gets what it asked for after it has paid and there is no trial; the
public demo is the trial. The owner is mailed a link saying the company is
ready; nothing exists until they open it, choose a password (or give the
password of the account the address already has), and are signed in with the
modules installed. Modules are not what a company pays for — seats are — so the
owner may switch them on and off afterwards at `POST /v1/modules`. The request
is on the platform audit trail as `signup.requested` under your name.

**Suspending a tenant** is billing's or a superadmin's, over
`POST /v1/platform/tenants/{id}/suspend` with a `reason`, and
`POST /v1/platform/tenants/{id}/reinstate` lifts it. There is no command for it.
The id is in the control plane (`SELECT id FROM tenant WHERE slug = 'acme'`) —
no route looks one up by name yet. **Write the reason for the tenant's owner**;
it goes into the audit trail about their tenant, under your name, and they read
it there: `GET /v1/audit` on their host answers them while everything else is
`503`.

A suspension has two halves. From the moment you act the tenant is
`suspending`: its members, keys and public pages get
`503 access.tenant_unavailable` everywhere but the owner's `GET /v1/audit`, and
every background job stops except the two that **sign and report its issued
documents to ZATCA** — a simplified invoice has 24 hours to be reported, and a
suspension must not be what makes one late. The worker keeps visiting for those
two until nothing is left for them, then moves the tenant to `suspended`,
records `tenant.suspension_complete` in its audit trail, and never claims it
again. A tenant that never finished ZATCA onboarding has nothing anybody can
sign or send, and completes at the first visit. If ZATCA is unreachable the
tenant stays `suspending` — shut, and retried — until it answers.

Payment gateway callbacks are refused from the first half on; the settle sweep
collects what they would have said once the tenant is back. Reinstating works
from either half. The fleet migrator keeps a suspended tenant's schema current.
Other API nodes refuse the tenant within five seconds, at once where they share
Redis.

**The control plane's dead letters** — signup, invitation and reset emails and
sign-in texts it gave up on — are support's or a superadmin's, over
`GET /v1/platform/effects/dead`. `POST /v1/platform/effects/dead/{id}/requeue`
sends one again; `DELETE /v1/platform/effects/dead/{id}` deletes one. Each is
recorded in the audit trail under your name. The `idempotency_key` says what it
was: `signup:`, `invitation:`, `reset:` or `code:`, then the id of the row.
**Dismiss `code:` and `reset:` letters; don't requeue them.** A sign-in code
expires in five minutes and a reset link in an hour, and the retries take about
four hours to give up, so one that died of an outage had expired long before it
died, and requeued it sends somebody a link that no longer works. A signup link
lives a day and an invitation two weeks: requeue those once whatever killed them
is fixed, if they are still inside it. Until each is dealt with, every worker logs `invariant violated` with
`check=no_dead_letters plane=control` every five minutes.

**The audit trail** is support's or a superadmin's to read, over
`GET /v1/platform/audit`, newest first: `?tenant=<id>` for one company,
`?identity=<id>` for what was done to a person and what they did, staff
included. Every actor is named. It records who changed members, roles, keys,
domains, origins, modules, invitations, staff and a tenant's status, support
opening a tenant, and dead letters handled. It does not record signing in or
out, passwords changed or reset, second factors enrolled or removed by the
person themselves, or a tenant's second-factor rule. It **does** record a second
factor reset by somebody *else* (`second_factor.reset`), with the reason when
platform support did it — that one is an act against another person's account. A tenant's settings, permission limits and the
sales document limit among them, are in its own `configuration` table with a
`set_by` of their own, and its business is in its event log; neither is in this
trail.

## Environment

```bash
CONTROL_DATABASE_URL   # the control plane
PRIMARY_CLUSTER_URL    # the tenant cluster named `primary` in the control plane
PRIMARY_CLUSTER_CAPACITY   # how many tenants it holds; the migrator refuses to
                       #   declare the cluster without it — size it from
                       #   measurement (architecture D13), never from a guess
PUBLIC_DOMAIN          # tenants are subdomains of this; defaults to `localhost`
BIND                   # the API's address; defaults to 0.0.0.0:8080
TRUST_X_FORWARDED_FOR  # `true` only behind a proxy you run that appends the
                       #   caller to X-Forwarded-For; otherwise every caller
                       #   behind it shares one rate-limit bucket — see below
SIGNUP                 # `open` lets anybody create a tenant at POST /v1/signups;
                       #   unset or `closed`, staff set companies up — see below
SEALING_KEY            # <id>:<64 hex>[,<id>:<64 hex>…]; the first seals — see below
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
FLEET_CONCURRENCY      # tenants the migrator walks at once; defaults to 16
DEMO_PASSWORD          # `bin/demo` only: the demo owner's password; no default
DEMO_SLUG              # `bin/demo` only: the tenant's name; defaults to `demo`
DEMO_TTL_DAYS          # `bin/demo` only: days until the reaper destroys it;
                       #   defaults to 7, and 0 keeps it for ever
RUST_LOG               # e.g. info,erp_worker=debug
```

**`TRUST_X_FORWARDED_FOR` decides whose address a rate limit counts.** Every
unauthenticated route — sign-in, signup, invitation acceptance, one-time codes,
the public booking site — is bounded per caller address. Behind a proxy the
socket's peer is the proxy, so without this every caller in the world is one
address and one budget: the first person to mistype a password five times locks
sign-in for everybody. Set it to `true` when, and only when, a proxy this
deployment controls terminates connections and appends the client to
`X-Forwarded-For`; `compose.yaml` sets it on `api` because `proxy` does exactly
that. With it on and no such proxy, the header is whatever the caller sends, and
the limit is nothing.

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
everything already promised goes out. It does not wait quietly: once the oldest
has waited five minutes, the worker's control-plane health check logs
`invariant violated` with `check=outbox_keeping_up plane=control` every five
minutes until it goes.

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
refuses to store a tenant's ZATCA private key or a payment gateway's API key
(503, `request.no_sealing_key`), and the worker registers neither the ZATCA
sweeps nor `payments.settle` — invoices are built and chained but never signed
or sent, and gateway payments are recorded but never settled. Generate one:

```bash
echo "$(date +%Y-%m):$(openssl rand -hex 32)"
```

The identifier before the colon is stored beside every row it seals, and a row
is opened with the key it names or not at all: one under an id this deployment
does not hold is refused (500, and the log names the id to put back). **The API
and the worker must have the same list**: the API writes the secrets and the
worker reads them. What is sealed: ZATCA signing keys and CSIDs, payment
gateway keys, saved-card tokens, webhook signing secrets, and every account's
authenticator-app secret, platform staff's included.

### Rotating the sealing key

`SEALING_KEY` is a list. The first key seals; the rest are only read. To move
from `old` to `new`:

1. `SEALING_KEY=old:…,new:…` on the API and the worker. Every process can now
   read `new`, and nothing is sealed under it yet.
2. `SEALING_KEY=new:…,old:…` on both. From here on, everything is sealed under
   `new`.
3. `migrator reseal`, with the same list: `docker compose run --rm api migrator
   reseal`, or `SEALING_KEY=… just migrate-fleet reseal`.
4. `migrator reseal check`. When it exits 0, set `SEALING_KEY=new:…` on both.

Step 1 is there so a process that has not been updated yet never meets a value
sealed under a key it lacks. Skip it in an emergency and the cost is refusals,
not loss: until the rollout finishes, processes still on the old list answer
500 on authenticator-app sign-ins and cannot sign ZATCA documents or settle
gateway payments.

`reseal` moves the control plane's authenticator secrets and every tenant's
module secrets, suspended tenants included, one row at a time; running it
again carries on where it stopped. It prints how many values are under each
key, names anything no key in the list opens (and leaves that row exactly as it
was), and lists any tenant it could not reach. It reads the control plane's
schema as this build has it, so run the deploy's `migrator` first.

**`reseal check` is the gate for retiring a key.** It writes nothing. It opens
every sealed value, those already under the first key included, and exits 0
only when every sealed value is under the first key, nothing failed to open,
and every tenant was reached. Until then, keep the old key in the list.

**An id names its bytes for good.** The new key takes an id no key has had
before, and the old key keeps its id until it retires. Never rename an entry:
every row names the id it was sealed under, so giving that id to other bytes
leaves those rows unopenable. `reseal check` lists every one of them, and the
fix is to put the id back on its old bytes.

**Run `migrator reseal` once after deploying the build that introduced this.**
Authenticator apps enrolled before it were sealed without a record of the key,
and `reseal check` counts them as `(unrecorded)` until they are stamped. They
work in the meantime: a row with no key id is tried under every key in the list.

**Keep a retired key offline for as long as you keep backups sealed under it.**
A restored backup from before a rotation holds values under the old id, and they
are refused until that key is back in the list and `reseal` has run.

**Do not roll back past this build in the middle of a rotation.** Older builds
read one key and refuse a list at startup. With one key in the list, before or
after a rotation, a rollback reads everything this build wrote.

Nothing reminds you to rotate. Put it on the operations calendar.

### If the sealing key leaks

Rotating protects what is written from then on. It does not un-leak anything:
whoever has the key and a copy of the databases, a backup included, has already
read every secret sealed under it. So rotate, skipping step 1 if you have to,
and take the leaked key out of `SEALING_KEY` as soon as `reseal check` exits 0.
The new key needs a new id even when the last rotation was this month
(`2026-09b`), not the leaked key's id with the leaked entry renamed.
Then replace every secret it protected. None of this is automated:

- **ZATCA.** Every onboarded tenant replaces its key and certificate. A tenant
  that is already live is refused by `…/onboarding/activate` (409), so it goes
  through the manual route: `POST /v1/tax_sa/zatca/onboarding` generates a new
  key pair and CSR, and `PUT …/onboarding/certificate` takes what ZATCA issues
  for it, with a new OTP from the Fatoora portal. Ask ZATCA to revoke the old
  certificates.
- **Payment gateways.** Regenerate each tenant's API keys at the provider and
  store the new ones (`PUT /v1/payments/gateways/{provider}`). Saved-card
  tokens were sealed too: ask each provider whether theirs work without the
  account's key, and have them revoked if they do.
- **Webhooks.** Rotate each provider's signing secret at the provider and store
  it again (`PUT /v1/hooks/{provider}/secret`).
- **Authenticator apps.** Ask everybody who has one to enrol again
  (`POST /v1/sessions/second-factor`, then confirm), platform staff first. A
  leaked TOTP secret makes valid codes until it is replaced.

## Seed something to look at

```bash
just demo correct-horse-battery-staple
```

Builds a tenant with every module on, filled through the public API: six
invoices (one credited, one discounted), four bills, three payments, a filed VAT
return, and a colleague with narrower permissions. It prints the slug, the
sign-in address, the tenant id, and the colleague's own password — minted for
this demo, because the owner's is the one on the screen while the permissions
model is being shown.

Export the same `SEALING_KEY` the API and the worker have before running it:
the demo saves a card and asks for it to be charged, and the card is sealed
under that key. Without one the demo mints a key for its own run and warns,
and the card it saved is readable by nobody else.

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

Every refusal is `application/problem+json` with a stable `code`. **Branch on
the code, never on `detail`** — the detail is prose in whichever language was
asked for. The codes are the module catalogs (`erp_api::CATALOG` is their
union), and every code has an English and an Arabic rendering, which a test
enforces.

## Turning on a second factor

Per person, not per tenant — an account is one identity across every tenant it
belongs to.

```bash
# 1. Start. Nothing about signing in changes yet.
curl -X POST "$API/v1/sessions/second-factor" -H "$AUTH"
# → { "uri": "otpauth://totp/ERP:...", "secret": "JBSWY3DPEHPK3PXP" }
```

Render `uri` as a QR for an authenticator app, or type `secret` in by hand.

```bash
# 2. Confirm with the first code the app shows. This is what turns it on.
curl -X POST "$API/v1/sessions/second-factor/confirmation" -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{ "code": "123456" }'
# → { "recovery_codes": ["ABCDE-FGHJK", ...] }
```

**Keep the recovery codes.** The server stores only their digests, so that list
cannot be produced again — enrolling afresh is the only way to get a new one,
and it invalidates the old set along with the old phone. Confirming also signs
the account out everywhere except the session that confirmed.

From then on, signing in takes both:

```bash
curl -X POST "$API/v1/sessions" -H 'Content-Type: application/json' -d '{
  "handle": "sara@acme.test", "password": "…", "code": "123456"
}'
```

Without `code`, an enrolled account gets `401` with code
`auth.second_factor_required` — that is the signal to ask for six digits and
retry, **not** to say the password was wrong. A recovery code goes in the same
field and is spent when used.

`GET /v1/sessions/second-factor` says whether an account is enrolled and how
many recovery codes are left. `DELETE` on the same path turns it off — except
where a second factor is required of the account. Platform staff are refused
(`403 auth.staff_keeps_second_factor`) until they are taken off the staff. A
live member of an organisation that requires one is refused
(`403 auth.tenant_keeps_second_factor`) until its owner removes them from it or
stops requiring it; a member cannot leave an organisation on their own. Either
can still replace their authenticator app. A suspended organisation's
requirement still counts, and while it is suspended its owner can do neither.

**This deployment needs a sealing key.** The shared secret is encrypted at rest,
so without one enrolment answers `503` rather than storing it in the clear.

### When somebody loses the phone *and* the paper

Then nothing they hold proves the factor, and only somebody else can help.

```bash
# The organisation's owner, or a member holding hr:reset_second_factor.
curl -X POST "$API/v1/members/$IDENTITY/second-factor-reset" -H "$AUTH"
```

**A person has to ask.** An API key is refused (`keys.not_a_person`) whatever it
is scoped for and whatever role it was issued: a machine identity has no
employee record, so it can hold no claim either, and taking somebody's sign-in
away is not an integration's act.

The app and all ten recovery codes stop working, every session they hold ends,
and they are emailed a one-time link. **Until they open one, their password
alone cannot set up a new app** — and that does not lapse when the link does, so
waiting an expired link out changes nothing. Send another by running the same
call again; there is no separate route, because a fresh link is the same act.
They enrol by sending the token as `link`:

```bash
curl -X POST "$API/v1/sessions/second-factor" -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{ "link": "…" }'
curl -X POST "$API/v1/sessions/second-factor/confirmation" -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{ "code": "123456", "link": "…" }'
```

**Four people this route will not touch**, each with its own code: yourself
(`second_factor.reset_yourself` — replace your app instead), the organisation's
owner (`second_factor.reset_the_owner`), platform staff
(`second_factor.reset_platform_staff`), and **anybody who also belongs to
another organisation** (`second_factor.reset_another_company`). The last is the
one to know: two-step sign-in belongs to the person's account everywhere, not to
one company, so a company that is not the only one they work for cannot weaken
it. They go to support.

```bash
# Platform support or a superadmin, for anybody at all. The reason is required
# and goes in the platform audit trail under your name.
curl -X POST "$API/v1/platform/identities/$IDENTITY/second-factor-reset" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{ "reason": "Ticket 4471: lost phone and recovery sheet, identity checked by video call." }'
```

Resetting a **staff** account needs `manage_staff`, so support cannot reset a
superadmin's; and nobody resets their own here either.

## Before you invoice anything untaxed

**A line that carries no tax must name the article it is untaxed under**, and
only you know which one covers your business. Set it once:

```bash
# A landlord letting residential property.
curl -X PUT "$API/v1/ledger/vat-rates" -H "$AUTH" -H 'Content-Type: application/json' -d '{
  "standard": 1500,
  "exempt_reason": "VATEX-SA-30"
}'
```

Until you do, issuing an exempt or zero-rated line is **refused** — in your
language, naming the treatment — rather than being sent to ZATCA with a reason
somebody guessed. The common codes:

| Code | What it covers | Treatment |
|---|---|---|
| `VATEX-SA-30` | Real estate transactions (Article 30) — **residential rent** | Exempt |
| `VATEX-SA-29` | Financial services (Article 29) | Exempt |
| `VATEX-SA-29-7` | Life insurance (Article 29) | Exempt |
| `VATEX-SA-32` | Export of goods | Zero-rated |
| `VATEX-SA-33` | Export of services | Zero-rated |
| `VATEX-SA-35` | Medicines and medical equipment | Zero-rated |
| `VATEX-SA-EDU` | Private education to a citizen | Zero-rated |
| `VATEX-SA-HEA` | Private healthcare to a citizen | Zero-rated |

`GET` the same path to see what is set. **Not retrospective**: every invoice
already issued carries the article it was issued under, so correcting this
cannot restate a filed return.

Standard-rated businesses need none of this — a taxed line has nothing to
explain.

## Which branches somebody belongs to

Until 2026-09-14 `X-Branch` was a header the caller wrote: nothing recorded
which branches a person belonged to, so nothing could refuse one they did not.
The owner records it now, per member:

```bash
# Sara works at Olaya, and nowhere else.
curl -X PUT "$API/v1/members/$SARA/branches" -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{ "branches": ["BR-OLAYA"] }'
```

Each branch must be open (`GET /v1/branches`). From Sara's next request:
`X-Branch: BR-MALAZ` is refused with `403 access.wrong_branch`; a request naming
no branch is one at Olaya; her shelves, lots and the stock summary show Olaya
alone, and `?branch=BR-MALAZ` on any of them is refused the same way; the org
chart cannot be read company-wide (`?scope=all` is `403 access.name_a_branch`).
Give her two branches and a request has to name one of them — the same
`access.name_a_branch`, listing hers. An API key is bound the same way through
its own membership: `GET /v1/members` lists the key's identity, and the same
route confines it. Nobody is confined until you say so; `{ "branches": [] }`
puts them back on every branch. Every change is on the audit trail as
`membership.branches_changed`.

## Who may issue a credit note

Cancelling an invoice, crediting part of one, a refund that clears one, **taking
a return at the till**, and asking a gateway for a refund that will leave one
owing all end in a credit note, and all of them need the
`sales:approve_credit_note` claim on your org chart — but only once your company
has granted **that** claim to somebody. A company that has never granted it is
asked nothing, whatever other claims it has granted, which is where every
company starts and is why nothing changes for you until you decide it should.

```bash
# Sara may approve credit notes.
curl -X POST "$API/v1/hr/employees/EMP-SARA/claims" -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{ "claim": "sales:approve_credit_note" }'
```

**Nobody else gains it.** This claim is on the segregation-of-duties list, so it
does not travel up the reporting line the way most do: the person who raises a
document and the person who cancels it must not be one pair of hands, and
`propagates` is ignored for it. Grant it to each person who needs it.

The refusal is `403 sales.not_approved`, at `/v1/sales` and at the counter
alike. **The till asking is a deliberate change**, decided by the product owner:
it used to be the one door that did not ask, so a clerk who could not credit an
invoice on the sales screen could hand the same money back at the counter and
get the same credit note. Your owner is never asked, and neither is a credit
note nobody issued — a worker's sweep, or the one a gateway's confirmed refund
writes, because by then the money has gone. **Asking** a gateway for that refund
is asked, though: whoever asks is judged when they ask, on the credit note their
refund will leave owing.

Resending the same `reference` stays a no-op, claim or no claim. A return whose
answer was lost still answers with the credit note it issued, even if the claim
has been revoked since — a retry is not a second document.

## ZATCA, end to end

Onboarding needs a six-digit OTP the taxpayer generates in the Fatoora portal.
The whole flow, from OTP to a tenant that can clear invoices:

```bash
# 1. Who the business is. Every document is stamped with this.
curl -s -X PUT $API/v1/tax_sa/registration -H "$H" -H "$A" -H 'content-type: application/json' -d '{
  "vat_number":"310122393500003","name":"روابي للاستشارات","scheme":"crn",
  "identifier":"1010101010","industry":"Consulting",
  "address":{"street":"طريق الملك فهد","building":"2322","district":"العليا",
             "city":"الرياض","postal_code":"12211","country":"SA"}}'

# 2. The OTP. This request generates the key, buys the compliance certificate
#    and answers 202; the worker submits the six samples and obtains the
#    production certificate on its next visit.
curl -s -X POST $API/v1/tax_sa/zatca/onboarding/activate -H "$H" -H "$A" -H 'content-type: application/json' -d '{
  "environment":"simulation","otp":"123456"}'

# 3. Where it stands. `state` goes checking -> live; `refusal` says what ZATCA
#    refused, if anything.
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
just migrate-fleet check      # is every tenant's schema, and read model, where this build expects?
just migrate-fleet versions   # can this build read what is already in the logs?
```

Both look without touching and exit non-zero when the answer is no. Run them
**before** the new pods go up; that is the whole point of them.

```bash
just migrate-fleet                    # apply outstanding migrations, rebuild stale read models
just migrate-fleet refresh sales      # rebuild one module's read models anyway
```

A rebuild replays into a staging schema, catches up under the checkpoint lock,
and swaps — the old read models keep serving until the new ones are complete.

**Read models are rebuilt by the bare command, not by you remembering to.**
Every projection group records the read-model version that built its tables,
and the bare command rebuilds every group — suspended tenants' and disabled
modules' included — whose version is not this build's, after the migrations.
`check` lists them as `acme: sales at 1, this build projects 2`. A group no
module in the build declares is listed too: a module was dropped rather than
deprecated. Deploy time grows with the logs of every group that changed, and
the first deploy after versions were introduced rebuilds every group once.

**Until a group is rebuilt**, the new pods refuse to project into it (the
worker logs `job failed; it is stalled`, naming the group and both versions)
and every module route served from it answers
`503 request.read_model_rebuilding` — in the tenant's language, retryable. That
is the answer to a rebuild that failed on one tenant: fix what failed and run
the bare command again; the first request after the swap is served.

**During the rollout, the old pods stop projecting the groups it rebuilt.** A
worker projects only into tables stamped with its own build's version, so the
old workers log the same `job failed; it is stalled` for each changed group,
and those groups wait for the new workers. A read with `?consistent_after=`
answers `503 request.not_caught_up` meanwhile. That is expected, and ends when
the old pods are gone; the same lines from a *new* pod are not.

**Run the bare command once more after the rollout** when the deploy changed a
read model. A tenant that signed up on an old pod while the rollout was under
way was built with the old read models, and `check` will list it. The one
exception is the rollout that first records read-model versions (tenant
migration `0016`): an old pod provisions without that column, so until the
bare command runs again every module route of such a tenant answers `500`, and
`check` lists it behind on migrations and `unreachable` among the read models.

## When something looks wrong

```bash
RUST_LOG=debug cargo run --bin worker            # what it visits and why it skips
psql "$CONTROL_DATABASE_URL" -c "SELECT * FROM audit_entry ORDER BY id DESC LIMIT 20"
psql "$CONTROL_DATABASE_URL" -c "SELECT slug, database_name, status FROM tenant"
```

Inside a tenant's database:

```sql
SELECT group_name, position, read_model_version FROM projection_checkpoint;   -- how far, and what built it
SELECT count(*) FROM event;                               -- how far the log is
SELECT kind, attempts, dead_at FROM outbox WHERE delivered_at IS NULL;
SELECT id, status, icv, signed_at FROM proj_tax_sa.zatca_document ORDER BY icv;
```

A projection stopped behind the log is a group that hit something it could not
apply — the worker logs it and stops that group rather than skipping the event.

An `invariant violated` line with `plane=control` is about the control plane's
outbox: `GET /v1/platform/effects/dead` as support lists what it gave up on,
and a backlog with no dead letters is a relay or SMS transport this worker was
not given.

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
cargo run --bin migrator -- check    # where did the restored schemas and read models come back?
cargo run --bin migrator             # bring them current, rebuilding older read models
SEALING_KEY=… cargo run --bin migrator -- reseal check   # anything under a retired key?
```

A tenant that comes back below `MIGRATION_FLOOR` is refused rather than migrated
in one hop — that is D17, and an old backup is exactly the case it exists for.
A backup from before a rotation holds secrets under the old key; see "Rotating
the sealing key".

This used to say `migrator -- survey`, which is not a mode. The migrator ran
any word it did not know as the bare command, so it applied migrations to the
fleet. It now refuses anything it does not know, with its usage and exit 2.

## Starting over

```bash
just clean-databases
```

Drops every database this project creates and clears the control plane rows that
went with them. **It refuses while a test run is in progress**: it drops with
`FORCE`, which would otherwise pull a database out from under a running test —
and that failure surfaces as an unrelated assertion somewhere else entirely.

## Watching a tenant live

Needs Redis (`REDIS_URL`); without it the routes answer 503. A staff screen:

```bash
curl -N $API/v1/events -H "$H" -H "$A"
```

```text
event: ready
data: {"groups":{"booking":1234,"sales":980}}

event: advanced
data: {"group":"booking","position":1235}

: keep-alive

event: reconnect
```

Re-fetch through the ordinary API with `?consistent_after=1235`; never apply a
delta, the stream carries none. The phone that booked watches its reservation:

```bash
curl -N $API/v1/booking/public/reservations/$RESERVATION/events -H "$H"
```

and re-fetches `…/reservations/$RESERVATION/deposit?consistent_after=<position>`.
Streams end after ten minutes with `reconnect`; `EventSource` reconnects by
itself and the fresh `ready` says what moved. Caps per business per server:
`REALTIME_STAFF_STREAMS_PER_TENANT` (256) and
`REALTIME_PUBLIC_STREAMS_PER_TENANT` (4096).

## The bell

`notifications` needs `messaging`, and a person only has an inbox once somebody
says which login is theirs:

```bash
curl -X PUT $API/v1/hr/employees/EMP-1/login -H "$H" -H "$A" -H 'content-type: application/json' \
  -d '{"identity":"<an identity from GET /v1/members>"}'
```

That grants nothing — no capability check reads it — and one login belongs to
one employee. Then:

```bash
curl "$API/v1/notifications?unread=true" -H "$H" -H "$A"
curl -X POST $API/v1/notifications/read -H "$H" -H "$A"
```

Every route here acts on the caller's own inbox; there is no way to name
somebody else. `GET /v1/notifications/preferences` returns the whole grid with
the defaults filled in — the bell on, every paid channel off — and `PUT` replaces
it whole:

```bash
curl -X PUT $API/v1/notifications/preferences -H "$H" -H "$A" -H 'content-type: application/json' \
  -d '{"entries":[{"kind":"booking_reserved","channels":["in_system","sms"]}]}'
```

The kinds are `booking_reserved`, `payments_settled`, `payments_failed`,
`tax_refused`, `document_expiring`, `stock_expiring` and `stock_expired`. The two
stock kinds go to logins, which have no address, so they take `in_system` and
nothing else. A tenant that wants its own words for one
saves a `messaging` template **named after the kind** on the `in_system`
channel; its bindings are checked when it is saved, like every other template.

The bell is a projection group like any other, so `advanced` names it on the
staff stream and a screen re-fetches its own inbox — the signal carries a
position and never the notification.

## Conversations

A thread is named by what it is about, so there is nothing to create:

```bash
curl $API/v1/conversations/reservation/BK-1 -H "$H" -H "$A"
curl -X POST $API/v1/conversations/reservation/BK-1/notes -H "$H" -H "$A" -H 'content-type: application/json' \
  -d '{"text":"Called, wants Thursday."}'
curl -X POST $API/v1/conversations/reservation/BK-1/messages -H "$H" -H "$A" -H 'content-type: application/json' \
  -d '{"text":"Thursday at 10 works.","channel":"sms"}'
```

A note stays inside. A message goes out through `messaging`, is charged to the
meter and answers `402` when the month's budget is spent. `sms` and `email` are
the channels a person may pick — WhatsApp takes approved templates outside a
24-hour window and push reaches a device rather than a person, so both are
refused with that as the reason. **Reading needs `post_entries`**, not `read`: a
thread holds private notes, and `read` is the external accountant's role.

### Replies coming back

Point your SMS gateway's inbound relay at the webhook route and register its
secret under the provider name `messages`:

```bash
curl -X POST $API/v1/hooks/messages -H "$H" \
  -H "x-webhook-timestamp: $(date +%s)" \
  -H "x-webhook-signature: <hmac-sha256 of '<timestamp>.<body>', hex>" \
  -d '{"id":"gw-123","from":"+966500000001","body":"Thursday works","sent_at":"2026-05-04T09:00:00Z"}'
```

The worker lands it in the thread it answers: what was last said to that number
**as of when they replied**, else that number's customer, else the tray:

```bash
curl $API/v1/conversations/unmatched -H "$H" -H "$A"
curl -X POST $API/v1/conversations/unmatched/+966599999999/assign -H "$H" -H "$A" -H 'content-type: application/json' \
  -d '{"topic":"reservation","id":"BK-1"}'
```

Assigning moves what has arrived. The next message from that number lands in the
tray again — put it on the customer's record to stop that, which is `crm`'s job
and not this one's.
