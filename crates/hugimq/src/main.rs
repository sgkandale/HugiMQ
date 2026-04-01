use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
    Router,
};
use dashmap::DashMap;
use futures::stream::Stream;
use rmp_serde::from_slice;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, sync::Arc};
use tokio::sync::broadcast;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Message {
    payload: String,
}

struct AppState {
    topics: DashMap<String, broadcast::Sender<Message>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state = Arc::new(AppState {
        topics: DashMap::new(),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/publish/:topic", post(publish))
        .route("/subscribe/:topic", get(subscribe))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:6379").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> &'static str {
    "OK"
}

async fn publish(
    State(state): State<Arc<AppState>>,
    Path(topic): Path<String>,
    bytes: Bytes,
) -> Result<&'static str, StatusCode> {
    let payload: Message = from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    let tx = state.topics.entry(topic).or_insert_with(|| {
        let (tx, _rx) = broadcast::channel(1024 * 64);
        tx
    });
    let _ = tx.send(payload);
    Ok("OK")
}

async fn subscribe(
    State(state): State<Arc<AppState>>,
    Path(topic): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tx = state.topics.entry(topic).or_insert_with(|| {
        let (tx, _rx) = broadcast::channel(1024 * 64);
        tx
    });
    let mut rx = tx.subscribe();

    let stream = async_stream::stream! {
        while let Ok(msg) = rx.recv().await {
            yield Ok(Event::default().data(msg.payload));
        }
    };

    Sse::new(stream)
}
