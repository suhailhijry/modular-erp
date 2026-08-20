//! Rebuilds this crate when a tenant migration changes.
//!
//! `sqlx::migrate!` embeds the migration files at **compile time**. Without
//! this, cargo has no idea the directory is an input: adding
//! `migrations/tenant/0004_*.sql` changes nothing it can see, the old migrator
//! stays baked into the binary, and `just migrate-fleet check` cheerfully
//! reports a fleet that is current against a migration it has never heard of.
//!
//! That is exactly the silent failure the fleet migrator exists to prevent,
//! sitting one layer underneath it. Found by adding a migration and watching
//! the check pass.

fn main() {
    watch("../../migrations/tenant");
}

/// Emits a rebuild trigger for a directory **and every file in it**.
///
/// Both, on purpose: cargo notices a directory when its mtime changes, which
/// covers adding and removing files but not editing one in place.
fn watch(dir: &str) {
    println!("cargo:rerun-if-changed={dir}");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }
}
