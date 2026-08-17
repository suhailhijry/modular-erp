//! The rules a transaction pooler imposes, pinned so they stay true.
//!
//! # Why this file exists before there is a pooler
//!
//! Because the cost of finding out later is a rewrite. Supavisor and `PgBouncer`
//! run in **transaction mode**: a client gets a different server backend for
//! each transaction, which is what lets 400 server connections serve 250,000
//! clients — and it means session state does not survive between transactions.
//!
//! Almost all of this system is already fine with that, and by accident of good
//! discipline rather than by design: the projection hot path uses `SET LOCAL`,
//! `pg_notify` was banned by D4, and there is no session advisory lock or temp
//! table anywhere in it.
//!
//! What is not fine is DDL. So the rule is: **anything that sets session state
//! or runs `CREATE DATABASE` asks for the direct route.** These tests are what
//! stop that rule quietly becoming untrue.

#![allow(clippy::expect_used, clippy::unwrap_used)]

/// **Every session-scoped `SET` in this workspace is on a DDL path.**
///
/// A `SET search_path` outside a transaction survives only as long as the
/// backend the client happens to hold. On the request path — pooled — the next
/// statement can land on a different backend with a different search path, and
/// the symptom is a query reading an empty table rather than an error.
///
/// The twelve sites that exist are all install, provisioning or rebuild, and
/// they run on connections opened from `maintenance_options`, which is
/// `Role::Direct`. If a thirteenth appears somewhere else, this fails and asks
/// for `SET LOCAL` instead.
#[test]
fn no_session_scoped_set_outside_a_ddl_path() {
    let allowed = [
        // Provisioning: `CREATE DATABASE` cannot be in a transaction at all.
        "crates/spa-control/src/provision.rs",
        // A projection rebuild, which drops and recreates a schema.
        "crates/spa-projection/src/shadow.rs",
        // Each module's test-only `install`, which mirrors provisioning.
        "modules/ledger/src/lib.rs",
        "modules/sales/src/lib.rs",
        "modules/purchases/src/lib.rs",
        "modules/tax_sa/src/lib.rs",
    ];

    let mut offenders = Vec::new();
    for path in sources() {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let relative = path.to_string_lossy().replace('\\', "/");
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if !code.contains("SET search_path") && !code.contains("SET TIME ZONE") {
                continue;
            }
            if code.contains("SET LOCAL") {
                continue;
            }
            if allowed.iter().any(|ok| relative.ends_with(ok)) {
                continue;
            }
            offenders.push(format!("{relative}:{}", number + 1));
        }
    }

    assert!(
        offenders.is_empty(),
        "session-scoped SET outside a DDL path — a transaction pooler will not \
         carry it to the next statement. Use `SET LOCAL`, or run on the direct \
         route and add the file to `allowed`:\n  {}",
        offenders.join("\n  ")
    );
}

/// **Nothing holds a session-scoped advisory lock.**
///
/// One taken with `pg_advisory_lock` outlives its transaction and belongs to a
/// backend. Through a pooler the backend goes back to the pool still holding it,
/// and the next tenant to be handed that backend inherits a lock nobody can find
/// the owner of. The transaction-scoped variant (`pg_advisory_xact_lock`) is
/// fine, and so is `SELECT … FOR UPDATE`, which is what the checkpoint lease
/// actually uses.
#[test]
fn no_session_scoped_advisory_locks_outside_the_test_harness() {
    let mut offenders = Vec::new();
    for path in sources() {
        let relative = path.to_string_lossy().replace('\\', "/");
        // The template fixture opens its own connection and is never pooled.
        if relative.ends_with("crates/spa-testkit/src/template.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if code.contains("pg_advisory_lock") || code.contains("pg_advisory_unlock") {
                offenders.push(format!("{relative}:{}", number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "session-scoped advisory lock; use `pg_advisory_xact_lock`:\n  {}",
        offenders.join("\n  ")
    );
}

/// **`LISTEN`/`NOTIFY` stays gone.**
///
/// D4 banned it as internal transport for reasons that had nothing to do with
/// pooling. It happens to also be the thing a transaction pooler cannot carry:
/// a `LISTEN` belongs to a backend that goes straight back into the pool.
#[test]
fn no_listen_notify() {
    let mut offenders = Vec::new();
    for path in sources() {
        let relative = path.to_string_lossy().replace('\\', "/");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if code.contains("pg_notify") || code.contains("LISTEN ") {
                offenders.push(format!("{relative}:{}", number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "D4 forbids this:\n  {}",
        offenders.join("\n  ")
    );
}

/// Every `.rs` and `.sql` in the workspace, from the repository root.
fn sources() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root is two levels above this crate")
        .to_path_buf();

    let mut found = Vec::new();
    let mut stack = vec![
        root.join("crates"),
        root.join("modules"),
        root.join("migrations"),
    ];
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
            } else if path.extension().is_some_and(|e| e == "rs" || e == "sql") {
                // Not this file. It names every pattern it hunts for, so the
                // first run of these tests found themselves — which at least
                // proved the walk reaches here.
                if path.file_name().is_some_and(|n| n == "pooler.rs") {
                    continue;
                }
                found.push(path);
            }
        }
    }
    assert!(
        found.len() > 50,
        "found only {} source files; the walk is broken and this whole file is \
         passing vacuously",
        found.len()
    );
    found
}
