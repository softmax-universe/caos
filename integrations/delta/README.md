# Delta protocol integration

This integration implements a standalone Delta protocol participant in Go,
backed by CAOS. It does not host the Delta application, a desktop, or a browser.

**Authentication and machine registration work from a CAOS worker. Agent-turn
execution and thread/workspace synchronization are still being implemented.**
The current CAOS entry runs a bounded diagnostic probe, not a persistent agent.

- [Connector implementation and operator instructions](connector/README.md)
- [Recovered protocol formats and offline reference checks](protocol/README.md)

## Entry points

The CAOS entry is `integrations/delta`, with a fresh `request-id` and an optional
`registration-name`. Its credentials come only from a code-pinned secret grant.

The native operator CLI is built from the same Go module:

```sh
nix run .#delta-runner -- auth-start --request-file=/private/path/device-request.json
```

The operator handles deployment and credential storage. The person using Delta
only approves its normal device sign-in. Never put credentials in arguments,
the source checkout, or result trees.

The earlier desktop-hosting prototype has been retired from the source tree.
It is not part of this implementation.
