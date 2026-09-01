//! **An aggregate is loaded only while handling a command.**
//!
//! Law L7: *reads are served by projections. Event sourcing is a write model and
//! a rebuild mechanism, not a query engine.*
//!
//! The law was stated and nothing enforced it, so two read paths had grown into
//! it — `tax_sa`'s onboarding-status endpoint and the worker's certificate-expiry
//! check both loaded the `Onboarding` aggregate to answer a question. Both were
//! correct and both were the wrong shape: a renewal **appends** another
//! `CsidIssued`, so the cost of answering "which environment is this tenant on"
//! grew with the number of certificates ever issued, to return one row's worth
//! of answer. They now read `proj_tax_sa.onboarding`.
//!
//! # What this permits
//!
//! Command handling, and nothing else. A command must load its aggregate — that
//! is the write model doing its job, and it is the only place a decision is made
//! from history rather than from state.
//!
//! # Why a file allowlist rather than something cleverer
//!
//! Because the convention is already file-shaped: every module puts its command
//! handling in `commands.rs`. A rule that matches how the code is actually laid
//! out is one people can follow without being told twice.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// The ways an aggregate gets loaded.
const LOADS: [&str; 3] = ["erp_eventlog::load", "aggregate::load", "load_since"];

/// Paths that may. Matched as suffixes.
const ALLOWED: [&str; 5] = [
    // Command handling. This is the whole point of the write model.
    "modules/crm/src/commands.rs",
    "modules/ledger/src/commands.rs",
    "modules/sales/src/commands.rs",
    "modules/purchases/src/commands.rs",
    "modules/tax_sa/src/commands.rs",
];

#[test]
fn an_aggregate_is_loaded_only_while_handling_a_command() {
    let sources = sources();

    // Without this the test passes for the wrong reason if the walk breaks.
    assert!(
        sources.len() > 50,
        "scanned only {} files; the walk is broken, not the code",
        sources.len()
    );
    let mut allowed_seen = 0;

    let mut offenders = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let relative = relative(path);
        let permitted = ALLOWED.iter().any(|ok| relative.ends_with(ok));

        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if !LOADS.iter().any(|l| code.contains(l)) {
                continue;
            }
            if permitted {
                allowed_seen += 1;
                continue;
            }
            offenders.push(format!("{relative}:{}  {}", number + 1, code.trim()));
        }
    }

    // The allowlist must still be describing something real. If every command
    // handler stopped loading aggregates, this rule would be enforcing nothing
    // and should be deleted rather than left to look like protection.
    assert!(
        allowed_seen > 0,
        "no aggregate is loaded anywhere, including in command handling. \
         Either the scan is broken or L7 no longer describes this system."
    );

    assert!(
        offenders.is_empty(),
        "an aggregate is loaded outside command handling (L7).\n\n\
         Reads are served by projections. Loading an aggregate to answer a query \
         makes the cost of that answer grow with the length of the stream, which \
         is exactly what a read model exists to stop. Add or extend a projection \
         and read that instead.\n\n  {}",
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

/// Every `.rs` file outside `erp-eventlog` itself and outside test code.
///
/// This crate defines `load`, so it necessarily names it. Tests load aggregates
/// to assert on them, which is not a production read path.
fn sources() -> Vec<PathBuf> {
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
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name == "target" || name == "tests" || name == "erp-eventlog" {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
