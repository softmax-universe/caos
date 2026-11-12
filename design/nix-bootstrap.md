# Bootstrapping the core with a nix primitive (no seeder)

Status: proposal. Nothing here is implemented.

## Problem

The seeded core is `flake-builder`, `cargo`, `rustc`, `deep-deps` and
`runner`. `stack/build-builtins.sh` builds it by hand for ONE caos build, and
the core-seeder-runner answers each item's `docker://seeded…` sentinel from
that build's seed records (design/caos-expr.md, Phase 3). Every sentinel binds
`required-pool=seeded`, so runnerd can never take one of these jobs.

A tree that pins any OTHER caos rev cannot resolve anything. An older session
is the usual case, since a consumer reaches caos through a `:@@=` locator at a
pinned rev, and the server evaluates that rev through caos' root expression,
which needs `deep-deps` first. The two ways it fails:

1. **A miss.** For `flake-builder`, `cargo` and `rustc` the key's `in` is the
   std entry's source, so another rev's entry forms a key no record has. The job
   parks, and after `CAOS_SEEDED_GRACE_SECS` it fails with a 503. `cargo`'s key
   also has `base` = THIS build's flake-builder delta, so an old cargo misses
   even when its own files are unchanged.
2. **A silent wrong answer.** `std/runner` and `std/deep-deps` hold only a
   `.caos-expr`, which `strip_caos_expr` removes before forming `in`. So their
   key is `{base: <sentinel blob>, in: <empty tree>, required-pool: seeded}`
   in EVERY rev, and any rev gets THIS build's runner and deep-deps binaries.
   What those are built from, the Rust workspace and its nix closure, is not in
   the key at all. The same gap is why `CAOS_CACHE_NAMESPACE` exists: `caos`
   appears only in the seeder's answer, never in a key (AGENTS.md, "A rebuilt
   `caos` re-keys every worker image").

Keeping several seed sets, or building a seed set on demand, would patch the
first failure. Neither touches the second, and both keep a hand-built path
that only `build-builtins.sh` knows.

## Why the core is a core

None of it can be built without the rest. Every evaluation starts with the root
expression `run --base:@=std/deep-deps --in:@=.`. `deep-deps` is a rustc-built
worker curried onto `runner`. `rustc` needs `cargo`, `cargo` is built by
`flake-builder`, and `flake-builder` takes its lock through
`DEEP-DEPS/flake.lock`, which needs `deep-deps`.

What closes the loop is that caos cannot build a flake without a caos tool
that builds flakes. Every item in the core is, underneath, a nix build of the
caos flake: `build-builtins.sh` builds `.#caos-worker-{runner,cargo,flake-builder}-docker`
and `.#worker-{deep-deps,rustc}` and assembles the two curries by hand.

## Proposal in one paragraph

