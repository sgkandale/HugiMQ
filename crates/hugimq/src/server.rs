/// HugiMQ server — UDP pub/sub with NACK-based reliability.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::net::UdpSocket;

use crate::protocol::*;
use crate::ring_buffer::RetransmitRing;

#[derive(Clone, Debug)]
pub struct Subscriber {
    pub addr: SocketAddr,
}

pub struct ServerState {
    pub topics: Mutex<HashMap<u32, Vec<Subscriber>>>,
    pub rings: Mutex<HashMap<u32, Arc<RetransmitRing>>>,
}

impl ServerState {
    pub fn new() -> Self {
        ServerState {
            topics: Mutex::new(HashMap::new()),
            rings: Mutex::new(HashMap::new()),
        }
    }

    pub fn subscribe(&self, topic_id: u32, subscriber: Subscriber) {
        let mut topics = self.topics.lock().unwrap();
        topics.entry(topic_id).or_insert_with(Vec::new).push(subscriber);
    }

    pub fn get_subscribers(&self, topic_id: u32) -> Vec<Subscriber> {
        let topics = self.topics.lock().unwrap();
        topics.get(&topic_id).cloned().unwrap_or_default()
    }

    pub fn get_or_create_ring(&self, topic_id: u32, capacity: usize, max_payload: usize) -> Arc<RetransmitRing> {
        let mut rings = self.rings.lock().unwrap();
        rings
            .entry(topic_id)
            .or_insert_with(|| RetransmitRing::new(capacity, max_payload))
            .clone()
    }

    pub fn store_for_retransmit(&self, topic_id: u32, data: &[u8], sequence: u64) {
        let ring = self.get_or_create_ring(topic_id, 65536, 256);
        ring.push(data, sequence);
    }

    pub fn fulfill_nack(&self, topic_id: u32, seq: u64) -> Option<Vec<u8>> {
        let rings = self.rings.lock().unwrap();
        if let Some(ring) = rings.get(&topic_id) {
            return ring.get(seq);
        }
        None
    }
}

const MAX_PACKET_SIZE: usize = 4096;

pub async fn run_server(listen_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(ServerState::new());
    let socket = UdpSocket::bind(listen_addr).await?;
    println!("HugiMQ server listening on {}", listen_addr);

    let mut buf = [0u8; MAX_PACKET_SIZE];

    loop {
        let (len, remote_addr) = socket.recv_from(&mut buf).await?;
        let packet = &buf[..len];

        if len < HEADER_SIZE {
            continue;
        }

        let header = Header::deserialize(packet);

        match header.msg_type {
            MSG_SUBSCRIBE => {
                let topic_id = header.topic_id;
                let subscriber = Subscriber { addr: remote_addr };
                state.subscribe(topic_id, subscriber);

                let ack = Packet {
                    header: Header {
                        msg_type: MSG_ACK_CTRL,
                        topic_id,
                        sequence: 0,
                        timestamp_ns: 0,
                    },
                    payload: vec![],
                };
                let _ = socket.send_to(&ack.to_bytes(), remote_addr).await;
            }

            MSG_NACK => {
                let topic_id = header.topic_id;
                if len < HEADER_SIZE + 16 {
                    continue;
                }
                let nack = NackMsg::deserialize(&packet[HEADER_SIZE..]);

                for seq in nack.start_seq..=nack.end_seq {
                    if let Some(data) = state.fulfill_nack(topic_id, seq) {
                        let mut retransmit_packet = data.clone();
                        retransmit_packet[0..4].copy_from_slice(&MSG_RETRANSMIT.to_le_bytes());
                        let _ = socket.send_to(&retransmit_packet, remote_addr).await;
                    }
                }
            }

            MSG_DATA => {
                let topic_id = header.topic_id;

                let subscribers = state.get_subscribers(topic_id);
                let wire_bytes = packet.to_vec();

                for sub in &subscribers {
                    let _ = socket.send_to(&wire_bytes, sub.addr).await;
                }

                state.store_for_retransmit(topic_id, packet, header.sequence);
                state.get_or_create_ring(topic_id, 65536, 256);
            }

            _ => {}
        }
    }
}