//! Documents, against a real tenant and a real disk.
//!
//! The test that carries this file is
//! [`a_document_that_came_back_different_is_refused`] — the checksum is the
//! reason a key is worth recording at all, and a `500` is the only honest
//! answer to a document that is not the document.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_storage::{Local, Storage, StorageError};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};
use files::{Owner, OwnerKind};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn on(day: &str) -> Timestamp {
    format!("{day}T09:00:00Z").parse().expect("a valid instant")
}

fn invoice(id: &str) -> Owner {
    Owner {
        kind: OwnerKind::Invoice,
        id: code(id),
    }
}

struct Fixture {
    db: TenantDb,
    pool: sqlx::PgPool,
    storage: Local,
    root: std::path::PathBuf,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    database: String,
}

impl Fixture {
    async fn new(slug: &str) -> Self {
        let control_db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("the test database URL parses");
        let control = Arc::new(ControlPlane::new(
            control_db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ));
        control
            .register_cluster(
                "primary",
                "ERP_CLUSTER_PRIMARY_URL",
                None,
                10_000,
                10_000,
                Actor::system(),
            )
            .await
            .expect("cluster registers");

        let tenant = control
            .register_tenant_on(slug, "Salon", "primary", Actor::system())
            .await
            .expect("tenant registers");
        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("tenant database is created");
        control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("tenant activates");

        let db = control
            .enter_for_maintenance(tenant.id)
            .await
            .expect("maintenance entry");
        {
            let mut conn = db.acquire().await.expect("connection");
            files::install(&mut conn).await.expect("files");
            ensure_group_schema::<files::Files>(&mut conn)
                .await
                .expect("checkpoint");
        }

        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
        let pool = sqlx::PgPool::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects");

        let mut root = std::env::temp_dir();
        root.push(format!("erp-files-{slug}-{}", std::process::id()));

        Self {
            db,
            pool,
            storage: Local::at(&root),
            root,
            _control: control,
            _control_db: control_db,
            database: tenant.database_name,
        }
    }

    async fn project(&self) {
        let owned = files::projections();
        let refs: Vec<&dyn Projection<Group = files::Files>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<files::Files>(&self.pool, &refs, files::upcasters(), 200)
            .await
            .expect("projects");
    }

