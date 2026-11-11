# Standalone Delta connector

This is a Go implementation of selected Delta protocol operations. It does not
launch, embed, or download the Delta application. The public reference Wasm is
used only by the separate [protocol investigation](../protocol/README.md).

**Current state: authentication and registration work; agent-turn execution is
not implemented yet.** The CAOS entry currently implements a bounded diagnostic
probe, not a persistent runner. Do not select a diagnostic machine for real work.

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
of this entry, following [CAOS's reader rules](../../../SPEC.md#readers).
The secret value is a private JSON bundle with this shape:

```json
{
  "account": {"user_id": 123, "access_token": "<issued credential>"},
  "actor": "<this connector's UUID>"
}
```

Keep that value in the private secret store, never in the repository, a job
argument, a result tree, or a transcript. Pin both the code revision and `since`
to the reviewed revision. A changed source tree needs a new matching grant.

Run the `integrations/delta/connector` entry with a fresh `request-id` and,
optionally, a `registration-name`. The worker uses only the production service
origins and the actor sealed into its credential bundle. Callers cannot override
the identity or redirect its token to another endpoint.

The image contains the statically compiled connector and CA certificates, plus
CAOS's normal additions. It contains no desktop, browser, Delta binary, native
Delta credential store, or Docker-engine grant.

## Remaining implementation

The connector still needs a persistent receive loop, reconnect/lease handling,
turn admission and ownership checks, history/workspace synchronization, and a
bridge to CAOS's existing agent execution. A completed implementation must
demonstrate a real phone-initiated turn, including its reply and workspace
changes, with the laptop disconnected. The current probe does not establish
any of those outcomes.
