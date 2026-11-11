# Secrets: deciding which tool is granted

**Status:** built, 2026-W46, in `rust/crates/server/src/eval_cache.rs`,
`approved.rs` and `secrets.rs`; not yet measured on a deployed server. It
replaced the origin index. SPEC.md, "Secrets", is the contract
(stores, readers, `secret-hash`, isolation); this file is how the server
decides that a tool is one a reader approved, and why it is done this way.

## The problem this replaces

A reader approves a tool by where it sits in a repository: "the code at
`std/llm-step` on caos main". A session rarely reaches the tool there. It
mounts caos through a pin and runs `caos-std/llm-step` from its own root, or
runs a `DEEP-DEPS` copy of the tool buried inside another tool. The walk that
reaches the tool starts at the session's root and knows nothing of the caos
commit the tool came from; that was established earlier, usually inside a job
whose result is now a cache hit.

The first implementation answered with an index from tree oid to every
`(root, path)` the tree had ever been seen at (its *origins*), and matched a
reader against all of them. The index could not know which roots a later
reader would name, so it recorded every root. Each session step has a new
root, and that root's evaluated tree holds all of `caos-std`, so every shared
subtree gained a few hundred origins per tool call and lost none. On
caosd-prod, 2026-11-11: sets of 56k to 328k origins per tree, about 170k
`SADD`s per tool call, and roughly 8 of the 15 seconds a tool call took spent
loading, cloning, hashing and scanning them. A run with no secrets paid about
half of that too, because origins were loaded before anyone asked whether the
run had readers.

Three earlier fixes (list to set, a sent-filter, per-origin markers) each cut
a constant. None bounded what a tree carries.

## Notation

- `T` is a tree oid. For a commit `C`, `C.tree` is its top-level tree.
- `tree/name` is the named child of a tree: a plain git lookup, nothing
  evaluated. Entering a gitlink means the tree of the commit it names.
- `eval(T, path)` is the recursive evaluation `eval-path` performs: start at
  `T`, apply the `.caos-expr` at its root if it has one, descend one
  component of `path` into the result, apply that directory's `.caos-expr`,
  and so on, applying the last directory's expression too.

So `eval(T, std)/llm-step` is: evaluate the root expression, evaluate the
one in `std`, then take the child `llm-step` without evaluating it.

## The rule

A reader is a set of root trees `R` and one path `dir`.

- `reader:@@=<locator> [since=<sha>]`: `R` is `C.tree` for the locator's
  commit and each first-parent ancestor back to `since` (or to the root
  commit), and `dir` is the locator's `dir=`. `grant_history::allowed_roots`
  computes `R` with `git log --first-parent --format=%T` and caches it per
  `(url, rev, since)`. It is a range and not one commit because a session
  keeps the caos build it started with, and its token has to go on working
  after main moves.
- `reader:@=<path> session=<id>`: the same shape with the session's head as
  the only root. A source tree's gitlink starts a walk of its own, so every
  split of `path` into a prefix that descends raw from the head and a
  remainder is tried, with the tree at the prefix as `T` and the remainder as
  `dir` (as `match_session` did).

A reader approves, for each `T` in `R`, exactly one tree:

> `eval(T, parent of dir) / last name of dir`

That is the directory a walk from an approved root has in hand at the moment
it is about to apply the tool's own `.caos-expr`: after every parent
expression has reshaped it, before its own has run. Call the set of these
trees, over all of `R`, the reader's **approved set**.

A tool is granted a reader's secret when the directory whose `.caos-expr`
evaluation is applying is in that reader's approved set. The test is on the
tree's oid. The path the run took to reach it is not consulted.

### Why that tree, and not the source directory

`T/std/llm-step`, the directory as committed, is the wrong thing to test. The
same source directory evaluates very differently depending on what its
parents put around it: caos's root expression mounts each tool's dependencies
into it as `DEEP-DEPS/<name>`, and the tool's expression names them by path.
The committed directory has no `DEEP-DEPS`; evaluated on its own it fails, or
is a program nobody reviewed in that form.

`eval(T, std)/llm-step` has the dependencies inside it. Its oid covers the
tool's files and everything the parents mounted, so it identifies the whole
program.

### Why testing one oid is sound

An expression can read only its own tree. `caos-eval`'s `eval_node` takes the
tree oid and nothing about where the tree sits, and a `:@=` path cannot leave
the directory. So:

- the same tree always evaluates to the same image, wherever it is reached
  from;
- a surrounding tree can only change what a tool does by changing what is in
  the tool's directory, which changes the oid, which then matches nothing;
- an attacker who places an approved tree somewhere of their choosing gets
  the approved program and nothing else.

This is also why a byte-identical copy is granted (`tests/secrets`,
`embedder/mytool`), and it always was: the origin index was keyed by oid, so
its rule was already "this content has been seen at an approved place". The
provenance bookkeeping bought no security the oid did not already give.

### What is deliberately not approved

