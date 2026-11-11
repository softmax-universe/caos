# Secrets: deciding which tool is granted

**Status:** built, 2026-W46; not yet measured on a deployed server. SPEC.md,
"Secrets", is the contract (stores, readers, `secret-hash`, isolation); this
file is how the server decides that a tool is one a reader approved, and why
it is done this way.

In one paragraph: a reader names a tool by where it sits in a repository.
`secrets-push` asks the server to evaluate that place at each of the reader's
commits and pushes the resulting trees with the secret. From then on the
server holds a list of trees per secret, and granting is a lookup.

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
reader against all of them at every evaluated node. The index could not know
which roots a later reader would name, so it recorded every root. Each
session step has a new root, and that root's evaluated tree holds all of
`caos-std`, so every shared subtree gained a few hundred origins per tool
call and lost none. On caosd-prod, 2026-11-11: sets of 56k to 328k origins
per tree, about 170k `SADD`s per tool call, and roughly 8 of the 15 seconds a
tool call took spent loading, cloning, hashing and scanning them. A run with
no secrets paid about half of that too.

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

A `reader:@@=<locator> [since=<sha>]` names a set of commits and a path
`dir`. The commits are the locator's commit and its first-parent ancestors,
back to and including `since`, or the newest 20 with no `since`
(`grant_history::DEFAULT_COMMITS`). A `ref=` in the locator is the branch's
head when the secret is pushed.

For each commit `C` the reader approves one tree:

> `eval(C.tree, parent of dir) / last name of dir`

That is the directory a walk from the commit's root has in hand at the moment
it is about to apply the tool's own `.caos-expr`: every parent's expression
applied, its own not yet. A tool is granted the secret when the directory
whose `.caos-expr` evaluation is applying is one of those trees. The test is
on the tree's oid. The path the run took to reach it is not consulted.

### Why that tree, and not the source directory

`C.tree/std/llm-step`, the directory as committed, is the wrong thing to
test. The same source directory evaluates very differently depending on what
its parents put around it: caos's root expression mounts each tool's
dependencies into it as `DEEP-DEPS/<name>`, and the tool's expression names
them by path. The committed directory has no `DEEP-DEPS`; evaluated on its
own it fails, or is a program nobody reviewed in that form.

`eval(C.tree, std)/llm-step` has the dependencies inside it. Its oid covers
the tool's files and everything the parents mounted, so it identifies the
whole program.

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

This is why a mount, a `DEEP-DEPS` copy and a byte-identical copy
(`tests/secrets`, `embedder/mytool`) are granted, and why nothing needs to
remember where a tree came from. The origin index was keyed by oid, so its
rule was already "this content has been seen at an approved place"; the
provenance bookkeeping bought no security the oid did not already give.

### Why a `DEEP-DEPS` copy is the approved tree

Tools are mostly run from copies, so this was checked against
`worker-deep-deps`:

- deep-deps stages a directory's `DEEP-DEPS/<name>` by calling the same
  `deepen` on the dependency that it calls on the dependency's own path, so
  the copy and `eval(T, "")/std/<name>` are one tree.
- `std/.caos-expr` runs deep-deps again over the already deepened `std`. No
  `DEPS` file is left, so its output is its input, and
  `eval(T, std)/<name>` is that same tree again.

So `stack-publish/DEEP-DEPS/github` is the tree a reader on `std/github`
approves. A parent expression that reshaped its children a second time would
separate the two, and copies made by the first pass would then stop matching.

### Why the directory and not the image the tool builds

`eval(C.tree, dir)`, the built tool, looks like the simpler thing to push,
and a `tree=` line naming one is honored. `secrets-push` does not compute it,
for three reasons.

