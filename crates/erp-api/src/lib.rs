//! The HTTP surface, and the one place that composes it.
//!
//! # What is here and what is not
//!
//! **Here:** the core's own routes — sessions, the tenant, members,
//! invitations, signing up, and turning modules on and off. A tenant cannot
//! disable any of it, which is what makes it core.
//!
//! **In the modules:** everything else. `sales::http::routes()` is a router the
//! sales crate owns, next to the aggregates and the read models it serves;
//! [`modules`] mounts it. Four route files used to live in this crate, which
//! meant a module's HTTP surface was written by the composition root and a
//! module could not be read in one place.
//!
//! **In [`erp_web`]:** what those routers are built *from* — extractors,
//! problem+json, the JSON and query rejections, paging. Below the modules,
//! because a module has to be able to name it.
//!
//! # What a handler cannot do
//!
//! Reach the wrong tenant. A handler that touches tenant data takes
//! [`erp_web::Tenant`], whose only constructor is `ControlPlane::enter` — so
//! "did we check the membership?" is answered by the signature rather than by
//! reading the body.
//!
//! # What a client can rely on
//!
//! - Every error is `application/problem+json` with a stable `code`. Branch on
//!   the code, never on `detail`, which is prose in whatever language was asked
//!   for. `docs/ERRORS.md` lists every one.
//! - `Accept-Language` is honoured on every response, including failures.
//! - The `OpenAPI` document at `GET /v1/openapi.json` — also `docs/openapi.json` —
//!   describes every route, and is generated from the router that serves them
//!   rather than written alongside it. See [`routes`].
//!
//! # What is not here yet
//!
//! `Idempotency-Key` and `ETag`/`If-Match`. Writes are already idempotent on a
//! client-chosen id, which is most of what the first buys; the other needs a
//! conflict real enough to shape it.

mod catalog;
mod invitations;
mod members;
mod modules;
mod routes;
mod signup;

pub use catalog::CATALOG;

/// Re-exported so a caller wiring up a server needs one crate, not two.
pub use erp_web::{AppState, Problem};
pub use modules::available as modules;
pub use routes::{openapi, router};
