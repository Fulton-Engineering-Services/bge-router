# DNS-Based Upstream Discovery

`bge-router` discovers its upstream pool dynamically by resolving DNS names
on a timer rather than reading a static list of addresses. This matches how
AWS ECS Cloud Map exposes service membership: each running task registers its
private IP as an A record, and the set of A records is the live task set.

## How DNS Discovery Works

The DNS discovery task resolves both configured DNS names concurrently on an
adaptive cadence (see "Refresh Cadence" below):

```
BGE_ROUTER_GPU_DNS  (default: bge-m3-gpu)
BGE_ROUTER_CPU_DNS  (default: bge-m3-cpu)
```

Resolution uses `tokio::net::lookup_host`, which delegates to the system
resolver. Each name is resolved to port 8081: the router always appends
`:8081` to every resolved IP — all bge-m3 upstreams must bind on port 8081.

The discovery loop distinguishes three outcomes per lookup:

| Outcome | Cause | Effect on pool |
|---|---|---|
| `Resolved(some addrs)` | DNS returned ≥1 A record | Merge with existing pool (see below) |
| `Resolved(empty)` | DNS returned 0 A records (legitimate scale-to-zero) | Clear the pool |
| `Failed` | NXDOMAIN, timeout, or network error | **Preserve** the previous pool; logs `WARN` |

Preserving the pool on `Failed` is what makes the router resilient to
transient DNS hiccups: the health poller continues running against the
existing addresses, so addresses that have genuinely gone away are marked
`Fail` within one health-poll cycle and become unroutable on their own
without requiring DNS to remove them.

## Merge Semantics

The discovery task does not replace the snapshot wholesale on each tick. It
*merges* the newly-resolved addresses with the existing snapshot using the
following rules:

| Address in DNS result | Address in existing snapshot | Result |
|-----------------------|------------------------------|--------|
| Yes | No (new) | Added with `status=Unknown, queue_depth=0` |
| Yes | Yes (existing) | Preserved with its current health state |
| No | Yes (disappeared) | Removed from snapshot |

**New addresses** enter as `Unknown`. They are not eligible for routing until
the health poll task polls them and receives a successful `Ok`-mappable
response. This prevents the router from forwarding to a task that just
registered in DNS but hasn't finished loading its model yet.

**Existing addresses** keep their current `status`, `queue_depth`,
`live_workers`, and `last_seen`. A DNS refresh that returns the same set of
IPs is a no-op for health state.

**Disappeared addresses** are pruned immediately on the next successful DNS
refresh (whether it returns a smaller non-empty set or an empty set). On a
DNS *error* (network/timeout/NXDOMAIN), the previous set is preserved — see
"DNS Failure Handling" below.

## Why DNS-Based Discovery

DNS provides a natural membership list for any service registry that exposes
members as A records — AWS ECS Cloud Map, ECS Service Connect, Docker Compose
networking, Kubernetes services, or plain `/etc/hosts` entries.

For ECS Cloud Map specifically: each task registers its private IP as an A
record when its health check passes and deregisters when the task stops.
Multiple tasks behind the same service name appear as multiple A records from
a single DNS query. The router never needs updating when ECS scales the
embedding service up or down — it discovers the new task set on the next DNS
refresh cycle. There is no load balancer target group or manual configuration
to keep in sync.

Set the DNS TTL to match `BGE_ROUTER_DNS_REFRESH_SECS` (default 30 s).
Effective maximum staleness in **steady state**:

```
effective_staleness = dns_refresh_secs + health_poll_secs
                    = 30 s (DNS) + 5 s (health) = 35 s
```

During cold start or after an upstream pool drains to zero, the discovery
loop switches to fast retry (see "Refresh Cadence"), and the effective
staleness collapses to `2 s + health_poll_secs = 7 s` until the pool
recovers.

## Refresh Cadence

The discovery loop runs on an adaptive schedule rather than a fixed timer:

| Condition | Sleep before next refresh |
|---|---|
| Both pools have addresses | `dns_refresh_secs` (default 30 s) |
| Just transitioned to "either pool empty / failed" | 2 s (`INITIAL_RETRY_INTERVAL`) |
| Still unhealthy | double the previous sleep, capped at `dns_refresh_secs` |

Concretely, a cold start where both pools start empty produces this
schedule: `0 s → 2 s → 4 s → 8 s → 16 s → 30 s → 30 s → …`. As soon as
**both** pools have addresses from a successful resolution, the loop drops
back to the steady-state interval. A subsequent failure resets the schedule
to 2 s on the very next tick.

