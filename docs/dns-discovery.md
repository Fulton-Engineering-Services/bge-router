# DNS-Based Upstream Discovery

`bge-router` discovers its upstream pool dynamically by resolving DNS names
on a timer rather than reading a static list of addresses. This matches how
AWS ECS Cloud Map exposes service membership: each running task registers its
private IP as an A record, and the set of A records is the live task set.

## How DNS Discovery Works

The DNS discovery task ticks every `BGE_ROUTER_DNS_REFRESH_SECS` (default
30 seconds). On each tick, it resolves both configured DNS names concurrently:

```
BGE_ROUTER_GPU_DNS  (default: bge-m3-gpu)
BGE_ROUTER_CPU_DNS  (default: bge-m3-cpu)
```

Resolution uses `tokio::net::lookup_host`, which delegates to the system
resolver. Each name is resolved to port 8081: the router always appends
`:8081` to every resolved IP — all bge-m3 upstreams must bind on port 8081.

If a DNS lookup fails (NXDOMAIN, timeout, or network error), the lookup
returns an empty set and logs a `WARN`. The merge step then removes any
previously-known addresses for that name from the snapshot.

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
refresh. If the DNS lookup itself fails, the previous set is *not* pruned —
the failed lookup produces an empty set, which would remove all upstreams.
See "DNS Failure Handling" below.

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
Effective maximum staleness of the router's view of the upstream pool:

```
effective_staleness = dns_refresh_secs + health_poll_secs
                    = 30 s (DNS) + 5 s (health) = 35 s
```

A task must appear in DNS for at least `dns_refresh_secs` before the router
can discover it, and then wait up to `health_poll_secs` more before it
receives a health poll.

## Scale-to-Zero Behaviour

The GPU pool can run at zero instances when idle. When all GPU tasks are stopped:

1. The DNS registry deregisters all GPU task IPs.
2. On the next DNS refresh, the GPU DNS name returns NXDOMAIN or an empty
   A record set.
3. The GPU pool is cleared from the `PoolSnapshot`.
4. All subsequent requests route to the CPU pool.

When GPU tasks start again (e.g. triggered by an autoscaling alarm):

1. New task IPs register in DNS once the task health check passes.
2. On the next DNS refresh (up to 30 s), new IPs appear in the GPU pool as
   `Unknown`.
3. On the next health poll (up to 5 s), the IPs are polled; if bge-m3 reports
   `"ok"`, they become eligible.
4. The router begins routing to GPU.

The cold start path — from task launch to GPU routing — is dominated by
bge-m3's model load + optional TensorRT compilation time (minutes), not by
the router's discovery latency (35 s). The router's contribution to cold
start is negligible.

## DNS Failure Handling

A DNS lookup failure (not an empty-but-valid response, but a network-level
error) is logged as `WARN` and returns an empty address set to the merge
function. This means a one-time DNS blip will **remove all upstreams from
the pool** for that name on that cycle.

```
WARN dns_name="bge-m3-gpu" err="..." DNS lookup failed
```

The pool recovers as soon as the next DNS refresh cycle succeeds.

> **Operational note:** A persistent DNS failure (e.g. a misconfigured
> namespace or Route 53 outage) will deplete the GPU pool after one refresh
> cycle and the CPU pool after two consecutive failures. Monitor `/router/health`
> for empty `gpu_upstreams` or `cpu_upstreams` arrays, and check VPC DNS
> resolution if both pools drain simultaneously.

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
