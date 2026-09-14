//! **An audit entry commits with the change it records, or not at all.**
//!
//! `ControlPlane::record` ran on the pool until 2026-09-14, after each caller's
//! own commit, so a crash between the two left an act that stood — a tenant
//! suspended, a key issued, a member removed — with no record that it had
//! happened. That is the one shape an audit trail must not have, and a
//! trail's tests cannot catch it: every one of them runs to completion.
//!
//! `record` now takes the connection the change was made on, and every writer
//! passes its transaction. The two acts with no control-plane write of their
//! own — support entering a tenant, and a confirmed signup whose build is many
//! transactions with their own entries — acquire a connection and say
//! `audit-only:` beside it, with why. This scan is what stops a third from
//! appearing quietly: a `record` handed anything but a transaction, with no
//! such comment, fails the build.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// How far above a `record` call the `audit-only:` comment may sit.
const REACH: usize = 8;

#[test]
fn an_audit_entry_is_written_on_the_transaction_of_its_change() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sites = 0;
    let mut offenders = Vec::new();

    for path in sources(&root) {
        let text = std::fs::read_to_string(&path).expect("a source file reads");
        let lines: Vec<&str> = text.lines().collect();
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        for (number, line) in lines.iter().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            // The call, not the definition and not a doc comment naming it.
            if !code.contains(".record(") || code.contains("fn record(") {
                continue;
            }
            sites += 1;

            // The first argument: on this line after the paren, or on the next
            // non-empty one — `rustfmt` puts it on the next line.
            let rest = code.split(".record(").nth(1).unwrap_or("").trim();
            let first = if rest.is_empty() {
                lines[number + 1..]
                    .iter()
                    .map(|l| l.split("//").next().unwrap_or(l).trim())
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
            } else {
                rest
            };
            // `&mut tx` auto-derefs to the connection; the explicit form is
            // the same thing spelled out.
            if first.starts_with("&mut tx") || first.starts_with("&mut *tx") {
                continue;
            }

            let excused = lines[number.saturating_sub(REACH)..number]
                .iter()
                .any(|l| l.contains("audit-only:"));
            if !excused {
                offenders.push(format!("{relative}:{}  {}", number + 1, code.trim()));
            }
        }
    }

    // Without this the test passes for the wrong reason if the call is renamed.
    assert!(
        sites >= 30,
        "found {sites} audit record sites; the control plane has over thirty. \
         The scan is broken, not the code."
    );
    assert!(
        offenders.is_empty(),
        "an audit entry written outside the transaction of its change — a crash \
         between the two loses the record of an act that stood. Pass `&mut tx`, \
         or, for an act with no control-plane write of its own, acquire a \
         connection and write `// audit-only: <why>` above the call:\n  {}",
        offenders.join("\n  ")
    );
}

fn sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the source tree lists") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
