//! **Migrations must be expand-only, so a deploy can overlap.**
//!
//! Zero-downtime means two builds run at once for a minute or two: the new pods
//! are up, the old ones are draining, and both are serving requests against the
//! same database. A migration that *removes* something the old build still uses
//! turns that minute into an outage — and it is an outage nobody sees in
//! staging, because staging deploys one pod.
//!
//! So the rule is expand/contract. **Expand** in the deploy that adds the new
//! shape: add columns, add tables, widen types, make things optional. Ship it,
//! let it roll out completely, and only then **contract** in a *later* deploy:
//! drop what nothing reads any more. Two deploys, the same discipline the
//! upcaster chain already uses for events (`spa_eventlog::upcast`).
//!
//! This test refuses the contract half by accident. A migration that genuinely
//! needs one exempts itself, by name, with a reason — see [`EXEMPTION`].
//!
//! # Why a test and not a review checklist
//!
//! Because the failure is invisible until it is expensive. Nobody reviewing
//! `ALTER TABLE tenant DROP COLUMN slug` is thinking about the pods that have
//! not finished draining, and the person who wrote it had a good reason for
//! dropping it. The check has to be the thing that remembers.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Where a migration says it means it.
///
/// `<chain>/<file>  <rule>  <reason>`, one per line, continuation lines
/// indented.
///
/// # Why not a comment in the migration itself
///
/// **Because an applied migration is immutable.** sqlx checksums the file and
/// refuses a database whose recorded hash no longer matches, so editing one —
/// *even to add a comment* — strands every environment that already ran it. The
/// first version of this mechanism put the marker in the SQL and broke `just
/// demo` on the next run: a rule about not changing what is already deployed,
/// enforced by changing what was already deployed.
///
/// Exemptions are per rule rather than per file, so excusing a `drop column`
/// does not quietly also permit a `rename`.
const EXEMPTIONS: &str = "../../migrations/EXEMPTIONS";

/// What an old pod, still serving, would break on.
///
/// Each is a phrase that only appears in `ALTER`-shaped statements — `not null`
/// on its own is in every `CREATE TABLE` here, and `set not null` is not.
const UNSAFE: &[(&str, &str, &str)] = &[
    ("drop-table", "drop table", "an old pod still queries it"),
    (
        "drop-column",
        "drop column",
        "an old pod still selects and inserts it",
    ),
    (
        "drop-schema",
        "drop schema",
        "an old pod still reads through it",
    ),
    ("drop-view", "drop view", "an old pod still reads it"),
    (
        "drop-sequence",
        "drop sequence",
        "an old pod still draws from it",
    ),
    ("rename", "rename to", "an old pod still uses the old name"),
    (
        "rename-column",
        "rename column",
        "an old pod still uses the old name",
    ),
    (
        "retype-column",
        "alter column",
        "an old pod may write the old type; add a new column instead",
    ),
    (
        "set-not-null",
        "set not null",
        "an old pod still inserts rows without it",
    ),
    (
        "drop-default",
        "drop default",
        "an old pod still relies on it",
    ),
    (
        "add-constraint",
        "add constraint",
        "an old pod still writes rows that would violate it",
    ),
];

/// Every migration this project ships, in both chains.
fn migrations() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations");

    let mut found = Vec::new();
    for chain in ["control", "tenant"] {
        let dir = root.join(chain);
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{} is not readable: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.extension().is_some_and(|e| e == "sql") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// The SQL with comments removed, lowercased, and whitespace collapsed.
///
/// Comments have to go first or the prose fails the check — half of these files
/// explain *why* something is not dropped, and the word is the same.
fn statements(sql: &str) -> Vec<String> {
    let stripped: String = sql
        .lines()
        .map(|line| line.split_once("--").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join(" ");

    stripped
        .split(';')
        .map(|statement| statement.split_whitespace().collect::<Vec<_>>().join(" "))
        .map(|statement| statement.to_lowercase())
        .filter(|statement| !statement.is_empty())
        .collect()
}

/// Every exemption, as `(migration, rule, reason)`.
fn exemptions() -> Vec<(String, String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(EXEMPTIONS);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} is not readable: {e}", path.display()));

    let mut found: Vec<(String, String, String)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // An indented line continues the reason above it, so a reason can be
        // long enough to be worth reading.
        if line.starts_with(char::is_whitespace) {
            if let Some(last) = found.last_mut() {
                last.2.push(' ');
                last.2.push_str(line.trim());
            }
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(file), Some(rule)) = (parts.next(), parts.next()) else {
            panic!("{EXEMPTIONS}: `{line}` is not `<file> <rule> <reason>`");
        };
        found.push((
            file.to_owned(),
            rule.to_owned(),
            parts.collect::<Vec<_>>().join(" "),
        ));
    }
    found
}

