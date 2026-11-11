# Delta protocol investigation

The goal is a **CAOS-backed Delta protocol participant**, not a hosted Delta
application. This directory contains reproducible, offline codec and state
probe. It does **not** register a runner or execute agent turns.

## What has been verified

The public Delta Web **0.24.2** bundle includes a public debug-symbol module.
Its function names identify the runner and history protocols; its executable
code provides an independent reference for checking recovered wire formats.

The probe exposes selected codecs and pure in-memory text/map operations from
the compiled module, initializes static memory, and runs them in Node. Every imported browser, network, and
storage function throws if called. It removes the application's exported entry
points, rejects a Wasm start section, and never initializes the Delta application.
No account, credentials, local Delta databases, or live threads are used.

The downloaded module is test equipment only. A production adapter should
implement the recovered messages itself, without shipping or loading Delta Web.

**9,627 checks pass**, covering:

- Turn-start encoding, including unsigned integer boundaries, optional strings,
  an optional generation-state timestamp, timeline IDs backed by UUID or Git
  commit actors, and the runner-started flag.
- Turn-start decoding with omitted newer fields, explicit nil defaults, and
  rejection of malformed field types and option payloads.
- Machine-runner name payloads and full client envelopes.
- Successful machine-turn acknowledgment encoding.
- Inbound machine-turn dispatch decoding, including the machine, request,
  history, sender, and epoch; unknown fields; and incomplete-message rejection.
- Git blob header encoding and decoding, unknown trailing fields, and rejection
  of truncated headers, missing fields, and incorrectly sized object IDs.
- Default thread-root references, including nested field paths, omitted and nil
  fields, and re-encoding explicit references.
- The per-history connection path, history request envelopes, and inline
  history-delta response decoding.
- Text edits, causal parent snapshots, concurrent insertion order, Unicode byte
  offsets, scratch-buffer creation, and payload-ID allocation.
- Observed-remove map behavior, including preservation of unseen concurrent
  values and removal that reveals a previously hidden value.
- Scratch-buffer anchors, including both biases, deleted text, and byte positions
  inside Unicode scalars, with 768 queries across the generated histories.
- No-op and causal `UpdateHistory` envelopes, outgoing batches, timeline updates,
  and decoding the acceptance response observed in the separate live write check.

These checks establish selected byte formats, **not service compatibility**.
They do not establish that the server will accept a registration, dispatch a
turn, or accept history updates from a replacement implementation.
The separate [standalone connector](../connector/README.md) has since passed
live authentication, heartbeat, registration, and history reads from CAOS. A
separately gated write added one unused map to the approved disposable fixture
and verified it in a fresh readback. Turn ownership, ongoing synchronization,
and agent-turn execution remain unfinished.

## Recovered wire facts

These messages use MessagePack through Delta's `versioned` codec. Structs are
positional arrays; the tested options encode as `[]` or `[value]`, not `nil`.
UUIDs are MessagePack binary values containing 16 bytes. Git object IDs in the
tested header are binary values containing 20 bytes.

| Message | MessagePack representation |
| --- | --- |
| `ClientMessage` | `[request_id_u32, request_body]` |
| `SetLocalMachineRunner`, present name | `[request_id, [51, [[name]]]]` |
| `SetLocalMachineRunner`, absent name | `[request_id, [51, [[]]]]` |
| Successful machine-turn acknowledgment payload | `[request_uuid, []]` |
| Inbound `LocalMachineTurn` | `[18, [machine_actor, request_uuid, history_id, start_turn]]` |
| Git blob frame header | `[nil, oid_20_bytes, byte_length_u64]` |

The full client envelope for an absent machine-runner name, with request ID 42,
is `922a92339190`. The standalone connector has since verified against the live
service that an absent name clears its diagnostic machine registration.

Disassembly also identifies request tags 52 (`SendLocalMachineTurn`) and
53 (`AcknowledgeLocalMachineTurn`). Their complete envelope encodings are not
covered by the current probe. These are not the same namespace as
`delta_remote_protocol::runner::RequestBody`, which includes terminal and model
discovery requests. Similar names do not establish protocol direction.