Caos learns to build a flake attribute itself (a `nix` primitive). The repo
root's expression builds the five core items from the repo's own flake with
that primitive, assembles the two curries as ordinary expression lines, and
grafts the result into the evaluated tree at `caos-core/`. Everything else
(std, tests, a consumer's own tools) names a core item by a `DEPS` path to
`caos-core/<item>`, exactly as it names any other dependency. A consumer gets
`caos-core/` and `caos-std/` by mounting them from its one caos pin. No tree
ever names a core item by commit hash; the pin is the only hash, and it is the
one consumers already write.

## 1. The `nix` primitive

```
run --base:nix=v1 --in:@=<flake dir> --attr=<attr>
```

- **Input.** A flake directory with its `flake.lock`. Pure evaluation (no
  `--impure`, no `path:` outside the input), built in the nix sandbox.
- **Output.** The attribute as a caos object: a docker-image attr becomes a
  CLEAN image delta (the conversion that already streams
  `caos-worker-*-docker` tarballs); a package attr becomes its store path as a
  tree, so a binary can be curried by an expression line.
- **Key.** `{base: "nix:v1", in, attr}`. The stack's version is deliberately
  NOT in it: a nix output is determined by the flake and its lock, not by the
  nix that ran the build. `v1` versions the output FORMAT (the delta layout), so
  a format change makes old keys miss instead of answering wrongly.
- **It injects nothing.** No `/bin/caos`, no layer of its own. That is what
  makes leaving the stack out of the key sound, and it is the one place
  hermeticity has to hold.
- **Where it runs.** caosd deliberately runs without nix at runtime
  (`build-builtins.sh` takes prebuilt paths through `CAOS_BUILTIN_IMAGES` /
  `CAOS_BUILTIN_BINS`), so "the host's nix" is not available. The stack already
  ships a nix: the one inside its own flake-builder image. runnerd runs the
  primitive in a container from that closure, which caosd already has. No
  outside docker image, and still no nix on the host.

## 2. Two small evaluator additions

Both are pure tree assembly, like `curry`: they dispatch nothing, so they are
available before any core exists.

- **`$VAR/<path>`** names the object at `<path>` inside `$VAR`'s result. Today
  a line cannot reach into a variable, so a consumer would have to repeat its
  pin to name something inside what it just mounted.
- **`graft`** replaces or adds entries in a tree:
  `T=graft --in:@=. --caos-core=$CORE` is the input tree with `caos-core/` set
  to `$CORE`; without `--in` it starts from the empty tree. **`mount`** is the
  same, and also records each name in the result's `.caos-mounts`, saying that
  entry is a finished tree (section 4). They are two verbs because assembling
  a tree that is not a mount point (an image layer, the core itself) must not
  leave a `.caos-mounts` file in it. (`std/flake-input-loader` mounts as a
  worker; it cannot be used here, because it is rustc-built and so needs the
  core.)

## 3. The caos repo

### Root expression

```
# The five core items, from this tree's own flake. Nothing here knows its rev.
RUNNER=run --base:nix=v1 --in:@=. --attr=caos-worker-runner-docker
FB=run --base:nix=v1 --in:@=. --attr=caos-worker-flake-builder-docker
CARGO=run --base:nix=v1 --in:@=. --attr=caos-worker-cargo-docker
DDBIN=run --base:nix=v1 --in:@=. --attr=worker-deep-deps
RUSTCBIN=run --base:nix=v1 --in:@=. --attr=worker-rustc
# The two curries build-builtins.sh assembles by hand today.
DD=curry --base=$RUNNER --worker1=$DDBIN/bin/worker-deep-deps
RUSTC=curry --base=$RUNNER --worker1=$RUSTCBIN/bin/worker-rustc --cargo=$CARGO --runner=$RUNNER --worker-common:@=rust/crates/worker-common
# Assemble the core and graft it into the tree, then deepen as today.
CORE=graft --runner=$RUNNER --flake-builder=$FB --cargo=$CARGO --rustc=$RUSTC --deep-deps=$DD
T=mount --in:@=. --caos-core=$CORE
run --base=$DD --in=$T
```

This IS `build-builtins.sh`'s seed half, written as an expression. Two notes:

- `--in:@=.` puts the whole repo in five keys, so each caos commit runs the
  primitive five times. Nix keeps that cheap (unchanged derivations are store
  hits). It can be narrowed to the flake and the sources it reads if needed.
- `FB` is flake-builder X, built from caos X's flake, which builds caos X into
  it. It keeps copying ITS OWN `/bin/caos` into what it builds (its clean image
  and its caos-additions layer are already separate), so anything built by
  flake-builder X carries caos X, whoever asks.

### The evaluated tree

```
/                       the repo, evaluated (deepened)
  caos-core/            grafted, BUILT objects (not source)
    runner              image delta
    flake-builder       image delta, carries caos X
    cargo               image delta
    rustc               curry(runner, worker-rustc, cargo, worker-common)
    deep-deps           curry(runner, worker-deep-deps)
  std/                  source entries, each with DEEP-DEPS/ filled in
  flake.lock
  rust/ tests/ …
```

The checked-in `std/{runner,cargo,rustc,deep-deps,flake-builder}` sentinel
entries are deleted, and so is `std/.caos-expr`: the repo root is the only
bootstrap, and a `dir=std/...` locator already descends through it.

### How std names a core item

By `DEPS` path, like any other dependency. Only the target changes:

```
# std/read/DEPS (today: ../rustc rustc)
../files files
../../caos-core/rustc rustc
```

`std/read/.caos-expr` is unchanged: `IMAGE=run --base:@=DEEP-DEPS/rustc
--src:@=DEEP-DEPS/files`. `DEEP-DEPS/rustc` is now a built curry node rather
than a sentinel entry; a path that carries no `.caos-expr` evaluates to itself,
so it is used as the base as-is.

The same for the other shapes:

```
# std/bash/DEPS: a flake tool
../../caos-core/flake-builder flake-builder
../../flake.lock flake.lock

# std/llm-stub/DEPS: a cargo build
../../caos-core/cargo cargo

# std/llm-step/DEPS: a rustc tool with crate deps
../../caos-core/rustc rustc
../git-runner git-runner
../llm-client llm-client
../../rust/crates/session-protocol session-protocol
…
```

No commit hash, no locator. `tests/<name>/DEPS` work the same way
(`../../caos-core/rustc rustc`).

## 4. A consumer tree (caos-session and the like)

### Mounting

The consumer pins caos once, as today: the rev in its root expression, checked
against `flake.lock` by `std/flake-input-loader`. The loader gains a way to
mount several directories of the input in one call, so the consumer gets
`caos-core/` beside `caos-std/`:

```
run --base:@@=github:org/caos?rev=<X>&dir=std/flake-input-loader --in:@=. --expr=$CAOS_EXPR --input=caos --input-tree:@@=github:org/caos?rev=<X> --mounts=std:caos-std,caos-core:caos-core
```

`--input-tree` has no `dir=`, so it is caos X's whole EVALUATED root: caos X's
root expression ran (section 3), so `caos-core/` holds X's built core and
`std/` is deepened against it. Every consumer at rev X shares those objects.

The consumer's evaluated tree:

```
/                 the consumer, evaluated
  caos-std/       caos X's std, already deepened
  caos-core/      caos X's built core
  flake.lock      pins caos X
  tools/…         the consumer's own entries
```

A consumer that only USES caos' tools (caos-session today) needs nothing more:
`caos-std/read` is resolved exactly as before.

### A consumer's own tools

They name caos by `DEPS` paths into the mounts:

```
# tools/report/DEPS: a rustc-built tool
../../caos-core/rustc rustc

# tools/report/.caos-expr
run --base:@=DEEP-DEPS/rustc --src:@=.

# tools/render/DEPS: a flake tool, built by caos X's flake-builder
../../caos-core/flake-builder flake-builder
../../flake.lock flake.lock

# tools/review/DEPS: a tool built on a caos std tool
../../caos-std/llm-step llm-step
```

For `DEEP-DEPS` to exist the consumer deepens its own tree with caos X's
deep-deps. With `$VAR/<path>`, the root expression becomes two lines and the
pin is still written only where it is today:

```
T=run --base:@@=github:org/caos?rev=<X>&dir=std/flake-input-loader --in:@=. --expr=$CAOS_EXPR --input=caos --input-tree:@@=github:org/caos?rev=<X> --mounts=std:caos-std,caos-core:caos-core
run --base=$T/caos-core/deep-deps --in=$T
```

### Why moving `caos-std` does not break the references inside it

std's `DEPS` already point outside `std/` (`../../flake.lock`,
`../../rust/crates/session-protocol`), and with this proposal they also point
at `../../caos-core/`. In a consumer, `caos-std/llm-step/../../rust` names
something that does not exist. This works today, and keeps working, for one
reason: **a mounted input is a finished tree.** It was deepened by its own
root expression at its own rev, so every `DEEP-DEPS/` inside it is already
filled, and nothing re-reads its `DEPS`.

