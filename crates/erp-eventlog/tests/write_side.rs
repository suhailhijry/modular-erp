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
const ALLOWED: [&str; 11] = [
    // Command handling. This is the whole point of the write model.
    //
    // `booking` loads more than the others and each one is a decision made from
    // history: whether a resource exists and is in service before a claim is
    // taken against it, and what a reservation now holds so the whole claim set
    // can be rebuilt from what the events actually said. Both are inside the
    // transaction that writes; neither answers a query.
    "modules/booking/src/commands.rs",
    "modules/crm/src/commands.rs",
    "modules/ledger/src/commands.rs",
    // `prepaid` loads to decide what a redemption is worth and how much of a
    // term has been served. Both are decisions taken from history inside the
    // transaction that writes; neither answers a query.
    "modules/prepaid/src/commands.rs",
    // `inventory` loads a shelf to decide which lots a movement comes out of
    // and what each portion costs — a lot's remaining quantity and remaining
    // value are facts about every movement that has touched it — and to ask
    // whether this movement has already been recorded. It loads a product to
    // answer whether stock of it may move at all and how closely it is tracked,
    // which is `crm`'s argument for reading the log rather than the read model.
    // Every one of them is a decision taken from history; none answers a query.
    "modules/inventory/src/commands.rs",
    // `pos` loads a shift to answer what the drawer should hold, and loads the
    // invoice a retried sale already issued to report its total back.
    "modules/pos/src/commands.rs",
    // `branches` answers "may a document be dated here" from the log, because
    // `proj_branches` is another projection group and a branch opened a moment
    // ago is not in it yet. Read by `ledger::post_entry_in`, in that
    // transaction — the same argument `crm::accepts_documents` makes.
    "modules/branches/src/commands.rs",
    "modules/sales/src/commands.rs",
    "modules/purchases/src/commands.rs",
    "modules/tax_sa/src/commands.rs",
    // `payments` reads the payment it has just ended to find what the deposit
    // was held against, so the booking's claim is released in the same
    // transaction. A decision from history inside the write; not a query.
    "modules/payments/src/commands.rs",
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

/// The part of a source file that ships: everything before its test module.
fn shipped(text: &str) -> &str {
    text.split("#[cfg(test)]").next().unwrap_or(text)
}

/// **A phone number is read by one function.** Three copies of the rule once
/// disagreed — the SMS transport refused the dashes the booking door and the
/// control plane stripped, so a number accepted at the door was dead-lettered
/// at the gateway. The rule lives in `erp_types::phone`; anything else that
/// parses a number is a second rule waiting to drift.
#[test]
fn a_phone_number_is_read_in_one_place() {
    const OWNER: &str = "crates/erp-types/src/phone.rs";
    let mut offenders = Vec::new();
    let mut owner_seen = false;
    for path in sources() {
        let relative = relative(&path);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (number, line) in shipped(&text).lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            let defines = code.contains("fn normalise(") || code.contains("fn msisdn(");
            // The shape of a hand-rolled parser: stripping a `+` or a `00`
            // off what is about to be treated as digits.
            let parses = code.contains("strip_prefix(\"00\")")
                || (code.contains("strip_prefix('+')") && code.contains("digit"));
            if relative.ends_with(OWNER) {
                owner_seen |= defines;
                continue;
            }
            // The transport's `msisdn` is a delegation, and says so on the
            // next line; a body that does the parsing itself is what this
            // refuses.
            if parses {
                offenders.push(format!("{relative}:{}: {}", number + 1, line.trim()));
            }
        }
    }
    assert!(
        owner_seen,
        "{OWNER} no longer defines the rule; the scan is broken"
    );
    assert!(
        offenders.is_empty(),
        "a phone number is parsed outside erp_types::phone.\n\n\
         One rule: `erp_types::phone::normalise` (E.164) or `msisdn` (digits). \
         A second parser is how a number accepted at the door is refused at \
         the gateway.\n\n  {}",
        offenders.join("\n  ")
    );
}

/// **A command takes its clock from the caller.** `at` is a parameter so a
/// retried request produces the same event and a test can say when a thing
/// happened. The HTTP layer and the worker are where "now" is read; a module's
/// own code never reads it. `payroll::approve_run` was the one exception, and
/// this is what stops there being a second.
#[test]
fn no_module_reads_the_wall_clock() {
    let mut offenders = Vec::new();
    for path in sources() {
        let relative = relative(&path);
        if !relative.starts_with("modules/") || !relative.contains("/src/") {
            continue;
        }
        // Routes are the boundary where a request becomes a command, and the
        // one place a module may look at the clock.
        if relative.ends_with("/http.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (number, line) in shipped(&text).lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if code.contains("Utc::now()") {
                offenders.push(format!("{relative}:{}  {}", number + 1, code.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a module reads the wall clock outside its HTTP layer.\n\n\
         Take `at` from the caller: a retried request must produce the same event, \
         and a test must be able to say when something happened.\n\n  {}",
        offenders.join("\n  ")
    );
}

/// The ways an instant is turned into a day, a month or a clock reading by
/// hand — every one of them in UTC, which is nobody's calendar.
const BY_HAND: [&str; 6] = [
    ".date_naive()",
    "and_hms_opt(",
    "and_hms(",
    "format(\"%Y-%m\")",
    "format(\"%Y-%m-%d\")",
    "format(\"%Y-%m-%d %H:%M",
];

/// Where the conversion is allowed to be written out.
const CALENDAR_ALLOWED: [&str; 3] = [
    // The type that owns the conversion.
    "crates/erp-types/src/calendar.rs",
    // Works on instants already moved onto the tenant's offset by its caller.
    "crates/erp-recurrence/src/availability.rs",
    // `occupancy_guard` keys a lock by resource and UTC day. It is a lock
    // granularity, not anybody's day: two overlapping spans always share a key
    // whatever the offset, and nothing reads the key as a date.
    "crates/erp-occupancy/src/lib.rs",
];

/// **An instant becomes a day only through the tenant's calendar.** Saudi
/// Arabia is `+03:00`: a quarter that starts at local midnight starts at
/// `21:00Z` the evening before, and every `date_naive()` on an instant was
/// filing the last three hours of March into April. `erp_types::Calendar` is
/// the one place that knows the offset; a projection reads it from the event
/// (`ctx.calendar()`), a command from configuration
/// (`erp_eventlog::configuration::calendar`).
#[test]
fn an_instant_becomes_a_day_only_through_the_calendar() {
    let mut allowed_seen = 0;
    let mut offenders = Vec::new();
    for path in sources() {
        let relative = relative(&path);
        let permitted = CALENDAR_ALLOWED.iter().any(|ok| relative.ends_with(ok));
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (number, line) in shipped(&text).lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if !BY_HAND.iter().any(|way| code.contains(way)) {
                continue;
            }
            if permitted {
                allowed_seen += 1;
                continue;
            }
            offenders.push(format!("{relative}:{}  {}", number + 1, code.trim()));
        }
    }
    assert!(
        allowed_seen > 0,
        "the scan found nothing, including where it is allowed; it is broken"
    );
    assert!(
        offenders.is_empty(),
        "an instant is turned into a day by hand, in UTC, which is nobody's calendar.\n\n\
         Use `erp_types::Calendar`: `calendar.day(at)`, `calendar.month(at)`, \
         `calendar.start_of(day)`, `calendar.clock(at)`. A projection has it as \
         `ctx.calendar()`; a command reads `erp_eventlog::configuration::calendar`.\n\n  {}",
        offenders.join("\n  ")
    );
}
