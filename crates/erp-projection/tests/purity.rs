//! **A projection writes. It does not read.**
//!
//! # Why this file exists while the rule is still true
//!
//! L3 bans reading *another* group — enforced, because the projection
//! transaction sets `search_path` to one schema and a cross-group query fails
//! with "relation does not exist". Nothing bans reading your *own* group, and
//! that is the gap this closes.
//!
//! The cost is measured, not assumed. A Laravel/Spatie projector doing the same
//! job replays at roughly **69 events/sec**, against ~4,100 here
//! (`tests/rebuild_throughput.rs`). Two structural differences account for it,
//! and one of them is this: that projector declares five other projections it
//! reads, so every event pays six or more lookups before it writes anything.
//! Its own comments carry the second cost — rebuilt in the same pass, it reads
//! those projections while they are empty, which needed a bespoke check to
//! police rebuild ordering.
//!
//! A read inside `apply` buys all of that:
//!
//! - **N+1 per event**, paid again on every rebuild, forever;
//! - an **ordering constraint** between projections that nothing declares;
//! - a silent dependence on rows that are absent mid-replay, so the rebuild
//!   produces different output than the live run did — which is L2 lost.
//!
//! There are currently **zero** such reads. A rule that is already true costs
//! nothing to enforce and a rewrite to retrofit, so it is enforced here.
//!
//! # What to do instead
//!
//! Everything a projection needs must arrive **in the event**. That is L5:
//! events carry resolved outcomes, never references to be looked up later. If a
//! projection wants a name, a rate or a branch, the command that emitted the
//! event resolves it and writes it into the payload. Reads belong on the query
//! side, where `projections.rs` already keeps 34 of them.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// sqlx's read terminals. Every read ends in one of these; `execute` does not.
const READS: [&str; 4] = ["fetch_one", "fetch_optional", "fetch_all", ".fetch("];

/// **No projection reads the database while applying an event.**
///
/// Scans the body of every `async fn apply` in the workspace — the one method
/// on `Projection` that runs per event — and fails on any sqlx read terminal
/// inside it.
#[test]
fn a_projection_does_not_read_while_applying() {
    let mut offenders = Vec::new();
    let mut scanned = Vec::new();

    for path in projection_sources() {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let relative = relative(&path);
        for (first, last) in apply_bodies(&text) {
            scanned.push(relative.clone());
            for (offset, line) in text.lines().enumerate().take(last).skip(first) {
                let code = line.split("//").next().unwrap_or(line);
                if READS.iter().any(|read| code.contains(read)) {
                    offenders.push(format!("{relative}:{}  {}", offset + 1, code.trim()));
                }
            }
        }
    }

    // Without this the test passes for the wrong reason the moment the scan
    // stops finding anything — a rename of `apply`, a reformat that moves the
    // closing brace, a walk that no longer reaches `modules/`.
    for required in [
        "modules/ledger/src/projections.rs",
        "modules/sales/src/projections.rs",
        "modules/purchases/src/projections.rs",
        "modules/tax_sa/src/projections.rs",
        "modules/tax_sa/src/documents.rs",
    ] {
        assert!(
            scanned.iter().any(|f| f == required),
            "found no `apply` body in {required}, so this test proved nothing. \
             The scan is broken, not the code."
        );
    }

    assert!(
        offenders.is_empty(),
        "a projection read the database while applying an event.\n\n\
         This is an N+1 paid on every event and again on every rebuild, and it \
         makes the projection depend on rows that may not exist yet mid-replay \
         — which is L2 lost. Put what it needs into the event instead (L5): the \
         command resolves it, the payload carries it. Reads belong on the query \
         side.\n\n  {}",
        offenders.join("\n  ")
    );
}

/// Line ranges (half-open, zero-based) of every `async fn apply` body.
///
/// Relies on `cargo fmt` closing a trait method at four spaces, which is
/// checked in CI and asserted above by requiring the known bodies to be found.
fn apply_bodies(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut bodies = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].contains("async fn apply") && lines[i].contains('(') {
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && lines[end] != "    }" {
                end += 1;
            }
            bodies.push((start, end));
            i = end;
        }
        i += 1;
    }
    bodies
}

fn relative(path: &Path) -> String {
    let root = workspace_root();
    path.strip_prefix(&root)
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

/// Every `.rs` file that implements `Projection`.
fn projection_sources() -> Vec<PathBuf> {
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
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                // Not this file: it names every pattern it hunts for.
                if path.file_name().is_some_and(|n| n == "purity.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                if text.contains("impl Projection for") {
                    found.push(path);
                }
            }
        }
    }
    found.sort();
    found
}