- **It depends on the server's build of caos.** A tool's image contains
  binaries the stack built, including a copy of the stack's own `caos`
  (AGENTS.md: "a rebuilt `caos` re-keys every worker image"). The same commit
  is a different image after a server deploy, so every user's grants would
  lapse at each deploy. The directory is stable across builds only as far
  as the expressions above it are: for caos they run deep-deps and nothing
  else, which rearranges the repository's own files, so the directory is
  the same on any build. That is a property of those expressions, not of
  this design. A repository whose parent expression builds something into
  the tool's directory gets a tree that changes with the server's build,
  and its secrets lapse at a deploy, silently, until the next push.
- **It depends on other grants.** A granted image is marked with
  `secret-hash`, and an image that binds another holds it by hash. If tool A
  binds tool B and B is granted, A's image differs per user (`tests/secrets`
  asserts exactly that of `embedder`). A push evaluates A before the store
  exists, with B unmarked; a run evaluates it with B marked. The pushed hash
  never matches. The directory is taken before any expression in it runs, so
  nothing in it is marked.
- **It costs a build per commit**, cold after every deploy. The directory
  costs the parents' expressions only: for caos, two deep-deps runs per
  commit, shared by every reader of that repository.

## Evaluated when the secret is pushed

`caos-cli secrets-push` does the work, before it signs:

1. For each `reader:@@=`, it asks the server for the reader's commits
   (`POST /secrets/commits`). The server resolves a `ref=` and reads the
   history; nothing is evaluated.
2. For each commit it asks the server to evaluate the parent of `dir`
   (`GET /eval?root:@@=`, the request `caos-cli run` makes for a `:@@=`
   argument), reads that tree, and takes the child. That oid becomes a
   `tree=<oid>` line of the pushed secret.
3. It signs and pushes. The signature covers the trees.

The server then holds a list of trees per secret, in `secrets.git`, and
decides nothing later. What follows from that:

- **The user sees what they approved.** `secrets-push` reports, per reader,
  how many commits had the tool, and warns about a reader that approves
  nothing and about a commit that failed to evaluate.
- **Nothing is silent.** A reader that names an alias of a tool works,
  because the path it names is what gets evaluated. A commit that cannot be
  evaluated is a warning at push, not a missing secret in a session.
- **Granting needs no cache to survive.** The trees are in the store. The
  eval cache only makes the push, and every walk, faster.
- **A grant does not follow a branch.** New code is approved by the next
  push. The approved tree includes the tool's mounted dependencies, so most
  changes to a repository change it: after moving a session to a newer caos,
  push again. Updating automatically is not built.
- **Old commits are the user's choice.** A session pinned to a commit older
  than the newest 20 needs a `since=` that reaches it. A long `since=` makes
  a long push: every commit in it is evaluated.

A commit where `dir` does not exist approves nothing and is not reported;
the newest commits of a branch routinely include ones older than the tool.

### What was built first, and why it went

The first version of this design kept the reader on the server and built the
set of approved trees there, from evaluations the server had happened to do:
a root with an unevaluated expression on the reader's path was parked and
followed again when the eval cache recorded a value. It passed the suite,
and it needed a shared live set per reader, wake-ups on every cached value,
a periodic refresh for values another process wrote, a fingerprint of each
set in every walk's cache key, and one case where the server evaluated on a
reader's behalf anyway (the suite grants `std/llm-step` and runs it from a
`DEEP-DEPS` copy, so nothing else applied `std/.caos-expr` for the commit).
A lost cache entry could deny a grant, and an alias reader was denied
without a word. Evaluating at push removes all of it, at the cost of pushing
again when the code changes.

## Session readers

`reader:@=<path> session=<id>` grants whatever is at `path` in the session's
current head, to requests of that session. The head moves with every edit,
so there is nothing to push. It is worked out when asked
(`approved::session_approves`): the same rule, with the head's tree as the
root, followed through values the eval cache already holds. The walk that
is asking has just evaluated each parent on its way to the tool, so they are
there. A walk may also start at a source tree inside the session, so every
split of `path` into a prefix descended raw from the head (a gitlink
entered) and a remainder is tried.

## The check, in two stages

