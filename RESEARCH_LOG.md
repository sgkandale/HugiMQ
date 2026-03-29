# HugiMQ Research Log & Benchmark Data

This file records every architectural iteration, the rationale behind it, and the resulting performance numbers.

## Baseline: Redis 7.x (Local)
- **Hardware:** Linux (Local Machine)
- **Throughput:** 332,904 msg/s
- **P50 E2E Latency:** 350,719 ns
- **P99 E2E Latency:** 792,063 ns
- **Notes:** Standard Redis Pub/Sub using `redis-rs` async client.

## Baseline: Redis 7.x (Cloud - AWS Spot)

### Iteration 1: Initial Cloud Run
- **Hardware:** AWS (Redis: t3.small, Benchmarker: c5.large)
- **AZ:** Different AZs
- **Throughput:** 262,789 msg/s
- **P50 E2E Latency:** 409,343 ns
- **P99 E2E Latency:** 695,807 ns
- **Notes:** Baseline distributed run with burstable instances and cross-AZ latency.

### Iteration 2: Optimized AZ & Hardware
- **Hardware:** AWS (Redis: c6i.large, Benchmarker: c6i.xlarge)
- **AZ:** Same AZ (us-east-1a)
- **Throughput:** 416,124 msg/s
- **P50 E2E Latency:** 274,943 ns
- **P99 E2E Latency:** 457,215 ns
- **Notes:** Significant improvement from Same AZ placement and compute-optimized hardware.

### Iteration 3: Scaled Compute & I/O Threading
- **Hardware:** AWS (Redis: c6i.xlarge, Benchmarker: c6i.2xlarge)
- **AZ:** Same AZ (us-east-1a)
- **Throughput:** 624,171 msg/s
- **P50 E2E Latency:** 175,103 ns
- **P99 E2E Latency:** 269,823 ns
- **Notes:** High-performance baseline. Enabled Redis I/O threads (2) and used larger instance classes. Sub-0.3ms P99 latency achieved.

---