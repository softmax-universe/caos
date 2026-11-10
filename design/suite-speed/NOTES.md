# Test-speed investigation notes (2026-11-09 night)

## base1 — CAOS_SALT cold, unchanged tree (no rust rebuild)
- total 8m28s; test phase 449s; bring-up ~60s
- sum of per-test times 3046s; max-parallel 8 -> 380s floor
- CPU during tests: 35% us, 26% sy, 34% idle (16 cores)
- sampled busy CPU: rustc 31%, git 20% (receive-pack connectivity rev-list 12%),
  go 13% (+link 3%), podman 10%, server 6%, caos 4%
- slowest: git-import 253, stack-publish 213, mcp-serve 153, stack-merge-to-master 134,
  tree-map 128, stack-sync 128, root-tools 124, unit-clippy 117, actor 107

## Findings
1. git-import: 30k-commit push = 30s of 58s locally. FIXED ("tests/git-import: overflow a 1 MiB stack"): 1 MiB stack, 8k commits. 58->23s local, 253->76s in suite.
2. cargo bake misses: jobs run `check/clippy --all-targets` and `build -p` (dep jobs) but
   bake warmed only check/clippy without --all-targets + test --no-run. caos-iroh's dev-dep
   (tempfile) adds bitflags/std, fastrand/default, rustix -> iroh stack re-fingerprinted.
   caos-iroh dep job = 68-101s, on the critical path of unit-clippy, unit-test, unit-doc.
   FIX in progress (bake.nix). Cost: bake 467MB -> 700MB+ zstd; first use of a new cargo
   image pays a conversion (~150s once per lockfile change).
3. server ticket race: `GET /result` read pending, then checked live set -> "server restarted"
   when the ticket finished in between. Failed std-files in run2. Open PR #405 fixes it
   (with tests); the overnight commit for it was dropped in favor of #405.
4. Go: primed GOCACHE works (0.7s/build in image) — not the problem.
5. container start ~110ms in dev stack — not the problem.
6. clippy dep DAG is waste: clippy-driver wrapper is in members' fingerprint, so dep jobs'
   artifacts never satisfy clippy (verified in 2-crate ws). FIXED ("worker-cargo: clippy jobs do not wait for dependency artifacts").
7. Host redis 1.95 GB: 1.87 GB was dead v1 `caos:secrets:origins:*` lists (one had 7.9M items)
   + dead `caos:secrets:indexed:*`. Code says unread. BGSAVE every ~60s wrote 365 MB, AOF fsync
   stalls -> server redis read timeouts -> run3's host /wait 500'd after 9.6 min.
   DELETED by hand (UNLINK) -> 114 MB. Likely same on caosd-prod.
   /wait now retries reads for 60s ("server: /wait rides out a redis stall").
8. run2: 16.5 min total (bake rebuild ~7 min + first-use conversion of a bigger cargo image).
9. Local reference: cold `cargo clippy -p server -p worker-cargo --all-targets` incl. all deps:
   23s wall / 3.5 min CPU on 16 cores.
10. git: receive-pack connectivity `rev-list --objects --stdin --not --all` ~12% of busy CPU.
   Not fixed: refs must stay advertised to receive-pack (main.rs comment).
11. ROOT CAUSE of bake misses: worker image had no CC; bake ran under stdenv CC=gcc; blake3
   build.rs rerun-if-env-changed=CC -> blake3/iroh-relay/iroh rebuilt in every job reaching iroh.
   CC=gcc + full warm list -> 0 dep recompiles over 15 members x 6 commands ("std/cargo: set CC as the bake had it").
12. run4 (all but CC): 8m43, phase 476 (alphabetical tail: unit-test/unit-doc started at 380s).
    run5 (+CC): phase 369, sum 2338 (base 3046). total 8m37 (incl one-time cargo image push).
    run6 (trivial rust edit): 6m50 total, phase 359, bring-up ~50s.
13. Process CPU (run5): server 331u+280s, rustc 415, pigz/skopeo 370 (one-time cargo image),
    go 159u+112s (684 runs), git rev-list 59u+149s (532 runs), podman 120.
    ~1900 jobs per suite -> ~80ms server CPU/job.
14. Tail = long chain tests started late (alphabetical, max-parallel 8). Trying 24.
15. mcp-serve has 60s of deliberate sleeps (25+15+20) in serial stages.
16. max-parallel 24 (run7): phase 369 (= same), 2 load failures (chat-squash-publish wait-terminal, resident). Keep 8.
17. opt-level 2 for stack crates (run8): dev server CPU 145->121s/run, phase unchanged; +14s compile per edit. REVERTED.
18. procacct bug: counted lifetime CPU of long-lived procs (servers). Fixed to deltas. Dev server ~150s CPU/run (5%).
19. PODMAN is the bottleneck: trivial run 0.1s idle -> median 0.7s, p90 4s, max 15-48s under suite.
    disk: btrfs on dm-crypt, 84% full; 300KB write+fsync: median 6ms, p90 4.6s, max 20.8s under load.
    NVMe avg write latency 27ms at 146 MB/s. Writers: podman 38MB/s (layers.json rewrites, db.sql),
    conmon 19MB/s (ctr.log), caos get+runner 35MB/s (/cas/args materialization), redis 12MB/s.
    podman run+rm = ~55 fsyncs (40 in libpod sqlite db.sql + journal).
    prune (run10): 285->49 images, layers.json 800->300KB: no latency change. (store size not the lever)
    --log-driver none (run11): phase 386, noise.
    --transient-store + runroot on /dev/shm: hand test under load run+rm 1-2.5s -> 0.3-0.5s. run12 testing.
20. session-protocol compiled 18x per suite (183s CPU) via per-crate decomposition. Wall time small. Left.
21. 6617 loose refs, no packed-refs; connectivity check 0.45s -> 0.3s if packed. Not done.

## Probes (probes/)
- runsuite.sh: `CAOS_PROBE_DIR=<dir> probes/runsuite.sh <name> [caos-test args]` — salted cold run + samplers.
- procacct.py: per-process user/sys CPU deltas by command (rustc/clippy split by crate) -> acct.txt.
- syssample.py: what the dev server's running threads are in.
- podlat.sh <dir> <test-stack container> <image>: trivial `podman run` latency every 5s.
- disklat.sh <dir>: nvme write MB/s, writes/s and ms per write every 5s.
- tree.py / crit.py / leaves.py / sched.py <trace.json> ...: views of
  `curl -s localhost:9090/status/<suite req>?all=1` (subtree, critical path, leaf
  durations over time, last-finishing tests).

## Results by run (test phase / total)
base1 449/8m28 (CAOS_SALT, no edit) · run4 476 · run5 369 · run6 359/6m50 (rust edit)
· run7 369 (max-parallel 24, 2 failures) · run8 357 (opt-level 2, reverted) · run9 361
· run10 502 (after image prune: re-pulls) · run11 386 (log-driver none) · run12 346
(transient runroot) · run13 437 (outlier) · run14 345/5m51 (final tree, stack up).

## Open lead (2026-11-10, morning)
22. The server's pre-receive hook (`loose::Staging::publish`, #414, landed 2026-11-09)
    calls `syncfs` TWICE PER PUSH. syncfs flushes every dirty page on the whole
    filesystem — here one btrfs holding the podman store, every container's upper
    layer, the nix volume and $HOME — so each push waits for everyone's writeback, and
    everyone's fsync waits behind it. ~140 pushes per suite run. Likely source of the
    4-20s fsync tails in 19. Not yet measured or changed.