The recovered `StartTurn` field order is:

1. `sender_actor`: UUID
2. `epoch`: unsigned 64-bit integer
3. `organization_id`: optional string
4. `generation_state_write_timestamp`: optional `BTreeNodeId`
5. `assistant_turn_id`: optional string
6. `timeline_id`: optional `TimelineId`
7. `runner_started`: boolean

Only sender and epoch are required. Later fields default when omitted or
explicitly nil. Canonical optional values still encode as `[]` or `[value]`.
A timeline ID encodes as `[timestamp_u64, actor]`, where the actor is either
`[0, commit_oid_20_bytes]` or `[1, uuid_16_bytes]`. These rules are checked against
the compiled encoder and decoder, including unsigned integer boundaries.

Field names came from the compiled debug formatter; field order and types came
from the encoder. The probe also distinguishes Rust in-memory layouts from wire
values: for example, the registration enum's in-memory discriminant is 39, but
its wire tag is 51. The Rust layouts are build-specific test inputs, not an
adapter interface.

## Connection and authentication clues

Disassembly of `RemoteSession::new` establishes the browser-facing paths
`/api/connect`, `/api/session`, and `/api/delta_tokens`. The browser token
provider constructs a **POST** request to its configured token endpoint and
decodes a JSON response. The WebSocket implementation's `requires_auth_token`
method returns false; its connection is routed through a same-origin
`/api/connect` URL rather than proving that the upstream service is anonymous.

The sign-in implementation redirects the browser to `/api/auth/sign_in`.
These are static observations, not a captured browser session. The standalone
connector instead uses the normal device-approval flow and obtains its own
credential. It does not extract or reuse browser cookies or native-app tokens.

## History transport

The public `history_websocket_path` helper returns
`/history/<hyphenated history UUID>/connect`. The history driver resolves it
under the service prefix and adds the actor query parameter. A live read of the
approved disposable test thread confirmed that the control socket rejects
history requests and the scoped socket accepts them.

The opaque Delta thread ID is URL-safe base64 of the positional pair
`[history_uuid, timeline_id]`. `FetchHistorySince` has request tag 6 and payload
`[history_uuid, timeline_id, known_history_version]`. Its response includes a
`CompressedDeltaBatchIds` value and timeline metadata. That compressed value is
a one-field struct `[binary_data]`, not a sequence of independent chunks.

`FetchHistoryDeltas` has request tag 27 and payload
`[history_uuid, compressed_delta_batch_ids]`. The reader forwards the index's
compressed value unchanged. It parses the `Deltas` response (variant 0) and
`DeltasAndReplicationPacks` (variant 2), retaining every batch's ID, causal
parents, and delta payloads. Superseded, error, and unsupported responses are
not treated as empty histories.

The native CLI has downloaded 24 inline batches from the fixture through this
path. That verifies transport and structural decoding, not replay, turn
ownership, or workspace synchronization. Replication-pack references are
retained but their download path is still unimplemented.

## Thread and history descriptors

The reference probe also invokes the compiled schema collectors for
`ThreadDataStub` and `HistoryDeltaBatch`. It recovers **553 schema descriptors**,
including message variants, generation state, register/map operations, text
edits, and worktree deltas. All named references resolve within the recovered
definitions, and repeated extraction must produce identical results.

This uses only public compiled metadata. It does not read a live database,
initialize the application, or enable browser/network/storage imports.
The Rust memory layouts are pinned to the checked build hash; they are not
interfaces that the production connector should depend on.

To export the descriptors, explicitly choose a new output file:

```sh
node integrations/delta/protocol/verify_reference.cjs "$cache/isolated" \
  --schema-output "$cache/thread-history-schema.json"
```

Default verification remains read-only, and export refuses to overwrite an
existing file. Numeric schema kinds and field flags are preserved rather than
assigned unverified meanings. The descriptors are evidence for implementing
state synchronization, not an implementation or proof of correct replay.

