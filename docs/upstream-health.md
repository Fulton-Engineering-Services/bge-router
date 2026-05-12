# Upstream Health Polling

`bge-router` maintains a live view of each upstream's health by polling the
bge-m3 `/health` endpoint on a fixed interval. The results feed directly into
the routing policy's eligibility checks.

## Poll Mechanics

The health poll task spawns at startup and ticks every `BGE_ROUTER_HEALTH_POLL_SECS`
(default 5 seconds). On each tick:

1. Load the current `PoolSnapshot` from the `ArcSwap`.
2. Collect every known upstream address (GPU pool + CPU pool combined).
3. Send `GET http://{addr}/health` concurrently to all addresses via a shared
   `reqwest::Client` with a **4-second per-request timeout**.
4. Build a new `PoolSnapshot` by applying poll results to the existing
   snapshot, then atomically store it.

All polls run in parallel via `tokio::task::JoinSet`. Under normal conditions
with a handful of upstreams, the entire round trip completes well within the
poll interval. With the maximum deployment scale (8 GPU + 25 CPU = 33
upstreams), all 33 polls still complete concurrently within the 4-second
timeout.

If no upstreams are known yet (fresh start before the first DNS refresh), the
health poll exits early without making any requests.

## bge-m3 Health Response Schema

The router parses a subset of the bge-m3 `/health` JSON response:

```json
{
  "status": "ok",
  "workers": { "live": 7, "total": 7 },
  "queue_depth": 2
}
```

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | Health state string (see mapping below) |
| `workers.live` | integer | Number of live worker threads |
| `queue_depth` | integer | Requests waiting for a free worker permit |

`workers` and `queue_depth` are optional in the router's deserializer; if
absent, `live_workers` and `queue_depth` default to `0` in the stored
`UpstreamInfo`.

## Status String to UpstreamStatus Mapping

| bge-m3 `status` string | `UpstreamStatus` | What it means |
|------------------------|------------------|---------------|
| `"ok"` | `Ok` | All workers healthy; accepting requests |
| `"warn"` | `Ok` | Some workers exited but service still accepts requests |
| `"loading"` | `Loading` | Model initialising at startup |
| `"idle"` | `Loading` | Models unloaded after idle timeout; will reload on demand |
| `"fail"` | `Fail` | All workers have exited; service is non-functional |
| Any other string | `Unknown` | Unrecognised status; treated as not eligible |

`"warn"` is deliberately mapped to `Ok` rather than `Fail`: bge-m3 sets
`warn` when at least one worker is still live. The service continues
accepting requests. Routing to a `warn` upstream is safe; the elevated error
rate from degraded worker capacity is still better than routing to a CPU
upstream that adds latency.

`"idle"` is mapped to `Loading` because the upstream's models are unloaded
and a subsequent embedding request would trigger a reload — adding seconds of
startup latency. Excluding `idle` upstreams from routing avoids this hidden
latency spike. In the GPU pool, `BGE_M3_IDLE_TIMEOUT_SECS=3600` in the CDK
deployment means GPU upstreams rarely go idle while tasks are running; the
`idle` case primarily applies to CPU upstreams under low load.

## Poll Failure Behaviour

A poll fails in two ways:

**Non-2xx HTTP response** — the upstream returned a valid HTTP response
outside the success range (e.g. 503 from bge-m3 while still loading). The
upstream is set to `Fail`.

**Connection error or timeout** — the upstream is unreachable, the connection
was refused, or the 4-second timeout elapsed. The upstream is also set to
`Fail`.

In both failure cases, `queue_depth` and `live_workers` are reset to `0` in
the snapshot. The upstream remains in the snapshot (it will be polled again
on the next tick) and is not eligible for routing until a successful poll
returns an `Ok`-mappable status.

This means a transient network blip — a single failed poll — sets the
upstream to `Fail` and removes it from rotation immediately. It will return
to `Ok` as soon as the next poll succeeds (up to `health_poll_secs` later).
This is intentional: a 5-second exclusion window is preferable to routing to
an upstream that might not respond.

## Stale Upstream Detection via last_seen

Every `UpstreamInfo` stores a `last_seen: Instant` field — the monotonic
timestamp when the most recent **successful** health poll was completed. On a
failed poll, `last_seen` is not updated (the previous value is preserved via
`upstream.clone()` in `update_pool`).

The `last_seen_secs_ago` field in the `/router/health` response exposes this
value as a floating-point number of elapsed seconds:

```json
{ "addr": "10.0.1.5:8081", "status": "fail", "last_seen_secs_ago": 47.3 }
```

A large `last_seen_secs_ago` value (e.g. > 30 s) on a `fail` upstream
indicates prolonged unreachability — the upstream may have crashed or been
deregistered from DNS before the router noticed. This field is a useful
starting point when investigating why an upstream is excluded from routing.

Alerting on `last_seen_secs_ago` is a future enhancement; today, the value
is diagnostic-only (visible in `/router/health` and indirectly through
CloudWatch log correlation).

## Relationship to DNS Discovery

Health polling and DNS discovery are independent background tasks:

- **DNS discovery** determines *which* addresses exist in each pool. It runs
  every `dns_refresh_secs` and adds or removes upstreams from the snapshot.
- **Health polling** determines *how healthy* each known address is. It runs
  every `health_poll_secs` and updates `status`, `queue_depth`, and
  `live_workers` for every address currently in the snapshot.

A newly-discovered address stays `Unknown` until the health poll task runs
and successfully polls it. With default settings, this window is at most
`health_poll_secs` (5 s) after the DNS refresh that introduced the address.
During this window, the address is not eligible for routing.

If DNS removes an address (task deregistered or IP changed), the next DNS
refresh drops it from the snapshot entirely. Subsequent health polls will not
include it because poll targets are derived from the current snapshot at poll
time.

## Observing Health State

```bash
# Real-time pool snapshot
curl http://localhost:8081/router/health | jq .

# Example: look for any upstream offline longer than 30 s
curl http://localhost:8081/router/health | \
  jq '[.gpu_upstreams[], .cpu_upstreams[]] | map(select(.last_seen_secs_ago > 30))'
```

Key diagnostic signals from `/router/health`:

| Condition | What to check |
|-----------|---------------|
| `gpu_upstreams: []` | GPU pool empty — check DNS resolution and ECS task count |
| Any upstream `status: "unknown"` | First poll not yet completed — wait `health_poll_secs` |
| `last_seen_secs_ago` > 30 on a `fail` upstream | Upstream has been unreachable for multiple poll cycles — check ECS task health |
| All upstreams `status: "loading"` | All upstreams initialising — check bge-m3 startup logs |
