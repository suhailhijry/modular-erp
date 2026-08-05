use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::json;
use spa::{
    accounting::{self, reactors},
    event_sourcing::{
        ProjectorMeta,
        composite_projector::CompositeProjector,
        kafka_relay_projector::{KafkaRelayProjector, run_kafka_listener, run_kafka_relay},
        pg_event_store::PgEventStore,
        projector::AlertSink,
    },
    platform::{CommandQueue, app_state::AppState},
};
use sqlx::postgres::PgPoolOptions;
use tower_http::cors::{AllowMethods, AllowOrigin, CorsLayer};

pub struct TracingAlertSink();

impl TracingAlertSink {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl AlertSink for TracingAlertSink {
    async fn alert(&self, message: &str) {
        tracing::error!(message = message, "failed to handle event")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // initialize tracing
    tracing_subscriber::fmt::init();

    let database_url = std::env::var("DATABASE_URL")?;
    let kafka_url = std::env::var("KAFKA_URL")?;
    let kafka_topic = std::env::var("KAFKA_TOPIC").unwrap_or_else(|_| "main".into());

    tracing::info!("connecting to Postgres");

    let read_pool = PgPoolOptions::new()
        .max_connections(32)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;

    let write_pool = PgPoolOptions::new()
        .max_connections(32)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;

    tracing::info!("running migrations");
    sqlx::migrate!("./migrations").run(&write_pool).await?;

    let event_store = Arc::new(PgEventStore::new(write_pool.clone(), read_pool.clone()));
    let queue = Arc::new(CommandQueue::new(128, 4096));

    tracing::info!("constructing Kafka producer (lazy, no connection yet)");
    let kafka_relay = Arc::new(KafkaRelayProjector::new(&kafka_url, &kafka_topic).await?);

    {
        let store = event_store.clone();
        let checkpoints = event_store.clone();
        tokio::spawn(async move {
            // The relay loops internally; an Err return here is an
            // infrastructure failure worth surfacing loudly. It does NOT
            // take the API down - commands keep succeeding against
            // Postgres; the relay resumes from its checkpoint when
            // restarted.
            if let Err(e) =
                run_kafka_relay(store, checkpoints, kafka_relay, Duration::from_millis(200)).await
            {
                tracing::error!(error = %e, "kafka relay exited with error - events are safe in Postgres, restart to resume publishing");
            }
        });
    }

    let reactors = reactors::get_reactors(write_pool.clone());
    let pipeline = Arc::new(CompositeProjector::new("accounting", reactors));
    let alert_sink = Arc::new(TracingAlertSink::new());

    let listener_event_store = event_store.clone();
    let listener_pool = write_pool.clone();
    tokio::spawn(async move {
        if let Err(e) = run_kafka_listener(
            kafka_url.to_string(),
            kafka_topic.as_ref(),
            pipeline.as_ref().name(),
            listener_event_store.clone(),
            listener_event_store.clone(),
            pipeline.clone(),
            listener_pool.clone(),
            alert_sink.clone(),
        )
        .await
        {
            tracing::error!(error = e.to_string(), "kafka listener failed");
        }
    });

    let app_state = AppState {
        write_pool,
        read_pool,
        event_store: event_store.clone(),
        checkpoint_store: event_store.clone(),
        event_bus: None,
        queue,
    };

    let cors = CorsLayer::new()
        .allow_methods(AllowMethods::any())
        .allow_origin(AllowOrigin::any());

    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root))
        // `POST /users` goes to `create_user`
        .route("/events", get(events))
        .merge(accounting::http::routes())
        .layer(cors)
        .with_state(app_state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("api shutdown");
    Ok(())
}

// basic handler that responds with a static string
async fn root() -> &'static str {
    "Hello, World!"
}

async fn events(State(state): State<AppState>) -> (StatusCode, Json<Vec<serde_json::Value>>) {
    if let Ok(rows) = sqlx::query!("SELECT * FROM events ORDER BY id")
        .fetch_all(&state.read_pool)
        .await
    {
        let result: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "id": row.id,
                    "event": row.event_name,
                    "payload": row.payload,
                    "created_at": row.created_at,
                })
            })
            .collect();
        (StatusCode::OK, Json(result))
    } else {
        (StatusCode::OK, Json(vec![]))
    }
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("ctrl_c handler") };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
