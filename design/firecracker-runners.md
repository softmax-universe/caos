# Firecracker runners proposal

**Status:** implemented and verified (2026-11-11), jailed and unjailed. Phase 0 is in the shared runner layer and covered by `caos-test`
(`tests/warm-pool`, `tests/warm-crash`). The Firecracker backend is
`crates/runnerd/src/firecracker.rs`, the guest is `caos vm-init`
(`crates/caos/src/vm_init.rs`) with `nix/firecracker-appliance.nix`, and
`dev/firecracker-check/run` runs the acceptance checks on a KVM host, and
`dev/bench-runners/run` puts ballpark numbers (cold, warm, fan-out, RSS) next
to the Docker backend on the same workload. The
sections below describe what was built. Where it departs from the earlier
proposal, the section says so.

Add Firecracker as an optional execution backend alongside Docker. A runner
deployment boots a ready-made VM appliance that contains `caos runner` and its
worker implementation. The guest uses the existing caos runner protocol to
receive work and return results. Each VM has private writable storage; durable
outputs cross the boundary as CAS references.

This extends [the runner protocol](runner-protocol.md) and its warm and resident
worker lifecycles. Docker remains the default.

Two gaps this backend depends on are NOT Firecracker-specific: warm guests drop
their parent's pool constraint, and the host's crash backstop only knows the
first job's nonce. Both are latent in the Docker path today. They are fixed once,
in the shared runner layer, before any VM code (phase 0 below), and verified on
Docker where the existing suite can exercise them.

## Scope

The initial backend accepts a prebuilt kernel and filesystem image, pinned by
content identity, with an explicit mapping to the caos workload base it can
execute. It does not build an appliance from each submitted OCI image.

The appliance supplies `/bin/caos`, `/worker`, the worker user and permissions,
and a small init that prepares the guest and starts the runner. Its worker may
process independent jobs or use the existing resident worker mechanism to
process messages for an affinity key.

The host handles VM provisioning and supervision. The guest handles execution,
CAS materialization, secrets, result publication, and subsequent polls. No new
job transport, result format, scheduler, or host artifact service is needed.

## Workload selection

A fixed appliance must claim only work it implements. Configure each Firecracker
pool with an accepted `base` object id and a pool name. Jobs targeting it carry
that base and `required-pool=<name>`; the host's polls require both corresponding
object ids. Symmetric matching (`server::runner::matches`) excludes ordinary
Docker runners from these jobs and excludes unrelated work from the Firecracker
pool.

The base identifies execution semantics. The pool name selects placement, but it
is also part of identity: `required-pool` is an ArgTree entry, so it is in the
request hash. Moving a workload between Docker and a Firecracker pool changes its
cache keys, and two pools serving one base do not share cache entries. That is
intended here (a Firecracker base is its own execution semantics anyway), and
callers should expect a cold cache on first use.

Pin the appliance identity in deployment configuration and maintain the
base-to-appliance mapping explicitly; a change to worker behavior requires a new
base identity. Where possible, build the base image and the appliance from the
same flake (`design/flake-images.md`), so the mapping comes out of that build
rather than being kept by hand. The guest must not silently execute an
arbitrary job's Docker image using its fixed worker.

The current dispatch payload includes `image_ref`, and the server resolves an
image before dispatch. For the first milestone, use a normal resolvable base
whose worker matches the appliance, retaining this existing path. The
Firecracker host does not pull or boot that OCI ref, and the guest ignores it, as
a warm runner already does.

