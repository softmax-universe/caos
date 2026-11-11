# Standalone Delta connector

This is a Go implementation of selected Delta protocol operations. It does not
launch, embed, or download the Delta application. The public reference Wasm is
used only by the separate [protocol investigation](../protocol/README.md).

**Current state: authentication, registration, conversation replay, and initial
workspace-tree reconstruction work; agent-turn execution is not implemented yet.**
The CAOS entry implements bounded checks, not a persistent runner. Do not select
a diagnostic machine for real work.

## Verified against the real service

- Start and complete Delta's ordinary device-approval flow.
- Obtain an account credential issued to this connector, without reading the
  native application's credentials.
- Exchange that credential for a Delta service token.
- Connect to `wss://delta.zed.dev/connect` with the connector's own actor UUID,
  `Authorization: Bearer <service-token>`, and `x-delta-protocol: delta-v49`.
- Send a MessagePack heartbeat and receive its correlated protocol response.
- Set a diagnostic machine-runner name, receive `Updated`, clear the name, and
  receive `Updated` again.

These live checks have passed both on the host and inside an actual CAOS
worker, using a credential grant pinned to the deployed source revision. The
diagnostic registration was cleared before the worker exited.

The protocol header and authentication flow were recovered from public client
code for the same 0.24.2 build as the reference codec. HTTP error bodies, access
tokens, device secrets, and message contents are not printed by the connector.

## Development checks

```sh
cd integrations/delta/connector
go test -race ./...
go vet ./...
```

The Go module reuses `caos/w` from `std/go/prelude`. The Nix package compiles
the same module, runs its tests, and vendors pinned dependencies. Its source
filter excludes research artifacts and the evaluated `DEEP-DEPS` tree.
`tests/delta-protocol` builds that image through the ordinary CAOS suite.

