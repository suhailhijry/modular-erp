//! The one signal every long-lived process drains on.
//!
//! It lived in `erp-worker`, and the API listened for Ctrl-C alone — so every
//! deploy, which sends SIGTERM, cut the API's requests in flight while the
//! worker beside it drained politely. One function, here, because both
//! binaries depend on this crate and neither should have its own idea of what
//! an orchestrator's stop means.

use tokio_util::sync::CancellationToken;

/// A token cancelled by SIGTERM or SIGINT.
///
/// SIGTERM is what an orchestrator sends; SIGINT is Ctrl-C. Both mean the same
/// thing here, and treating them differently is how a local run behaves unlike
/// production.
///
/// A **second** signal aborts immediately. An operator pressing Ctrl-C twice
/// means it, and a drain that will not finish must not be the only way out.
#[must_use]
pub fn shutdown_signal() -> CancellationToken {
    let token = CancellationToken::new();
    let listener = token.clone();

    tokio::spawn(async move {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::error!(error = %e, "could not install the SIGTERM handler");
                    return;
                }
            };

        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM received; draining"),
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => tracing::info!("interrupt received; draining"),
                Err(e) => {
                    tracing::error!(error = %e, "could not listen for interrupts");
                    return;
                }
            },
        }
        listener.cancel();

        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        tracing::warn!("second signal; exiting without finishing the drain");
        std::process::exit(130);
    });

    token
}