The reference now also decodes an empty `ThreadDataStub` with an explicit root
field context and encodes the resulting references. Its output matches bytes
composed independently from the declared field paths, both at the root and
under a nested context. The field-reference branch is `[1, binary_field_path]`;
each path component is one byte. `FieldId::push` confirms the inline path
representation and appends components up to a maximum depth of 15.

For these thread fields, omitted or nil values create references from their
field context. Register and map stubs contain only those references, not their
values or entries; the default text buffer is a scratch-buffer reference.
This verifies the contextual-default behavior represented by schema kind 23
in this root. Other untested schema wrappers remain unverified. This is an
offline serialization fixture, not a synchronized thread or a history replay.

## Reproduce without signing in

Requires Python 3 and Node with shared-memory WebAssembly support. The downloaded
debug module is **739 MB**; the prepared module is about 69 MB. Keep both outside
the source tree. The scripts use only standard libraries.

```sh
cache="$(git rev-parse --absolute-git-dir)/delta-protocol-reference"
mkdir -p "$cache"
curl -fSL \
  'https://cdn.delta.dev/delta-web/production/45918daa2b408d0f2efa573d44165d038c0d3109/symbols/delta_web-8204d5b93885d614_bg.debug.wasm' \
  -o "$cache/delta-web.debug.wasm"
python3 integrations/delta/protocol/prepare_reference.py \
  --debug-wasm "$cache/delta-web.debug.wasm" \
  --output-dir "$cache/isolated"
node integrations/delta/protocol/verify_reference.cjs "$cache/isolated"
python3 -B -m unittest discover -s integrations/delta/protocol -p 'test_*.py'
```

Preparation checks the source SHA-256:
`e10998d63abd1a7ea7fafe21d8216ee2612a8d749e8c46d0701eb7b969fae13d`.
It discovers codec functions by their names rather than assuming function
indices. The verifier is read-only; only explicit preparation writes generated
files to the supplied output directory.

To compare Go text replay against the native reference's 128 generated histories:

```sh
node integrations/delta/protocol/verify_reference.cjs "$cache/isolated" \
  --text-output "$cache/new-text-cases.json"
cd integrations/delta/connector
CAOS_DELTA_TEXT_REFERENCE_CASES="$cache/new-text-cases.json" \
  go test -race -run=TestNativeTextReferenceCorpus -count=1 .
```

The cases include forks, multi-parent merges, Unicode, replacements, deletions,
and anchor-resolution queries. The Go check also shuffles batch delivery. The native map checks
establish a separate rule: updates remove only values observed by their parent,
and reading chooses the greatest surviving insertion ID. A deletion is not a
last-writer-wins tombstone that can erase an unseen insertion.

The explicit text export refuses to overwrite a file. Default Go tests do not
download the reference or use real thread/account state. Default verification
does not write generated results into the source tree.

The asset URL comes from the production Wasm's `external_debug_info` section,
not a guessed private endpoint. The pinned web manifest is:
<https://cdn.delta.dev/delta-web/production/45918daa2b408d0f2efa573d44165d038c0d3109/manifest.json>.
No downloaded application code or large generated artifacts belong in Git.

## Next compatibility gates

Authentication, connection setup, and diagnostic machine registration now pass
in the standalone connector. The remaining acceptance gates are:

1. Demonstrate a dispatched turn being received and admitted by the connector,
   using a dedicated test thread rather than an active conversation.
2. Implement the history synchronization and turn-ownership rules, including epoch
   checks, cancellation, retry, and reconnect. A Git snapshot alone is not
   Delta's replicated thread/workspace state.
3. Connect that protocol participant to CAOS execution, then demonstrate a
   complete phone-initiated turn with the laptop disconnected.

CAOS already supplies scheduling, content-addressed inputs/results, and resident
worker affinity. Those should remain CAOS's machinery. The adapter must translate
Delta's identity, history, and ownership semantics rather than treating the two
runner protocols as interchangeable. The final execution proof is still pending.
