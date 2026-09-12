# erp-api

The HTTP surface, and the one place that composes it.

**Depends on:** `erp-web`, `erp-tenant` and `erp-rules` (for permission limits), and every module.
**Used by:** `erp-demo`, and `bin/api.rs` inside it.

## What is here and what is not

**Here:** the core's own routes. Sessions, the tenant, members, invitations,
signing up, and turning modules on and off. A tenant cannot disable any of it,
which is what makes it core.

**In the modules:** everything else. `sales::http::routes()` is a router the
sales crate owns, next to the aggregates and the read models it serves, and
`modules.rs` mounts it. Four route files used to live in this crate, which meant
a module's HTTP surface was written by the composition root and a module could
not be read in one place.

**In `erp-web`:** what those routers are built *from*. Below the modules, because
a module has to be able to name it.

## The files

| File | What is in it |
|---|---|
| [`routes.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/routes.rs) | `router`, `openapi`, health, the OpenAPI conventions |
| [`modules.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/modules.rs) | `REGISTERED`, `available`, `mounted`, dependency checks |
| [`catalog.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/catalog.rs) | The complete message catalog |
| [`signup.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/signup.rs) | `POST /v1/signups` and `POST /v1/signups/{token}` |
| [`members.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/members.rs) | `/v1/members` |
| [`platform.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/platform.rs) | `/v1/platform/staff`, `/v1/platform/tenants/{id}/…`, `/v1/platform/effects/dead` and `/v1/platform/audit` — the platform's own surface |
| [`audit.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/audit.rs) | `GET /v1/audit` and `GET /v1/sessions/current/audit`, and the view all three trail readers answer in |
| [`permission_limits.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/permission_limits.rs) | `GET`/`PUT /v1/tenant/permission-limits`, the owner's writer for `erp_tenant::Limits` |
| [`invitations.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/invitations.rs) | `/v1/invitations` and `/v1/join/{token}` |
| [`bin/api.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/bin/api.rs) | The API process |

## The public surface

```rust
pub use catalog::CATALOG;
pub use erp_web::{AppState, Problem};       // so a caller wires up with one crate
pub use modules::available as modules;
pub use routes::{openapi, router};

pub fn router(state: AppState) -> Router;
pub fn openapi() -> utoipa::openapi::OpenApi;
pub fn modules() -> Vec<(&'static str, ModuleSetup)>;
```

That is the whole of it. Everything else in the crate is `pub(crate)`.

## The module list

```rust
struct Registered {
    name: &'static str,
    setup: fn() -> ModuleSetup,
    http: fn() -> OpenApiRouter<AppState>,
}

const REGISTERED: &[Registered] = &[
    Registered { name: "ledger",    setup: ledger::setup,    http: ledger::http::routes },
    Registered { name: "sales",     setup: sales::setup,     http: sales::http::routes },
    Registered { name: "purchases", setup: purchases::setup, http: purchases::http::routes },
    Registered { name: "tax_sa",    setup: tax_sa::setup,    http: tax_sa::http::routes },
];
```

**Why a list and not a `Module` trait.** Half of what a trait would carry cannot
cross this boundary. A module ships its routes, and those are in the module now,
but its worker jobs are registered in `bin/worker.rs`, and a module cannot depend
on `erp-worker` any more than `erp-worker` can be made to know what a ZATCA
document is. A trait with two of its three methods implemented somewhere else is
a trait that describes nothing.

So each composition root lists what it composes. This one carries both views a
caller needs, the `ModuleSetup` the control plane installs from and the router
the server mounts, from **one entry per module**. That is the property that
matters: a module cannot be added to the platform and have its routes forgotten,
because there is nowhere to add it that does not also mount them.

Several things read this list and they must not disagree: signup, this file, the
worker's job registry, the migrator's fleet check, and the demo tenant, which
enables all of it. "The demo has every module enabled" is a requirement nothing
could check while the set was a `match` arm.

```rust
pub fn available() -> Vec<(&'static str, ModuleSetup)>;
pub(crate) fn mounted() -> OpenApiRouter<AppState>;
pub(crate) fn find(name: &str, locale: Locale) -> Result<ModuleSetup, Problem>;
pub(crate) fn check_offered(setup: &ModuleSetup, locale: Locale) -> Result<(), Problem>;
pub(crate) fn check_requirements(setup: &ModuleSetup, present: &[String], locale: Locale)
    -> Result<(), Problem>;
```

`check_requirements` is shared by signup, where `present` is what was asked for,
and by enabling, where it is what the tenant already has. "Sales needs the
ledger" must not be true in one and forgotten in the other. It checks both kinds:
everything in `requires`, and at least one of `requires_any`.

`check_offered` is called where a module is **added**, and deliberately not in
`find`, because `find` also serves disabling and module roles. A tenant on a
deprecated module has to be able to turn it off and to keep managing who uses it
in the meantime; refusing there would trap them.

## Adding a module to the build

1. Create the crate under `modules/`, depending on `erp-tenant`, `erp-web`, and
   whichever modules sit below it. **Never `erp-control`.**
2. Export `setup()`, `module_id()`, `upcasters()`, `CATALOG`, `projections()`
   and `http::routes()`. `setup()` names its group as `(NAME, SCHEMA, VERSION)`
   from the `ProjectionGroup`, and `.reading(&[…])` the modules its crate
   depends on.
3. Add one `Registered` entry here.
4. Add its catalog to `catalog.rs`.
5. Register its `ProjectionJob` (with `.for_module`) and any invariants or jobs
   in `bin/worker.rs`.
