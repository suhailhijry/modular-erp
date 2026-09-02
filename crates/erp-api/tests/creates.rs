//! **Every create is reached with an idempotency key.**
//!
//! `erp_eventlog::try_create` decides whether a repeated identity is a retry or
//! two different things given one name, and it decides it from the fingerprint
//! in the metadata. A handler that calls a create command with plain
//! `metadata(&tenant)` instead of `creating(&tenant, &key)` passes no
//! fingerprint — and `try_create` then treats *every* repeat as a retry, which
//! is exactly the silent document loss the whole mechanism replaced.
//!
//! Nothing in the type system stops that today. The extractor is a parameter a
//! handler opts into, and forgetting it compiles. So this test is the guard, and
//! it is the same shape as `idempotence.rs` next door: read the source, and
//! refuse the shape that would be wrong.
//!
//! # Why not enforce it in the types
//!
//! Because `try_create` has a second, legitimate caller: commands invoked with
//! **derived** ids — `ledger::post_entry_in` posting `si.INV-0001` on behalf of
//! `sales`. Those cannot collide, so they carry no fingerprint by design. A
//! newtype that made the fingerprint mandatory would have to carry an escape
//! hatch for them, and an escape hatch is what this test can check the use of.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Where a module's write path lives.
fn modules() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("the workspace root")
        .join("modules");

    let mut found: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("modules/ is there")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.join("src/commands.rs").exists())
        .collect();
    found.sort();
    found
}

/// Comments are stripped before matching, so prose about `try_create` in a doc
/// comment does not make a command look like one.
fn code(source: &str) -> String {
    source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `pub async fn`s in a module's commands that create an aggregate.
fn creating_commands(commands: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current: Option<String> = None;
    let mut body = String::new();

    for line in code(commands).lines() {
        if let Some(rest) = line.strip_prefix("pub async fn ") {
            if let Some(name) = current.take()
                && (body.contains("try_create::<") || body.contains(".create::<"))
            {
                found.push(name);
            }
            current = rest.split('(').next().map(str::to_owned);
            body.clear();
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(name) = current
        && (body.contains("try_create::<") || body.contains(".create::<"))
    {
        found.push(name);
    }
    found
}

/// Every handler in a module's routes, as `(name, body)`.
fn handlers(http: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut current: Option<String> = None;
    let mut body = String::new();

    for line in code(http).lines() {
        if let Some(rest) = line.strip_prefix("async fn ") {
            if let Some(name) = current.take() {
                found.push((name, std::mem::take(&mut body)));
            }
            current = rest.split('(').next().map(str::to_owned);
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(name) = current {
        found.push((name, body));
    }
    found
}

/// **A handler that creates must pass the key through.**
///
/// The failure this refuses, concretely: a new `POST /v1/x/things` written the
/// obvious way, with `metadata(&tenant)` copied from the handler above it. It
/// compiles, it passes its own test, and the second till to send `thing-1`
/// silently loses a record.
#[test]
fn every_handler_that_creates_passes_the_idempotency_key() {
    let mut offenders = Vec::new();
    let mut checked = 0;

    for module in modules() {
        let name = module.file_name().unwrap_or_default().to_string_lossy();
        let commands = std::fs::read_to_string(module.join("src/commands.rs")).unwrap_or_default();
        let creates = creating_commands(&commands);
        if creates.is_empty() {
            continue;
        }

        let http_path = module.join("src/http.rs");
        let Ok(http) = std::fs::read_to_string(&http_path) else {
            continue;
        };

        for (handler, body) in handlers(&http) {
            let calls = creates
                .iter()
                .find(|command| body.contains(&format!("crate::{command}(")));
            let Some(command) = calls else { continue };
            checked += 1;

            // `publicly(&key)` is the third form, and it carries the same
            // fingerprint. A public create is the one most likely to be
            // retried — a phone that lost signal mid-submit — so it is held to
            // exactly this rule; what it does not carry is an actor, because
            // nobody was behind it.
            // `importing(&tenant, &key, row)` is the fourth form. An import is
            // a command per row, and it folds the row's own identity into the
            // fingerprint so a re-upload of a corrected file does not duplicate
            // the rows that already went in — see `erp_web::importing`.
            if !body.contains("creating(&tenant")
                && !body.contains("publicly(&key)")
                // Not `importing(&tenant`: an import's per-row work is a helper
                // that already holds a `&Allowed<_>`, so the call reads
                // `importing(tenant, …)`. Comments are stripped before this
                // runs, so the bare call is signal enough.
                && !body.contains("importing(")
            {
                offenders.push(format!(
                    "{name}::http::{handler} calls {command}, which creates, but passes no \
                     idempotency key — use `creating(&tenant, &key)` rather than \
                     `metadata(&tenant)`"
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a create is reachable without an idempotency key:\n  {}",
        offenders.join("\n  ")
    );

    // A count, so that a refactor which stops this test finding anything at all
    // is a failure rather than a silent pass.
    //
    // Eight and not ten, which is the number of handlers taking the extractor:
    // `ledger::reverse_entry` and `ledger::post_entry` reach `try_create`
    // through `post_entry_in`, so a scan that only matches the call by name
    // sees one of them. Both take the key regardless, and
    // `every_handler_that_takes_a_key_threads_it_through` is what checks they
    // use it.
    assert!(
        checked >= 8,
        "expected at least eight creating handlers, found {checked} — has the scan stopped matching?"
    );
}

/// **A handler that takes the key uses it.**
///
/// The mirror of the above, and the cheaper mistake: adding the extractor,
/// forgetting to thread it into the metadata, and getting a 400 for a missing
/// header while still not telling a retry from a collision.
#[test]
fn every_handler_that_takes_a_key_threads_it_through() {
    let mut offenders = Vec::new();

    for module in modules() {
        let name = module.file_name().unwrap_or_default().to_string_lossy();
        let Ok(http) = std::fs::read_to_string(module.join("src/http.rs")) else {
            continue;
        };

        for (handler, body) in handlers(&http) {
            if body.contains("key: IdempotencyKey") && !body.contains("&key") {
                offenders.push(format!("{name}::http::{handler}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a handler takes an idempotency key and never uses it: {offenders:?}"
    );
}
