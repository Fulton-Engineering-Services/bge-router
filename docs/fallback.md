# Fallback Routing

`bge-router` picks one of two strategies per request based on the path. The
classifier lives in `RoutePolicy::for_path` (`src/router/route_policy.rs`):

| Route family | Strategy | Default budget |
|---|---|---|
| `/v1/embeddings`, `/v1/sparse-embeddings`, `/v1/embeddings:both` (any `/v1/*embeddings*`) | Hedged race (GPU first; fire CPU in parallel after hedge delay; first non-5xx response wins) | `BGE_ROUTER_HEDGE_DELAY_MS=5000` |
| `/health`, `/v1/models`, anything else | Sequential GPU → CPU with a hard timeout per upstream | `BGE_ROUTER_CONTROL_TIMEOUT_MS=1000` |

## Why two strategies

**Inference is expensive and hideable.** A typical `/v1/embeddings` request
can take hundreds of milliseconds on GPU and several seconds on CPU. Pure
`tokio::time::timeout`-then-fallback wastes GPU-seconds (the GPU keeps
computing the abandoned request after the timeout) and adds a hard 1-second
"GPU theater tax" before CPU is tried. The hedged race fires CPU only after
the GPU has been given a chance to win, and cancels the loser at the source
so the abandoned upstream stops working.

**Control-plane requests are cheap and idempotent.** `/health` and
`/v1/models` should fail fast when an upstream is misbehaving — racing them
adds load with no latency benefit. The sequential timeout path keeps the
existing 1-second-per-upstream budget.

## Hedged Race (Inference)

```
            t=0ms                   t=hedge_delay              t = winner
              │                          │                         │
GPU upstream ─┤────────────────────────────────────────────────┤◀── reply
              │                                                │
CPU upstream  │                          │────────────────┤◀───┘
                                         ▲
                                "hedge: firing CPU race"
```

1. The router picks the best `Ok` GPU upstream and the best `Ok` CPU upstream.
2. The GPU `proxy::forward` future is created and pinned on the stack.
3. The CPU side is wrapped in `async { sleep(hedge_delay); proxy::forward(...) }`,
   pinned alongside.
4. `tokio::select! { biased; }` polls both. The first one that yields a
   non-5xx response wins; the function returns and the loser's future is
   dropped at scope exit — `reqwest` closes the underlying TCP connection,
   which propagates as a client cancellation upstream.
5. If the GPU returns 5xx or an error, the router waits for the CPU result.
6. If both fail, the router emits `hedge: both failed` and returns the GPU
   outcome (preserves prior sequential semantics so existing observability
   does not regress).

### Logged events (target: `bge_router::router::hedge`)

| Event | Level | Fields | Emitted when |
|-------|-------|--------|--------------|
| `hedge: firing CPU race` | INFO | `path`, `hedge_delay_ms`, `gpu_upstream`, `cpu_upstream` | Hedge delay elapses; CPU side is about to call `proxy::forward` |
| `hedge: GPU won` | INFO | `path`, `winner_latency_ms`, `gpu_upstream`, `cpu_upstream`, `loser_status` | GPU returns a non-5xx first |
| `hedge: CPU won` | INFO | `path`, `winner_latency_ms`, `gpu_upstream`, `cpu_upstream`, `loser_status` | CPU returns a non-5xx first |
| `hedge: both failed` | WARN | `path`, `gpu_upstream`, `cpu_upstream` | Both upstreams returned 5xx or errored |

`loser_status` values:

- `not_started` — winner returned before hedge delay elapsed; CPU race never fired.
- `cancelled` — loser was still in-flight; its future was dropped, cancelling the upstream call.
- `errored` — loser already finished (with 5xx or error) before the winner returned.

### Sample log lines

Hedged race where CPU wins (from
`router::fallback::tests::hedged_race_fast_cpu_wins_and_gpu_is_cancelled`):

```
INFO bge_router::router::hedge: hedge: firing CPU race path=/v1/embeddings hedge_delay_ms=20 gpu_upstream=127.0.0.1:59339 cpu_upstream=127.0.0.1:59340
INFO bge_router::router::hedge: hedge: CPU won path=/v1/embeddings winner_latency_ms=74 gpu_upstream=127.0.0.1:59339 cpu_upstream=127.0.0.1:59340 loser_status="cancelled"
```

Both upstreams returned 5xx (from
`router::fallback::tests::hedged_race_both_fail_returns_gpu_error`):

```
INFO bge_router::router::hedge: hedge: firing CPU race path=/v1/embeddings hedge_delay_ms=10 gpu_upstream=127.0.0.1:59427 cpu_upstream=127.0.0.1:59428
WARN bge_router::router::hedge: GPU upstream returned 5xx in race upstream=127.0.0.1:59427 pool="GPU" status=500 elapsed_ms=22
WARN bge_router::router::hedge: CPU upstream returned 5xx in race upstream=127.0.0.1:59428 pool="CPU" status=500 elapsed_ms=56
WARN bge_router::router::hedge: hedge: both failed path=/v1/embeddings gpu_upstream=127.0.0.1:59427 cpu_upstream=127.0.0.1:59428
```

