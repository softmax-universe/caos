# Standalone Delta connector

This is a Go implementation of selected Delta protocol operations. It does not
launch, embed, or download the Delta application. The public reference Wasm is
used only by the separate [protocol investigation](../protocol/README.md).

**Current state: authentication, registration, and read-only history download
work; agent-turn execution is not implemented yet.** The CAOS entry implements
bounded checks, not a persistent runner. Do not select a diagnostic machine for
real work.

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

For source revision `ced02e82db97134a5714ef731fb5e54d8e428e1c`, validation
included the race-enabled Go tests, `go vet`, a native Nix package build, the
live CAOS probe, and the full salted CAOS suite: **79/79 passed**. This verifies
the connector's current operations; it is not evidence of agent-turn execution.

The canonical root entry at revision `7624c1d767cae4866080226c08a01731c6cee0a0`
also passed the live CAOS probe and the full salted suite (**79/79**), as well as
`nix build --no-link .#delta-runner`. The offline reference now checks 382 wire
cases. The dispatch decoder reads typed epochs and timeline identities, accepts
the reference's defaults for omitted newer fields, and rejects malformed types.
Parsing a dispatch does not admit the turn or acknowledge execution.

The typed dispatch decoder at revision
`868fb0085c5b92ce238e66f28d35f651c391f1ad` passed the race-enabled Go tests
(`-count=10`), `go vet`, the native Nix build, a live CAOS probe, and the full
salted suite (**79/79**). The history reference checks additionally establish
default root-object IDs offline; replay and live turn execution remain unbuilt.

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

The image contains the statically compiled connector and CA certificates, plus
CAOS's normal additions. It contains no desktop, browser, Delta binary, native
Delta credential store, or Docker-engine grant.

## Remaining implementation

The connection reader now preserves notifications in order while correlating
concurrent RPCs. A canceled or disconnected request is treated as an uncertain
server outcome, not proof that its effects were canceled.

The connector still needs a long-lived worker runtime, reconnect/lease handling,
turn admission and ownership checks, history/workspace synchronization, and the
bridge to CAOS's existing agent execution. A completed implementation must
demonstrate a real phone-initiated turn, including its reply and workspace
changes, with the laptop disconnected. The current probe does not establish
any of those outcomes.
