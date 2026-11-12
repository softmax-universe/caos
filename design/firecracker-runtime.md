# Firecracker runtime — the same job, in a microVM

**Status:** implemented as an opt-in runnerd runtime (`CAOS_RUNTIME=firecracker`,
`rust/crates/runnerd/src/firecracker.rs`, guest init
`rust/crates/runnerd/src/bin/caos-fc-init.rs`), run as a dedicated host pool
next to the stack. Nothing routes to it unless a job asks for its pool. Goal
for now: benchmark it against docker; maybe later, move work onto it.

---

## Shape

Only runnerd's "run this claimed job" step changes. The server, the runner
protocol, the job payload, the image and its entrypoint are all identical, so
a job produces the same ArgTree and result on either runtime:

```text
docker:       docker run … --entrypoint /bin/caos <image_ref> runner --job=<json>
firecracker:  firecracker --no-api --config-file vm.json
                vda  rootfs.ext4  read-only, shared   <- the image (converted once per digest)
                vdb  scratch.ext4 per VM, sparse      <- the container's writable layer
                vdc  job.json     argv/env/cwd/…      <- what `docker run` would pass
              guest PID 1 = caos-fc-init -> /bin/caos runner --job=<json>
```

What the VM rebuilds of an engine, and where:

| engine does                          | here                                                                |
|--------------------------------------|---------------------------------------------------------------------|
| pull + flatten the image             | `docker create` + `docker export` (same engine, same registry)      |
| image config `Env`/`WorkingDir`/`User` | `docker image inspect`, applied by init; `User` must be numeric   |
| overlay rootfs                       | overlayfs in the guest: vda lower, vdb upper                        |
| `/proc` `/sys` devpts shm mqueue cgroup2 | mounted by init                                                 |
| `/etc/hosts` `resolv.conf` `hostname` | written by init; DNS = host's non-loopback upstreams              |
| `--network caos-net`                 | per-slot tap on bridge `caosfc0`, NAT out                           |
| `-e CAOS_SERVER_URL=…`               | same, loopback host rewritten to the bridge address                 |
| exit status                          | init prints `caos-fc-init: exit=<n>` on the console, runnerd parses it |
| container log                        | the serial console (stdout and stderr merged)                       |
| `docker rm -f`, reaper               | VM dir removed; firecracker gets `PR_SET_PDEATHSIG`                 |

The rootfs is built WITHOUT root. `mkfs.ext4 -d <tarball>` (e2fsprogs ≥ 1.47.1)
reads the export directly and keeps its ownership and modes, including the
setuid-root `/bin/caos` the runner depends on. The cache key is the image
digest plus a hash of the init binary, because every rootfs carries its own
copy of the init and a rebuilt init must not be masked by an old conversion.
Tag refs (no `@sha256:`) are converted fresh every time.

Refused by name rather than run degraded: images that declare
`CAOS_GRANT_ENGINE_SOCKET`, `CAOS_GRANT_VOLUMES` or `CAOS_GRANT_DEVICES`. A VM
has no engine socket to bind in, volumes would have to be persistent disks, and
the guest kernel has no FUSE. `CAOS_GRANT_SYS_ADMIN` needs nothing, because the
guest's root is root of its own kernel.

## Running it

```sh
# once per boot, as root (bridge, user-owned taps, NAT, firewall accepts)
as-root --why "firecracker worker networking" -- env CAOS_RUNNER_SLOTS=4 dev/firecracker/setup-host

dev/firecracker/runnerd              # pool "firecracker"
dev/firecracker/runnerd docker       # the control: same runnerd, docker runtime, pool "docker"

caos-cli run --base:@=std/hello --who=vm --required-pool=firecracker
dev/firecracker/bench -n 20 -p 4     # same job on both pools, alternating rounds
caos-cli run --base:@=std/bash --worker1:@=dev/firecracker/probe.sh --required-pool=firecracker --probe=$RANDOM
```

