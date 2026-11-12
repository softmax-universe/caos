# Operational hardening — what was closed, and what was not

**Status:** implemented 2026-11-11 for the first six items; the last three are
open and say why. Written after a review of the runner protocol, the compute
pipeline and the stack bring-up for the ways an ordinary workload, rather than
an attacker, could take a stack down.

## Closed

### Worker containers run under resource ceilings

`docker run` set no memory, CPU or pids limit on a worker. Capacity is counted
in slots, and a slot says nothing about memory, so one runaway `cargo test` or
an agent's bash tool could take the host — and caosd is one process group on
that host, so the OOM killer takes redis, the registry and the server with it.

`crates/runnerd/src/docker.rs` (`Limits`) passes `--memory`, `--cpus` and
`--pids-limit` from `CAOS_WORKER_MEMORY`, `CAOS_WORKER_CPUS` and
`CAOS_WORKER_PIDS_LIMIT`. Defaults: three quarters of the host's MemTotal, no
CPU cap, 32768 pids. `0` or `none` lifts one.

The defaults are opportunistic: they apply only where `docker info` reports
the engine can enforce them (`MemoryLimit`, `PidsLimit`). The podman a dev
stack carries runs with `cgroups = "disabled"` (`dev/devbox/flake.nix`) and
reports neither, and a limit that engine cannot apply would fail every job
rather than bound one. An explicit value is passed through whatever the
engine says, so a misconfiguration fails loudly on the first job.

The Firecracker backend already bounded its guests (`CAOS_FC_VCPUS`,
`CAOS_FC_MEM_MIB`); nothing changed there.

### Host-level grants need the runner's trust, not only the image's word

`CAOS_GRANT_ENGINE_SOCKET`, `CAOS_GRANT_SYS_ADMIN` (which also turns seccomp
and AppArmor off) and `CAOS_GRANT_DEVICES` are declared in an image's own
config env. That is a statement of need: anyone who can publish an image to
the server — any ticket holder, through the flake-builder — can declare one,
and runnerd honored every declaration.

`CAOS_RUNNER_TRUSTED_IMAGES` on runnerd (`docker::Trust`) now decides. Unset
keeps the old behavior, logged per image so it is visibly a choice; `none`
grants no image any of the three; a list of `sha256:<digest>` grants only
those. `CAOS_GRANT_VOLUMES` stays outside the gate: runnerd derives the
volume names, so a volume grant reaches only this deployment's storage.

The default is still permissive, and deliberately: the dev placement's devbox
image declares SYS_ADMIN and `/dev/fuse` (it has no engine socket — it runs a
podman of its own), and its digest moves with every build, so the host stack
that runs dev stacks cannot allowlist it statically. Turning the gate on for
a production pool is one env var on that pool.

### Runner auth is on by default

With `CAOS_RUNNER_TOKEN` unset, anything that could reach the server could
park a poll, claim a job and post a result; the result is cached under the
ArgTree hash and pinned at `refs/caos/res/`, with nothing checking it came
from running the image. `stack/serve` never set the token.

It mints one now on first boot, keeps it at `$CAOS_STACK_STATE/runner-token`
(0600) so a restart keeps it, and hands it to the server, every runnerd pool
and the seeder; workers inherit it through the job payload, as before. A
runner outside the group reads the file (`dev/firecracker-check/run` does,
from the stack container). `CAOS_RUNNER_TOKEN=none` turns auth off for a
stack whose network is itself the trust boundary.

### Redis connections are pooled

Every redis exchange dialed a fresh TCP connection. A map's fan-out appends
several trace events per child, so a thousand-child map was thousands of
handshakes to a daemon in the same container, each inside the 5s
`REDIS_TIMEOUT` budget. `compute::redis_exchange` keeps up to 32 idle
connections per address; a connection that completed an exchange cleanly is
at a command boundary and is reused, one that failed in any way is dropped,
and a pooled connection redis closed while idle is retried once on a fresh
one.

### A client re-attaches to its ticket

A run is a ticket on the server, content-addressed and joinable, but the
client's `GET /wait/<ticket>` was one HTTP call: a dropped connection or a
server restart failed the run on the client, and nothing retried. The server
also reported a restarted-under ticket as a 500.

The server says 503 now, and `caos::wait_for_ticket` treats a 503 or a
transport failure as "the wait failed, not the work": it starts the same
bundle again (the same ticket — joined if still running, re-run if its server
died) and waits again, up to thirty attempts with backoff. Any other answer,
the result or a failure the work recorded, is final. `tests/tracing` kept its
own restart handling; it should no longer fire.

Found on the way, by `tests/expression-http`: a ticket whose work fails at
once (a `:@=` base is refused before anything runs) could finish between its
waiter's first state read and the liveness check, and the waiter reported
"the server restarted". `expression_result` re-reads a ticket that is pending
by this very process and not live here, since the ticket's thread records its
answer before leaving `live_tickets`; only a failed record stays pending, and
the message now says that.

### The server bounds its in-flight requests

The accept loop spawned a 64 MiB-stack thread per connection with no cap.
`CAOS_MAX_INFLIGHT_REQUESTS` (default 4096, `0` lifts it) bounds it, by
refusing rather than waiting: past the cap a request is answered 503 with
`Retry-After: 1` on the accept thread. A runner's poll just polls again, a
client's wait re-attaches, a worker's fetch fails its job — which under a
real flood is the right thing to lose. Blocking accept instead would be a
deadlock in waiting, since the parked polls and the `/object` fetches a
running job needs are themselves requests.

## Open

### Authoritative state shares an LRU cache with the result cache

One redis, `allkeys-lru` at 2 GB, holds results, converted images, layers,
traces — and also tickets, ticket-owner leases and the secrets origin index,
which are state, not cache. Eviction of a state key is a correctness failure:
a waited-on ticket vanishing reads as "unknown ticket". It is mitigated, not
fixed, by the waiter's own polling keeping the key recent.

Not changed here because `allkeys-lru` is a documented decision
(`stack/serve`): ccache's redis backend, a tenant of the same instance, sets
no TTL and no prefix, so `volatile-lru` would evict nothing of its and the
cap would turn into OOM errors. The fix is a second redis for state, with
`noeviction`, in the group — a bring-up change touching every placement, and
one the dev placement (which shares the host's redis by namespace) needs its
own answer for. It should land before `durable-resolution.md` moves pending
nodes and leases into redis, which would make eviction far more consequential.

### In-flight promise resolution is not durable

Known and designed (`durable-resolution.md`): resolution blocks one server
thread per pending node, a restart loses it, and a second server cannot
participate. The client re-attach above covers the top-level wait — the
restarted server re-runs the ticket, and memoization makes the replay cheap —
but a map in flight at the restart is recomputed from its last cached
children, not resumed.

### The object store has no collector

Known and open (`caos-store-and-gc.md`, with the store half decided in
`loose-object-store.md`): git's collector would delete most of the store,
because the server's liveness model is not git's, so GC is off and the store
grows without bound. The retention policy and the collector are the open
sections of that note.
