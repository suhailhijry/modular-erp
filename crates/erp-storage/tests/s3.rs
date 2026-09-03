//! The S3 engine, against a real bucket.
//!
//! # Why this is not a mock
//!
//! Everything that can go wrong in `erp_storage::s3` is on the wire: a `SigV4`
//! signature computed over the wrong canonical request, a checksum header the
//! provider rejects, path-style addressing that resolves to a bucket that is
//! not there. None of it is testable against something that agrees with you,
//! which is what a mock does by construction.
//!
//! So this talks to `MinIO`, which speaks the same protocol as Hetzner, Contabo
//! and Amazon:
//!
//! ```text
//! docker compose up -d minio createbucket
//! S3_BUCKET=documents S3_ENDPOINT=http://localhost:9000 S3_REGION=us-east-1 \
//!   S3_ACCESS_KEY_ID=minioadmin S3_SECRET_ACCESS_KEY=minioadmin \
//!   S3_ALLOW_HTTP=true cargo test -p erp-storage --test s3
//! ```
//!
//! # Why it skips rather than failing when nothing is configured
//!
//! The same call the database tests make. A developer with no Docker running
//! should not be unable to run the suite — and the skip says so out loud rather
//! than reporting a pass it did not earn.
//!
//! What this does **not** prove is Hetzner or Contabo specifically. `MinIO` is a
//! faithful S3 implementation and not those two, and neither vendor's
//! exceptions — Contabo's Kong gateway answering JSON instead of S3 XML,
//! Hetzner's documented `CopyObject` caveat — is reachable from here.

#![allow(clippy::expect_used, clippy::panic)]

use erp_storage::{S3, Storage, StorageError};

/// The engine this run is pointed at, or a printed skip.
fn engine() -> Option<S3> {
    match S3::from_env() {
        Ok(Some(s3)) => Some(s3),
        Ok(None) => {
            eprintln!("skipped: S3_BUCKET is not set; see this file's docs");
            None
        }
        Err(why) => panic!("S3 is configured and not usable: {why}"),
    }
}

/// A prefix nothing else in this run will touch.
fn key(name: &str) -> String {
    format!(
        "test/{}-{}/{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )
}

#[tokio::test]
async fn bytes_go_in_and_come_back_out_of_a_bucket() {
    let Some(s3) = engine() else { return };
    let key = key("a.txt");

    let stored = erp_storage::store(&s3, &key, b"hello", "text/plain")
        .await
        .expect("stores");
    assert_eq!(stored.engine, "s3");
    assert_eq!(stored.size, 5);

    let back = erp_storage::fetch(&s3, &stored).await.expect("fetches");
    assert_eq!(back, b"hello");

    s3.delete(&key).await.expect("deletes");
    assert_eq!(s3.get(&key).await, Err(StorageError::NoSuchFile));
    // And again, because deleting twice is the same world either way.
    s3.delete(&key).await.expect("deletes");
}

/// **The signature over a payload, not just over headers.** A digest that
/// streamed its input wrongly would pass on five bytes and fail on a real
/// document, which is the size of thing this system actually stores.
#[tokio::test]
async fn a_document_sized_upload_round_trips() {
    let Some(s3) = engine() else { return };
    let key = key("big.bin");

    // Not all one byte: a run-length-friendly payload would hide a chunking
    // bug that a varied one exposes.
    let bytes: Vec<u8> = (0..4_000_000u32)
        .map(|i| u8::try_from(i % 251).unwrap_or_default())
        .collect();

    let stored = erp_storage::store(&s3, &key, &bytes, "application/octet-stream")
        .await
        .expect("stores");
    assert_eq!(stored.size, 4_000_000);

    let back = erp_storage::fetch(&s3, &stored).await.expect("fetches");
    assert_eq!(back, bytes);

    s3.delete(&key).await.expect("deletes");
}

/// The failure a checksum exists to catch, against a bucket rather than a disk:
/// something else wrote over the object.
#[tokio::test]
async fn a_file_that_came_back_different_is_refused() {
    let Some(s3) = engine() else { return };
    let key = key("overwritten.txt");

    let stored = erp_storage::store(&s3, &key, b"the real thing", "text/plain")
        .await
        .expect("stores");
    s3.put(&key, b"something else").await.expect("overwrites");

    assert!(matches!(
        erp_storage::fetch(&s3, &stored).await,
        Err(StorageError::Corrupt { .. })
    ));

    s3.delete(&key).await.expect("deletes");
}

#[tokio::test]
async fn a_key_nothing_wrote_is_not_found_rather_than_unavailable() {
    let Some(s3) = engine() else { return };
    assert_eq!(
        s3.get(&key("never-written")).await,
        Err(StorageError::NoSuchFile)
    );
}

/// Keys nest, and a key with a `/` in it is a path in the bucket rather than a
/// name with a slash in it. `Local` puts the same key in a directory tree; both
/// have to accept the same string.
#[tokio::test]
async fn a_nested_key_is_a_path_in_the_bucket() {
    let Some(s3) = engine() else { return };
    let key = key("invoice/INV-1/a1b2c3.pdf");

    let stored = erp_storage::store(&s3, &key, b"%PDF-1.4", "application/pdf")
        .await
        .expect("stores");
    assert_eq!(
        erp_storage::fetch(&s3, &stored).await.as_deref(),
        Ok(&b"%PDF-1.4"[..])
    );
    s3.delete(&key).await.expect("deletes");
}
