#!/bin/bash
# usage: CAOS_PROBE_DIR=<dir> runsuite.sh <name> [caos-test args...]
# A cold suite run (a fresh CAOS_SALT) from the repo root, with the samplers
# beside it; everything lands in $CAOS_PROBE_DIR/<name>. Each sampler stops when
# the run touches <name>/stop.
out="${CAOS_PROBE_DIR:?set CAOS_PROBE_DIR to a directory for run output}"
here=$(cd "$(dirname "$0")" && pwd)
d=$out/$1; shift; mkdir -p "$d"; rm -f "$d/stop"
cd "$here/../../.."
"$here/sampler.sh" "$d" & "$here/rustcsampler.sh" "$d" &
nix run nixpkgs#python3 -- "$here/procacct.py" "$d" &
nix run nixpkgs#python3 -- "$here/syssample.py" "$d" &
date -Is > "$d/start"
( export CAOS_SALT=$(basename "$d")-$(date +%s); time result/bin/caos-cli run-tool caos-test "$@" ) > "$d/out.log" 2>&1
echo "EXIT=$?" >> "$d/out.log"
touch "$d/stop"
