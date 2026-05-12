# Performance Characteristics

`bge-router` is a thin forwarding layer. Its own overhead is dominated by two
network hops (client → router → upstream) rather than any computation.
Understanding where time is spent helps distinguish router bottlenecks from
upstream bottlenecks.

## Router Overhead Breakdown

| Operation | Mechanism | Typical cost |
|-----------|-----------|-------------|
| Snapshot read (routing decision input) | `ArcSwap::load_full()` — atomic pointer load | ~10–50 ns |
| Routing policy evaluation | `O(n)` filter + `min_by_key` over upstream list | ~1–5 µs (n ≤ 33) |
| Request body buffering | `collect()` on Axum body stream | Proportional to body size; ~100 µs for a 10 KB embedding request |
| Additional network RTT | One extra TCP round trip (router → upstream) within the VPC | ~0.3–1 ms per hop within a single AZ; ~1–2 ms cross-AZ |
| Header filtering + injection | Iterate over hop-by-hop list; inject 2 response headers | < 10 µs |
| Response body streaming | Zero-copy `bytes_stream()` pipe | No additional latency beyond byte transmission time |

**Total router overhead at P50:** < 2 ms added to any embedding request. The
upstream's inference time (100 ms–minutes depending on batch size and hardware)
dwarfs this overhead.

## Latency Profile

### CPU Pool (Measured — Production Incident, May 2026)

These measurements come from a production load event where the GPU pool was
unavailable and all traffic was handled by the CPU pool at near-saturation
throughput.

| Metric | Value |
|--------|-------|
| Inference p50 (`inference_ms`) | ~105 s |
| Inference p99 (`inference_ms`) | ~235 s |
| Hardware | bge-m3-embedding-server, fp16, MLAS CPU EP |
| Workload | Document ingestion batch, mixed sequence lengths |

> **Note:** These are inference times reported by the upstream, not router
> latency. Router overhead is not separately measured in production; at < 2 ms
> it is lost in the noise of inference time.

### GPU Pool (Expected — Not Yet Measured in Production)

| Metric | Estimate | Basis |
|--------|----------|-------|
| Inference speedup vs CPU | 5–15× | NVIDIA GPU vs MLAS for transformer inference on typical embedding lengths |
| p50 inference (fp16, TensorRT) | 7–20 s | Extrapolated from CPU p50 ÷ 7× speedup |
| Cold start (first request after scale-out) | 2–6 min | bge-m3 model load + TensorRT engine compilation |
| Warm steady-state | Sub-second for typical batch sizes | Expected from GPU parallelism |

These GPU estimates will be replaced with measured values once the GPU pool
has been in production under real workloads.

## Router Memory Footprint

The router is intentionally lightweight. All state lives in two structures:

**`PoolSnapshot`** — a pair of `Vec<UpstreamInfo>` (GPU + CPU). Each
`UpstreamInfo` contains a `SocketAddr` (8 bytes), a `PoolType` enum (1 byte),
an `UpstreamStatus` enum (1 byte), two `u32` fields (8 bytes), and one
`Instant` (8 bytes). Total: ~30 bytes of data per upstream, plus Vec overhead.
At maximum scale (8 GPU + 25 CPU = 33 upstreams), the snapshot uses < 10 KB.
Two `Arc<PoolSnapshot>` exist at any moment during an atomic swap — roughly
20 KB peak.

**Request body buffer** — the only per-request allocation. Each buffered
request body lives in memory for the duration of the routing attempt:
connection setup + response headers from GPU (or CPU). For a 10 KB embedding
request this is ~10 KB × 2 Fargate tasks × concurrent request rate. At 100
concurrent requests per task, peak body buffer is ~1 MB per task.

The Fargate task definition allocates 512 CPU units / 1024 MiB memory. The
router itself uses well under 100 MB at peak; the allocation is sized to give
the OS and Tokio runtime headroom, not because the router needs it.

## Throughput and Queuing

`bge-router` does not add queuing. Every incoming request is immediately
forwarded to the selected upstream (or rejected with 503 if no upstream is
available). The upstream's `BGE_M3_WORKERS` setting and its internal
semaphore queue are the throughput bottleneck for the end-to-end system.

Because two Fargate router tasks run in parallel (`desiredCount: 2`), the
ALB distributes incoming requests across both. Each router task independently
reads the same `ArcSwap<PoolSnapshot>` and may select the same upstream for
concurrent requests — the upstream's queue depth absorbs this concurrency.

## Bottleneck Identification

Use these checks to determine whether latency problems originate in the router
or in an upstream:

1. **Check `/router/health`** for `queue_depth` on each upstream. High queue
   depths (> 10) mean upstreams are saturated; the router is working correctly.

2. **Correlate router WARN logs** (`GPU upstream timed out`, `GPU upstream
   returned 5xx`) with inference_ms spikes in the upstream's logs. If WARN
   rate is zero but latency is high, the bottleneck is upstream inference.

3. **CloudWatch Logs Insights** — use the `X-Bge-Router-Pool` response header
   (logged by the router) to separate GPU-routed vs CPU-routed request
   latency:

   ```
   fields route, total_ms, pool
   | filter ispresent(route)
   | stats pct(total_ms, 99) as p99_ms by pool
   | sort p99_ms desc
   ```

   > This query applies to **upstream** bge-m3 logs (which emit `total_ms`).
   > The router itself does not currently emit per-request timing events.

4. **Compare p50 vs p99** — a large P99/P50 ratio at the upstream level
   (e.g. p50 = 105 s, p99 = 235 s) indicates queue saturation, not model
   variance. Add CPU workers (`BGE_M3_WORKERS`) or GPU capacity to reduce
   the gap.

## Scaling Considerations

| Resource | Scaling action | Expected effect |
|----------|----------------|-----------------|
| Router tasks (currently 2) | Increase `desiredCount` | Reduces ALB connection concurrency per router; no upstream benefit |
| CPU pool tasks | Add bge-m3-embedding-server tasks | Linear throughput increase; router discovers them on next DNS refresh |
| GPU pool tasks | ECS autoscaling or manual scale-out | Dramatic latency improvement for long-sequence batches; 35 s discovery delay |
| `BGE_M3_WORKERS` per upstream | Increase (CPU tasks) | More concurrent inference workers per task; linear up to available vCPUs |
| `BGE_ROUTER_HEALTH_POLL_SECS` | Decrease (e.g. to 2 s) | Faster reaction to upstream state changes; slightly higher polling overhead |
