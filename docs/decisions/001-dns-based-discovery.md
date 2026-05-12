# ADR 001 — Use DNS-Based Upstream Discovery Instead of a Static Config List

**Status:** Accepted

**Date:** 2026-01

---

## Context

`bge-router` needs to maintain a current list of `bge-m3-embedding-server`
upstream addresses. The number of upstreams changes continuously as ECS scales
the CPU pool in response to load and the GPU pool in response to a CloudWatch
queue depth metric (scale-to-zero to scale-8).

AWS ECS Cloud Map integrates natively with both Fargate and EC2 launch types:
each task registers its private IP as an A record in a Route 53 private hosted
zone when its health check passes, and deregisters when the task stops. The
set of A records returned for a service name is therefore always the live task
set.

The alternative considered was a static environment variable containing a
comma-separated list of upstream IPs, updated by a separate mechanism (e.g.
Lambda + SSM Parameter Store) whenever ECS scaling events occur.

---

## Decision

Resolve `BGE_ROUTER_GPU_DNS` and `BGE_ROUTER_CPU_DNS` on a timer using
`tokio::net::lookup_host` to discover the current upstream set. Merge
newly-resolved addresses into the in-memory `PoolSnapshot` using the
semantics described in [DNS Discovery](../dns-discovery.md):

- New addresses → `Unknown` status (not yet eligible)
- Existing addresses → preserve current health state
- Disappeared addresses → remove from snapshot

---

## Consequences

### Positive

- **Zero config on scale events.** ECS scaling in or out of the embedding
  service requires no router redeployment, no config change, and no external
  webhook. The router self-heals within one `dns_refresh_secs` cycle (default
  30 s).

- **Scale-to-zero GPU is native.** When all GPU tasks stop, the DNS name
  returns empty; the router routes to CPU automatically. When GPU tasks start,
  they appear in DNS and the router picks them up.

- **Identical behaviour in local Docker.** Custom `/etc/hosts` entries or a
  local DNS server let developers point both DNS names at localhost. No special
  testing mode is needed.

- **Simple implementation.** The entire discovery module is ~130 lines of Rust,
  with no AWS SDK dependency, no polling of ECS APIs, and no IAM permissions
  required at runtime.

### Negative / Trade-offs

- **Discovery latency.** A newly-launched task is not routed to until
  `dns_refresh_secs + health_poll_secs` seconds have elapsed (35 s at
  defaults). This is acceptable because the GPU cold-start path is dominated
  by model load time (minutes), not by the router's discovery window.

- **DNS failure removes upstreams.** A transient DNS lookup failure returns
  an empty set, which clears the pool for that name on that cycle. A
  persistent DNS outage drains both pools. Monitoring `/router/health` for
  empty pools and alerting is the mitigation.

- **No fine-grained control over individual upstream weights.** All upstreams
  resolving from the same DNS name are treated equally (tiebreaking only by
  queue depth). Weighted routing by instance type or capacity is not supported.

- **Port is hardcoded to 8081.** All upstreams must bind on port 8081. The
  router appends `:8081` to every resolved IP. Non-standard ports require a
  config change to the router itself.