This pattern collapses cold-start time when an upstream service comes up
shortly after the router boots — which is common with concurrent ECS
deploys — without hammering DNS during extended outages.

The discovery loop emits an INFO log on transitions into the healthy state
and a WARN log on transitions out:

```
INFO  ... "DNS discovery recovered: both pools populated"
WARN  ... "DNS discovery degraded: at least one pool empty or unresolved; \
            entering fast-retry backoff"
```

## Scale-to-Zero Behaviour

The GPU pool can run at zero instances when idle. When all GPU tasks are stopped:

1. The DNS registry deregisters all GPU task IPs.
2. On the next DNS refresh, the GPU DNS name either returns NXDOMAIN
   (treated as `Failed` → pool preserved this cycle) or an empty A record
   set (treated as `Resolved(empty)` → pool cleared immediately).
3. The health poller marks any still-pinned-but-actually-dead addresses
   `Fail` within ~5 s, so routing stops sending traffic to them regardless
   of which DNS outcome we get.
4. With both pools eventually empty-or-failing, the discovery loop switches
   to its fast-retry schedule (2 s, 4 s, 8 s, …).
5. All requests route to whichever pool still has healthy addresses
   (typically CPU).

When GPU tasks start again (e.g. triggered by an autoscaling alarm):

1. New task IPs register in DNS once the task health check passes.
2. The next DNS refresh — usually within 2–8 s thanks to fast-retry —
   picks up the new IPs and adds them to the GPU pool as `Unknown`.
3. The next health poll (up to 5 s) probes the IPs; if bge-m3 reports
   `"ok"`, they become eligible.
4. The router begins routing to GPU.

The router's contribution to cold-start latency in this scenario is roughly
`fast_retry_interval + health_poll = 7–13 s`, down from the previous
30 s + 5 s = 35 s. The dominant cost is still bge-m3's model load plus
optional TensorRT compilation (minutes); the router's contribution is
negligible.

## DNS Failure Handling

A DNS lookup failure (network-level error, not an empty-but-valid response)
is logged as `WARN`, and the merge step **preserves** the previous pool for
that name:

```
WARN dns_name="bge-m3-gpu" err="..." DNS lookup failed
```

The health poller keeps probing the existing addresses. Any address that is
genuinely gone fails its next `/health` request and is marked `Fail`, which
removes it from the routable set within one health-poll cycle (~5 s). When
the next DNS refresh succeeds, the snapshot reconverges to the authoritative
DNS answer.

This means a one-time DNS blip is invisible to clients: the routing snapshot
keeps its last-known-good addresses, health polling is independent of DNS,
and routing continues using the same addresses it was already using.

A *valid-but-empty* DNS response is treated differently — see "Scale-to-Zero
Behaviour" above. Cloud Map / ECS Service Discovery removes A records when
no tasks are healthy, which usually surfaces as `NXDOMAIN` (failure path, so
the pool is preserved) for the first refresh after every task stops, then as
a successful empty response after the registry GC catches up. In practice
both are handled correctly: a stale address either continues to serve (if
it's still alive) or gets marked `Fail` by the health poller (if it's not).

> **Operational note:** A persistent DNS failure (e.g. a misconfigured
> namespace or Route 53 outage) keeps the snapshot pinned to its
> last-known-good addresses. If those addresses are still reachable, traffic
> continues uninterrupted. If they have gone away, the health poller marks
> them `Fail` within ~5 s and routing falls back as normal. Monitor
> `/router/health` and the `DNS discovery degraded` WARN to detect the
> condition.

## Local Development and Testing

When running the router locally against a non-ECS deployment, set the DNS
names to `localhost` or to IP addresses directly:

```bash
# Both pools pointing at a single local bge-m3 instance
BGE_ROUTER_GPU_DNS=localhost \
BGE_ROUTER_CPU_DNS=localhost \
  cargo run
```

For local multi-instance testing, add entries to `/etc/hosts`:

```
127.0.0.1  bge-m3-gpu.test
127.0.0.1  bge-m3-cpu.test
```

Then:

```bash
BGE_ROUTER_GPU_DNS=bge-m3-gpu.test \
BGE_ROUTER_CPU_DNS=bge-m3-cpu.test \
  cargo run
```

`lookup_host` resolves via the system resolver, so `/etc/hosts` entries are
honoured without any special configuration.
