/// Binary protocol for HugiMQ UDP pub/sub.
///
/// Header layout (24 bytes):
///   [0..4]   msg_type    (u32, little-endian)
///   [4..8]   topic_id    (u32)
///   [8..16]  sequence    (u64)
///   [16..24] timestamp_ns (u64) — present in DATA and REPLY
///
/// Message types:
///   1 = DATA        (publisher -> server -> subscribers)
///   2 = SUBSCRIBE   (subscriber -> server)
///   3 = NACK        (subscriber -> server, request retransmission)
///   4 = RETRANSMIT  (server -> subscriber, re-sending a DATA packet)
///   5 = ACK_CTRL    (control-plane ack for SUBSCRIBE/NACK confirmation)

use std::mem;

pub const HEADER_SIZE: usize = 24;
pub const PAYLOAD_OFFSET: usize = HEADER_SIZE;

/// Message type constants
pub const MSG_DATA: u32 = 1;
pub const MSG_SUBSCRIBE: u32 = 2;
pub const MSG_NACK: u32 = 3;
pub const MSG_RETRANSMIT: u32 = 4;
pub const MSG_ACK_CTRL: u32 = 5;

/// Wire header — exactly 24 bytes, #[repr(C)] for zero-copy layout.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub msg_type: u32,
    pub topic_id: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
}

/// Benchmarker payload — fixed-size data after the header.
/// This is what publishers fill and subscribers validate.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BenchPayload {
    pub topic_id: u32,
    pub sequence: u64,
    pub send_timestamp_ns: u64,
}

impl Header {
    pub fn serialize(&self, buf: &mut [u8]) {
        assert!(buf.len() >= HEADER_SIZE);
        buf[0..4].copy_from_slice(&self.msg_type.to_le_bytes());
        buf[4..8].copy_from_slice(&self.topic_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.sequence.to_le_bytes());
        buf[16..24].copy_from_slice(&self.timestamp_ns.to_le_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Self {
        assert!(buf.len() >= HEADER_SIZE);
        Header {
            msg_type: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            topic_id: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            sequence: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            timestamp_ns: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        }
    }
}

impl BenchPayload {
    pub fn serialize(&self, buf: &mut [u8]) {
        assert!(buf.len() >= mem::size_of::<Self>());
        buf[0..4].copy_from_slice(&self.topic_id.to_le_bytes());
        buf[4..12].copy_from_slice(&self.sequence.to_le_bytes());
        buf[12..20].copy_from_slice(&self.send_timestamp_ns.to_le_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Self {
        assert!(buf.len() >= mem::size_of::<Self>());
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

/// A full packet: header + optional payload bytes.
/// The total wire size is HEADER_SIZE + payload_bytes.
pub struct Packet {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_SIZE + self.payload.len()];
        self.header.serialize(&mut buf);
        buf[HEADER_SIZE..].copy_from_slice(&self.payload);
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Self {
        let header = Header::deserialize(buf);
        let payload = buf[HEADER_SIZE..].to_vec();
        Packet { header, payload }
    }
}

/// Control-plane messages (SUBSCRIBE, NACK, ACK_CTRL).
/// These use the header fields directly; the "payload" area carries extra control data.

/// SUBSCRIBE body (sent after header, topic_id in header identifies the topic).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SubscribeMsg {
    pub subscriber_addr: [u8; 18], // 4 bytes IP + 2 bytes port + 12 padding for alignment
}

/// NACK body — requests retransmission of [start_seq..end_seq] for a topic.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NackMsg {
    pub start_seq: u64,
    pub end_seq: u64,
}

impl SubscribeMsg {
    pub fn serialize(&self, buf: &mut [u8]) {
        let bytes = unsafe { std::slice::from_raw_parts(self as *const _ as *const u8, mem::size_of::<Self>()) };
        buf[..mem::size_of::<Self>()].copy_from_slice(bytes);
    }

    pub fn deserialize(buf: &[u8]) -> Self {
        let mut inner = SubscribeMsg {
            subscriber_addr: [0u8; 18],
        };
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(&mut inner as *mut _ as *mut u8, mem::size_of::<Self>())
        };
        bytes.copy_from_slice(&buf[..mem::size_of::<Self>()]);
        inner
    }
}

impl NackMsg {
    pub fn serialize(&self, buf: &mut [u8]) {
        buf[0..8].copy_from_slice(&self.start_seq.to_le_bytes());
        buf[8..16].copy_from_slice(&self.end_seq.to_le_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Self {
        NackMsg {
            start_seq: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            end_seq: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        }
    }
}
