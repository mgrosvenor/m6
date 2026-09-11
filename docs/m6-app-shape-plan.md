# One app shape: single-threaded, non-blocking, one loop and a switch

**Goal, stated by the owner: one good way to build every app.** Every service
the same structure, powered by m6-core, documented well enough to pick up from
outside this repo. **No performance regression.**

The target shape, stated plainly:

> **One thread. One event loop. A switch over connection state. Everything
> non-blocking. Anything that must block gets its own sync thread and signals
> the loop through an fd.**

This is memcached over libevent, and it is QJump's applications over CamIO. It
is also, already, m6-http.

`m6-app-anatomy.md` describes the shape as the services present it today. This
file is the target and the route to it.

---

## 1. This is not a proposal, it is a promotion

**m6-http is already this shape**, and it is the part of the system under the
most pressure: public TCP and UDP, TLS, three protocol versions, the cache.

| | m6-http server | `App` services |
|---|---|---|
| `thread::spawn` | **0** (all ten are in `bench_*.rs`) | 3 |
| `.lock()` on the request path | **0** | mutex per request |
| conformance | h2spec 146/146, h1spec 32/32 | n/a |

`ParamsCache::get` (`app.rs:279`) takes a `Mutex` on every request that reads a
params file, and `rx.lock()` (`app.rs:1038`) takes another to pull work off the
queue. m6-http's loop takes none.

So the question is not "should we adopt an unproven model". It is "why do five
services use the model that is demonstrably losing, inside a project where the
winning one is already running".

## 2. The evidence, all of it from this repository

### The concurrency bug already happened, and cost thirty days

> Blocking a signal is per-thread and threads inherit the mask at creation.
> Every service initialised logging first, and `tracing_appender::non_blocking`
> spawns a writer thread, so by the time `main` blocked SIGTERM that thread had
> had it unblocked for a hundred lines. The kernel delivered every
> process-directed SIGTERM there and the default disposition killed the
> process; the `sigwait` thread never received a signal in its life. **On syd,
> `m6-file` has not logged a shutdown line in thirty days.**

**That bug cannot exist in a single-threaded program.** It is not a bug threads
made harder to find, it is one only threads can have. It was silent for a
month, `systemctl stop` reported success throughout, and the current defence is
a runtime assertion in `install_with_hooks` that refuses to start if the mask
is wrong. We are paying to guard against a bug class the other model deletes.

### Two flaky tests, still unexplained

Both spawn external processes, both have failed exactly once "inside a loaded
full-workspace run", neither reproduces in isolation. That is the signature of
a timing-dependent defect. See `HANDOVER.md` §7.

### The pool was widened reactively, not calculated

`configs/m6-file.conf`:

> "Default is num_cpus, easily exhausted by a page that fires off dozens of
> concurrent image requests at once (e.g. the photo gallery grid) - each
> stalled/queued request shows up as 'pool empty' backend errors and slow..."

Raised from the `num_cpus` default to **32**. Pool exhaustion was reached from
one page, at zero publicity.

### On one core, threads are worse, not neutral

**syd is 1 core and 950MB.** m6-file runs 32 workers on it; m6-html, which
renders every HTML page, runs **1** (no `size` line, so `num_cpus`).

On one core, threads provide no parallelism. They provide preemption. For
CPU-bound work like a 6ms Tera render, preemption is actively worse for tail
latency: round-robin between two 6ms renders finishes both at ~12ms;
run-to-completion finishes one at 6ms and the other at 12ms. Same throughput,
better tail, lower variance, no context switches.

So single-threaded run-to-completion is not a choice that pays off at some
future scale. **It is the better choice at the scale we have**, and 32 threads
on one core is the clearest sign the current model is being fought rather than
used.

## 3. The offload primitive already exists, twice

Anything that genuinely must block gets **its own sync thread, which signals
the loop through an fd**. The loop never blocks; blocking appears to it as just
another readable descriptor.

Core already does this in two places, on both platforms, and has never named it
a pattern:

**`ConfigWatcher`, BSD path** (`watcher.rs`):

```rust
pub struct ConfigWatcher {
    /// Read end of self-pipe - returned from raw_fd(), registered with poller.
    pipe_read: RawFd,
    /// Write end of self-pipe - written by background watcher threads.
    pipe_write: RawFd,
    /// Background threads kept alive for the lifetime of this struct.
    _threads: Vec<std::thread::JoinHandle<()>>,
}
```

**`ShutdownHandle::wake_fd`** (`signal.rs`), fed by m6-http from
`poller.rs:371`: "The raw write end, for `m6_core::signal::Service::wake_fd`".

Generalising this is naming a mechanism we already ship and have already
debugged on two platforms.

**The resulting concurrency model is one sentence:** the loop owns all state
and takes no locks; each offload thread owns exactly one blocking call and
shares nothing but a completion fd.

That is what makes it debuggable. Not "fewer threads", but **no shared mutable
state to reason about.**

## 4. What genuinely blocks

Narrower than assumed, and worth stating because the first draft of this plan
got it wrong.

**Does not block.** The renderers are CPU-bound, not I/O-bound:

- Templates load at worker init (`template.rs` `build_tera`, from
  `RendererFactory::build`).
- Global params load at worker init (`app.rs:436`, immediately after the
  factory call).
