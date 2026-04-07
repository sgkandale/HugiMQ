pub mod hugimq {
    tonic::include_proto!("hugimq");
}

use dashmap::DashMap;
use futures::stream::Stream;
use hugimq::hugi_mq_service_server::{HugiMqService, HugiMqServiceServer};
use hugimq::{PublishRequest, PublishResponse, SubscribeRequest, SubscribeResponse};
use std::{pin::Pin, sync::Arc};
use tokio::sync::broadcast;
use tonic::{transport::Server, Request, Response, Status};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Clone)]
struct Message {
    payload: bytes::Bytes,
}

struct AppState {
    topics: DashMap<String, broadcast::Sender<Message>>,
}

pub struct HugiMQServer {
    state: Arc<AppState>,
}

#[tonic::async_trait]
impl HugiMqService for HugiMQServer {
    type SubscribeStream = Pin<Box<dyn Stream<Item = Result<SubscribeResponse, Status>> + Send>>;

    async fn publish(
        &self,
        request: Request<PublishRequest>,
    ) -> Result<Response<PublishResponse>, Status> {
        let req = request.into_inner();
        let topic = req.topic;
        let payload = Message { payload: bytes::Bytes::from(req.payload) };

        // Fast-path: check if topic already exists with a read-lock
        let tx = if let Some(tx) = self.state.topics.get(&topic) {
            tx.clone()
        } else {
            // Slow-path: create the topic with an entry lock
            self.state.topics.entry(topic).or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(1024 * 256);
                tx
            }).clone()
        };

        let _ = tx.send(payload);
        Ok(Response::new(PublishResponse { ok: true }))
    }

    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let req = request.into_inner();
        let topic = req.topic;

        // Same optimization for subscribe
        let tx = if let Some(tx) = self.state.topics.get(&topic) {
            tx.clone()
        } else {
            self.state.topics.entry(topic).or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(1024 * 256);
                tx
            }).clone()
        };
        let mut rx = tx.subscribe();

        let stream = async_stream::stream! {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        yield Ok(SubscribeResponse { payload: msg.payload.into() });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Consumer lagged by {} messages", n);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(stream)))
    }
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

    let server = HugiMQServer { state };

    let addr = "0.0.0.0:6379".parse().unwrap();
    tracing::info!("listening on {}", addr);

    Server::builder()
        .add_service(HugiMqServiceServer::new(server))
        .serve(addr)
        .await
        .unwrap();
}
