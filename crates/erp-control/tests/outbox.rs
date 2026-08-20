//! The control plane's outbox, and the thing it must keep being.
//!
//! `erp_eventlog`'s `Dispatcher` and `enqueue` are compile-time-checked against
//! a table named `outbox` with a particular set of columns. The control plane
//! reuses every line of them — claim under `SKIP LOCKED`, leases, backoff, dead
//! letters, the at-least-once idempotency key — and pays for it with one
//! obligation: `migrations/control/0008_outbox.sql` must not drift from
//! `migrations/tenant/0003_outbox.sql`.
//!
//! Nothing in the compiler can see that obligation. Both files are raw SQL run
//! against different databases, and sqlx validates its queries against **one**
//! type-check database where the two are the same table. A column added to one
//! chain and not the other would type-check perfectly and fail at runtime, in
//! whichever plane was touched second.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use erp_testkit::{Schema, TestDb};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

/// A column, as `information_schema` describes it.
type Column = (String, String, String, Option<String>);

async fn columns(db: &TestDb) -> Vec<Column> {
    sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'outbox'
          ORDER BY column_name",
    )
    .fetch_all(db.pool())
    .await
    .expect("reads the columns")
}

async fn constraints(db: &TestDb) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT conname, pg_get_constraintdef(oid)
           FROM pg_constraint
          WHERE conrelid = 'public.outbox'::regclass
          ORDER BY conname",
    )
    .fetch_all(db.pool())
    .await
    .expect("reads the constraints")
}

async fn fresh(schema: &'static Schema) -> TestDb {
    erp_testkit::Template::get(schema)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clones")
}

/// **The two outboxes are the same table, and this is what keeps them that way.**
#[tokio::test]
async fn the_two_outboxes_are_the_same_table() {
    let control = fresh(&CONTROL).await;
    let tenant = fresh(&TENANT).await;

    let control_columns = columns(&control).await;
    let tenant_columns = columns(&tenant).await;

    assert!(
        !tenant_columns.is_empty(),
        "the tenant chain has no `outbox`, so this test is comparing nothing"
    );
    assert_eq!(
        control_columns, tenant_columns,
        "the two `outbox` tables have drifted. `erp_eventlog`'s queries are \
         checked against whichever one `just prepare` loads first, so this \
         difference is a runtime failure in the other plane, not a build one."
    );

    // Constraint *names* are part of it too: a check the tenant table enforces
    // and the control one does not is a row the dispatcher can produce in one
    // plane and not the other.
    assert_eq!(constraints(&control).await, constraints(&tenant).await);
}

/// The control plane can enqueue and the dispatcher can find it.
///
/// The narrow thing this proves that the schema comparison does not: `enqueue`
/// with **no causing log position** works. The control plane has no event log,
/// so every effect here must pin its own idempotency key, and a caller that
/// forgot would get `EnqueueError::NoKey` rather than a silent nothing.
#[tokio::test]
async fn the_control_plane_can_promise_an_effect_without_an_event_log() {
    let control = fresh(&CONTROL).await;
    let mut conn = control.pool().acquire().await.expect("connection");

    let effect = erp_eventlog::Effect::with_key(
        erp_types::EffectKind::new("email.send").expect("a kind"),
        "invitation:abc",
        serde_json::json!({ "to": "sara@example.test" }),
    );

    let written = erp_eventlog::enqueue(&mut conn, None, std::slice::from_ref(&effect))
        .await
        .expect("enqueues with no causing position");
    assert_eq!(written, 1);

    // Pinned, so the same promise twice is one row. That is what makes a
    // retried request send one email.
    let again = erp_eventlog::enqueue(&mut conn, None, &[effect])
        .await
        .expect("enqueues");
    assert_eq!(again, 0, "a pinned key deduplicated into a second email");

    // And an effect with no key at all is refused rather than dropped, because
    // there is no position here to derive one from.
    let keyless = erp_eventlog::Effect::new(
        erp_types::EffectKind::new("email.send").expect("a kind"),
        serde_json::json!({}),
    );
    assert!(
        erp_eventlog::enqueue(&mut conn, None, &[keyless])
            .await
            .is_err(),
        "an unkeyed effect in the control plane has nothing to derive a key from"
    );
}
