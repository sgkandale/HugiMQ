use axum::{
    extract::{Path, State},
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, sync::Arc};
use tokio::sync::broadcast;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Message {
    payload: String,
}

struct AppState {
    tx: broadcast::Sender<Message>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let (tx, _rx) = broadcast::channel(1024 * 64); // Large buffer for high throughput

    let state = Arc::new(AppState { tx });

    let app = Router::new()
        .route("/health", get(health))
        .route("/publish/:topic", post(publish))
        .route("/subscribe/:topic", get(subscribe))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> &'static str {
    "OK"
}

async fn publish(
    State(state): State<Arc<AppState>>,
    Path(_topic): Path<String>,
    Json(payload): Json<Message>,
) -> &'static str {
    let _ = state.tx.send(payload);
    "OK"
}

async fn subscribe(
    State(state): State<Arc<AppState>>,
    Path(_topic): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.tx.subscribe();

    let stream = async_stream::stream! {
        while let Ok(msg) = rx.recv().await {
            yield Ok(Event::default().data(msg.payload));
        }
    };

    Sse::new(stream)
}