1. **When evaluation applies a directory's `.caos-expr`**
   (`EvalHost::evaluated`, given the directory's tree and the value): for
   each secret of the run, if the directory, or the value, is one of the
   secret's trees, or a session reader approves the directory, the image gets
   a `secret-hash` entry and the server records "this image, these names"
   under the run's scope. Marking has to happen where the image is formed, so
   that anything embedding the tool is keyed per user too.
2. **When an ArgTree is dispatched:** a secret is injected only if the
   ArgTree contains every entry of a recorded image and carries the
   `secret-hash` the server computes. The final ArgTree never equals
   anything in a list; containing a recorded image is what lets a caller add
   arguments to a granted tool. A granted tool must treat those arguments as
   untrusted.

## The eval cache

`caos-eval` asks its host for a memo at each `.caos-expr`
(`MemoKind::Node`, keyed on the tree alone); the server did not implement
it, so every walk re-evaluated every expression on its way, and a `:@@=`
mount resolved inside a walk re-walked its whole path. Job results were
cached, so this cost redis lookups and tree assembly, not containers, but it
was most of the time before dispatch.

- **Entry:** "tree `Y` evaluated to `<kind> <oid>`", for the unmarked value
  `eval_node` returns.
- **Variants:** one tree can have more than one value. A salted run re-keys
  every job under it, and a value that embeds a granted tool carries that
  tool's `secret-hash`. So an entry is a redis hash per tree,
  `caos:eval:[<namespace>:]<tree>`, with one field per `(salt, walk key)`.
  A walk reads its own variant; a session reader's path reads them all.
- **Namespace:** the same `CAOS_CACHE_NAMESPACE` prefix as job results. An
  evaluation's value is built from job results, and those differ between
  builds of caos.
- **Only successes are stored.** One exposure is shared with the walk cache
  that already existed (`evaluate`): a job that caught a failure returns a
  value, is itself not cached, and an evaluation built on it is. Expressions
  run builds, which do not catch, so this has not been closed.

It is a cache and nothing more: losing an entry costs time. Only a session
reader reads it to decide a grant, and the walk that asks has just written
what it reads.

## What a walk's result depends on

A walk under secrets is cached on its tree, its path and a walk key: the
stores' trees, the session, and the session's head when a session reader
names it. The approved trees are part of a store's tree, so nothing else is
needed: a push changes the key, and a branch moving does not. A run that
carries secrets used to re-key every walk each time a `ref=` branch moved.

Still true, and worth changing later: the session id is in the key whether
or not a session reader uses it, so two sessions of one user share no walks.

## What is removed

`Origin`, `with_known` and `EvalHost::origins_of` in `caos-eval`; in the
server, `KNOWN_ORIGINS`, `SENT`, `INDEXED`, `index_subtrees`, `index_root`,
`record_mount`, the per-request matching of readers against roots, and the
redis keys `caos:secrets:origins2:*` and `caos:secrets:indexed3:*`. Nothing
reads those keys afterwards, so they can be deleted from a deployed redis at
any time after the change is live.

A store pushed before this change holds readers and no trees, so its
`reader:@@=` lines grant nothing until it is pushed again.

## Not yet done

- Measure against the baselines in the 2026-11-11 investigation (median
  server time per tool call 15.2 s; about 170k `SADD` and 27 `SMEMBERS` per
  call).
- Update a store automatically when the code a reader names moves.
- `secrets-push` sends no Git token, so a reader on a private repository
  the server cannot already read is not evaluated.
- How far back a commit still evaluates on the current server is untested;
  an old commit that does not is a warning at push.
- An eval-cache entry gains a field for every (salt, walk key) a tree is
  evaluated under. Nothing trims them short of a new namespace, which every
  deploy brings.
- Separately: build the prod server optimized, and give each stack its own
  `caos:secrets:*` keys.
- Separately: a tool's image depends on the server's build of caos because
  every worker image carries the stack's own `caos` binary. If images were
  built from the pinned commit alone, a result would not need the build in
  its cache key, and the server would not redo everything after a deploy.