    /// Stores bytes and records them, in the order a handler does it.
    async fn upload(&self, id: &str, owner: &Owner, name: &str, bytes: &[u8]) {
        let key = files::key_for(owner, id);
        let stored = erp_storage::store(&self.storage, &key, bytes, "application/pdf")
            .await
            .expect("stores");

        files::attach(
            &self.db,
            &code(id),
            name,
            owner,
            &stored,
            on("2026-05-01"),
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{id} attaches: {e:?}"));
        self.project().await;
    }

    async fn attached(&self, owner: &Owner) -> Vec<files::Attachment> {
        let mut conn = self.db.acquire().await.expect("connection");
        files::attached_to(&mut conn, owner.kind, owner.id.as_str(), 100)
            .await
            .expect("reads")
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = tokio::fs::remove_dir_all(&self.root).await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// A document goes on, comes back byte for byte, and is listed against what it
/// belongs to.
#[tokio::test]
async fn a_document_is_attached_and_comes_back_unchanged() {
    let fixture = Fixture::new("attach").await;
    let owner = invoice("INV-1");

    fixture
        .upload("DOC-1", &owner, "عقد موقّع.pdf", b"%PDF-1.7 signed")
        .await;

    let found = fixture.attached(&owner).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "عقد موقّع.pdf");
    assert_eq!(found[0].owner_id, "INV-1");
    assert_eq!(found[0].stored.engine, "local");
    assert_eq!(found[0].stored.media_type, "application/pdf");
    // **The key, not a URL.** See `files::file`.
    assert_eq!(found[0].stored.key, "invoice/INV-1/DOC-1");

    let back = erp_storage::fetch(&fixture.storage, &found[0].stored)
        .await
        .expect("fetches");
    assert_eq!(back, b"%PDF-1.7 signed");

    // Attached to something else, and it does not appear here.
    fixture
        .upload("DOC-2", &invoice("INV-2"), "other.pdf", b"x")
        .await;
    assert_eq!(fixture.attached(&owner).await.len(), 1);

    fixture.cleanup().await;
}

/// **The failure a checksum exists to catch.**
///
/// Something wrote over the object. A document that comes back different from
/// what was stored is a failure, not a warning — handing somebody a contract
/// that is not the contract is worse than handing them nothing.
#[tokio::test]
async fn a_document_that_came_back_different_is_refused() {
    let fixture = Fixture::new("corrupt").await;
    let owner = invoice("INV-1");
    fixture
        .upload("DOC-1", &owner, "contract.pdf", b"the signed one")
        .await;

    let record = fixture.attached(&owner).await.remove(0);
    fixture
        .storage
        .put(&record.stored.key, b"something else entirely")
        .await
        .expect("overwrites");

    assert!(
        matches!(
            erp_storage::fetch(&fixture.storage, &record.stored).await,
            Err(StorageError::Corrupt { .. })
        ),
        "a document that changed underneath us was handed back"
    );

    fixture.cleanup().await;
}

/// Taking a document off leaves the bytes alone.
///
/// A document that was on an invoice is part of what happened, and erasing it
/// on a click would erase evidence.
#[tokio::test]
async fn taking_a_document_off_does_not_erase_it() {
    let fixture = Fixture::new("detach").await;
    let owner = invoice("INV-1");
    fixture
        .upload("DOC-1", &owner, "contract.pdf", b"the signed one")
        .await;
    let record = fixture.attached(&owner).await.remove(0);

    files::detach(
        &fixture.db,
        &code("DOC-1"),
        "أُرفق بالفاتورة الخطأ",
        on("2026-05-02"),
        &Metadata::default(),
    )
    .await
    .expect("detaches");
    fixture.project().await;

    assert!(
        fixture.attached(&owner).await.is_empty(),
        "it is off the invoice"
    );

    // The record survives, and says why.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let still = files::attachment(&mut conn, "DOC-1")
        .await
        .expect("reads")
        .expect("the record is still there");
    assert!(still.removed_at.is_some());
    assert_eq!(still.removed_why.as_deref(), Some("أُرفق بالفاتورة الخطأ"));
    drop(conn);

    // And so do the bytes.
    assert_eq!(
        erp_storage::fetch(&fixture.storage, &record.stored)
            .await
            .expect("still there"),
        b"the signed one"
    );

    // Taking it off again is nothing, not an error.
    let again = files::detach(
        &fixture.db,
        &code("DOC-1"),
        "",
        on("2026-05-03"),
        &Metadata::default(),
    )
    .await
    .expect("is not an error");
    assert!(again.did_nothing());

    fixture.cleanup().await;
}

/// The same upload twice records one document (L8).
#[tokio::test]
async fn uploading_the_same_document_twice_records_it_once() {
    let fixture = Fixture::new("retry").await;
    let owner = invoice("INV-1");

    for _ in 0..3 {
        fixture
            .upload("DOC-1", &owner, "contract.pdf", b"the signed one")
            .await;
    }

    assert_eq!(fixture.attached(&owner).await.len(), 1);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM event")
        .fetch_one(&fixture.pool)
        .await
        .expect("counts");
    assert_eq!(events, 1, "three uploads, one event");

    fixture.cleanup().await;
}

/// **A rebuild reproduces the table and touches no file.**
///
/// L2 for a module whose whole subject lives outside the database: the bytes
/// are in an engine and the record is derived from the log, and replaying the
/// log must give the same record without going near storage.
#[tokio::test]
async fn a_rebuild_reproduces_the_records_without_reading_a_file() {
    let fixture = Fixture::new("rebuild").await;
    let owner = invoice("INV-1");
    fixture
        .upload("DOC-1", &owner, "contract.pdf", b"the signed one")
        .await;
    fixture
        .upload("DOC-2", &owner, "amendment.pdf", b"the amendment")
        .await;
    files::detach(
        &fixture.db,
        &code("DOC-2"),
        "superseded",
        on("2026-05-02"),
        &Metadata::default(),
    )
    .await
    .expect("detaches");
    fixture.project().await;

    // Storage is taken away entirely. A replay that needed it would fail here.
    let _ = tokio::fs::remove_dir_all(&fixture.root).await;

    let projections = files::projections();
    let refs: Vec<&dyn Projection<Group = files::Files>> =
        projections.iter().map(AsRef::as_ref).collect();
    let report = erp_projection::replay_shadow::<files::Files>(
        &fixture.pool,
        &refs,
        files::upcasters(),
        500,
    )
    .await
    .expect("replays");

    assert!(
        report.is_reproducible(),
        "a rebuild does not reproduce what is live: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}
