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
caos flake. `build-builtins.sh` builds them with the host's nix and assembles
the curries by hand.

## Proposal

### 1. Caos blesses nix: a `nix` primitive

Add one primitive that runnerd executes itself, with the host's nix:

```
run --base:nix=v1 --in:@=<flake dir> --attr=<attr>
```

- **Input.** A flake directory with its `flake.lock`. The flake is evaluated in
  pure mode (no `--impure`, no `path:` outside the input) and built in the nix
  sandbox.
- **Output.** The built attribute as a caos object:
  - a docker-image attr becomes a CLEAN image delta, using the conversion code
    that already streams host-built images (`caos-worker-*-docker`);
  - a package attr becomes its store path as a tree, so a binary can be curried
    onto `runner` by an expression (today `build-builtins.sh` does that curry
    by hand for `rustc` and `deep-deps`).
- **Key.** `{base: "nix:v1", in, attr}`. The STACK'S version is deliberately
  not in it: a nix output is determined by the flake and its lock, not by the
  nix that ran the build (a derivation hash does not include nix's version).
  `v1` versions the output FORMAT (the delta layout), so a format change makes
  old keys miss instead of answering wrongly.
- **The primitive injects nothing.** No `/bin/caos`, no layer of its own. This
  is what makes leaving the stack out of the key sound, and it is the one place
  that has to hold.

No outside docker image is involved. Every host that runs caos already has nix,
because caos is built with it.

### 2. The seeded entries become ordinary flake attrs

Each of `runner`, `cargo`, `rustc`, `deep-deps` and `flake-builder` becomes an
expression over the caos flake at its own rev:

```
run --base:nix=v1 --in:@=<caos flake> --attr=<the attr build-builtins uses today>
```

followed, where today's seed result is a curry, by the same curry written as an
expression (`curry --base=$RUNNER --worker1=$BIN …`).

The sentinels, `required-pool=seeded`, `SEEDED_POOL`, the seed records, the
core-seeder-runner and the hand assembly in `build-builtins.sh` all go away.
`CAOS_SEEDED_GRACE_SECS` and the "seeder answers X, the job asks Y" verdict go
with them.

### 3. flake-builder stays, and carries its own caos

`flake-builder` already separates a clean image (`nix build #caosImage`,
registry tag `flake-<H>`, keyed on the flake tree alone) from the caos
additions layer (`layer00`: `/bin/caos` setuid, `/tmp`, the user db,
`/usr/bin/env`). Keep that split, and keep the rule that flake-builder copies
ITS OWN `/bin/caos` into what it builds.

The change is where flake-builder X comes from: the nix primitive builds it
from caos X's flake, and that flake builds caos X into it. So flake-builder X
always carries caos X, and anything it builds carries caos X. That is already
true downstream of flake-builder, because a built entry's `base` is the
flake-builder delta, which contains `layer00/bin/caos`. With this, it is true
for flake-builder itself, and "built by flake-builder X" gives the same bytes
whoever asks.

Only the primitive crosses from the stack's version to a tree's version, and it
injects nothing (section 1).

### 4. One bootstrap, the same for every tree

Nothing knows whether it is the current version, so there is no current-version
path. caos' root expression bootstraps from its own source:

```
DD=run --base:nix=v1 --in:@=. --attr=<deep-deps>
run --base=$DD --in:@=.
```

The stack's own caos commit goes through exactly this; so does every other rev.
`caosd up` already builds these derivations with the host's nix, so for the
stack's own rev the first run of the primitive is mostly nix store hits. That
is the only sense in which the current version is faster, and nothing depends
on it. Pre-warming, if wanted, is `caosd up` evaluating its own tree, not a
separate path.

`--in:@=.` puts the whole repo in the bootstrap key, so every caos commit runs
the primitive once. Nix keeps that cheap (unchanged derivations are store
hits), but the input can be narrowed to the flake and the sources it reads if
the per-commit run costs too much.

### 5. Consumer trees

Most trees look like caos-session, not caos: their root runs caos'
`std/flake-input-loader`, reached by a `:@@=` locator at the rev their
`flake.lock` pins, to mount caos' std at `caos-std/`. The server evaluates that
locator through caos' root expression, so it goes through section 4 at the
pinned rev. Consumers need no change for old revs to work, and every consumer
at one rev shares that rev's bootstrap.

A consumer that also deepens its OWN tree with the caos it imported needs one
evaluator addition, because a line cannot yet descend into a variable's
result:

```
T=run --base:@@=<caos>&dir=std/flake-input-loader --in:@=. … --output-path=caos-std
run --base=$T/caos-std/deep-deps --in=$T
```

`$VAR/<path>` names the object at `<path>` inside `$VAR`'s result, evaluated
there. Today `eval_path` deliberately does not re-run an expression on a
result's root, so this has to be explicit. The alternative is for the loader to
deepen the tree itself, which hides deep-deps inside the loader.

## What has to be true

- **The primitive is pure.** It injects nothing and runs in pure eval mode.
  Test: build flake-builder X with a stack at X and with a stack at X−1; the
  deltas must be byte-identical.
- **Everything in the core is a nix output or an expression.** Today
  `build-builtins.sh` also stages `worker-common` from the working tree for
  `rustc` and binds `--cargo`/`--runner` as hash literals. Each such step has to
  move into the flake or into the expression. This note does not yet inventory
  every one.
- **The delta format is versioned.** A format change bumps `nix=v2`.

## Costs and risks

- **Trust.** Any session can run a nix evaluation and build on the host's nix
  daemon. Pure eval and the sandbox contain it; resource limits and store GC
  are still needed.
- **Reproducibility.** Not every nix build is bit-for-bit reproducible. caos
  caches the first result per key, so one stack is consistent with itself; two
  stacks may hold different bytes, which is no worse than today.
- **Network.** Builds fetch fixed-output derivations and the lock's inputs.
- **First build of an old rev** is a cold Rust workspace build, once per rev,
  then cached by key. Tolerable only with a persistent store or a substituter.
- **The host stack** (server, runnerd, the primitive's runtime) is still built
  by `caosd up` outside caos. Its version no longer reaches any tree's results.

## Order of work

1. The `nix=v1` primitive in runnerd and the server, with the X / X−1
   byte-identity test.
2. `runner` and `deep-deps` first, since they are the silent wrong answers
   today.
3. `flake-builder`, then `cargo` and `rustc`, moving each remaining hand step
   into the flake or an expression.
4. caos' root expression and `std/.caos-expr` bootstrap through the primitive.
5. Delete the seeder, the seed records, `required-pool=seeded` and the seed
   half of `build-builtins.sh`; then try dropping `CAOS_CACHE_NAMESPACE`.
6. `$VAR/<path>` in the evaluator, for consumers that deepen themselves.