That has to become a stated rule, because a consumer that deepens itself runs
deep-deps over a tree CONTAINING the mounts:

- **deep-deps does not descend into a mount.** It treats `caos-std/` and
  `caos-core/` as opaque objects, referenced by oid. The mount step records
  what it mounted (a `.caos-mounts` list at the root, written by `mount` and by
  the loader) so deep-deps knows where to stop.
- The same rule keeps deep-deps from materializing `caos-core/`'s image deltas,
  which are hundreds of MB and have no `DEPS` to find.

### Where a hash appears

Only in the consumer's pin (the root expression, and `flake.lock`), as today.
Inside caos, nothing names a core item by rev: the core is built from `.`.
Inside std and inside a consumer, a core item is a relative `DEPS` path.

## 5. No current-version path

Nothing knows whether it is the current version. The stack's own caos commit
goes through section 3 like any other. `caosd up` has already built these
derivations, so for its own rev the primitive's first run is mostly nix store
hits; that is the only sense in which the current version is faster, and
nothing depends on it. Pre-warming, if wanted, is `caosd up` evaluating its own
tree.

## What has to be true

- **The primitive is pure.** It injects nothing and evaluates in pure mode.
  Test: build caos X's core with a stack at X and with a stack at X−1; every
  object must be byte-identical.
- **Everything in the core is a nix output or an expression line.** Section 3's
  root expression covers what `build-builtins.sh` does today, including binding
  `--cargo`/`--runner` and `worker-common`. Anything else it does (stripping,
  modes, the caos-additions layer) has to be checked line by line.
- **The delta format is versioned.** A format change bumps `nix=v2`.
- **deep-deps stops at mounts.**

## Costs and risks

- **Trust.** Any session can run a nix evaluation and build in the primitive's
  container. Pure eval and the sandbox contain it; resource limits and store GC
  are still needed.
- **Reproducibility.** Not every nix build is bit-for-bit reproducible. caos
  caches the first result per key, so one stack is consistent with itself; two
  stacks may hold different bytes, which is no worse than today.
- **Network.** Builds fetch fixed-output derivations and the lock's inputs.
- **First build of a rev** is a cold Rust workspace build, once per rev, then
  cached by key. Tolerable only with a persistent store for the primitive's
  container or a substituter.
- **Eager core.** The expression language has no lazy evaluation, so every
  evaluation of a rev builds all five items, even when only `rustc` is used.
  Cached per rev, so this costs once.
- **Revs from before this lands** still carry seeded sentinels. They keep
  needing a seeder, so either the seeder stays for a transition with the seed
  sets of the revs that matter, or those revs are declared unsupported.

## Order of work

1. The `nix=v1` primitive (runnerd, in the stack's own nix closure), with the
   X / X−1 byte-identity test.
2. `graft` and `$VAR/<path>` in the evaluator.
3. Root expression builds and grafts `caos-core/`; std and tests `DEPS` move to
   `../../caos-core/<item>`; delete the five sentinel entries and
   `std/.caos-expr`.
4. deep-deps stops at mounts; the loader takes `--mounts`.
5. Delete the seeder, the seed records, `required-pool=seeded` and the seed
   half of `build-builtins.sh`; then try dropping `CAOS_CACHE_NAMESPACE`.