The origin index also granted the committed source directory and every
half-evaluated form of it (`eval(T, "")/std/llm-step`), as a side effect of
recording every subtree of every tree it saw. Those are dropped. Where no
parent has an expression, `eval(T, parent)` is the parent itself, and the
rule reduces to the committed directory, so a repository without reshaping
parents behaves as before.

### Why a `DEEP-DEPS` copy is the approved tree

Dropping the half-evaluated forms is only safe if the copies tools are
actually run from are the fully reshaped tree, so this was checked against
`worker-deep-deps`:

- deep-deps stages a directory's `DEEP-DEPS/<name>` by calling the same
  `deepen` on the dependency that it calls on the dependency's own path, so
  the copy and `eval(T, "")/std/<name>` are one tree.
- `std/.caos-expr` runs deep-deps again over the already deepened `std`. No
  `DEPS` file is left, so its output is its input, and
  `eval(T, std)/<name>` is that same tree again.

So `stack-publish/DEEP-DEPS/github` is the tree a reader on `std/github`
approves. A parent expression that did reshape its children a second time
would separate the two, and copies made by the first pass would then stop
matching: the rule approves what a walk to `dir` reaches, and nothing else.

## Building the approved set

### Nothing is evaluated to build it

Evaluating every ancestor of a branch would run real jobs for each (caos's
root expression alone is a deep-deps run over the repository), and old
commits may no longer evaluate at all. So the set is built from evaluations
the server has **already done**, read from the eval cache below. For each `T`
in `R` the server follows `parent of dir` one component at a time:

- a directory with no `.caos-expr` is its own value: descend by lookup;
- a directory with one: use the cached values of that tree (there may be
  several, see "Variants") and descend into each;
- a directory with one and no cached value: this root contributes nothing
  yet. It is **parked** on that tree.

A commit nobody has evaluated on this server therefore contributes nothing,
which loses nothing: a tree equal to `eval(T, …)/name` gets into a session by
being evaluated from `T`, and that evaluation is what fills the cache. (The
exception is a tree someone built by hand to be identical to an evaluation
the server never ran. It is denied.)

### The set is live, not built at admission

Admission fixes the reader's commit set `R`, as before, so one run sees one
answer to "where is the branch". The approved set underneath is a structure
shared by the whole server process, per `(R, dir)`, and each check reads it as
it is at that moment.

It has to be. The evaluation that makes a tool's tree exist often happens
inside the run that then uses the tool (the first tool call after a pin
moves). A set frozen at admission would not contain it.

When the server caches "tree `Y` evaluated to `V`", it looks `Y` up among the
parked roots. If a root is waiting on `Y`, it continues that root's path
from `V` and adds what it reaches to the set, before the walk that produced
`V` moves on. A session root is never parked on, so ordinary evaluation pays
one hash lookup.

### Why the entry exists by the time it is needed, and when it does not

For a walk to hold the tree `eval(T, parent)/name`, something must have
produced `eval(T, parent)`:

- this walk did, having started at `T`; or
- an earlier evaluation did, and the tree arrived inside a mount or a copy.
  Only the server resolves a `:@@=` locator, and it does so by evaluating
  from the commit's tree.

Either way the cache entry is written before the tool's directory can be
tested, so the approved set already holds it.

This argument holds when the reader names the place the tool's content
**came from**. It fails when the reader names an alias. Suppose the grant is
on `my-repo`, `dir=custom-tree/caos-std/github`, where `custom-tree` mounts
caos, and a run reaches the identical tool through a direct caos mount
elsewhere. The trees are equal, but the server can only know that by
evaluating `eval(my-repo.tree, custom-tree)`, and nothing in the run makes it
do so. The root stays parked and the grant is denied. The origin index had
the same gap.

### The two sides are not symmetric

- **Grant side:** the reader names a position under a commit root, and the
  server must have evaluated that position from that root.
- **Usage side:** any path at all, so long as the directory is the identical
  tree.

In normal use the second forces the first: the identical tree gets into a
session through a locator mount, and resolving the mount is the grant-side
evaluation.

| The grant names | The run reaches the tool via | Result |
|---|---|---|
| caos, `std/github` | `custom-tree/caos-std/github` in a session | granted: resolving the mount evaluated caos from its root |
| my-repo, `custom-tree/caos-std/github` | a direct caos mount elsewhere | denied: nothing evaluated my-repo's `custom-tree` |

So: **grant where the tool is defined, use it from anywhere.** Usage does not
connect when the surrounding tree changed anything inside the tool's
directory (a different program, correctly refused) or when the tool was
vendored as source and evaluated under different parents.

A denied alias reader is silent today. Two remedies, neither built here:
report, per reader, which roots resolved and which are parked on what; and
evaluate the branch head's `parent of dir` in the background when a reader is
first used. Either can be added without changing the model.

## The eval cache

