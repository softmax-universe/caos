# Delta protocol investigation

The goal is a **CAOS-backed Delta protocol participant**, not a hosted Delta
application. This directory currently contains a reproducible, offline codec
probe. It does **not** register a runner or execute agent turns.

## What has been verified

The public Delta Web **0.24.2** bundle includes a public debug-symbol module.
Its function names identify the runner and history protocols; its executable
code provides an independent reference for checking recovered wire formats.

The probe exposes selected codec functions from the compiled module, initializes
their static memory, and runs them in Node. Every imported browser, network, and
storage function throws if called. It removes the application's exported entry
points, rejects a Wasm start section, and never initializes the Delta application.
No account, credentials, local Delta databases, or live threads are used.

The downloaded module is test equipment only. A production adapter should
implement the recovered messages itself, without shipping or loading Delta Web.

**152 checks pass**, covering:

- Turn-start encoding, including unsigned integer boundaries, optional strings,
  an optional generation-state timestamp, and the runner-started flag.
- Machine-runner name payloads and full client envelopes.
- Successful machine-turn acknowledgment encoding.
- Inbound machine-turn dispatch decoding, including the machine, request,
  history, sender, and epoch; unknown fields; and incomplete-message rejection.
- Git blob header encoding and decoding, unknown trailing fields, and rejection
  of truncated headers, missing fields, and incorrectly sized object IDs.

These checks establish selected byte formats, **not service compatibility**.
They do not establish that the server will accept a registration, dispatch a
turn, or accept history updates from a replacement implementation.

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
is `922a92339190`. This was generated locally, **not sent to Delta**. The effect
of an absent name on server registration has not been tested.

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

Field names came from the compiled debug formatter; field order and types came
from the encoder. The probe covers an absent timeline ID, not its full schema.
It also distinguishes Rust in-memory layouts from wire values: for example,
the registration enum's in-memory discriminant is 39, but its wire tag is 51.
The Rust layouts are build-specific test inputs, not an adapter interface.

## Connection and authentication clues

Disassembly of `RemoteSession::new` establishes the browser-facing paths
`/api/connect`, `/api/session`, and `/api/delta_tokens`. The browser token
provider constructs a **POST** request to its configured token endpoint and
decodes a JSON response. The WebSocket implementation's `requires_auth_token`
method returns false; its connection is routed through a same-origin
`/api/connect` URL rather than proving that the upstream service is anonymous.

The sign-in implementation redirects the browser to `/api/auth/sign_in`.
These are static observations, not an authenticated exchange. The browser's
same-origin session and the credentials needed by a standalone connector are
still to be worked through. None of these endpoints was called with an account
session during this investigation.

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

The asset URL comes from the production Wasm's `external_debug_info` section,
not a guessed private endpoint. The pinned web manifest is:
<https://cdn.delta.dev/delta-web/production/45918daa2b408d0f2efa573d44165d038c0d3109/manifest.json>.
No downloaded application code or large generated artifacts belong in Git.

## Next compatibility gates

1. Recover connection setup, ordinary sign-in, machine identity/presence,
   registration response, and dispatch acknowledgment semantics. Use a dedicated
   test identity/thread for live tests; do not extract native-app credentials.
2. Register a separately implemented machine runner and demonstrate a dispatched
   request being received and acknowledged without the Delta application.
3. Recover the history synchronization and turn-ownership rules, including epoch
   checks, cancellation, retry, and reconnect. A Git snapshot alone is not
   Delta's replicated thread/workspace state.
4. Connect that protocol participant to CAOS execution, then demonstrate a
   complete phone-initiated turn with the laptop disconnected.

CAOS already supplies scheduling, content-addressed inputs/results, and resident
worker affinity. Those should remain CAOS's machinery. The adapter must translate
Delta's identity, history, and ownership semantics rather than treating the two
runner protocols as interchangeable. The final execution proof is still pending.
