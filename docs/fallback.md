# Fallback Routing

When the preferred upstream fails to serve a request, `bge-router` attempts
a fallback to the alternate pool before returning an error to the caller.
This section covers exactly when fallback fires, what counts as a failure,
and the timing semantics of the budget window.

## Attempt Order

```
1. GPU upstream (if any Ok GPU upstreams exist)
2. CPU upstream (if any Ok CPU upstreams exist, and GPU attempt failed)
3. 503 Service Unavailable (if no healthy candidate in either pool)
```

When no Ok GPU upstreams exist at routing time, the request goes directly to
the best Ok CPU upstream with no fallback budget involved. The fallback budget
only applies to the GPU→CPU transition.

## What Triggers Fallback

Fallback fires when the GPU attempt fails in one of these ways, **within the
`BGE_ROUTER_FALLBACK_BUDGET_MS` window (default 1000 ms)**:

| Trigger | Log level | Log message |
|---------|-----------|-------------|
| Connection refused or network error | `WARN` | `GPU upstream error, attempting CPU fallback` |
| GPU returns HTTP 5xx | `WARN` | `GPU upstream returned 5xx, attempting CPU fallback` |
| Timeout (GPU didn't respond within budget) | `WARN` | `GPU upstream timed out within fallback budget, attempting CPU fallback` |

## What Does NOT Trigger Fallback

| Condition | Behaviour |
|-----------|-----------|
| GPU returns HTTP 4xx | Response returned directly to client — 4xx is a client error, not an upstream failure |
| GPU returns HTTP 2xx | Response returned directly to client — success |
| GPU returns HTTP 3xx | Response returned directly to client — redirect passes through |
| No Ok GPU upstream selected | Direct CPU routing, no fallback budget involved |
| CPU attempt fails | Error returned directly to client — there is no CPU→GPU fallback |

## The Fallback Budget Window

The 1-second budget (`BGE_ROUTER_FALLBACK_BUDGET_MS=1000`) is measured from
when `proxy::forward` is called for the GPU upstream — covering:

- TCP connection establishment to the GPU task IP
- TLS handshake (none — VPC internal HTTP only)
- Writing the request (headers + buffered body)
- Receiving the HTTP response line and response headers

Once response headers are received, `proxy::forward` returns with a `Response`
containing a streaming body. At that point, the fallback budget decision has
already been made — if the response status was non-5xx, the response is
returned to the client and streaming begins. The response body stream is
committed: there is no mechanism to retry mid-stream.

The budget is implemented with `tokio::time::timeout` wrapping the entire
`proxy::forward` call:

```
tokio::time::timeout(fallback_budget, proxy::forward(gpu_addr, ...))
```

If the timeout fires, the GPU connection is dropped and the CPU fallback
attempt proceeds immediately (no additional delay).

## Why 1 Second

The 1-second default balances two competing needs:

**Fast failure detection:** A GPU task that just launched, passed the ECS
health check, and registered in DNS (making it `Ok` from the router's
perspective) may still refuse connections briefly while bge-m3 finishes
binding its TCP socket. Connection refused errors resolve within a few
hundred milliseconds.

**P50 latency impact:** At 1 second, a GPU attempt that ultimately times out
adds at most 1 second to the end-to-end latency of a request that was going
to fall back to CPU anyway. For most workloads where GPU requests complete in
< 200 ms, a 1-second budget is hit only under abnormal conditions.

Setting the budget lower (e.g. 200 ms) risks falling back prematurely when
the GPU is healthy but momentarily slow (e.g. during TensorRT kernel
recompilation after a model reload). Setting it higher (e.g. 5 s) allows
deep GPU queue stalls to propagate to CPU, defeating the purpose of the
budget entirely.

## Request Body Buffering

The request body is fully buffered in memory before routing begins. This is
required for fallback retry: if the GPU attempt fails and we need to try CPU,
the body must be re-readable. Axum's default body type is a stream that can
only be consumed once; buffering converts it to `Bytes` that can be cloned
for both attempts.

The buffer ceiling is 32 MiB. Requests exceeding this limit receive a 400 Bad
Request response before routing is attempted.

## CPU-Only Path (No GPU Available)

When the routing policy finds no Ok GPU upstreams (GPU pool empty, all GPU
upstreams `Loading`/`Fail`/`Unknown`), `fallback::route` skips the GPU
attempt entirely and calls `proxy::forward` on the best Ok CPU upstream
directly. No timeout wrapper is applied to CPU-only routing: the request
proceeds to the CPU upstream with no artificial time limit beyond the TCP
connection timeout of the underlying `reqwest::Client`.

## Observing Fallback Events

All fallback transitions log at `WARN` with structured fields. In CloudWatch
Logs Insights (JSON log format):

```
# Count fallback events in the last hour
fields @timestamp, upstream, budget_ms, status
| filter @message like "CPU fallback"
| stats count(*) as fallback_count by bin(5m)
```

A sustained fallback rate (> 0 events per minute) at steady state indicates a
GPU upstream that is repeatedly unhealthy within the budget window. Check
`/router/health` to see `queue_depth` and `last_seen_secs_ago` for the GPU
upstream in question.

## Interaction with Health Polling

Fallback and health polling are independent mechanisms operating at different
time scales:

- **Health polling** (5 s interval) determines which upstreams are *eligible*
  for selection — it controls whether an upstream appears in routing decisions
  at all.
- **Fallback budget** (per-request, 1 s) handles transient failures in
  upstreams that health polling believes are healthy (`Ok`) — it protects
  individual requests from brief connection windows between health polls.

The combination means: health polling filters out long-term failures cleanly,
while the fallback budget absorbs the residual gap where health polling hasn't
yet caught a newly-failing upstream.
