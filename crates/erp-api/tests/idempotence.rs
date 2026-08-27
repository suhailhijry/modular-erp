//! **A write path does not mint its own identity.**
//!
//! Law L8: every mutation is idempotent under retry. The mechanism is that a
//! mutation's identity comes from the caller — an invoice carries the key the
//! client sent, a payment carries the bank's reference — so a retry carries the
//! same identity as the attempt it repeats and the log's
//! `UNIQUE (stream_domain, stream_id, sequence)` refuses the second write.
//!
//! That holds only while no handler generates an id of its own. One that does
//! makes its own retries indistinguishable from new requests, and the database
//! has nothing left to refuse. For `POST /v1/sales/invoices/{invoice}/payments`
//! that is taking the money twice — which is the reason this file exists and not
//! a hypothetical.
//!
//! # Where randomness *is* allowed
//!
//! `erp-eventlog`'s secret sealing needs a nonce, and it is not a write path in
//! this sense — nothing about a sealed secret is addressed by that value.
//! Projections must derive keys from the position instead (`ctx.derive_id`),
//! which L2 requires and shadow replay checks; the comments in each
//! `projections.rs` saying so are why this test strips comments before matching.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// Ways to conjure an identity that a retry cannot reproduce.
const MINTS: [&str; 5] = [
    "Uuid::new_v4",
    "Uuid::now_v7",
    "rand::",
    "thread_rng",
    "OsRng",
];

/// Files that make up a request's write path.
const WRITE_PATHS: [&str; 2] = ["http.rs", "commands.rs"];

#[test]
fn no_write_path_mints_its_own_identity() {
    let sources = write_paths();

    // Without this the test passes for the wrong reason if the layout changes.
    assert!(
        sources.len() >= 8,
        "found {} write-path files; expected at least the four modules' http.rs \
         and commands.rs. The scan is broken, not the code.",
        sources.len()
    );

    let mut offenders = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let relative = relative(path);
        for (number, line) in text.lines().enumerate() {
            // Comments explaining why *not* to do this are the common case.
            let code = line.split("//").next().unwrap_or(line);
            if MINTS.iter().any(|m| code.contains(m)) {
                offenders.push(format!("{relative}:{}  {}", number + 1, code.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a write path generates its own identity (L8).\n\n\
         A retry then arrives as a new request, the log's uniqueness constraint \
         has nothing to refuse, and the mutation happens twice. Take the identity \
         from the caller — a key in the body, or the id already in the path.\n\n  {}",
        offenders.join("\n  ")
    );
}

fn relative(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above this crate")
        .to_path_buf()
}

fn write_paths() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut found = Vec::new();
    let mut stack = vec![root.join("crates"), root.join("modules")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "target" || n == "tests")
                {
                    continue;
                }
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|n| WRITE_PATHS.contains(&n.to_string_lossy().as_ref()))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