The approved set is only as good as the server's memory of what it has
evaluated, and until now the server did not keep one per directory.
`caos-eval` asks its host for a memo at each `.caos-expr`
(`MemoKind::Node`, keyed on the tree alone); the server did not implement it,
so every walk re-evaluated every expression on its way, and a `:@@=` mount
resolved inside a walk re-walked its whole path. Job results were cached, so
this cost redis lookups and tree assembly, not containers, but it was most of
the time before dispatch.

The cache is built first because the approved set reads it, and because it
is worth having on its own.

- **Entry:** "tree `Y` evaluated to `<kind> <oid>`", for the unmarked value
  `eval_node` returns.
- **Variants:** one tree can have more than one value. A salted run re-keys
  every job under it, and a value that embeds a granted tool carries that
  tool's `secret-hash`, which differs per secret store. So an entry is a
  redis hash per tree, `caos:eval:[<namespace>:]<tree>`, with one field per
  `(salt, walk key)` variant. A walk reads its own variant. The approved set
  reads every variant: any value this server produced from an approved tree
  is a legitimate parent to descend through.
- **Namespace:** the same `CAOS_CACHE_NAMESPACE` prefix as job results. An
  evaluation's value is built from job results, and those differ between
  builds of caos.
- **Only successes are stored.** One exposure is shared with the walk cache
  that already existed (`evaluate`): a job that caught a failure
  returns a value, is itself not cached, and an evaluation built on it is.
  Expressions run builds, which do not catch, so this has not been closed.

### Durability

Until now a lost cache entry cost time. Now it can deny a grant: if the entry
for a mounted commit is evicted while the job that resolved the mount is
still a cache hit, nothing re-evaluates the commit, its root parks, and the
tool is refused. caosd-prod's redis is `allkeys-lru`. The origin index had
the same exposure and it was never observed, but these entries should be kept
where they are not evicted; that is deployment work, tracked under "Not yet
done".

## The check, in two stages

Only the first stage changes.

1. **When evaluation applies a directory's `.caos-expr`** (`EvalHost::evaluated`,
   now given the directory's oid in place of its origins): for each secret of
   the run, if the oid is in the approved set of one of its readers, the
   image the expression produced gets a `secret-hash` entry and the server
   records "this image, these names" under the run's scope. Marking has to
   happen where the image is formed, so that anything embedding the tool is
   keyed per user too.
2. **When an ArgTree is dispatched:** a secret is injected only if the
   ArgTree contains every entry of a recorded image and carries the
   `secret-hash` the server computes. The final ArgTree never equals
   anything in a set; containing a recorded image is what lets a caller add
   arguments to a granted tool. A granted tool must treat those arguments as
   untrusted.

## What a walk's result depends on

A walk under secrets is cached on its tree, its path and a walk key: the
stores' trees, the session and its head, the commits its branches resolved
to, and now a fingerprint of each locator reader's approved set. The
fingerprint is there because the set can grow after a walk was cached: a
reader that matched nothing when a tool was first evaluated (the alias case,
once someone evaluates the alias) must not go on being answered from the walk
that denied it. It changes only when a reader's set gains a tree, which is
when a commit of the granted repository is evaluated for the first time.

Grant records stay keyed by the scope without the fingerprint: a record says
what was granted to an image, and that does not become false when the set
grows.

Still true, and worth changing later: a `ref=` reader re-keys every walk of
its users each time the branch moves, whether or not the code at `dir`
changed, and the session id is in the scope whether or not a session reader
uses it. With the approved set in hand both could be replaced by the
fingerprint alone, which would let sessions share walks. That waits for
measurements.

## What is removed

`Origin`, `with_known` and `EvalHost::origins_of` in `caos-eval`; in the
server, `KNOWN_ORIGINS`, `SENT`, `INDEXED`, `index_subtrees`, `index_root`,
`record_mount`, and the redis keys `caos:secrets:origins2:*` and
`caos:secrets:indexed3:*`. Nothing reads those keys afterwards, so they can
be deleted from a deployed redis at any time after the change is live.

## Order of work

1. This document.
2. Stop loading origins for a run with no secrets. One line; no behavior
   change; it stands alone as relief if the rest is delayed.
3. The eval cache.
4. Approved sets, the new check, and removal of the origin index.
5. Measure against the baselines in the 2026-11-11 investigation (median
   server time per tool call 15.2 s; about 170k `SADD` and 27 `SMEMBERS` per
   call).

## Not yet done

- Keep eval-cache entries out of an evicting redis.
- Report parked readers to the user (`secrets-push` reply or a status call).
- Evaluate a reader's branch head on first use, so alias readers work.
- Entries written by another server process sharing the redis wake nothing
  here; parked roots are re-read at most once a minute.
- A tool directory whose parent is itself a granted image is not followed:
  the approved set descends through unmarked values only.
- Re-key walks on the fingerprint alone (see above).
- Separately: build the prod server optimized, and give each stack its own
  `caos:secrets:*` keys (dev test stacks wrote about 53k origins into prod's
  sets).
