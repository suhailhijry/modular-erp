//! The API process.

use std::sync::Arc;
use std::time::Duration;

use erp_api::{AppState, router};
use erp_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// The hard ceiling on any request body.
///
/// **A file upload is the largest thing this API takes**, and it is the reason
/// this is not a megabyte any more. The megabyte is still the default for every
/// other route — see `DefaultBodyLimit` below — so raising this ceiling did not
/// make every JSON endpoint a memory-exhaustion vector.
const MAX_BODY: usize = erp_storage::MAX_BYTES;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let control_url =
        std::env::var("CONTROL_DATABASE_URL").map_err(|_| "CONTROL_DATABASE_URL is not set")?;
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());

    // **Tenants are subdomains of this.** `bassat.erp.com` is one company and
    // `najd.erp.com` is another, which is why no path carries a tenant name.
    //
    // Defaults to `localhost` so a developer gets `acme.localhost` working with
    // no DNS and no `/etc/hosts` — every browser and curl resolve `*.localhost`
    // to the loopback already.
    let domain = std::env::var("PUBLIC_DOMAIN").unwrap_or_else(|_| "localhost".to_owned());

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect(&control_url)
        .await?;

    // Primary and, if this deployment has one, its read replica.
    let clusters = ClusterRegistry::from_env()?;
    // **Shared state, when this deployment runs more than one of these.**
    //
    // Sessions read from Redis instead of the control database on every
    // request, and a cache invalidation reaches every node rather than only
    // this one. Without `REDIS_URL` both fall back to what this system did
    // before — see `erp_control::shared`.
    let mut control = ControlPlane::new(pool, TenantPools::new(clusters, PoolConfig::from_env()));
    if let Some(shared) = erp_control::shared::Shared::from_env().await? {
        control = control.sharing(shared);
    }
    let control = Arc::new(control);
    let _invalidations = erp_control::shared::apply_invalidations_in_background(&control);

    // **Where open streams wait.** Only with Redis, because a projection
    // advance on a worker reaches an API node through it; without one the
    // stream routes refuse rather than sit silent.
    let realtime = if control.shared().is_some() {
        let caps = erp_web::realtime::Caps {
            staff_per_tenant: env_usize("REALTIME_STAFF_STREAMS_PER_TENANT", 256)?,
            public_per_tenant: env_usize("REALTIME_PUBLIC_STREAMS_PER_TENANT", 4096)?,
        };
        tracing::info!(?caps, "live streams enabled");
        Some(Arc::new(erp_web::realtime::Hub::new(caps)))
    } else {
        tracing::warn!("REDIS_URL is not set; nothing can be watched live");
        None
    };
    let _advances = realtime
        .as_ref()
        .and_then(|hub| erp_web::realtime::listen_in_background(&control, Arc::clone(hub)));

    // States what this process could demand against what the server allows.
    // Nothing wrote either number down before, which is how four processes each
    // holding a 400-permit budget against a 200-connection server went unnoticed.
    control.pools().report_budget("primary").await;

    // **The key module secrets are sealed under.** Optional, and its absence is
    // not a degraded mode: without it, anything that would store a tenant's
    // ZATCA signing key refuses rather than storing it in the clear.
    //
    // `<id>:<64 hex characters>`. The identifier is stored beside every row it
    // seals, so a rotation can find what it has not re-sealed yet — generate one
    // with `openssl rand -hex 32`.
    let mut state = AppState::on(control, &domain);
    // **Only behind a proxy you run.** Set when a load balancer this deployment
    // controls terminates connections and appends the client to
    // `X-Forwarded-For`; with it on and no proxy, every caller is whoever they
    // say. See `erp_web::extract::caller_address`.
    let trust_forwarded = std::env::var("TRUST_X_FORWARDED_FOR")
        .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
    if trust_forwarded {
        tracing::info!("trusting the last hop of X-Forwarded-For as the client address");
    } else {
        tracing::info!(
            "rate limiting by socket peer address; set TRUST_X_FORWARDED_FOR behind a proxy"
        );
    }
    state = state.trusting_forwarded_for(trust_forwarded);
    if let Ok(configured) = std::env::var("SEALING_KEY") {
        let key = erp_eventlog::SealingKey::parse(&configured)?;
        tracing::info!(key = ?key, "sealing key loaded");
        state = state.sealing_with(key);
    } else {
        tracing::warn!("SEALING_KEY is not set; anything that stores a tenant secret will refuse");
    }

    // **Where files go.** `S3_BUCKET` first, then `FILE_ROOT`, then nowhere —
    // and nowhere is a real answer: every file route refuses, which is the same
    // call the sealing key makes. A tenant told their contract uploaded when it
    // went into a container that is about to be replaced is worse served than
    // one told it did not.
    if let Some(storage) = erp_storage::from_env()? {
        tracing::info!(engine = storage.engine(), "storage configured");
        state = state.storing_in(storage);
    } else {
        tracing::warn!(
            "neither S3_BUCKET nor FILE_ROOT is set; the file routes will refuse uploads"
        );
    }

    if let Some(hub) = realtime {
        state = state.streaming_through(hub);
    }

    let app = router(state)
        .layer(TraceLayer::new_for_http())
        // 504, not 408: the request was fine, we were slow.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(RequestBodyLimitLayer::new(MAX_BODY));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "api listening");

    // Same drain discipline as the worker, on the same signal: stop accepting,
    // let in-flight requests finish. A request killed mid-transaction rolls
    // back, so this costs latency rather than correctness — but a 502 to a
    // customer mid-deploy is still a 502. It listened for Ctrl-C alone until
    // 2026-09-14, and an orchestrator sends SIGTERM, so every deploy was that
    // 502; `tests/shutdown.rs` keeps it on the shared signal.
    // `with_connect_info` is what gives the rate limiter a peer address when no
    // trusted proxy supplies one. Without it every caller shares one bucket.
    let shutdown = erp_control::shutdown_signal();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.cancelled().await;
        tracing::info!("shutting down");
    })
    .await?;

    Ok(())
}

/// A number from the environment, or its default; a value that is set and
/// does not parse stops the process rather than silently becoming the default.
fn env_usize(
    name: &str,
    default: usize,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    match std::env::var(name) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|e| format!("{name} is not a number: {e}").into()),
        Err(_) => Ok(default),
    }
}
