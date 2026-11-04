# caos: a loose-object store

Status: decided; the server change is built (this section's PR), the
migration is not. The store stays on prod's ext4 `/data`: power-cut tests
showed ext4 keeps this design crash-safe and btrfs does not (section 3).
Replaces the pack-maintenance part of `design/caos-store-and-gc.md`;
retention (the collector) is still open there.

## Summary

Stop packing. Every object in the server repo becomes a git loose object,
written at zlib level 1 (git's own default for loose objects), on the ext4
volume it lives on today. Nothing ever rewrites the store, so there is no
repack, no downtime consolidation, no pack-count ceiling and no reader that
can see a pack move under it. Git and gix keep reading the directory as an
ordinary object database, so every tool and the git protocol keep working.

The real work:

1. **Ordered publication.** Every path that writes packs today does so on
   purpose: a pack becomes visible all at once, when its index appears. Loose
   objects appear one file at a time, and git publishes them in directory
   order, not dependency order, so a crash or a concurrent reader could see a
   tree before its children. Every ingress path goes through one routine that
   publishes children before parents (section 5.1).
2. **Imports and locators** wrote packs from server code (`import.rs`,
   `locator.rs`). They switch to that routine.
3. **The stack limit.** Every push is now unpacked by
   `unpack-objects --strict`, which recurses once per commit of a new chain;
   the server raises the stack limit its git inherits (section 5.3).
4. **Migration**: a short downtime that explodes the existing packs into a
   fresh `objects` directory on the same disk and swaps it in (section 7).

The cost is about 4x the disk of one pack (10.9 GiB for today's 1.30M
objects, against an estimated ~3.3 GiB packed) and more CPU to serve large
fetches. Both were judged acceptable.

## 1. Why

The store only grows packs, and every read pays for every pack.

- Every push leaves a pack (`receive.unpackLimit 1`), every import hard-links
  one, every locator indexes one. caosd-prod went from 2,864 packs at 22:00 to
  3,180 at 23:20 UTC on 2026-11-05, about 4 a minute under load.
- gix probes each pack's index until one hits. On dev, 8,583 packs took the
  test suite from 2 min 28 s to 68 min 39 s (PR #351).
- gix fixes its pack slot table at open: the server reserves the maximum,
  32,767, and refuses to start at that count (`main.rs`).
- Consolidating is only safe with the stack down (a live repack crashes gix
  readers and truncates fetches), so it ran at `caosd up`
  (`repack --geometric=2 -d` under `CAOS_STORE_CHECK`). Between restarts the
  count climbs.

Loose objects have none of this: a read is one path lookup, a write is one new
file, and no operation ever moves an existing object.

## 2. Measurements

### 2.1 What is in the store (2026-11-05, 993,381 objects)

| type | n | total | p50 | p90 | p99 | max | <= 2 KiB |
| --- | --- | --- | --- | --- | --- | --- | --- |
| all | 991,823 | 14.3 GiB | 294 B | 8.8 KB | 279 KB | 124 MB | 79.8% |
| blob | 290,757 | 12.3 GiB | 1,977 B | 43 KB | 546 KB | 124 MB | 50.5% |
| tree | 623,181 | 1.8 GiB | 160 B | 1.2 KB | 52 KB | 489 KB | 92.7% |
| commit | 77,885 | 158 MiB | 561 B | 2.8 KB | 28 KB | 2.1 MB | 86.2% |

Only 269-296 MB of the ~2.5 GiB (packed) is reachable from the 14,773 refs:
about 90% of the store is addressed by hash alone, which is why git's own gc
must never run here.

### 2.2 Disk (2026-11-07, 1,296,765 objects, prod's ext4 features)

Exploded twice from a scratch repo with alternates into a loop-mounted ext4
image with prod's exact feature list; both runs matched the object count and
a 2,000-object hash sample.

| layout | on disk | apparent | explode (CPU) |
| --- | --- | --- | --- |
| ext4, loose, level 0 | 19.5 GiB | 15.3 GiB | 219 s (190 s) |
| **ext4, loose, level 1 (chosen)** | **10.9 GiB** | 6.6 GiB | 407 s (319 s) |
| btrfs `compress-force=zstd:3`, level 0 (993,381 objects, 2026-11-05) | 6.79 GiB | - | 136 s |
| one pack (993,381 objects, 2026-11-05) | 2.51 GiB | - | 8:51-9:16 |

- Every object under 4 KiB still takes a whole 4 KiB block on ext4
  (no `inline_data`), which is the 4.2-4.3 GiB between on-disk and apparent
  size at either level.
- Level 1 halves the disk cost for a few microseconds of inflate per object
  read (a few KB each) and zlib CPU per write. On ext4 nothing else compresses.
- Each object costs one inode: 1.30M of the volume's 67.1M.
- The store grew from 993,381 to 1,296,765 objects between the two
  measurements (open question in section 9). With no collector yet, level 1
  and 786 GB free leave years at 2026-11-05's rate, and well over a year even
  if the jump is real growth.

### 2.3 Reads (10,000 random objects, `cat-file --batch`, on btrfs)

| | loose cold | loose warm | one pack cold | one pack warm |
| --- | --- | --- | --- | --- |
| wall | 7.37 s | 0.12 s | 13.72 s | 0.43 s |
| user+sys | 0.72 s | 0.11 s | 0.87 s | 0.41 s |

Warm reads are 3.6x faster loose: no delta chains to resolve. Measured at
level 0; level 1 adds an inflate per object, small next to the file open.

### 2.4 Serving fetches (`pack-objects`, as upload-pack runs it, on btrfs)

user+sys CPU / bytes out. L10 is git's default delta window; L0 is
`pack.window=0`; L0C1 adds `pack.compression=1`. "Live" is the packed repo,
warm.

| fetch | objects | loose L10 | loose L0 | loose L0C1 | one pack | live |
| --- | --- | --- | --- | --- | --- | --- |
| all refs | - | 192 s / 269 MB | 32.7 s / 561 MB | 16.3 s / 745 MB | 12 s / 296 MB | 8.3 s / 242 MB |
| conversation head | 794 | 0.19 s / 307 KB | 0.17 s / 682 KB | 0.12 s / 1.07 MB | 0.41 s / 549 KB | 0.15 s / 315 KB |
| imported source commit | 13,283 | 3.42 s / 31.8 MB | 4.04 s / 70.9 MB | 2.48 s / 92.5 MB | 0.85 s / 60.4 MB | 0.33 s / 35.0 MB |
| incremental, 10 commits | 30 | 0.03 s / 31.5 KB | 0.03 s / 43.8 KB | 0.03 s / 80.7 KB | 0.18 s / 31.4 KB | 0.07 s / 31.5 KB |

Small and incremental fetches cost what they cost today. Large fetches cost
several times more CPU, because the loose store compresses every object again
for every fetch, while packs hand out the deltas clients already computed when
they pushed. Accepted: large fetches are rare and not on any
latency-sensitive path.

### 2.5 Write cost (ext4 against btrfs, same box, no other load)

| operation | ext4 | btrfs, tree log | btrfs, `notreelog` |
| --- | --- | --- | --- |
| `write_durable`-style, 1 thread | 167/s, 5.9 ms | 185/s, 5.4 ms | 84/s, 12.6 ms |
| `write_durable`-style, 8 threads | 767/s, 10.6 ms | 410/s, 19.0 ms | 205/s, 39.2 ms |
| `POST /object`, 1 client | 152/s, 6.2 ms | 163/s, 6.0 ms | 72/s, 14.0 ms |
| `POST /object`, 8 clients | 779/s, 10.3 ms | 490/s, 16.3 ms | 218/s, 36.6 ms |
| push of git.git `v1.5.0` (~34k objects) | 104.8 s | 79.0 s | 262.4 s |

Rates with p50 latency, at level 0.

## 3. Crash safety: why ext4 and not btrfs

The design needs one property from the filesystem: after a power cut, the
store holds a closure-complete set of objects, i.e. renames issued in order
are never persisted as a later one without an earlier one, and no renamed
object is persisted without its data. Two power-cut campaigns tested it on
the PR's binary and on synthetic rename loops (reports: "Loose objects on
btrfs: power-cut results" and "Loose objects on ext4: power-cut results",
2026-11-06 and 2026-11-07).

**btrfs failed.** With a concurrent `write_durable`-style writer (which
fsyncs a fan-out directory), 5 of 6 synthetic runs lost ordering, and real
pushes cut mid-publish left broken closures. An fsync on btrfs goes through
its log tree, which can make one directory's late rename durable without the
earlier renames in other directories. Mounting `notreelog` fixed the
synthetic runs, at 2-4x the write cost (section 2.5).

**ext4 held.** All 35 synthetic runs passed, and all 6 real-server runs cut at
10-90% through a push's publish passed the closure check and started with
`CAOS_STORE_CHECK=yes` (plus 9 time-triggered runs). ext4 in `data=ordered`
mode has one metadata journal: transactions commit in order, and an fsync
forces the current transaction, which carries every earlier metadata change
with it, so a directory fsync cannot make a late rename durable without the
earlier ones. A commit also writes the data of the files it allocates first,
and the server fsyncs every object's data before its rename anyway. The
control run showed what that fsync is for: written with no sync and renamed
(gix's old pattern), every surviving file was zero-length.

Two filesystem settings this depends on, both true of prod today:

- **`data=ordered`** (the default; prod mounts `rw,relatime`).
- **No `fast_commit`.** ext4's per-inode fast-commit path would bring back a
  log-tree-like shortcut. Prod's volume does not have the feature and the
  tests ran without it. It must stay off (`tune2fs -l` lists features).

What the tests do not cover: `sysrq b` drops the guest's page cache but not
EBS's acknowledged writes, so a disk that loses its own cache is out of scope.

## 4. The store

- **Format**: git loose objects under `objects/xx/yyyy...`, zlib level 1.
  Every git and gix reader handles it. The object id hashes the
  uncompressed bytes, so the level never changes an id, and objects written
  at other levels (git's, or level 0 by an earlier draft) sit beside them.
- **Never packed.** `objects/pack` stays empty. Nothing deletes or rewrites an
  object except a future collector, by unlink.
- **Volume**: prod's `/data` as it is: ext4, `data=ordered`, no
  `fast_commit`, 1 TiB gp3 at 4,000 IOPS. No host configuration changes.
  (The btrfs host change, softmax-universe/caos#419, is closed. Its one
  finding that outlives it: prod's Docker runs the containerd `overlayfs`
  snapshotter, not the `overlay2` graph driver this doc once named.)
- **Dev stacks** keep whatever filesystem they have.

Server settings. All of them, these and the ones the server already had, are
passed to git in the server's own environment (`GIT_CONFIG_COUNT`, from
`main::git_settings`) on `http-backend` and the server's other git commands
against the store, and are NEVER written into the repository's config.
Several servers of different builds share one repository (every test stack
shares its host stack's), so a setting or hook written into the repo would
apply to all of them; an earlier draft of this change did exactly that and
broke pushes on the host. A repository the server creates itself gets the
settings written in once, and its config left read-only (0444) as a marker.
The mode does not stop git, which rewrites a config by lock and rename
whatever its mode; `chattr +i` would, but servers before this change rewrite
the config at every start and fail when they cannot, so that is a step for
the host once every server sharing it runs this build.

| setting | value | why |
| --- | --- | --- |
| `core.looseCompression` | `1` | the level of the server's own writer (`loose::LEVEL`), so git's loose objects match |
| `receive.unpackLimit` | `2147483647` (replaces `1`) | pushes arrive as loose objects in quarantine |
| `transfer.unpackLimit`, `fetch.unpackLimit` | `2147483647` | anything fetched into this repo lands loose |
| `gc.auto 0`, `receive.autogc false`, `maintenance.geometric-repack.enabled false` | unchanged | still nothing may maintain this repo |
| `core.fsync objects,reference`, `core.fsyncMethod batch` | unchanged | |

## 5. What is more than configuration

### 5.1 Ordered publication

The invariant (SPEC): an object is stored only after everything it references
is stored. `POST /object` enforces it per object (`require_object`), and
`caosd doctor` (`server doctor`) finds a store that breaks it. Packs kept it for free:
a pack becomes visible when its index is published, after the pack is synced.

Loose publication breaks it in two ways if done naively:

- **Concurrent readers.** git migrates a push's quarantine into the store in
  directory order. A tree can be visible before its subtrees. A `POST /object`
  naming that tree passes `require_object` (the tree exists) and the new object
  rests on an incomplete closure.
- **Crash.** A crash mid-migration leaves the visible prefix behind. The next
  start's closure check fails, and the server will not start until someone
  intervenes.

The fix is one routine, `loose::Staging::publish`, used by every ingress
path:

1. The objects are already written and verified in a staging directory on the
   same filesystem.
2. `syncfs` once, so every staged byte is durable.
3. Rename each object into `objects/` in children-first order: a depth-first
   walk that renames an object only after every object it references that is
   also in staging.
4. `syncfs` once more, then return.

A reader can then never see a parent without its children, and on ext4 a
crash leaves a closure-complete subset (section 3). The closure check stays as
the audit, run by hand (`caosd doctor`) rather than at every start: on
caosd-prod's store of 1.38M loose objects it took about 14 minutes of
downtime per start (2026-11-10), and the power-cut tests found nothing for it
to catch.

The ingress paths:

| path | before | after |
| --- | --- | --- |
| `git push` (receive-pack) | pack, `receive.unpackLimit 1` | `unpack-objects --strict` writes loose objects into git's quarantine (`objects/tmp_objdir-incoming-*`, same filesystem). A `pre-receive` hook publishes the quarantine children first; git then finds nothing left to migrate and updates the refs. A push whose ref update then fails leaves closure-complete objects behind, which is harmless. The hook is not in the repository: each server makes its own hooks directory (a `pre-receive` symlink to its own binary, which knows it was run as the hook by its invocation name) and hands git `core.hooksPath` in the `http-backend` environment. Only pushes through that server run it. |
| `POST /git/import` | verified pack hard-linked into `objects/pack` | verify in the staging repo as before, explode the pack into staging, publish |
| locator | `index-pack --strict` of the tree closure straight into the live store | check in a private repository, explode, publish |
| `POST /object`, `store_git_blob`, `store_git_tree` | gix loose write + `sync_loose_object` | one object at a time, closure-checked as before, through the server's own writer (`loose::write_durable`): temporary file, fsync, rename, fsync the fan-out directory. gix's writer never fsyncs, which is what `sync_loose_object` patched after the fact; the new writer cannot leave a zero-length object behind. |
| seed (`stack/serve` copies `CAOS_STACK_SEED_GIT`) | copies packs | unchanged: a seeded test store may keep its packs, which readers handle; the server warns at start whenever `objects/pack` holds anything |

The server also drops its one-time cleanup of an older managed hook in the
repository (`remove_managed_pre_receive_hook`): it too modified the shared
repository, and with `core.hooksPath` in the environment no hook in the
repository runs for this server's pushes.

### 5.2 Durability cost

`loose::write_durable` fsyncs each object and its directory: 5.9 ms an object
single-threaded and 767 objects/s at 8 writers on ext4 (section 2.5). That is
the same two fsyncs `sync_loose_object` paid per object before; the batch
paths (push, import, locator) flush once per batch with `syncfs`. If it shows
up, the fix is group commit in `storage.rs`: concurrent writes share one
`syncfs` per few milliseconds and each request is answered only after its
sync.

### 5.3 The stack limit

With `receive.fsckObjects` and every push unpacked, receive-pack runs
`unpack-objects --strict`, whose fsck walk recurses once per commit of a new
chain. Under git's default 8 MiB stack, a chain of 10,000 new commits
unpacks and 30,000 dies with SIGSEGV. Before this change pushes stayed packs
and went through `index-pack`, which does not recurse that way. The server
sets its own `RLIMIT_STACK` to 1 GiB at start (`main::GIT_STACK_BYTES`), so
every git it spawns (`http-backend`, receive-pack, the hook's git, imports)
inherits it; 100,000 commits unpack in 6 s under it. It is address space,
not memory. A unit test checks both sides on 30,000 commits, and
`tests/git-import` pushes such a chain through a server started under 8 MiB.

### 5.4 What goes away

- The `repack --geometric=2 -d` in the `CAOS_STORE_CHECK` block of `main.rs`
  and its comments. The closure check stays, as `caosd doctor`, not at start.
- The pack-count fatal and warning at startup become a warning if
  `objects/pack` holds anything at all, since nothing should write a pack any
  more.
- `sync_loose_object` and `stored_despite` in `storage.rs`: the server's own
  writer fsyncs before it renames, and tolerates a racing writer of the same
  object.
- The import's pack hard-link and the locator's `index-pack` into the live
  store.
- The watchdog runbook's pack-count step (`prod/watchdog/runbook/CLAUDE.md`)
  becomes "any pack is a bug". Neither the watchdog nor Datadog checks pack
  count today.

### 5.5 What does not change

- No garbage collection. Retention stays in `design/caos-store-and-gc.md`, and
  is simpler on this layout (deleting is an unlink, with no pack to rewrite).
- Refs, reflogs, repair, the git protocol and every client.
- `secrets.git` keeps its own rules (no alternates, immediate prune).

## 6. Risks

| risk | mitigation |
| --- | --- |
| someone enables `fast_commit` or mounts `data=journal`/`writeback` | stays as prod has it; the runbook checks `tune2fs -l` and the mount options |
| a path still writes a pack | startup warning; watchdog alert on any file in `objects/pack` |
| a long history pushed through a git that did not inherit the limit | every server git inherits it from `main`; tests cover push |
| disk fills with no collector | ~11 GiB for today's store against 786 GB free; alert on `/data` use, and size retention work by the growth rate (section 9) |
| inode exhaustion | one per object: 1.3M of 67.1M |
| large fetches cost more CPU | accepted; a pack cache for immutable roots is the fix if it ever matters |

## 7. Migration

The server from this change is deployed first, on the still-packed store:
it keeps reading the packs (with a warning at start) and writes everything
new as loose objects, so the server change and the store change go live at
different times. Then one short downtime, rehearsed first on a copy:

1. Stop the server.
2. Explode every object into a FRESH object directory on the same disk:
   the object names from `git cat-file --batch-all-objects
   --batch-check='%(objectname)'` in the repo, piped to
   `git pack-objects --stdout --window=0`, piped to
   `git -c core.looseCompression=1 unpack-objects` with
   `GIT_OBJECT_DIRECTORY` set to the new directory (e.g.
   `/data/caos/stack/git/objects.new`). It must be a fresh directory:
   `unpack-objects` skips any object the repository already has, so
   exploding in place writes nothing for packed objects. Order does not
   matter: the server is down, and a failure means start again.
   Measured: 407 s (319 s CPU) for today's store, 10.9 GiB of new disk.
3. Verify: the object count matches, 2,000 sampled objects are
   byte-identical through `cat-file --batch`, and the closure check passes.
4. Swap: `objects` to `objects.old`, `objects.new` to `objects` (two renames
   on one filesystem). Copy over anything in `objects/info` other than
   `packs`.
5. Start the server with `CAOS_STORE_CHECK=yes`; confirm a push and a fetch
   work, `objects/pack` stays empty after a few pushes, and the watchdog
   probes pass.
6. Keep `objects.old` for two weeks, then delete it. Rollback: stop, copy
   the loose objects written since into `objects.old` (any server reads
   loose objects), swap back, start.

The side repos' `objects/info/alternates` hold the canonical path of the
server's `objects/`, which is the same after the swap.

## 8. Plan

1. This document.
2. Server: config (section 4), ordered publication and the three ingress
   paths (5.1), the level-1 writer, the stack limit (5.3), removal of the
   repack (5.4), tests: a push, an import and a locator each leave zero
   packs, a long chain pushes, and a reader polling during a push never sees
   a parent without its children.
3. Power-cut tests: done (section 3). One confirming real-server run on ext4
   with this PR's final binary, then retire the test box.
4. Merge and deploy this PR; it is safe on the packed store.
5. The migration (section 7), rehearsed on a copy first.
6. **TODO, once every server sharing a repository runs this build (prod,
   the host stack and every dev stack on it): harden the server against
   changes to the repository's git config.** This build stops writing the
   config; nothing yet stops anyone else from writing it, and a setting there
   (a `core.hooksPath`, an `unpackLimit`, a `gc.auto`) reaches every git that
   does not carry the server's environment. Do then:
   - make the repository's `config` immutable on the host and prod
     (`chattr +i`, root; ext4 supports it): file mode alone does not stop
     git, which rewrites a config by lock and rename. It cannot happen
     earlier, because servers before this build rewrite the config at every
     start and stop when they cannot;
   - have the server check at start that `config` is unchanged from what it
     expects (no `core.hooksPath`, no maintenance or unpack settings that
     contradict `git_settings`) and refuse to start, naming the setting,
     rather than run with it;
   - a test that a server started on an existing repository leaves its
     `config` and `hooks/` byte-identical (the unit test covers the settings
     path; this covers the whole start-up).

## 9. Open questions

- Growth: 993,381 objects on 2026-11-05 and 1,296,765 on 2026-11-07. Is that
  real growth or a difference in how the two runs counted? It sets the disk
  and the retention timeline.
- Should the server serve with `pack.window=0`? It cuts the CPU of a large
  fetch from 192 s to 33 s and roughly doubles its bytes; small fetches do
  not care.