### Cancellation guarantee

The race loser is **actually cancelled**, not just ignored. When the winner
returns, the function returns from `run_race`, which drops the still-pinned
loser future. Dropping a `reqwest` request future closes its underlying
TCP connection. Both `bge-m3-embedding-server` and any HTTP/1.1 backend
treat a closed connection as request abandonment and stop computing.

The `hedged_race_fast_cpu_wins_and_gpu_is_cancelled` integration test
verifies this end-to-end: it spins up two axum mock upstreams with
`Drop`-guarded handlers and asserts that the GPU mock observes the
cancellation drop before its `tokio::time::sleep` would have completed.

## Sequential Timeout (Control Plane)

For `/health`, `/v1/models`, and any other non-inference path, the router
keeps the existing GPU → CPU sequential structure but bounds each upstream
independently:

```
1. proxy::forward(GPU)  wrapped in tokio::time::timeout(per_upstream)
2. If GPU OK and not 5xx → return
3. If GPU times out / errors / returns 5xx
   → proxy::forward(CPU) wrapped in tokio::time::timeout(per_upstream)
4. If CPU also fails or times out → 503
```

Worst-case latency is `2 × control_timeout`. Default 1 s × 2 = 2 s.

| Trigger | Log level | Log message |
|---------|-----------|-------------|
| GPU connection refused or network error | `WARN` | `GPU upstream error, attempting CPU fallback` |
| GPU returns HTTP 5xx | `WARN` | `GPU upstream returned 5xx, attempting CPU fallback` |
| GPU timeout (`> control_timeout`) | `WARN` | `GPU upstream timed out within fallback budget, attempting CPU fallback` |
| CPU timeout (`> control_timeout`) | `WARN` | `CPU upstream timed out within control-plane budget` |

This preserves the original WARN message shapes so existing CloudWatch
filters keep working.

## TLS Awareness

Both the hedged-race path and the sequential-timeout path are TLS-aware. When
`BGE_ROUTER_UPSTREAM_TLS=1` is set, every upstream URL constructed by the proxy
uses `https://` instead of `http://`. The scheme is carried by the `UpstreamScheme`
type and applied uniformly across:

- GPU and CPU legs of the hedged race.
- GPU and CPU legs of the sequential-timeout path.
- The upstream health poller (which determines which upstreams are eligible to enter
  either path).

No per-path TLS configuration is needed. See [docs/tls.md](tls.md) for the full
TLS setup guide.

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `BGE_ROUTER_HEDGE_DELAY_MS` | `5000` | Inference: ms to wait before firing parallel CPU race |
| `BGE_ROUTER_CONTROL_TIMEOUT_MS` | `1000` | Control plane: per-upstream hard timeout |
| `BGE_ROUTER_FALLBACK_BUDGET_MS` | _unset_ | **Deprecated.** When set without `BGE_ROUTER_HEDGE_DELAY_MS`, seeds `hedge_delay` for safer migration. Never seeds `control_timeout`. Logged as a one-time `WARN` at startup. |

Both `*_MS` variables must be `> 0` if explicitly set; the server fails fast
with an error message at startup if either is `0`.

## What Does NOT Trigger Fallback

| Condition | Behaviour |
|-----------|-----------|
| GPU returns HTTP 4xx | Response returned directly to client — 4xx is a client error, not an upstream failure |
| GPU returns HTTP 2xx | Response returned directly to client — success |
| GPU returns HTTP 3xx | Response returned directly to client — redirect passes through |
| No `Ok` GPU upstream selected | Direct CPU routing; no hedge or timeout |
| CPU response already streaming | No retry mid-stream — the response body is committed |

## Choice of "first 2xx wins"

The race winner is technically the first **non-5xx** response, not strictly
the first 2xx. Mirrors the existing `proxy::forward` semantics: 4xx is a
deterministic client error and should be surfaced to the caller, not
hidden behind a fallback. 3xx is rare for this API but propagates without
fallback. 5xx and connection errors are the only loss conditions.

## Request Body Buffering

The request body is fully buffered in memory before routing begins. Both
the hedged race (`body.clone()` per upstream) and the sequential timeout
need a re-readable body. Axum's default body type is a one-shot stream;
buffering converts it to `Bytes` that is cheap to clone (refcounted).

The buffer ceiling is 32 MiB; requests exceeding this limit receive 400
Bad Request before routing is attempted.

## Observing Race Outcomes

```
# Hedged-race outcomes (CloudWatch Logs Insights, JSON format)
fields @timestamp, path, winner_latency_ms, loser_status
| filter @message like "hedge:" and @message like "won"
| stats count(*) as wins, avg(winner_latency_ms) as avg_ms by @message
```

A sustained `hedge: CPU won` rate at steady state is the signal that the
GPU pool is consistently slower than the hedge delay — either tune the
delay down (cheap, but loses some GPU-only latency benefit) or investigate
the GPU pool (warm spare, capacity, model variant choice).

A non-zero `hedge: both failed` rate indicates a real outage — neither
pool can serve the request.
