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
//! # What to do instead
//!
//! Everything a projection needs must arrive **in the event**. That is L5:
//! events carry resolved outcomes, never references to be looked up later. If a
//! projection wants a name, a rate or a branch, the command that emitted the
//! event resolves it and writes it into the payload. Reads belong on the query
//! side, where `projections.rs` already keeps 34 of them.
//!
//! # The one shape that L5 cannot reach, and what it has to declare
//!
//! A **report module subscribes to other modules' events** (Phase 10), and it
//! cannot put anything into them: `sales.invoice.cancelled` carries the credit
//! note and not the invoice's amounts — rightly, because they have not changed
//! — and `pos.shift.sold` carries tenders and not the operator, because the
//! operator has not changed since the shift opened. A report that nets credits
//! off, or groups takings by person, has to have remembered.
//!
//! So it keeps a **working table in its own group**, and reads back what an
//! earlier event in the same replay wrote. That costs (a) — one indexed lookup
//! on a subset of events — and costs neither (b) nor (c): the row is written by
//! the same projection, earlier in log order, so a rebuild reproduces the live
//! run exactly. `the_demo_replays_to_exactly_what_is_live` is what holds that
//! down.
//!
//! Such a read must say so, on the line above it:
//!
//! ```text
//! // projection-read: `invoiced`, written by this projection on `Issued`.
//! ```
//!
//! Undeclared reads still fail. The marker is not an escape hatch — it is the
//! sentence somebody has to write and somebody else has to read in review.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// sqlx's read terminals. Every read ends in one of these; `execute` does not.
const READS: [&str; 4] = ["fetch_one", "fetch_optional", "fetch_all", ".fetch("];

/// How far above a read the declaration may sit.
///
/// A read terminal is the last line of a builder chain that often starts twenty
/// lines earlier, and the comment belongs at the top of it where somebody
/// reading the query will see it.
const DECLARATION_WINDOW: usize = 24;

/// **No projection reads the database while applying an event.**
///
/// Scans the body of every `async fn apply` in the workspace — the one method
/// on `Projection` that runs per event — **and the bodies of the helpers it
/// calls by name in the same file**, which is where three of the four reads in
/// `reports` live. One hop, not a call graph: a read two helpers deep would slip
/// through, and the day one exists is the day this grows a second hop.
#[test]
fn a_projection_does_not_read_while_applying() {
    let mut offenders = Vec::new();
    let mut scanned = Vec::new();
    let mut declarations = 0;

    for path in projection_sources() {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let relative = relative(&path);
        let lines: Vec<&str> = text.lines().collect();
        for (first, last) in apply_paths(&text) {
            scanned.push(relative.clone());
            for (offset, line) in lines.iter().enumerate().take(last).skip(first) {
                let code = line.split("//").next().unwrap_or(line);
                if !READS.iter().any(|read| code.contains(read)) {
                    continue;
                }
                if declared(&lines, offset) {
                    declarations += 1;
                    continue;
                }
                offenders.push(format!("{relative}:{}  {}", offset + 1, code.trim()));
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

    // **The helper hop has to still work.** Every declared read below lives in
    // a named function rather than inline, so if `apply_paths` stops following
    // them this count collapses to zero and the test starts passing for the
    // wrong reason — the failure mode this file already guards against for
    // `apply` bodies.
    assert!(
        declarations >= 8,
        "expected at least eight declared working-table reads, found {declarations}. \
         The helper scan is broken, not the code."
    );

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

/// Whether the read on `offset` says why it is there.
///
/// See the module docs: a report module's working-table read is the one shape
/// L5 cannot reach, and it has to be declared rather than assumed.
fn declared(lines: &[&str], offset: usize) -> bool {
    lines
        .iter()
        .take(offset)
        .rev()
        .take(DECLARATION_WINDOW)
        .any(|line| line.contains("projection-read:"))
}

/// Every `apply` body, plus the body of every free function it names.
///
/// The helper hop is what makes the scan honest about a module that keeps its
/// query in a named function rather than inline — which is most of them, and all
/// of `reports`.
fn apply_paths(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut ranges = apply_bodies(text);
    let mut reached: Vec<String> = Vec::new();

    // To a fixed point, because a helper calls a helper: `tax_sa`'s chain link
    // is two hops from `apply`, and stopping at one would mean declaring a read
    // this scan never looks at — a marker nobody is holding to anything.
    loop {
        let mut grew = false;
        for (name, range) in helpers(text) {
            if reached.contains(&name) {
                continue;
            }
            let called = ranges.iter().any(|&(first, last)| {
                lines[first..last.min(lines.len())]
                    .iter()
                    .any(|line| calls(line, &name))
            });
            if called {
                reached.push(name);
                ranges.push(range);
                grew = true;
            }
        }
        if !grew {
            return ranges;
        }
    }
}

/// Whether a line calls `name`, on a word boundary.
///
/// `contains` alone reads `add_revenue(` as a call to `revenue`, which pulled
/// this module's public query functions into the scan — and those are exactly
/// the reads that are supposed to exist.
fn calls(line: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    let mut from = 0;
    while let Some(at) = line[from..].find(&needle) {
        let at = from + at;
        let before = line[..at].chars().next_back();
        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Every free `async fn` in the file, as `(name, body range)`.
///
/// Relies on `cargo fmt` closing a free function at column zero, the way
/// [`apply_bodies`] relies on it closing a trait method at four spaces.
fn helpers(text: &str) -> Vec<(String, (usize, usize))> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("async fn ") || line.starts_with("pub async fn ") {
            let Some(name) = line
                .split("async fn ")
                .nth(1)
                .and_then(|rest| rest.split(['(', '<']).next())
            else {
                i += 1;
                continue;
            };
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && lines[end] != "}" {
                end += 1;
            }
            found.push((name.to_owned(), (start, end)));
            i = end;
        }
        i += 1;
    }
    found
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
