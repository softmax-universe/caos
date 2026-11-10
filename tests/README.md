# Shared test state

Suite runs for the same built image may reuse a persistent test stack, CAS, and
Git remote. This is independent of the outer run's `CAOS_SALT`, so mutable refs
and session names can collide between runs.

Generate a prefix inside each test that owns mutable names, and reuse it for all
of them:

```bash
test_run_id="$(date +%s%N)-$$-$RANDOM"
session="${test_run_id}-tools"
```

`CAOS_SALT` controls caching; it is not a state namespace.

## A request a test makes is shared state too

A request's identity is its content, and so is a ticket's (`/eval`, `/start`).
Every dev stack of one build on a host shares one redis under one
`CAOS_CACHE_NAMESPACE` (dev/stack-up), so a request that is the same in every
run is the same request on every concurrent suite of that commit. On a laptop
there is one stack and this never shows; on a shared host it is how
`tests/std-files` and `tests/expression-http` failed while passing locally.

So a test that makes requests itself (`POST /eval/run`, `/start/run`, a
sub-run whose payload it chooses) salts EVERY one with something unique to
the invocation, the same `test_run_id` as above:

- Not the pid alone. A fresh container's pid is the same every run, so
  `<name>-<pid>-<n>` is one fixed request, a cache hit forever:
  `--test-salt` never re-runs it, and concurrent suites start it together.
- Not nothing, even for a call that is meant to be refused. A refusal is a
  failed ticket, and a failed ticket is started afresh by the next caller,
  wherever it is.
- Fold `--test-salt` in as well, so the request is legible as this run's.

The exception is a test whose subject IS the memo (`tests/file-count` wants its
rustc build cached); it says so.

The git-import suite entry runs a private HTTPS Git remote and a separate
server, checking exact-commit imports, authentication, complete history,
incremental transfers and retries. It also checks leased pushes, lost replies,
concurrent updates, code-history preservation and publication guards. It needs no external service.

The llm-import-path entry exercises agent-driven importing in an empty session
through `caos-std/import-source`, including provenance, readable history,
occupied destinations, and invalid sources and revisions.

# Writing a test

A new test is a Go worker: `worker.go` curried onto `DEEP-DEPS/go`, importing
`caos/w`, staged with `curry` + `run-then` as `tests/create-squashed-stack` and
`tests/flake-update` are. The `worker.sh` tests predate `std/go`; do not copy
them for a new one.

# Disabling a test

Disable a test by making its worker return early. Do not move or delete it.

Put this straight after `set -euo pipefail`, before the test sources anything or
does any work, with a comment saying why it is disabled and what would let it
come back:

```bash
# DISABLED: <why, and what would let it come back>.
echo "<name>: SKIPPED (disabled)" > /tmp/report
caos put /tmp/report /cas/out
exit 0
```

The suite runs every directory under `tests/` (`dev/run-tests/worker.sh`), and a
disabled test stays one of them, so:

- It is still listed in the report, still evaluated, and its `DEPS` still
  resolve. A broken `.caos-expr` or a dependency that has gone away is still
  caught. A test moved out of `tests/` drops out of the report unannounced, and
  nothing notices it rotting.
- It reports as passed (✓), because the worker finishes normally. Its narration
  says `SKIPPED`: `caos-std/caos-test-result` with the hash from the report prints
  it.
- Re-enabling it is deleting those lines.