/// Which rules apply to one migration, keyed by its `<chain>/<file>` path.
fn exempted(key: &str) -> Vec<(String, String)> {
    exemptions()
        .into_iter()
        .filter(|(file, _, _)| file == key)
        .map(|(_, rule, reason)| (rule, reason))
        .collect()
}

/// `control/0002_clusters.sql` from a full path.
fn key_of(path: &Path) -> String {
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    let chain = path
        .parent()
        .and_then(std::path::Path::file_name)
        .unwrap_or_default()
        .to_string_lossy();
    format!("{chain}/{file}")
}

// ---------------------------------------------------------------------------

/// **The check.** Nothing a draining pod still depends on may be taken away.
#[test]
fn every_migration_is_expand_only() {
    let files = migrations();
    assert!(files.len() >= 10, "only found {} migrations", files.len());

    for path in files {
        let sql = std::fs::read_to_string(&path).expect("a readable migration");
        let name = key_of(&path);
        let exempted = exempted(&name);

        for statement in statements(&sql) {
            for (rule, phrase, why) in UNSAFE {
                if !statement.contains(phrase) {
                    continue;
                }
                let excused = exempted.iter().find(|(exempt, _)| exempt == rule);
                assert!(
                    excused.is_some(),
                    "{name} is not expand-only: `{phrase}` — {why}.\n\
                     \n\
                     Statement: {statement}\n\
                     \n\
                     Expand now and contract in a later deploy, once every pod \
                     that reads it is gone. If this one genuinely cannot wait, \
                     say so in `migrations/EXEMPTIONS` — **not** in the \
                     migration, which is immutable once applied:\n\
                     \n    {name}  {rule}  <why this is safe to run while the \
                     previous build is still serving>"
                );
                assert!(
                    excused.is_some_and(|(_, reason)| !reason.is_empty()),
                    "{name} exempts `{rule}` and gives no reason. The reason is \
                     what somebody reads when they are deciding whether this \
                     deploy can overlap."
                );
            }
        }
    }
}

/// An exemption that no longer excuses anything is a licence left lying around.
#[test]
fn no_migration_carries_an_exemption_it_does_not_need() {
    let files: Vec<String> = migrations().iter().map(|p| key_of(p)).collect();
    for (file, _, _) in exemptions() {
        assert!(
            files.contains(&file),
            "{EXEMPTIONS} names {file}, which is not a migration"
        );
    }

    for path in migrations() {
        let sql = std::fs::read_to_string(&path).expect("a readable migration");
        let name = key_of(&path);
        let statements = statements(&sql);

        for (rule, _) in exempted(&name) {
            let Some((_, phrase, _)) = UNSAFE.iter().find(|(known, _, _)| *known == rule) else {
                panic!(
                    "{name} exempts `{rule}`, which is not a rule. Known: {:?}",
                    UNSAFE.iter().map(|(r, _, _)| r).collect::<Vec<_>>()
                )
            };

            assert!(
                statements.iter().any(|s| s.contains(phrase)),
                "{name} exempts `{rule}` and does nothing that needs it"
            );
        }
    }
}

/// A column added as `NOT NULL` with no default breaks every insert an old pod
/// makes — the same failure as `SET NOT NULL`, arriving through a statement
/// that otherwise looks purely additive.
#[test]
fn a_new_column_is_never_mandatory_without_a_default() {
    for path in migrations() {
        let sql = std::fs::read_to_string(&path).expect("a readable migration");
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        for statement in statements(&sql) {
            if !statement.contains("add column") {
                continue;
            }
            assert!(
                !statement.contains("not null") || statement.contains("default"),
                "{name} adds a mandatory column with no default, so every insert \
                 an old pod makes fails until it drains.\n\nStatement: {statement}"
            );
        }
    }
}

/// The check is only worth having if it says no.
#[test]
fn the_check_refuses_what_it_claims_to() {
    let destructive = "\
        -- A migration that drops something.\n\
        ALTER TABLE tenant DROP COLUMN slug;\n";
    let found: Vec<&str> = statements(destructive)
        .iter()
        .flat_map(|s| {
            UNSAFE
                .iter()
                .filter(|(_, phrase, _)| s.contains(phrase))
                .map(|(rule, _, _)| *rule)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(found, vec!["drop-column"]);

    // And prose about dropping something is not dropping something. Half these
    // files explain why a thing is *not* deleted, in the same words.
    let prose = "-- Nothing here will DROP COLUMN or RENAME TO anything.\nSELECT 1;\n";
    for statement in statements(prose) {
        for (rule, phrase, _) in UNSAFE {
            assert!(
                !statement.contains(phrase),
                "a comment tripped the {rule} rule: {statement}"
            );
        }
    }

    // A widening column is fine; a mandatory one is not.
    assert!(statements("ALTER TABLE t ADD COLUMN c TEXT;")[0].contains("add column"));
    assert!(
        statements("ALTER TABLE t ADD COLUMN c TEXT NOT NULL DEFAULT '';")[0].contains("default")
    );
}