- Route params are an in-memory LRU:
  `Mutex<lru::LruCache<String, Arc<Map<String, Value>>>>`, and the `Mutex`
  exists only because of the pool.

**Does block**, and needs the offload:

- `m6-file`: `handler.rs:276`, `std::fs::read` per request.
- The CMS and contact form: `Request::read_json`, `write_json_atomic`,
  `list_json`, `write_bytes`.

That is the whole list.

## 5. What this deletes

- `ThreadPool`, its `sync_channel`, and the `rx.lock()` per work item.
- `ParamsCache`'s `Mutex`: a single-threaded loop wants a plain `LruCache`.
- Most of core's 57 `Arc`s, which exist to share state across workers.
- The read-timeout gap: unreachable by construction once reads are on the loop.
- The `install_with_hooks` mask assertion's reason for existing.

## 6. What is genuinely hard

Not objections. The parts with real work in them.

**The handler contract changes meaning.** `Fn(&Request) -> Result<Response>`
currently promises a handler *may* block. It has to come to mean *must not
block, except through the offload*. This is the API change everything rests on,
and it is why the renderers, which do not block, migrate easily and m6-file,
which does, migrates last.

**A 6ms render head-of-line blocks the loop.** Single-threaded makes this
visible rather than hiding it behind a queue depth, which is an improvement,
but it does not remove it. What actually handles it is the edge cache at
`s-maxage=86400`, so the render lands only on a miss. **That cache is
load-bearing for this design and should be treated as such.**

**Rust makes borrow-checked state machines awkward.** m6-http's
`H1State::Reading { buf }` is the existence proof that it is tractable here,
not that it is pleasant.

## 7. What this does not touch

- **m6-http.** It is already the target. It converges on core module by module
  (ten today); it does not need to become an `App`.
- **`m6-md` and `m6-auth-cli`** are CLI tools, not services.

## 8. The smaller enhancements, re-framed

These were the plan before the shape question was settled. They survive, with
changed justifications.

| | now |
|---|---|
| **Read timeout** | Still do it first. Cheap, closes a live exposure on five services, and public traffic makes it urgent. Understand it as a stopgap: §3 removes the need. |
| **`send_with_length`** | Zero callers. m6-file's HEAD does a full `fs::read` + minify + brotli-6 then discards the body at `h1.rs:700`. Free to fix, independent of any of this. |
| **Wildcard route segment** | Still required. `Segment` is `Literal\|Param` and `match_route` demands exact segment-count equality, so no router can express a static file server. |
| **Socket permissions key** | Still required for m6-auth-server's `0666`. |
| **Streaming body** | Now clearly in scope rather than optional: a loop that cannot serve a body it has not materialised will buffer a whole file on a 950MB box. |
| **Unify `PoolManager`** | `PoolManager { pools, url_backends }` is two parallel collections for two transports. It already costs us: `total_active_members()` counts only socket pools, reads 0 on every cache node forever, and `monitoring.rs` special-cases health per role because of it. Good pilot: contained in m6-http, fixes a live defect. |

## 9. Bounding the work, and why it comes first

QJump's argument applies inside one box: **you do not get a latency bound by
optimising the mean, you get it by admitting work against a computed bound.**

m6 has no computed bound anywhere. It sheds at queue-full and calls that
admission control. And the parameters are not all known: worker count is, but
**maximum handler time is unbounded**, so there is no epoch to rate-limit
against.

This matters more than it did a week ago, because the site has not been
publicised yet and the current numbers are a pre-launch snapshot.

An event loop is also the only model in which admission control is
*expressible*: a loop can decline work, a blocking pool can only be full.

## 10. Sequence

**Pre-launch, none of it requires the rewrite:**

1. **Negative caching.** `BLOCKLIST.md` already calls it "the highest-value fix
   and is not yet implemented". Every 404 is uncacheable, so on a cache node
   each junk request crosses the Pacific and back at ~207ms.
2. **Compute the pool sizes.** m6-html at 6ms on one worker is a ~167/sec
   ceiling on the page every visitor lands on. m6-file at 32-on-1-core is the
   opposite error. Neither number came from arithmetic.
3. **Read timeout**, and bound handler execution.
4. **Classify `/health` in `App`**, as m6-http already does. The QJump move at
   its smallest, and it is what lets monitoring stay honest under load.

**Then, in order:**

5. **Benchmark Phases 5 and 6.** Owed already, and nothing below can claim "no
   regression" without it. On the build host, not the laptop.
6. **`send_with_length`**, and m6-file's HEAD path. A win, not a cost.
7. **Unify `PoolManager`** behind one transport-agnostic backend. The pilot for
   a single I/O interface, contained inside m6-http.
8. **Name the offload primitive in core**, generalising the `ConfigWatcher`
   self-pipe and `wake_fd` into the supported way to do blocking work.
9. **Wildcard route segment** and **socket permissions key**.
10. **Move `App` to the loop**: reads on the loop, connection state machine,
    handlers inline, offload for the blocking few. The handler contract change
    lands here.
11. **Migrate m6-file and m6-auth-server**, deleting two hand-rolled mains.
12. **Streaming body**, last, because it changes what a `Response` is.

After 11 there is one shape. m6-http is already in it.
