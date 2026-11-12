#!/bin/bash
# tests/nix-primitive — a WORKER test: no client, no repo.
#
# `run --base:nix=v1 --in:@=<flake> --attr=<attr>` (design/nix-bootstrap.md)
# builds a flake attribute with the stack's own nix and adds NOTHING to what
# nix built. The fixture flakes have no inputs, so nothing is fetched, and they
# build with the /bin/bash and /bin/mkdir the flake-builder image carries.
#
# WHAT IT MUST SHOW:
#   - a single-file store path comes back as that file, byte for byte;
#   - a directory store path comes back as that tree, nothing added;
#   - the same flake and attr is the same key, so asking twice is one answer.
#
# It also covers the stack's side: the server can run `nix:v1` only if the
# publish (build-builtins.sh) both named the `caos-worker-nix` image in
# CAOS_NIX_IMAGE_FILE and pushed its objects. A name without the push fails
# here as "converting git image <hash>: object not found".
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt "$@"; }

# A flake with no inputs, its lock, and an expression building <attr>. The
# salt rides in the derivation, so a fresh --test-salt is a fresh build.
fixture() { # <attr> -> a /cas path
  local r=/tmp/fx salt
  caos get /cas/args/test-salt
  salt=$(cat /cas/args/test-salt)
  rm -rf "$r"; mkdir -p "$r/prim"
  cat > "$r/prim/flake.nix" <<NIX
{
  outputs = { self }: {
    packages.x86_64-linux = {
      file = derivation {
        name = "file"; system = "x86_64-linux"; builder = "/bin/bash";
        args = [ "-c" "echo 'hi from nix $salt' > \$out" ];
      };
      tree = derivation {
        name = "tree"; system = "x86_64-linux"; builder = "/bin/bash";
        args = [ "-c" "/bin/mkdir -p \$out/bin && echo tool > \$out/bin/x" ];
      };
    };
  };
}
NIX
  printf '{ "nodes": { "root": {} }, "root": "root", "version": 7 }\n' > "$r/prim/flake.lock"
  printf 'run --base:nix=v1 --in:@=. --attr=%s\n' "$1" > "$r/prim/.caos-expr"
  caos put "$r" "/cas/fixture-$1" >/dev/null || fail "publishing the fixture"
  echo "/cas/fixture-$1"
}

case "$stage" in

start)
  echo "== a single-file output is that file ==" >&2
  caos eval-path-then "$(fixture file)" --eval=prim --then:hash="$(next file)"
  ;;

file)
  caos get /cas/args/result || fail "reading the result"
  caos get /cas/args/test-salt
  [ "$(cat /cas/args/result)" = "hi from nix $(cat /cas/args/test-salt)" ] \
    || fail "wrong file: $(cat /cas/args/result 2>&1)"
  echo "  ok: the store file, byte for byte" >&2

  echo "== a directory output is that tree, nothing added ==" >&2
  caos eval-path-then "$(fixture tree)" --eval=prim --then:hash="$(next tree)"
  ;;

tree)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/bin/x")" = "tool" ] || fail "wrong tree content"
  [ "$(ls -A "$R")" = "bin" ] || fail "the primitive added to the output: $(ls -A "$R")"
  [ "$(ls -A "$R/bin")" = "x" ] || fail "the primitive added to bin/: $(ls -A "$R/bin")"
  echo "  ok: exactly what nix built" >&2

  echo "== the same flake and attr is the same answer ==" >&2
  caos eval-path-then "$(fixture tree)" --eval=prim \
    --then:hash="$(next again --first:@=/cas/args/result)"
  ;;

again)
  [ "$(caos hash /cas/args/result)" = "$(caos hash /cas/args/first)" ] \
    || fail "two evaluations of one flake+attr gave two objects"
  echo "  ok: one key, one object" >&2

  printf 'nix-primitive: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
