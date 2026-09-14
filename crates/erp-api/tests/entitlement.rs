//! **Every module route asks whether the tenant has the module.**
//!
//! `erp_web::require_module` is a call at the top of each handler, and a
//! handler that forgets it serves a module the tenant switched off — or never
//! had — as if it were on. The tenant's role still gates the route, so what
//! leaks is not another tenant's data; it is the product's own boundary,
//! which is what a customer who declined a module was promised. Nothing at
//! runtime notices: the route answers normally, from tables that are there
//! because disabling a module never drops them.
//!
//! So this reads every `modules/*/src/http.rs` and refuses a handler that does
//! not call it. The exception is a handler that takes [`erp_web::Anonymous`]:
//! there is no tenant on such a route, so nothing to require — today the two
//! catalogue routes a signup form reads before a company exists,
//! `GET /v1/ledger/charts` and `GET /v1/booking/trades`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

#[test]
fn every_module_route_requires_its_module() {
    let modules = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules");
    let mut handlers = 0;
    let mut offenders = Vec::new();

    let mut files: Vec<_> = std::fs::read_dir(&modules)
        .expect("the modules directory lists")
        .map(|entry| entry.expect("an entry").path().join("src/http.rs"))
        .filter(|http| http.exists())
        .collect();
    files.sort();

    for http in files {
        let text = std::fs::read_to_string(&http).expect("http.rs reads");
        let lines: Vec<&str> = text.lines().collect();
        let name = http
            .strip_prefix(&modules)
            .unwrap_or(&http)
            .to_string_lossy()
            .replace('\\', "/");

        let mut after_path_attribute = false;
        let mut at = 0;
        while at < lines.len() {
            let line = lines[at];
            if line.starts_with("#[utoipa::path") {
                after_path_attribute = true;
            }
            let is_handler = after_path_attribute
                && (line.starts_with("async fn ") || line.starts_with("pub async fn "));
            if !is_handler {
                at += 1;
                continue;
            }
            after_path_attribute = false;
            handlers += 1;

            // The signature runs to the line that opens the body; the body to
            // the line that closes the function at column zero.
            let opens = (at..lines.len())
                .find(|&i| lines[i].trim_end().ends_with('{'))
                .expect("a handler opens a body");
            let closes = (opens..lines.len())
                .find(|&i| lines[i] == "}")
                .expect("a handler closes");
            let signature = lines[at..=opens].join("\n");
            // Comments stripped: a call commented out is a call not made, and
            // the first version of this scan was satisfied by exactly that.
            let body: String = lines[opens + 1..closes]
                .iter()
                .map(|line| line.split("//").next().unwrap_or(line))
                .collect::<Vec<_>>()
                .join("\n");

            // No tenant on the request, nothing to require.
            if !signature.contains("Anonymous") && !body.contains("require_module(") {
                offenders.push(format!("{name}:{}  {}", at + 1, line.trim()));
            }
            at = closes + 1;
        }
    }

    // Without this the test passes for the wrong reason if the layout changes.
    assert!(
        handlers >= 200,
        "found {handlers} module handlers; nineteen modules have over two hundred. \
         The scan is broken, not the code."
    );
    assert!(
        offenders.is_empty(),
        "a module route that never asks whether the tenant has the module, so it \
         answers for a module switched off or never bought. Call \
         `require_module(&tenant.db, &crate::module_id(), locale)?` first, or take \
         `Anonymous` if there is genuinely no tenant on the route:\n  {}",
        offenders.join("\n  ")
    );
}
