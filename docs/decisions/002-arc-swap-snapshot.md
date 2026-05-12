# ADR 002 — Use ArcSwap<PoolSnapshot> for Lock-Free Routing State

**Status:** Accepted

**Date:** 2026-01

---

## Context

`bge-router` has two categories of concurrent actors operating on shared
upstream state:

**Writers (background tasks):**
- DNS discovery task — rewrites the upstream address set every 30 s
- Health poll task — rewrites per-upstream status, queue depth, and worker
  count every 5 s

**Readers (request path):**
- Every incoming HTTP request reads the snapshot to select an upstream
- At peak throughput (e.g. 100 concurrent requests), 100 goroutines may be
  reading simultaneously

The simplest approach — wrapping the snapshot in a `std::sync::Mutex` or
`tokio::sync::RwLock` — was evaluated and rejected on the grounds that:

1. **Writers run frequently (every 5 s for health poll).** An `RwLock` write
   lock blocks all concurrent readers until the write completes. Even a brief
   stall on the hot path is undesirable.
2. **The snapshot is immutable once built.** Writers don't mutate the existing
   snapshot in place; they construct an entirely new `PoolSnapshot` and replace
   the pointer. Immutable-replace semantics are a natural fit for a pointer-swap
   primitive.
3. **Readers need no synchronisation with each other.** The concern is
   only reader/writer coordination.

---

## Decision

Use the `arc-swap` crate's `ArcSwap<PoolSnapshot>`:

```rust
Arc<ArcSwap<PoolSnapshot>>
```

Writers call `pool.store(Arc::new(new_snapshot))`, which atomically replaces
the pointer. Readers call `pool.load_full()`, which performs an atomic load
and returns a reference-counted pointer to the current snapshot. The reader
holds the `Arc` for the duration of its routing decision; the old snapshot is
dropped when all readers that loaded it have released their references.

---

## Consequences

### Positive

- **Readers never block.** The `load_full()` operation is a single atomic
  instruction. A health poll running its write cannot stall an in-flight
  request — at most, the request sees the pre-update snapshot.

- **Writers never block each other.** DNS and health tasks write
  independently; both call `store()` which is atomic. There is no write-write
  contention.

- **No lock ordering to reason about.** A `Mutex`-based design would require
  careful ordering across the discovery and health modules. With `ArcSwap`,
  each write is self-contained.

- **Immutable snapshot is a clean data model.** Each `PoolSnapshot` is fully
  constructed before it is visible to any reader. There is no possibility of a
  reader observing a snapshot mid-construction.

### Negative / Trade-offs

- **Slight memory overhead during transition.** While a reader holds a
  reference to an old snapshot and a writer stores a new one, two
  `Arc<PoolSnapshot>` allocations exist simultaneously. At ~10 KB per snapshot
  (33 upstreams at maximum scale) and at most one transition in flight, the
  peak overhead is ~20 KB — negligible.

- **Readers may observe stale state.** A reader that acquired a snapshot just
  before a health poll update completed will use the pre-update snapshot for
  its routing decision. With a 5-second poll interval and request latencies
  in the millisecond range, the window of staleness is typically < 1 ms.
  This is an acceptable trade-off for zero blocking.

- **Dependency on the `arc-swap` crate.** This adds a compile-time dependency.
  The crate is well-maintained, has no unsafe code exposed to users, and is
  widely used in the Rust ecosystem (e.g. in `tokio`, `sentry`, `servo`).

- **Snapshot replacement is all-or-nothing.** DNS and health tasks each
  replace the full snapshot. If both tasks race to write in the same moment,
  one write wins and one is overwritten immediately. Each task reads the
  current snapshot before constructing the next one (`pool.load()` at the
  start of each cycle), so the losing write's data is not silently discarded —
  it will be recomputed on the next cycle.
