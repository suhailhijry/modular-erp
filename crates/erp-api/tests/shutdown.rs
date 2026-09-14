//! **The API drains on the signal an orchestrator sends.**
//!
//! `bin/api.rs` waited on `tokio::signal::ctrl_c` alone until 2026-09-14. An
//! orchestrator stops a pod with SIGTERM, which that future never resolves on,
//! so every deploy killed the process with requests in flight — a 502 to a
//! customer on every release, while the worker beside it drained politely on
//! `shutdown_signal`. Both binaries now take the one signal `erp-control`
//! defines, and this is what keeps the API on it: a source scan, because a
//! test that sends a real SIGTERM to a spawned API needs a database, a listener
//! and a race, and the property is a single line.

#![allow(clippy::expect_used)]

#[test]
fn the_api_drains_on_the_shared_shutdown_signal() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/bin/api.rs"))
        .expect("bin/api.rs is where the API process starts");
    // Comments explaining why *not* to wait on Ctrl-C are the common case.
    let code: Vec<&str> = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(line))
        .collect();

    assert!(
        code.iter().any(|line| line.contains("shutdown_signal()")),
        "bin/api.rs no longer drains on `erp_control::shutdown_signal`, the token \
         SIGTERM and Ctrl-C both cancel; a deploy will cut requests in flight"
    );
    assert!(
        !code.iter().any(|line| line.contains("signal::ctrl_c")),
        "bin/api.rs waits on Ctrl-C, which an orchestrator never sends"
    );
}
