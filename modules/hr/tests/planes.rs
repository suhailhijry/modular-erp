//! **The two lines §9c said must stay true.**
//!
//! The decision was that `hr` claims are *domain* claims: they answer "may you
//! approve this particular thing", checked inside module commands, while the
//! control plane keeps answering "may you reach this endpoint at all".
//!
//! That decision is not enforced by any type. It is enforced by nobody having
//! written the line that breaks it — which is exactly the kind of decision that
//! erodes six months later, one reasonable-looking commit at a time. So it is
//! a test.
//!
//! Source-scanning, like `erp-api/tests/creates.rs`: crude, and it reads what a
//! reviewer would have to read.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("modules/hr sits two below the workspace root")
        .to_path_buf()
}

fn sources(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(sources(&path));
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            out.push((path, text));
        }
    }
    out
}

/// **No `hr` type appears in `erp-control` or `erp-web`.**
///
/// If one does, the org chart has started deciding platform access — decision
/// (b), which was rejected because it needs an employee-to-identity mapping and
/// a tenant-wide session invalidation on every re-parent, and because it puts a
/// customer-editable tree in front of the platform's own authorization.
///
/// It is also the harder direction to leave: claims that turn out to belong at
/// the edge can be promoted later, and a control-plane hierarchy shipped first
/// cannot be quietly demoted.
#[test]
fn the_platform_does_not_know_what_an_employee_is() {
    let root = workspace();
    let mut offenders = Vec::new();

    for crate_name in ["erp-control", "erp-web", "erp-tenant"] {
        let dir = root.join("crates").join(crate_name).join("src");
        for (path, text) in sources(&dir) {
            for (n, line) in text.lines().enumerate() {
                // Comments are where this decision is *explained*, and the
                // explanations name `hr` on purpose.
                let code = line.split("//").next().unwrap_or("");
                if code.contains("hr::") || code.contains("use hr") {
                    offenders.push(format!(
                        "{}:{} names `hr` in {crate_name}",
                        path.display(),
                        n + 1
                    ));
                }
            }
        }

        let manifest =
            std::fs::read_to_string(root.join("crates").join(crate_name).join("Cargo.toml"))
                .unwrap_or_default();
        assert!(
            !manifest.contains("\nhr = "),
            "{crate_name} depends on `hr`. Authorization has crossed the plane \
             boundary that §9c decided it would not."
        );
    }

    assert!(
        offenders.is_empty(),
        "the control plane has started knowing about the org chart:\n  {}",
        offenders.join("\n  ")
    );
}

/// **No org-chart event invalidates a session.**
///
/// The other half of the same decision. `shared.rs` warns that a stale *logout*
/// is the one thing the session cache must not serve; a stale *promotion* would
/// have joined it, and nothing in `hr` may reach for that lever.
///
/// A promotion taking effect immediately is what this buys: the claim is
/// write-side state read at the moment of the decision, so there is no cache to
/// be stale in the first place.
#[test]
fn promoting_somebody_does_not_touch_the_session_cache() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    for (path, text) in sources(&dir) {
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for reach in [
                "Invalidate::",
                "apply_invalidation",
                "forget(",
                "ControlPlane",
            ] {
                if code.contains(reach) {
                    offenders.push(format!(
                        "{}:{} reaches for `{reach}`",
                        path.display(),
                        n + 1
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "`hr` has started invalidating platform state:\n  {}",
        offenders.join("\n  ")
    );
}

/// **A claim check reads write-side state, never a projection.**
///
/// The reason is the same one that makes `sales` validate a customer against
/// `crm`'s log: a command deciding whether somebody may approve something
/// cannot read a table that may be a second behind, and a claim revoked a
/// moment ago has to bite now.
///
/// So nothing in `claims.rs` may name `proj_hr`.
#[test]
fn an_authorization_answer_never_comes_from_a_read_model() {
    let claims =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/claims.rs"))
            .expect("claims.rs is there");

    for (n, line) in claims.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !code.contains("proj_"),
            "claims.rs:{} reads a projection. An authorization answer that can \
             be a second behind is one that lets a revoked claim keep working.",
            n + 1
        );
    }
}
