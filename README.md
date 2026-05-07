# HugiMQ: Racing the Wire ⚡

**HugiMQ** is a high-performance distributed pub/sub research engine implemented in Rust, designed to push the absolute physical limits of network throughput and latency. 

By systematically eliminating "Software Taxes" - memory copies, syscall overhead, and lock contention - HugiMQ achieves **physical I/O saturation**, sustaining over **25M+ msg/s** and **43 Gbps** of raw bandwidth on high-end cloud hardware with **zero message loss**.

---

## 🚀 Performance Highlights

HugiMQ was benchmarked against Redis 7.x on AWS `hpc7g` (Graviton3E) nodes with 100 Gbps network interfaces.

| Metric | Redis 7.x | HugiMQ (Persistent) | Improvement |
| :--- | :--- | :--- | :--- |
| **Avg Bandwidth** | 17.56 Gbps | **43.55 Gbps** | **+148%** |
| **Peak Bandwidth** | 19.66 Gbps | **46.54 Gbps** | **+136%** |
| **Throughput** | ~600K msg/s | **25M+ msg/s** | **~40x** |
| **Message Loss** | 0% | **0%** | ✅ |

> **Note:** HugiMQ is limited by physical capacity. At 46 Gbps, it saturates the nitro ceiling of cloud instances.

---

## 🏗️ Architectural Pillars

HugiMQ's performance is built on four core engineering principles:

### 1. Mechanical Sympathy
Software structures are aligned to hardware realities. Using `#[repr(align(64))]`, hot data structures (like `Topic`) are kept from straddling cache lines, preventing "False Sharing" and L1/L2 cache invalidation jitter.

### 2. Lock-Free Topic Registry
The routing path is 100% lock-free. By utilizing the `ArcSwap` pattern, the topic registry is treated as an immutable snapshot. Message routing requires only a single atomic load instruction, allowing millions of publishes per second without thread stalling or lock thrashing.

### 3. Single-Encoding Broadcast (Zero-Copy)
Traditional systems encode messages uniquely for every subscriber. HugiMQ implements "Write-Once-Read-Many" at the application layer. A batch of messages is encoded exactly once per topic into an `Arc<Bytes>` buffer, which is then shared across all subscriber tasks, reducing memory traffic by up to 6.25x.

### 4. Persistent Topic Workers
To avoid "Tokio Worker Starvation" and task churn, HugiMQ uses a persistent worker model. Each topic has a dedicated, long-running task that handles encoding and broadcasting, ensuring a stable CPU/Memory footprint even during 100M+ message bursts.

---

## 📜 Protocol Evolution

The development of HugiMQ spanned multiple "Epochs," each addressing a specific performance barrier:

*   **Epoch 1 (High-Level)**: Evaluated HTTP/1.1 and gRPC. Found that "Software Taxes" and HTTP/2 framing overhead limited throughput to ~2.7M msg/s.
*   **Epoch 2 (WebSockets)**: Hypothesized that removing multiplexing would help, but found it 3.1x slower than gRPC due to the lack of native pipelining.
*   **Epoch 3 (Raw TCP & Contention Mastery)**: Transitioned to a minimalistic binary protocol. Hit the "Internal Contention Wall" at the routing layer, resolved by moving to a 100% lock-free registry with `ArcSwap`.
*   **Epoch 4 (I/O Efficiency & 25M msg/s)**: Eliminated per-subscriber encoding tax via **Single-Encoding Broadcast**, reaching the 25M msg/s milestone on AWS `c6in` hardware.
*   **Epoch 5 (Stability & HPC Saturation)**: Solved "Task Explosion" and "The OOM Trap" using **Persistent Topic Workers**, achieving **43.55 Gbps** on Graviton3E (HPC) nodes.

> **Research Note:** We also explored UDP-based approaches (Aeron and Custom NACK-based retransmission). While Aeron achieved 0% loss, it was limited by relay architecture overhead. Raw TCP with optimized batching was ultimately chosen for its superior throughput-to-complexity ratio.

---

## 🛠️ Getting Started

### Prerequisites
- Rust (Latest Stable)
- Linux (Optimized for `io_uring` and high-performance networking)

### Installation
```bash
git clone https://github.com/your-username/hugimq_scratch.git
cd hugimq_scratch
cargo build --release
```

### Running the Server
```bash
# Default port 6379
./target/release/hugimq-server
```

### Benchmarking
HugiMQ includes a high-performance benchmarker capable of saturating 100Gbps links.

```bash
# Run TCP benchmark (20 connections, 1M messages per connection)
cargo run --release --package benchmarker -- tcp --connections 20 --messages-per-conn 1000000 --payload-size 128
```

---

## 📚 Technical Background

For a deep dive into the engineering decisions and performance analysis, see:
*   [Racing the Wire: Achieving Physical I/O Saturation](./racing_the_wire.pdf) (Research Paper)
*   [RESEARCH_PAPER.md](./RESEARCH_PAPER.md) (Detailed Implementation Notes)
*   [RESEARCH_LOG.md](./RESEARCH_LOG.md) (Iterative Benchmark Data)

