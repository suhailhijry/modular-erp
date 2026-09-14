//! **Every fact a rule may name is one somebody supplies.**
//!
//! Phase 5b asks for "per-request fact assembly with startup coverage
//! assertions — an unsatisfiable condition fails the build, not a user's
//! request". This is that assertion, and it is a test rather than a startup
//! check for the reason every other law here is: a build that cannot ship is
//! cheaper than a deployment that starts and then refuses somebody.
//!
//! # What goes wrong without it
//!
//! `FactRegistry` says which facts a rule may name. Nothing connects that list
//! to the code that fills them in. Declare a fact and forget the assembly, and
//! a tenant can author a rule that validates, stores, reads back correctly, and
//! **is never once true** — the worst kind of broken, because every part of it
//! looks like it works.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use erp_tenant::limits;

fn declared() -> Vec<String> {
    limits::registry()
        .names()
        .iter()
        .map(|n| (*n).to_owned())
        .collect()
}

/// The constant a fact is named by, so the scan looks for the identifier the
/// extractor actually writes rather than the string it stands for.
fn constant_for(fact: &str, limits_src: &str) -> String {
    let line = limits_src
        .lines()
        .find(|line| line.contains(&format!("= \"{fact}\";")))
        .unwrap_or_else(|| panic!("{fact} is declared in the registry with no constant"));
    line.split_whitespace()
        // `pub const AMOUNT: &str = "amount";` — the identifier carries the
        // colon, which is what a naive uppercase filter trips on.
        .map(|word| word.trim_end_matches(':'))
        .find(|word| word.len() > 2 && word.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
        .unwrap_or_else(|| panic!("no screaming-case constant in {line:?}"))
        .to_owned()
}

/// Every `.rs` in the workspace, from the repository root — because a fact is
/// assembled in two places and a scan of one of them is the mistake this file
/// caught on its first run.
fn sources() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root is two levels above this crate")
        .to_path_buf();

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
                // Not this file, which names every fact it hunts for.
                if path.file_name().is_some_and(|n| n == "facts.rs") {
                    continue;
                }
                found.push(path);
            }
        }
    }
    assert!(
        found.len() > 50,
        "the walk is broken and this file passes vacuously"
    );
    found
}

/// **Every declared fact is supplied by somebody.**
///
/// The edge supplies what a request knows before its body is read —
/// `capability` and `branch`, through `limits::facts_at` — and
/// `Limits::permit` adds `role`, which only the person's access knows. A handler
/// that has parsed a body supplies the rest — `amount`, from
/// `ledger::post_entry`. All count; none alone does.
///
/// **`facts_at` and `Limits::permit` live beside the registry**, so the scan
/// also reads `limits.rs`'s own code for a fact put in by its bare name — the
/// code above its tests only, which build facts of their own and supply
/// nobody's request.
#[test]
fn every_declared_fact_is_assembled_somewhere() {
    let limits_src =
        std::fs::read_to_string("../erp-tenant/src/limits.rs").expect("limits is readable");
    let limits_code: String = limits_src
        .split("#[cfg(test)]")
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    let corpus: String = sources()
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect::<Vec<_>>()
        .join("\n");

    for fact in declared() {
        let ident = constant_for(&fact, &limits_src);
        let assembled = corpus.contains(&format!("limits::{ident}"))
            || limits_code.contains(&format!(".with({ident},"))
            // `capability` is put in by `facts_for`, which every caller uses.
            || (fact == "capability" && corpus.contains("facts_for("));
        assert!(
            assembled,
            "the registry declares `{fact}` and nothing anywhere supplies it. \
             A tenant could author a rule about it that is never once true."
        );
    }
}

/// The reverse: something supplied but never declared can never be named by a
/// rule, so assembling it is work nobody asked for.
#[test]
fn nothing_is_assembled_that_no_rule_may_name() {
    let declared = declared();
    for path in sources() {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in source.lines() {
            let Some(rest) = line.split("limits::").nth(1) else {
                continue;
            };
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                .collect();
            if ident.len() < 3 {
                continue;
            }
            assert!(
                declared.iter().any(|f| f.to_ascii_uppercase() == ident),
                "{} supplies `{ident}` and the registry declares no such fact",
                path.display()
            );
        }
    }
}

/// The registry is not empty, or both tests above pass vacuously.
#[test]
fn the_registry_declares_something() {
    assert!(
        declared().len() >= 4,
        "expected at least amount, branch, capability and role; found {:?}",
        declared()
    );
}