The [landed transport checkpoint](https://github.com/softmax-universe/caos/pull/448)
passed the full salted CAOS suite (**79/79**), CAOS and native Nix builds,
race-enabled Go tests, and live connection/history checks. The replay engine was
added afterward and has separate compatibility checks.

The offline reference runs 10,395 protocol and in-memory state checks, including
128 generated text-CRDT histories and 768 queries checked with both ordinary and
visible-anchor semantics. See the protocol README for exporting those
cases and comparing the Go implementation against them. The production worker
does not include or invoke that reference module.

## Read-only history capture

The native CLI has downloaded and parsed 24 history delta batches from an
explicitly approved disposable Delta thread. It uses a separate authenticated
socket at `/history/<history UUID>/connect`; the control socket rejects history
requests. The public reference verifies that path and the request envelopes.

```sh
connector fetch-history \
  --credential-file=/private/path/credentials.json \
  --actor-file=/private/path/actor.json \
  --thread-id="<Delta thread ID>" \
  --output-file=/private/path/new-history-snapshot.json
```

The capture preserves batch identities, causal parents, delta payloads, timeline
metadata, replication-pack references, and ordered notifications. It does not
apply them to a local state database or execute a turn. A capture containing
replication-pack references is incomplete until those packs are fetched; that
fetch path is still unimplemented. Raw content and pack metadata stay in the
private file, never in standard output.

## Read-only conversation state replay

```sh
connector replay-history \
  --snapshot-file=/private/path/new-history-snapshot.json \
  --output-file=/private/path/new-replay-report.json
```

This command needs no network or account credential. It selects the requested
timeline frontier, verifies causal closure, and reconstructs registers, maps,
scratch text, serialized-version state, and seal records. Text edits use UTF-8
byte offsets in their causal parent view. Concurrent text insertions and
deletions retain their identities; maps remove only values observed by their
parent, preserving unseen concurrent updates.

The approved fixture's 24 batches reconstruct a 194-byte conversation buffer,
8 registers, 10 maps, and one seal range. An independent replay with the public
client's in-memory text engine produces the same buffer. The optional native
corpus test also compares 128 synthetic forked histories, under shuffled delivery,
without adding vendor code to the production worker.

The report contains private thread text. Captures can also contain delegated
model vouchers, so keep both outside source checkouts and never bind raw captures
into CAOS arguments or results. Output files are private and never overwritten.

This reconstructs already accepted records; it does not authorize new edits.
Seal enforcement for a writer, live ownership/epoch admission, native message
translation, incremental workspace-delta replay, and replication-pack retrieval
remain unimplemented. Unsupported deltas and incomplete histories fail explicitly.
A stored generation-state tag is not a liveness check: the fixture retains a
Streaming value even after its assistant response has completed.

## Workspace identity and journal-write checks

Workspace attachments identify their own history, timeline, pinned version, and
worktree root. The connector follows those descriptors from an authorized thread
and reconstructs their initial Git trees, preserving blob IDs, executable modes,
symlinks, and Git's directory ordering. It does not use the operator's checkout
as a substitute for the thread's files. Unsafe paths, unresolved attachments,
missing causal parents, unfetched packs, and incremental edits fail explicitly.

The approved fixture's 1,022 initial entries reconstruct Git tree
`d997a9ffc38b9e37a31d57175036fb8fb086973d`, exactly matching its recorded base
commit `75bd5aaa6a054150042c4ab3a6425a36d9697379`. This establishes the content
identity needed to reuse CAOS's object store; it does not establish file-delta
synchronization or prove that every blob is available on the execution server.

The outgoing history encoder also matches the compiled native reference. A
separately gated live test wrote one unused empty map into the disposable
fixture, received acceptance, then found that exact batch in a fresh download.
The conversation text and files were unchanged. The fixture now has 25 batches.
No turn was acknowledged or executed by that test.

The encoder is not an authorization layer or a turn controller. A writer must
retain an allocated batch across an uncertain send and reconcile it by identity,
not allocate another batch and assume the first one failed.

## Authentication

An operator runs `auth-start`, opens the displayed official Delta verification
URL, and resumes with `auth-wait`. The person using the runner only approves the
normal browser sign-in; they should not have to manage keys or run these commands.

Supply private paths outside the source checkout:

```sh
connector auth-start --request-file=/private/path/device-request.json
connector auth-wait \
  --request-file=/private/path/device-request.json \
  --credential-file=/private/path/credentials.json
connector auth-check --credential-file=/private/path/credentials.json
connector probe \
  --credential-file=/private/path/credentials.json \
  --actor-file=/private/path/actor.json \
  --registration-name="CAOS protocol test"
```

State files are created mode `0600`. Existing files are not overwritten.
Device approval expires, polling observes the server's interval and `slow_down`,
and account credentials never enter command-line arguments or CAOS ArgTrees.

## CAOS probe

Grant a secret named `delta-runner-credentials` to a published, pinned revision
of the `integrations/delta` entry, following [CAOS's reader rules](../../../SPEC.md#readers).
The secret value is a private JSON bundle with this shape:

```json
{
  "account": {"user_id": 123, "access_token": "<issued credential>"},
  "actor": "<this connector's UUID>",
  "allowed_threads": ["<explicitly approved Delta test thread ID>"]
}
```

Keep that value in the private secret store, never in the repository, a job
argument, a result tree, or a transcript. Pin both the code revision and `since`
to the reviewed revision. A changed source tree needs a new matching grant.

Run the `integrations/delta` entry with a fresh `request-id` and,
optionally, a `registration-name`. The worker uses only the production service
origins and the actor sealed into its credential bundle. Callers cannot override
the identity or redirect its token to another endpoint.

`op=inspect-history` with `thread-id` performs the read-only download and returns
only counts. It requires an exact match in the bundle's `allowed_threads`; an
absent or empty list denies all history inspection. This check happens before
requesting a service token. No thread content or replication-pack URL is
published to a CAOS result. `registration-name` is accepted only by `op=probe`.

`op=replay-history` uses the same sealed allowlist, downloads the history, and
reconstructs it in the worker. It returns only counts, not text, map values,
model vouchers, or other private state. Neither history operation accepts turns.

`op=inspect-workspaces` follows the authorized thread's resolved attachments and
reconstructs their pinned initial trees. It returns only workspace and object
counts. It does not write files or accept turns.

`op=bridge-context` imports separate native user and completed assistant messages
into a CAOS session, using the existing `std/llm-step` recording operations.
It then exports the pinned context through the normal model-context renderer
and checks its roles and text. The operation requests no inference and makes
no Delta edits. Its public report contains only counts; the imported conversation
and workspace live in the configured CAOS server, like other CAOS sessions.
Native inference vouchers and unrelated metadata are never passed to that session.

The bridge reconstructs the native workspace tree and verifies that its recorded
base commit resolves to that exact tree in CAOS before using it. Each continuation
rechecks the authorized native frontier. A changed thread aborts the import rather
than mixing versions. Staged jobs release their worker slots between operations.

The execution backend uses CAOS's existing `prepare-request` and `sub-run`
mechanisms. It does not obtain another CAOS credential or implement a second
model/tool loop. `std/run-and-update-ref` can publish immutable task-result refs
for a caller without a CAOS session record; duplicate callbacks reconcile the
same result by identity without force-writing a ref.

### Model authorization preflight and current external blocker

Delta connection credentials are not themselves model credentials. The native
operator can check the connector's own model authorization without inference:

```sh
connector auth-model-check --credential-file=/private/path/credentials.json
```

`op=check-model` performs the same check in CAOS, after requiring the sealed
thread allowlist match. It reads `/client/users/me`, selects only the returned
default organization (which must also appear in the returned membership list),
and posts that organization to `/client/llm_tokens`. It never reads application
settings, obtains another provider key, sends a completion, or changes Delta
history. Tokens and account metadata are never printed or returned.

On 2026-11-11 the approved connector's account lookup returned HTTP 200, but its
default organization's model-token request returned HTTP 402:
“Organization does not have an active subscription.” The approved fixture's
one existing user message contains no nonempty delegated inference voucher.
Consequently, no real model response or visible CAOS-generated Delta reply has
been demonstrated. Fixing the external account entitlement or supplying an
explicitly authorized alternative inference credential is necessary before
that path can run; this work does neither.

This is not a claim that enabling billing completes the integration. **Unfixed
bugs:** the live context bridge still fails during CAOS request preparation,
and the existing driver only implements Anthropic transport, not Delta's model
API. Turn admission and visible reply writes are also not connected yet. The
preflight reports only model-token authorization, not end-to-end readiness.

**Unfixed protocol-support gaps:** structured native tool/thinking history,
attachments, inherited parent context, and multiple or incrementally edited
workspaces still fail explicitly. They are not flattened or silently omitted.

The image contains the statically compiled connector and CA certificates, plus
CAOS's normal additions. It contains no desktop, browser, Delta binary, native
Delta credential store, or Docker-engine grant.

## Remaining implementation

The connection reader now preserves notifications in order while correlating
concurrent RPCs. A canceled or disconnected request is treated as an uncertain
server outcome, not proof that its effects were canceled.

The connector still needs a long-lived worker runtime, reconnect/lease handling,
turn admission and ownership checks, history/workspace synchronization, and the
connection from its prepared execution backend to admitted turns and Delta
reply/file updates. A completed implementation must
demonstrate a real phone-initiated turn, including its reply and workspace
changes, with the laptop disconnected. The current probe does not establish
any of those outcomes.
