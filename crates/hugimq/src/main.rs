pub mod hugimq {
    tonic::include_proto!("hugimq");
}

use dashmap::DashMap;
use futures::stream::Stream;
use futures::StreamExt;
use hugimq::hugi_mq_service_server::{HugiMqService, HugiMqServiceServer};
use hugimq::{PublishRequest, PublishResponse, SubscribeRequest, SubscribeResponse};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tonic::{transport::Server, Request, Response, Status};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Shared message payload — reference-counted to avoid copying the underlying
/// byte buffer when fanning out to multiple subscribers.
#[derive(Debug, Clone)]
struct Message {
    payload: Arc<bytes::Bytes>,
}

/// Per-subscriber bounded channel capacity.
///
/// This creates natural backpressure: when a consumer's queue fills up,
/// the producer awaits space instead of flooding memory with unbounded
/// messages. This prevents bufferbloat and the resulting consumer timeouts.
const SUBSCRIBER_CHANNEL_CAPACITY: usize = 4096;

/// A single topic with per-subscriber mpsc channels.
///
/// # Why not `broadcast`?
/// `tokio::sync::broadcast::send()` acquires a `Mutex` on the shared ring-buffer
/// tail, serializing ALL producers to the same topic through a single lock.
/// At multi-million msg/s this becomes the dominant bottleneck.
///
/// Here we use an `RwLock<Vec<mpsc::Sender>>` instead:
///   - The read-lock allows unlimited concurrent producers (no serialization).
///   - Each `mpsc::Sender::send()` operates on an independent bounded channel,
///     providing per-subscriber backpressure without global contention.
struct Topic {
    /// Subscribers list — read lock is held briefly to clone senders,
    /// then released before any async sends occur.
    subscribers: tokio::sync::RwLock<Vec<mpsc::Sender<Message>>>,
    /// Tracks how many subscribers exist (atomic snapshot for fast-path logging).
    subscriber_count: AtomicUsize,
}

struct AppState {
    topics: DashMap<String, Arc<Topic>>,
}

pub struct HugiMQServer {
    state: Arc<AppState>,
}

/// Hand-written `Stream` implementation wrapping an `mpsc::UnboundedReceiver`.
///
/// This avoids the state-machine overhead of `async_stream::stream!` which
/// generates extra boilerplate for yield/resume transitions. A direct wrapper
/// is a single `Poll` match per message.
struct SubscribeStream {
    rx: mpsc::Receiver<Message>,
}

impl Stream for SubscribeStream {
    type Item = Result<SubscribeResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.rx).poll_recv(cx) {
            Poll::Ready(Some(msg)) => Poll::Ready(Some(Ok(SubscribeResponse {
                payload: msg.payload.as_ref().clone().into(),
            }))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[tonic::async_trait]
impl HugiMqService for HugiMQServer {
    type SubscribeStream = Pin<Box<dyn Stream<Item = Result<SubscribeResponse, Status>> + Send>>;

    async fn publish(
        &self,
        request: Request<tonic::Streaming<PublishRequest>>,
    ) -> Result<Response<PublishResponse>, Status> {
        let mut stream = request.into_inner();

        // Per-stream cache for topics to avoid global map contention.
        // 4-slot LRU-ish cache for hot topics.
        let mut cache: Vec<(String, Arc<Topic>)> = Vec::with_capacity(4);

        while let Some(req) = stream.next().await {
            let req = req?;
            let topic_name = req.topic;

            // Check cache first
            let topic = if let Some(found) = cache.iter().find(|(name, _)| name == &topic_name) {
                found.1.clone()
            } else {
                let topic = self.get_or_create_topic(&topic_name);

                if cache.len() >= 4 {
                    cache.remove(0);
                }
                cache.push((topic_name.clone(), topic.clone()));
                topic
            };

            let msg = Message {
                payload: Arc::new(bytes::Bytes::from(req.payload)),
            };

            // Phase 1: Clone senders under the read lock (fast), then release.
            // We must release the lock before any .await calls to avoid deadlocking
            // with concurrent subscriber registration/cleanup.
            let subs: Vec<mpsc::Sender<Message>> = {
                let subs = topic.subscribers.read().await;
                subs.iter().cloned().collect()
            };

            // Phase 2: Send to each subscriber with backpressure.
            // If a channel is full, we await space — this slows the producer
            // to the consumer's pace instead of flooding memory or dropping messages.
            let mut dead_senders = Vec::new();
            for (i, sub) in subs.iter().enumerate() {
                if sub.send(msg.clone()).await.is_err() {
                    dead_senders.push(i);
                }
            }

            // Phase 3: Prune dead subscribers under write-lock if needed.
            if !dead_senders.is_empty() {
                let mut subs_mut = topic.subscribers.write().await;
                for i in dead_senders.into_iter().rev() {
                    subs_mut.swap_remove(i);
                }
                topic.subscriber_count.store(subs_mut.len(), Ordering::Relaxed);
            }
        }

        Ok(Response::new(PublishResponse { ok: true }))
    }

    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let req = request.into_inner();
        let topic_name = req.topic;

        let topic = self.get_or_create_topic(&topic_name);
        let (tx, rx) = mpsc::channel(SUBSCRIBER_CHANNEL_CAPACITY);

        // Register this subscriber
        {
            let mut subs = topic.subscribers.write().await;
            subs.push(tx);
            topic.subscriber_count.fetch_add(1, Ordering::Relaxed);
        }

        let stream = SubscribeStream { rx };
        Ok(Response::new(Box::pin(stream)))
    }
}

impl HugiMQServer {
    fn get_or_create_topic(&self, topic: &str) -> Arc<Topic> {
        // Fast-path: read-only lookup (DashMap read-lock)
        if let Some(entry) = self.state.topics.get(topic) {
            return entry.clone();
        }

        // Slow-path: create if missing (DashMap entry lock)
        self.state
            .topics
            .entry(topic.to_string())
            .or_insert_with(|| {
                Arc::new(Topic {
                    subscribers: tokio::sync::RwLock::new(Vec::new()),
                    subscriber_count: AtomicUsize::new(0),
                })
            })
            .clone()
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