`--required-pool` keeps the two pools apart. The rendezvous is symmetric, so
the stack's generic pool never takes these jobs and the pool never takes
anything else. Continuations inherit `required*` args
(`server::compute::inherited_pool_entries`), so a pinned job's chain stays on
its pool. Everything binary comes from the flake's `caos-fc-host` output:
runnerd, `caos-fc-init`, firecracker and e2fsprogs from cache.nixos.org, and the
guest kernel, which is the Firecracker project's own CI build pinned by hash.
That kernel has virtio, ext4, overlayfs and `ip=` autoconfig built in, so no
initramfs is needed. A nixpkgs kernel would carry those as modules.

Config (env, all optional except the kernel; `dev/firecracker/runnerd` sets
the paths): `CAOS_FC_KERNEL`, `CAOS_FC_BIN`, `CAOS_FC_INIT`, `CAOS_FC_MKFS`,
`CAOS_FC_STATE` (`~/.cache/caos-fc`), `CAOS_FC_VCPUS` (2), `CAOS_FC_MEM_MIB`
(2048), `CAOS_FC_SCRATCH_MIB` (16384, sparse), `CAOS_FC_HOST_IP` (10.231.0.1),
`CAOS_FC_PREFIX_LEN` (24), `CAOS_FC_TAP_PREFIX` (`caosfc-tap`), `CAOS_FC_DNS`,
`CAOS_FC_WORKER_REDIS_ADDR`. runnerd checks the kernel, init, DNS and every
slot's tap at startup and refuses to start without them, so a missing one
cannot show up later as a failed job. `CAOS_WORKER_SERVER_URL` (both runtimes)
separates the URL workers dial from the one runnerd polls.

## First numbers (2026-11-12, one laptop, rootless docker, `std/hello`)

Every job was a cold worker start on both sides: 26 containers and 26 VMs
over 26 jobs each. Client-side `caos-cli run` latency, in ms:

| run          | pool        | p50  | p90  | mean |
|--------------|-------------|------|------|------|
| sequential   | docker      | 586  | 670  | 804  |
| sequential   | firecracker | 875  | 980  | 1019 |
| 4 concurrent | docker      | 603  | 1392 | 824  |
| 4 concurrent | firecracker | 1204 | 1409 | 1153 |

Each figure is from one run of 10 (sequential) or 16 (4 concurrent) jobs per
pool. The maxima include a one-time image pull for docker, and the conversion
for firecracker (≈1.5 s, once per image digest).

Per-VM split from runnerd's `firecracker … ms` line, mean of 22 VMs: host setup
9, guest boot to the job's start 298, job ≈255 plus the runner's 2 s idle TTL
(`CAOS_RUNNER_TTL_MS`, the same wait a warm container pays), ≈170 firecracker
start and reboot. **The ~300 ms cold-start gap is almost all guest boot.**

## Known differences and next steps

- **PID 1 is `caos-fc-init`, not the runner**, so that the exit status can be
  reported. `tests/resident` identifies a runner by `/proc/1/stat`, and would
  see init.
- **Names on the engine network don't resolve** in a guest (`caos-registry`,
  `caos-redis`). Workers that dial them directly (flake-builder's
  `skopeo copy`) would need the bridge address and the published ports.
- **No `/proc` masking**, unlike docker's defaults. The boundary is the hypervisor.
- **Console throughput.** Output is carried over an emulated UART, one VM exit
  per byte. A chatty job would pay for its logging here; vsock would remove it.
- **VMs have fixed CPU and memory limits** (2 vCPU / 2 GiB), while containers run
  without limits. Benchmarks of real work must account for this.
- **Boot.** Firecracker snapshot/restore of a booted, idle guest would cut most
  of the 300 ms. So might a trimmed kernel command line or kernel.
- **Placement.** The pool runs on the host because the stack container has
  neither `/dev/kvm` nor the taps. Moving it inside means granting both.
