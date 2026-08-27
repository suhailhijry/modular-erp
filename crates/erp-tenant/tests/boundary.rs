//! **A module does not link the fleet.**
//!
//! D15 says the tenant runtime may not contain fleet management, cluster
//! placement, or any other tenant's credentials, because that binary may ship to
//! a customer's own cloud. Before this crate existed the rule was false: every
//! module depended on `erp-control`, which exports `ClusterRegistry`,
//! `FleetPlan`, `PlacementPolicy`, `TenantPools` and `WorkSchedule`.
//!
//! It was false for a small reason. Modules used six symbols from that crate —
//! `TenantDb`, `CommandError`, `PoolError`, `ModuleSetup`, `EnabledModules` and
//! one message code — and linked everything else for free. Those six live here
//! now, and this is what stops the sixth module from reaching past them.
//!
//! # What this does *not* yet prove
//!
//! `erp-web` still depends on `erp-control`, and every module depends on
//! `erp-web`, so the **transitive** path survives. It is not removable by moving
//! code: `erp-web` holds `AppState { control: Arc<ControlPlane> }` and its
//! extractors check a session against the control database on every request.
//! Closing it needs D16 — a tenant verifying a signed token locally instead of
//! asking the control plane — which is deliberately not built yet.
//!
//! So this test pins the half that is true and that would otherwise decay: no
//! module reaches the fleet **directly**. The other half is D16's, and §1.15
//! records it as outstanding.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// Crates a module must never name in `[dependencies]`.
const FORBIDDEN: [&str; 1] = ["erp-control"];

#[test]
fn no_module_depends_on_the_control_plane() {
    let modules = module_manifests();

    // Without this the test passes for the wrong reason if `modules/` moves or
    // the manifest layout changes.
    assert!(
        modules.len() >= 4,
        "found {} module manifests; expected at least the four that exist. \
         The scan is broken, not the code.",
        modules.len()
    );

    let mut offenders = Vec::new();
    for (name, path) in &modules {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let deps = dependencies_section(&text);
        assert!(
            !deps.is_empty(),
            "no [dependencies] found in {}, so this test proved nothing about it",
            path.display()
        );
        for line in deps.lines() {
            let Some(dep) = line.split(['=', ' ', '.']).next().map(str::trim) else {
                continue;
            };
            if FORBIDDEN.contains(&dep) {
                offenders.push(format!("{name} -> {dep}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a module names the control plane in [dependencies].\n\n\
         That crate carries ClusterRegistry, FleetPlan, PlacementPolicy, \
         TenantPools and WorkSchedule — the map of every other tenant — into a \
         binary that may ship to one customer's own cloud (D15). Depend on \
         `erp-tenant` instead; it holds the handle and nothing else. If the \
         symbol you need is genuinely missing from it, move the symbol, not the \
         dependency.\n\n  {}",
        offenders.join("\n  ")
    );
}

/// The body of `[dependencies]`, stopping at the next section header.
fn dependencies_section(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with('[') {
            inside = line.trim() == "[dependencies]";
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn module_manifests() -> Vec<(String, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above this crate")
        .join("modules");

    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let manifest = entry.path().join("Cargo.toml");
        if manifest.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            found.push((name, manifest));
        }
    }
    found.sort();
    found
}
