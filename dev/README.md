# dev

caos's own development tooling: the dev container image, the test-stack daemon
and the tools that drive it. (Harness glue is under `integrations/`, and the
tools a model calls are under `std/`.)

| entry | what it is |
|---|---|
| `devbox/` | The container image the suite and the stacks run on: nix, podman, git. |
| `test-stack/` | The daemon that builds, tests and runs a stack from a tree (`design/daemons.md`). |
| `caos-stack/` | The tool that sends the daemon `start`, `status`, `logs`, `harvest` and `stop`; `start` with `cloud-env` also makes a cloud environment pointed at the stack. |
| `remove-dev-envs/` | Deletes the cloud environments `caos-stack start` made. |

## Driving a cloud session at a test stack

A worked pass from an agent session. Import caos's `main` once, UNEDITED, as
`imports/caos/main` (`caos-std/import-source` with `root=imports/caos`,
`source=https://github.com/softmax-universe/caos`, `into=main`). That import is where you RUN the tools from. If you
are changing caos, `copy` it to `feature/<name>` and edit THAT: it is the tree
under test, not the place the tools run.

- Reach the tools BY PATH in the import: `imports/caos/main/dev/caos-stack` and
  `imports/caos/main/integrations/claude-code/drive`, with `run_tool`. (`caos-std/`
  is the std pinned by the client repo, not the tree under test, and no longer
  holds `caos-stack`.) Pass the tree to bring up as `in`: `caos-stack` takes it
  (`tool_help` lists it), e.g. `in=feature/<name>`. Leave `in` off to test the
  import itself.
- Do NOT run the tools from `feature/<name>`. `drive` is granted its token only
  when the root tree it is run from is one `caosd up` published (AGENTS.md,
  "Resident workers"), so an edited checkout can fail every `drive` call with
  "no token at /secret/claude-oauth-token". One did, after `drive` itself was
  edited; whether an edit elsewhere also breaks it has not been tested. The
  untouched import is the blessed copy, so it avoids the question.
- `caos-stack` `op=start` needs `relay=http://<ip>/`: the relay your own session
  reaches the server through. `help` (no path) shows it on its `push:` lines
  (`over RELAY relay:http://<ip>/`), but only once the session has pushed from
  its own container, so a session that has only called tools shows none. The
  reliable source is the server's ticket, which carries the relay:
  1. `drive` `verb=env-show`, `env=Caos` (or whichever environment your
     session runs in). Its setup script ends in `--server=caos://endpoint<base32>`,
     optionally followed by `.<token>`: that is the ticket. It is a bearer
     address, so extract the relay from it and do not paste it anywhere.
  2. Decode it in a scratch worker with `caos-std/bash-tool` (any small
     directory as `root`; the worker has coreutils but no Python). The relay is
     stored in the ticket as a plain string, so print the printable runs:

     ```sh
     t=<the base32 between `endpoint` and the dot, if any>
     t=$(printf %s "$t" | tr a-z A-Z)
     while [ $(( ${#t} % 8 )) -ne 0 ]; do t="${t}="; done
     printf %s "$t" | base32 -d | tr -c '[:print:]' '\n' | grep -E '.{6,}'
     ```

     That prints `http://<ip>/`, which is the `relay` to pass. The filter skips the
     endpoint id and any direct addresses on purpose. No output means the ticket
     names no relay: ask whoever runs the stack for theirs.

  Use `cloud-env=1` to get the environment, and a fresh `request-id` for every
  call.
- `drive` `verb=start` with `env=z Caos Dev <commit>`, `repo=<owner>/<repo>`
  and a `prompt`. `at` is any integer not used before. Provisioning takes about
  two minutes; read the result with `drive info` (its `summary` is the turn's
  outcome, empty while the turn runs).
- Each stack start mints a new ticket and dev commit, and the stack is evicted
  when its slot is wanted, so keep one `status` going between turns; after an
  eviction `start` again WITH `cloud-env` and start a new session on the new
  environment. An old environment cannot be repointed.
- A stack is keyed by the hash of the tree under test, so editing
  `feature/<name>` while a stack is up makes `stop` and `status` address a
  different stack. `stop` before editing, or keep the tree you started with
  untouched.
- Not understood yet: `harvest` returned `harvested=0` and `drive conv` found no
  head on the host server, even after a session had completed turns against the
  stack. Do not rely on either to retrieve the session until that is
  explained.
- Clean up: `drive archive`, `drive env-delete`, then `caos-stack stop`.