Appliance-only bases are a follow-up, and they have a precedent to start from:
the `docker://seeded…` sentinels are bases that are never pulled and are answered
only by a dedicated pool (`resolve_image` and `validate_docker_reference` in
`server/src/compute.rs`, and the seeder's `required-pool=seeded`). The
follow-up should decide whether to generalize that (for example a
`docker://appliance-<digest>` sentinel), and whether the provably-unanswerable
verdict (`seeded_verdict`) extends to appliance pools, before designing a new
identity scheme.

## Appliance boot contract

An appliance is three files and a manifest, built by
`nix/firecracker-appliance.nix` from a WORKLOAD (`dev/fc-test/workload.nix`):
the packages, the `/worker`, and the image env. The workload's Docker base is
built from that same file (`dev/fc-test/flake.nix`), so the appliance can't
disagree with the base a job names.

- `vmlinux`: nixpkgs' stock kernel. Its virtio and ext4 drivers are modules,
  so
- `initrd` (busybox plus those modules, decompressed at build time, and
  `nix/appliance-init.sh`) loads them, brings up `eth0` from the kernel command
  line, mounts the root disk, and `switch_root`s into `caos vm-init`;
- `rootfs.ext4` is the userland plus the caos additions `build-builtins.sh`
  layers onto Docker images (setuid-root `/bin/caos`, `/etc/passwd`,
  `/usr/bin/env`, a sticky `/tmp`), all root-owned, and
  `/etc/caos/image-env`;
- `appliance.json` pins the three files' sha256 and lists the image env.

**The image env is baked in at build time**, from the same list the Docker
image's config gets. The earlier proposal had the host read the base image's
config at startup and pass it in. Building both from one definition is simpler
and can't drift. `caos vm-init` gives the runner exactly that env plus the boot
env. Without it, a resident image would quietly run non-resident.

**Per VM**, the host supplies three disks and a command line:

| What | Where | Why there |
|---|---|---|
| private copy of `rootfs.ext4` | `/dev/vda`, rw | `/cas` and `/tmp` live on it |
| config: `{job, env}` JSON, NUL-padded (`caos_world::appliance::BootConfig`) | `/dev/vdb`, ro | the payload carries the runner token and the job's secrets, and `/proc/cmdline` is readable by the worker |
| report: the runner's stdout | `/dev/vdc`, rw | claim records, then the exit record `vm-init` appends; read after the VM exits |
| `caos.ip=`, `caos.gw=`, `caos.dns=` | kernel command line | not secret |

The boot env is the shared `worker_env` (`CAOS_SERVER_URL`, the
`CAOS_RUNNER_PARENT_REQUIRED` set, `CAOS_WORKER_REDIS_ADDR` when given), with the
server address replaced by one that resolves from the guest.

`caos vm-init` runs only as PID 1 with a real uid of 0, because `/bin/caos` is
setuid root and a worker must not be able to use it to mount or reboot. It
mounts the pseudo-filesystems, starts the runner, prints the readiness line
`caos-vm-init: runner started` to the console, reaps orphans until the runner
exits, appends `caos-vm-init exit=<code>` to the report, syncs, and reboots.
Under `reboot=k` a guest reboot makes Firecracker exit, so the VM ends exactly
when its runner does. The exit record goes through a dup of the runner's
stdout, which shares its offset. `O_APPEND` on a raw block device wrote it at
offset 0, over the claims.

## Job lifecycle

1. A free host slot polls the existing `/runner/poll`, constrained to its
   configured base and pool.
2. On assignment, the host creates the VM's private storage, report device,
   network interface, and boot configuration, then starts Firecracker (under the
   jailer, unless `CAOS_FC_JAILER=none`) with the configured kernel and
   appliance.
3. Guest init applies the boot contract and invokes
   `caos runner --job=<payload>`.
4. The guest materializes the ArgTree, executes `/worker`, publishes outputs to
   CAS, and posts `/runner/result` using the existing nonce and authentication.
5. The guest polls for further compatible work, carrying the host's required
   set on every poll (phase 0). Resident workers retain their existing affinity
   ownership, leases, and `caos next` behavior.
6. When the guest runner exits, init shuts down the VM. The host reads the
   report device, collects exit information, posts the crash backstop if needed,
   and removes private disks, network resources, and boot configuration before
   reusing the slot.

The host does not poll while its VM is alive. One slot represents one VM running
one job at a time, including a resident worker processing successive messages.

## Messages and durable results

Requests and results remain ArgTrees and git objects. A message can contain
small values directly and refer to larger inputs or outputs by CAS object id.

A worker may write a file to its private filesystem while processing a job.
Before reporting success, it publishes the durable output through the existing
CAS facilities, including the runner's `/cas/out` collection. The result carries
the resulting object reference. A guest-local pathname alone is not a durable
result: the VM and its disks may disappear immediately after completion.

Resident scratch state can survive successive messages within the same VM.
Durability across VM replacement requires explicit CAS publication; this
backend does not add automatic actor state recovery.

## Storage and connectivity

Share only immutable appliance material. Each VM receives its own writable root
copy or a read-only root with private writable storage, according to the
appliance's boot contract. Start with the simplest supported private-copy
layout. Never attach one writable filesystem to multiple VMs, and never mount
a live guest filesystem on the host to extract results. (The report device is
read only after the VM has exited.)

Use guest networking to reach the caos server and CAS through their existing
interfaces. Configure routing and DNS explicitly; Docker bridge names and
container-local addresses are not assumed to work in a guest. Both
`CAOS_SERVER_URL` and `CAOS_WORKER_REDIS_ADDR` take the same two-value shape
they have in `stack/serve`: the deployment supplies whichever form resolves from
the guest, and an unset redis address reaches the guest as an absent variable,
never an empty one. Workloads that need external services may receive outbound
connectivity under deployment policy. Vsock is not needed for job submission or
results.

## Grants: refused when the appliance is built

Docker grants are declared in an image's config env (`CAOS_GRANT_*`), and
runnerd reads them per job (`image_grants`). None has a VM translation. The
appliance builder asserts that its workload declares none, so a workload that
needs one fails at `nix build`. The host checks `appliance.json`'s image env
again at startup and refuses to start if a grant appears there.

## Supervision and failure handling

The host waits `CAOS_FC_BOOT_TIMEOUT_SECS` (20s) for the readiness line. Missing
it is a BOOT failure: the VM is killed and the job fails with the console tail.
After the readiness line there is no timeout. An ordinary slow job is not
grounds for requeue or duplicate execution.

The console goes to a FILE in the VM's directory, which runnerd follows. It is
not a pipe to runnerd. With a pipe, a runnerd restart broke the pipe and the
orphaned Firecracker hung in the guest's shutdown instead of exiting, so a VM
couldn't outlive its supervisor cleanly.

If boot fails before handoff completes, post a failure (not a requeue) for the
assigned nonce through the existing result endpoint. The job never ran, so
requeueing would not duplicate work, but a broken appliance would then requeue
forever. The server never caches a failure, so the caller can retry.

On a guest crash, preserve first-result-wins and resident lease-expiration
semantics. The host learns which job was in flight from the claim records
defined in phase 0 (it reads the report device after the VM exits), and posts
the backstop failure for that nonce and for the initial one; a 410 for either
just means the job was already settled. That covers a crashed or killed guest. A
guest that hangs while still alive is not covered, and neither is a hung Docker
container. Per-job leases (the runner-protocol doc's "dead-worker detection")
remain future work for both backends.

State lives under `CAOS_FC_STATE/<owner id>/`. The owner id is runnerd's
hostname, its container id under Docker, as for the Docker reaper. That
directory holds `vms/<nonce>/` (pid, tap name, console, chroot path, and the
disks when unjailed), `jail/` (the jailer's chroot base), and `appliance/` (the
verified kernel and initrd copies). At startup runnerd reaps every
`vms/<nonce>` whose Firecracker is gone: tap, chroot, and directory. It leaves a
live one alone. "Live" means a non-zombie process named `firecracker`, since an
orphan that exited lingers as a zombie and a pid can be reused. Another
deployment's directory is never read. A VM's storage is deleted only after its
process has exited. The config disk (token, secrets) is mode 0600 and goes with
the VM.

Firecracker runs under its **jailer** by default (`CAOS_FC_JAILER`, `none` to
opt out, with a warning). Each VM gets a chroot holding only its own files and
hard links to the verified kernel and initrd. Slot `s`'s VMs run as uid and
gid `CAOS_FC_JAIL_UID + s` (30000 by default), their disks are chowned to that
uid, and their tap is created `user <uid>`, so one VM's process can't open
another's disks. Resource limits are the VM's vCPU and memory
(`CAOS_FC_VCPUS`, `CAOS_FC_MEM_MIB`) plus a jailer rlimit. Cgroup limits are not
set, because Docker mounts the cgroup tree read-only. Startup refuses: no
usable `/dev/kvm`, a digest mismatch, a grant in the image env, a missing
`firecracker`/`jailer`, and a required set without both `base` and
`required-pool`. That last one exists because a generic Firecracker pool would
run any job's args with its one worker. The VM boundary doesn't change the
authority the runner token grants.

## Implementation plan

### Phase 0: shared runner fixes (Docker only, no VM code) — done

These go first and ship on their own. Each is testable with the existing suite.

**0a. Carry the parent's required set through guest polls.**
- runnerd passes its resolved required set (name → oid, the same map it polls
  with) to each container as `CAOS_RUNNER_PARENT_REQUIRED`, a JSON object. It
  passes oids rather than `name=value` pairs, so the guest needs no hashing and
  the value doubles as the lineage entry.
- `caos runner` reads it once (absent = `{}`). `next_job`
  (`caos/src/runner.rs`) polls `required = parent ∪ {base}`, and `poll_owner`
  polls `parent ∪ {base, affinity}`. Both send `lineage: [parent]` instead of
  the hard-coded `[{}]`.
- Today `stack/serve` only starts a generic runnerd, so nothing hits this gap
  yet. A pooled runnerd's warm containers currently can't match their own
  pool's jobs (the symmetric half of `matches` rejects them), and can take
  same-base work from outside the pool.
- Unit test, next to `a_pool_arg_excludes_the_generic_runner_…` in
  `server/src/runner.rs`: a guest poll formed from a pooled parent matches that
  pool's same-base job and rejects the unpooled one. Its eviction lineage also
  matches a pending pooled job.

**0b. Claim records for the crash backstop.**
- On every job it claims, including the initial one, `caos runner` writes
  `caos-runner claimed req=<arg_tree> nonce=<nonce>` to its own **stdout** and
  flushes it before starting the worker (with `sync_data` when stdout is a file
  or device). Nothing else uses the runner's stdout in runner mode: worker
  stdout and stderr are piped and relayed to stderr, so a worker cannot forge a
  record.
- runnerd already captures container stdout (`.output()`). On a nonzero exit it
  posts the backstop failure for the last claimed nonce as well as the initial
  one. It never relays stdout to its own log, which keeps nonces out of stack
  logs.
- Test: a Go worker test on `std/go`, following the `tests/resident` pattern,
  where the second job on a warm container kills the runner. The caller must
  receive a failure instead of hanging. Today it blocks forever, because claimed
  jobs have no deadline.

**0c. Backend seam in runnerd.**
- Factor out a small `Backend` trait: `start(job) → handle`,
  `wait(handle) → {status, stdout, stderr_tail}`, `remove(handle)`,
  `reap_leftovers()`, and `validate()` at startup. The poll loop, required-set
  parsing, and `post_failure` stay shared. Docker becomes the first
  implementation with no change in behavior. Grants stay Docker-specific
  (per-job `image_grants`).
- Selected with `CAOS_RUNNER_BACKEND=docker|firecracker` (default `docker`).
- `CAOS_RUNNER_REQUIRE` also takes `name:hash=<oid>`, the caos CLI's own form.
  A pool names its `base`, an image TREE, that way. `name=value` still names
  the blob of `value`.

### Phase 1: Firecracker, one cold job — done

1. Choose packaged Firecracker and jailer releases. Check that the exact Nix
   dependency closure is in trusted caches before any substantial source build.
   Keep all other package versions as they are.
2. Configuration: kernel and rootfs paths with their content digests (verified
   at startup), accepted base (as a `CAOS_RUNNER_REQUIRE`-style pair together
   with the pool), vCPU and memory limits, tap/bridge and DNS settings,
   state/chroot root, boot readiness timeout. Startup checks: `/dev/kvm`
   usable, jailer present, digests match, no grant in the image env. (As
   built, the image env is baked into the appliance from the workload
   definition rather than captured from the base at startup; see "Appliance
   boot contract".)
3. Appliance build: a flake output producing kernel plus ext4 rootfs with
   `/bin/caos`, `/worker`, the worker user, and an init that mounts the boot
   config, exports its env, sets `caos runner`'s stdout to the report device,
   and powers off when the runner exits.
4. `FirecrackerBackend`: per-VM rootfs copy, report device, tap, boot config,
   jailer launch, readiness timeout, wait, teardown in order (VM, then storage,
   then network).
5. Complete one job end to end on a resolvable base with a CAS file result.

### Phase 2: warm, resident, recovery — done

With phase 0 in place, warm reuse and resident messages need no new mechanism.
Exercise them, then add owner-scoped reconciliation and test supervisor restart
with live and exited VMs present.

### Phase 3: docs and checks — done (see "Deployment")

Deployment documentation. Run the full `caos-std/caos-test` suite (salted where
push paths are involved) and `caos-std/caos-build`, plus the integration checks
below on a KVM-capable host.

### Acceptance

On Docker (phase 0): a pooled warm container takes its own pool's next job, does
not take same-base work from outside its pool, and is evicted toward its pooled
parent. A crash during a later warm job fails that job promptly.

On Firecracker: two independent jobs on one warm VM; successive resident
messages; a file result retrievable after VM destruction; exclusion from the
wrong pool; isolated concurrent VM writes; boot-failure, crash, and restart
cleanup; and the later-job crash path specifically, not only the initial
handoff. Identify VMs the way `tests/resident` identifies containers, by
`/proc/1/stat`'s start time, which in a VM is guest init and is still unique
per VM. Don't use `/etc/hostname`.

### Verified on a rootless-Docker machine

The jailer creates device nodes in each chroot, and `mknod` never works in a
user namespace, so under ROOTLESS Docker the pool container can only run
unjailed. `dev/firecracker-check/run` handles both:

- `FC_POOL=docker` (the default) runs the pool as a container, and runs it
  unjailed under rootless Docker, saying so.
- `FC_POOL=host` runs runnerd as root on the host, under the jailer. It builds
  as the user, then re-runs itself as root ONCE, through `$SUDO` (default
  `sudo`). All nine checks passed both ways.

Two things only the jailed run found:

- **The jailer needs a STATIC Firecracker.** It copies the binary into the
  chroot and execs it there, so nixpkgs' glibc build fails with "Failed to exec
  into Firecracker: No such file or directory". The pool uses upstream's
  release (`nix/firecracker-release.nix`, pinned to upstream's published sha256,
  x86_64 only). nixpkgs' `pkgsStatic`/`pkgsMusl` builds are not in the binary
  cache.

### Measured (one host, single observations, not a benchmark)

On the build machine (Linux 7.2, nixpkgs kernel 6.18 guest, 1 vCPU / 512 MiB):
an unnetworked appliance boots, runs its runner, and powers off in about 0.45s.
A first pool job took 1.6s end to end from `caos-cli run`, VM boot included.
The next job on the warm VM took 0.21s. Measure on the target host before
making performance claims.

### Out of scope

Snapshots, automatic OCI-to-VM conversion, shared persistent filesystems,
grant translation, appliance-only base resolution, per-job leases, and
alternate transports. Measure boot time, end-to-end latency, and host resource
use before making performance claims.

## Deployment

A pool is one runnerd with `CAOS_RUNNER_BACKEND=firecracker`. The image
`.#caos-runnerd-firecracker-docker` packages it with what it shells out to
(firecracker, jailer, iproute2, iptables, coreutils). It needs:

- **KVM and taps**: `--device /dev/kvm --device /dev/net/tun --cap-add
  NET_ADMIN`; for the jailer also `--cap-add SYS_ADMIN --security-opt
  seccomp=unconfined` (Docker's default profile refuses `pivot_root`), and a
  ROOTFUL engine, or runnerd as root on the host. The jailer also needs the
  static upstream Firecracker, which `.#firecracker` and both pool packages
  provide.
- **The appliance**: `nix build .#caos-fc-test-appliance` (one per workload,
  and per world: `-test-world` for a dev stack), mounted read-only, at
  `CAOS_FC_APPLIANCE`.
- **What it serves**: `CAOS_RUNNER_REQUIRE="required-pool=<name>
  base:hash=<oid>"`, where `<oid>` is what `caos-cli eval-path <workload dir>`
  prints. Jobs name the pool with `--required-pool=<name>`.
- **A route from the guests to the server**: each slot's guest is
  `172.30.<n>.<4s+2>/30` (`CAOS_FC_SUBNET`), with the host end as its gateway.
  If the server is not on that host, set `CAOS_FC_GUEST_SERVER_URL` and NAT the
  subnet (`iptables -t nat -A POSTROUTING -s 172.30.0.0/16 -o eth0 -j
  MASQUERADE` with `net.ipv4.ip_forward=1`), as `dev/firecracker-check/run`
  does. That NAT is also all the outbound access a workload gets, so a
  deployment that wants less should filter it. `CAOS_FC_GUEST_REDIS_ADDR` and
  `CAOS_FC_DNS` are optional. Without a redis address, no cache is offered.
- Sizing: `CAOS_RUNNER_SLOTS`, `CAOS_FC_VCPUS`, `CAOS_FC_MEM_MIB`, and the
  appliance's `diskMiB` (the root disk, which bounds `/cas`). State goes in
  `CAOS_FC_STATE`, which must be one filesystem (the kernel is hard-linked
  from it).

`stack/serve` can start dedicated Docker pools too (`CAOS_STACK_POOLS="name=slots
…"`). The dev stack runs one, `test-pool`, for `tests/warm-pool`.

## References

- [Runner implementation](../rust/crates/caos/src/runner.rs): `next_job`,
  `poll_owner`, `Heartbeat`
- [Host runner implementation](../rust/crates/runnerd/src/main.rs):
  `image_grants`, `run_container`, `reap_leftovers`, `post_failure`
- [Server matching and results](../rust/crates/server/src/runner.rs): `matches`,
  `pool_oid`, `seeded_verdict`
- [Image resolution and seeded sentinels](../rust/crates/server/src/compute.rs):
  `resolve_image`, `validate_docker_reference`
- [Firecracker boot requirements](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md)
- [Firecracker networking](https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md)
- [Firecracker jailer](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md)