6. Teach `erp-demo` to seed it, or the demo enables it and shows nothing.
7. `just prepare`, `just errors`, `just openapi`, and commit the diffs.

Several tests will tell you if you missed a step. `a_modules_schema_is_named_after_its_crate`
pins the naming. `a_modules_reads_are_its_crate_dependencies` compares `reads`
with its `Cargo.toml`. `a_read_model_change_bumps_its_version` in `bin/migrator`
prints the read-model pin to add, and `every_module_can_be_rebuilt` wants an arm
in the migrator's `rebuild`. The shadow-replay coverage assertion fails if the group is not
replayed. `every_role_against_every_endpoint` fails if a route has no
authorization row. The completeness audit fails if a message has no Arabic.

## Router and document are the same object

```rust
pub fn router(state: AppState) -> Router;
pub fn openapi() -> utoipa::openapi::OpenApi;
```

`router` fills `AppState::read_models` from the same list that mounts the module
routes, so a server refuses a stale read model by the list it serves from, and
the document's conventions add the resulting `503 request.read_model_rebuilding`
to every route of a module with read models.

A route this crate serves under a module's path is refused by that module, and
the module's `reads` is its own crate's dependencies, which cannot see this
crate. So `COMPOSED` in `modules.rs` names what those routes run beyond it: the
reservation invoice under `/v1/booking` is issued through `sales`, and the
public deposit goes through `payments`, `ledger`, `messaging` and `tax_sa`.
`composed_routes_run_only_what_their_module_is_refused_on` scans each file here
that declares a `/v1/{module}/` path, and fails on a module it names as `x::`
that is not in that module's closure. A new route under a module's path that
calls another module needs its line there.

`OpenApiRouter` registers an axum route *from* its handler's `#[utoipa::path]`
attribute, so the path and the method a client reads are the path and the method
the server answers on. The same string, not two that agree today. A handler with
no attribute does not compile inside `routes!`, and one with an attribute that is
never registered is dead code. Neither half can grow a route the other does not
have.

Schemas come from the wire types by derive, so renaming a field renames it in the
document. What is left hand-written is the response declarations, which status a
handler answers with and what it carries, and those are checked against real
responses by `tests/contract.rs`.

`GET /v1/openapi.json` serves it, and `docs/openapi.json` is the same document on
disk. `tests/openapi.rs` fails when the two disagree, and `just openapi`
regenerates it.

There is deliberately no bundled UI. The bundles are megabytes of vendored assets
fetched at build time, which is a network dependency in a build that otherwise
has none. Point any OpenAPI viewer at the document.

## The complete catalog

```rust
pub static CATALOG: Composite = Composite::new(&[
    &erp_web::CATALOG,      // itself a composite: request-level, control, eventlog
    &ledger::CATALOG,
    &sales::CATALOG,
    &purchases::CATALOG,
    &tax_sa::CATALOG,
]);
```

This is the *complete* composite and it can only exist here, because this is the
only crate that names every module. A module renders its own failures through a
smaller composite of its catalog and `erp_web::CATALOG`, because it cannot name
its siblings and has no reason to.

`docs/ERRORS.md` is generated from this one, and the completeness audit runs
against it, so a code missing from any part is a failing build.

## What a client can rely on

- Every error is `application/problem+json` with a stable `code`. **Branch on the
  code, never on `detail`**, which is prose in whatever language was asked for.
  `docs/ERRORS.md` lists every one.
- `Accept-Language` is honoured on every response, including failures.
- The OpenAPI document describes every route and is generated from the router
  that serves them.
- Tenants are subdomains. No path carries a tenant name.

## What is not here yet

`ETag`/`If-Match`. It needs a conflict real enough to shape it.

## The API process

[`bin/api.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-api/src/bin/api.rs). Environment it reads:

| Variable | Required | What it does |
|---|---|---|
| `CONTROL_DATABASE_URL` | yes | The core database |
| `PRIMARY_CLUSTER_URL` | yes | Where tenant databases live |
| `PRIMARY_REPLICA_URL` | no | Reads that tolerate lag route here |
| `BIND` | no | Default `0.0.0.0:8080` |
| `PUBLIC_DOMAIN` | no | Default `localhost`, so `acme.localhost` works with no DNS |
| `REDIS_URL` | no | Shared sessions and cross-node invalidation |
| `SEALING_KEY` | no | `<id>:<64 hex>[,<id>:<64 hex>…]`; the first seals, the rest are read during a rotation. Without it, storing a tenant secret refuses |

Three layers, and nothing else:

```rust
router(state)
    .layer(TraceLayer::new_for_http())
    .layer(TimeoutLayer::with_status_code(GATEWAY_TIMEOUT, Duration::from_secs(30)))
    .layer(RequestBodyLimitLayer::new(1 << 20))
```

The timeout answers 504 and not 408, because the request was fine and the server
was slow.

**There is no rate limit here**, and signup is unauthenticated by design. That is
still a gap, but a smaller one than it was: `POST /v1/signups` builds nothing
until the address answers, so a call that gets past validation costs one row and
one email instead of a database. `REQUEST_INTERVAL` caps that per address. The
per-caller limiter is Phase 12c's, and the roadmap chapter says where it sits.

Generate a sealing key with:

```bash
openssl rand -hex 32
```

Prefix it with an id, `2026-09:<hex>`. The id is stored beside every row it
seals, a row is opened with the key it names or refused, and `migrator reseal`
moves rows onto a new first key. RUNNING has the rotation procedure.
