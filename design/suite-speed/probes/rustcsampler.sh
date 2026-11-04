#!/bin/bash
# usage: rustcsampler.sh <dir>; logs every rustc/cargo/nix process every 3s until <dir>/stop
d=$1
while [ ! -e "$d/stop" ]; do
  t=$(date +%T)
  for p in /proc/[0-9]*; do
    c=$(tr '\0' ' ' < $p/cmdline 2>/dev/null) || continue
    case "$c" in
      rustc*|*/rustc\ *|cargo*|*/cargo\ *|*clippy-driver*|*rustdoc*|nix\ *|*/nix\ *|*nix-daemon*|*skopeo*|*tar\ *|*zstd*|*cp\ -R*|*git\ *pack*|*podman*\ run*)
        echo "$t $(basename $p) $(readlink $p/cwd 2>/dev/null) :: $(echo "$c" | cut -c1-300)";;
    esac
  done >> "$d/procs.log"
  sleep 3
done
