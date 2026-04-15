/// Shared protocol definitions — must match the hugimq server exactly.
///
/// Header layout (24 bytes):
///   [0..4]   msg_type    (u32)
///   [4..8]   topic_id    (u32)
///   [8..16]  sequence    (u64)
///   [16..24] timestamp_ns (u64)

use std::mem;

pub const HEADER_SIZE: usize = 24;

/// Message types
pub const MSG_DATA: u32 = 1;
pub const MSG_SUBSCRIBE: u32 = 2;
pub const MSG_NACK: u32 = 3;
pub const MSG_RETRANSMIT: u32 = 4;
pub const MSG_ACK_CTRL: u32 = 5;

/// Benchmarker payload — 20 bytes
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BenchPayload {
    pub topic_id: u32,
    pub sequence: u64,
    pub send_timestamp_ns: u64,
}

impl BenchPayload {
    pub fn serialize(&self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&self.topic_id.to_le_bytes());
        buf[4..12].copy_from_slice(&self.sequence.to_le_bytes());
        buf[12..20].copy_from_slice(&self.send_timestamp_ns.to_le_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Self {
        BenchPayload {
            topic_id: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            sequence: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
            send_timestamp_ns: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
        }
    }

    pub fn size() -> usize {
        mem::size_of::<Self>()
    }
}

/// Build a complete wire packet (header + payload).
pub fn build_data_packet(topic_id: u32, sequence: u64, timestamp_ns: u64) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + BenchPayload::size()];
    // Header
    buf[0..4].copy_from_slice(&MSG_DATA.to_le_bytes());
    buf[4..8].copy_from_slice(&topic_id.to_le_bytes());
    buf[8..16].copy_from_slice(&sequence.to_le_bytes());
    buf[16..24].copy_from_slice(&timestamp_ns.to_le_bytes());
    // Payload
    let payload = BenchPayload {
        topic_id,
        sequence,
        send_timestamp_ns: timestamp_ns,
    };
    payload.serialize(&mut buf[HEADER_SIZE..]);
    buf
}

pub fn build_subscribe_packet(topic_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE];
    buf[0..4].copy_from_slice(&MSG_SUBSCRIBE.to_le_bytes());
    buf[4..8].copy_from_slice(&topic_id.to_le_bytes());
    buf[8..16].copy_from_slice(&0u64.to_le_bytes());
    buf[16..24].copy_from_slice(&0u64.to_le_bytes());
    buf
}

pub fn build_nack_packet(topic_id: u32, start_seq: u64, end_seq: u64) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + 16];
    buf[0..4].copy_from_slice(&MSG_NACK.to_le_bytes());
    buf[4..8].copy_from_slice(&topic_id.to_le_bytes());
    buf[8..16].copy_from_slice(&0u64.to_le_bytes());
    buf[16..24].copy_from_slice(&0u64.to_le_bytes());
    buf[24..32].copy_from_slice(&start_seq.to_le_bytes());
    buf[32..40].copy_from_slice(&end_seq.to_le_bytes());
    buf
}

/// Parse an incoming packet. Returns (msg_type, topic_id, sequence, timestamp, payload_opt).
pub fn parse_packet(buf: &[u8]) -> Option<(u32, u32, u64, u64, Option<BenchPayload>)> {
    if buf.len() < HEADER_SIZE {
        return None;
    }
    let msg_type = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    let topic_id = u32::from_le_bytes(buf[4..8].try_into().ok()?);
    let sequence = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    let timestamp_ns = u64::from_le_bytes(buf[16..24].try_into().ok()?);

    let payload = if buf.len() >= HEADER_SIZE + BenchPayload::size() {
        Some(BenchPayload::deserialize(&buf[HEADER_SIZE..]))
    } else {
        None
    };

    Some((msg_type, topic_id, sequence, timestamp_ns, payload))
}
