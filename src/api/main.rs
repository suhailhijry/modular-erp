use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::json;
use spa::{
    accounting::{self},
    event_sourcing::{
        pg_checkpoint_store::PgCheckpointStore, pg_event_store::PgEventStore,
        pg_notify_event_bus::PgNotifyEventBus,
    },
    platform::{CommandQueue, app_state::AppState},
};
use sqlx::PgPool;

#[tokio::main]
async fn main() {
    // initialize tracing
    tracing_subscriber::fmt::init();

    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap().to_string())
        .await
        .unwrap();

    let event_store = Arc::new(PgEventStore::new(pool.clone()));
    let checkpoint_store = Arc::new(PgCheckpointStore::new(pool.clone()));
    let event_bus = Arc::new(PgNotifyEventBus::new(pool.clone(), "main"));
    let queue = Arc::new(CommandQueue::new(128, 4096));

    let app_state = AppState {
        pool,
        event_store,
        checkpoint_store,
        event_bus,
        queue,
    };

    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root))
        // `POST /users` goes to `create_user`
        .route("/events", get(events))
        .merge(accounting::http::routes())
        .with_state(app_state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// basic handler that responds with a static string
async fn root() -> &'static str {
    "Hello, World!"
}

async fn events(State(pool): State<PgPool>) -> (StatusCode, Json<serde_json::Value>) {
    if let Ok(rows) = sqlx::query!("SELECT * FROM events").fetch_all(&pool).await {
        let result: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "id": row.id,
                    "event": row.event_name,
                })
            })
            .collect();
        (StatusCode::OK, Json(serde_json::Value::Array(result)))
    } else {
        (StatusCode::OK, Json(json!({})))
    }
}
