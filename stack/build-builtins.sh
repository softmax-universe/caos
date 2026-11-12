#!/usr/bin/env bash
# Stream the NIX PRIMITIVE's image to the stack: the one image a stack still builds
# itself, because it cannot be built by the machinery it is.
#
# `run --base:nix=v1 --in:@=<flake> --attr=<attr>` (design/nix-bootstrap.md) builds
# a flake attribute with the stack's own nix, in a container from this image. The
# core (runner, flake-builder, cargo, rustc, deep-deps) is built that way, by the
# repo's root expression, from the tree's own flake. So there is nothing to seed:
# no hand-built core images, no seed records, no sentinel entries, no seeder. This
# script used to do all of that; what is left is the primitive's own image.
#
# It publishes NO std library: every `std/<name>` is a checked-in source
# directory whose `.caos-expr` says how it is built, and a consumer reaches it by
# DESCENT (design/caos-expr.md). Nothing resolves a builtin by an ambient name.
#
# What it does: push the clean nix-primitive image to the registry once (keyed on
# its store path alone), describe it as a git-docker delta (`base` + `config.json`
# + the caos additions as one content-keyed layer), push the delta's objects to
# the server, and write its hash to CAOS_NIX_IMAGE_FILE, which the server reads
# per `nix:v1` job. The delta is pushed here because nothing else names it: the
# server holds only what a request or a push gives it, and it never evaluates
# this image, so without the push it could not read it ("converting git image
# <hash>: object not found").
#
# Usage: ./stack/build-builtins.sh   (no arguments)
# Requires the dev server running and git + skopeo + curl on PATH. No docker:
# this script composes no images — it pushes the clean one and describes a delta.
# stdout is empty; everything it says goes to stderr.
set -euo pipefail
# IN `stack/` BECAUSE `stack/serve` IS ITS ONLY CALLER, and `..` because THE TREE
# IS THE SUBJECT: `nix build .#…` resolves the flake at the cwd, so this runs from
# the repo root wherever the file itself happens to live.
cd "$(dirname "$0")/.."
PROJECT=$PWD

# caos-cli: a prebuilt binary if the caller injected one (CAOS_CLI — how caosd
# runs us from a store copy with no `nix` at runtime), else built from the flake.
if [ -n "${CAOS_CLI:-}" ]; then
  caos=$CAOS_CLI
else
  nix build .#caos-cli -o result-caos
  caos=$PROJECT/result-caos/bin/caos-cli
fi
SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
export CAOS_SERVER_URL=$SERVER_URL

# A local client working repo with the server as its `caos` remote — the same
# shape a user has. Reused across runs (git init is idempotent). CAOS_CLIENT_REPO
# relocates it off PROJECT (which is read-only when caosd runs us from the store).
CLIENT=${CAOS_CLIENT_REPO:-$PROJECT/.caos-dev/client-repo}
git init -q "$CLIENT"
# When the caller says this repo dies with the process, turn zlib OFF: deflating
# freshly built binaries was measured as most of a push. A persistent client repo
# must NOT get this: there the objects are kept.
if [ "${CAOS_CLIENT_REPO_THROWAWAY:-no}" = yes ]; then
  git -C "$CLIENT" config core.compression 0
  git -C "$CLIENT" config pack.compression 0
fi
git -C "$CLIENT" remote add caos "$SERVER_URL" 2>/dev/null \
  || git -C "$CLIENT" remote set-url caos "$SERVER_URL"

# The nix image tarball's store path. If the caller prebuilt it
# (CAOS_BUILTIN_IMAGES, how caosd hands us the flake's images with no `nix` at
# runtime), use that; else build it here. Either way pick it out by the image
# name baked into its store path (<hash>-caos-worker-nix.tar.gz).
if [ -n "${CAOS_BUILTIN_IMAGES:-}" ]; then
  built_paths=$CAOS_BUILTIN_IMAGES
else
  echo "building the nix primitive's image..." >&2
  if ! built_paths=$(nix build .#caos-worker-nix-docker --no-link --print-out-paths); then
    echo "build-builtins: nix build failed" >&2; exit 1
  fi
fi
tarball=
# Unquoted: word-split on whitespace (nix-build prints one path per line, and
# CAOS_BUILTIN_IMAGES is space-separated). Store paths contain neither whitespace
# nor glob characters.
# shellcheck disable=SC2086
for p in $built_paths; do
  case "$p" in *-caos-worker-nix.tar.gz) tarball=$p ;; esac
done
[ -n "$tarball" ] || { echo "build-builtins: no nix image built" >&2; exit 1; }

# Where `bin/caos` lives, for the additions layer below.
if [ -n "${CAOS_BUILTIN_BINS:-}" ]; then
  bin_paths=$CAOS_BUILTIN_BINS
else
  bin_paths=$(nix build .#caos --no-link --print-out-paths) \
    || { echo "build-builtins: nix build failed" >&2; exit 1; }
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# A phase clock, into the SAME file the calling stage's `ts` writes, so this
# script's phases interleave with the build's in one report. Off unless the
# caller names a file.
bts() { if [ -n "${CAOS_PHASES:-}" ]; then echo "$(date +%s%3N) builtins: $*" >> "$CAOS_PHASES"; fi; }
bts "start"

REGISTRY=localhost:5000 # the compose stack's registry, host-published (caosd)
# The same registry as THIS SCRIPT reaches it over HTTP. On the host the two
# names coincide. Inside the test stack they do not: we run in a container on
# caos-net and must call it caos-registry:5000.
REGISTRY_HTTP=${CAOS_REGISTRY_HTTP:-$REGISTRY}
# How the SERVER reaches the same registry: it pulls a delta's `base` with skopeo
# from inside its own container, where the registry is a service on the docker
# network. Mirrors the server's own CAOS_REGISTRY_PUSH_URL default.
REGISTRY_BASE_HOST=${CAOS_REGISTRY_BASE_HOST:-caos-registry:5000}
# <repo:tag> -> the registry's manifest digest, or empty when the tag is absent.
# ABSENT AND BROKEN ARE DIFFERENT: a 404 is the ordinary first-push case and
# returns empty, but an unreachable registry or any other status is fatal —
# swallowing those would silently re-push (or worse, mint a `base` ref for an
# image nothing can pull).
manifest_digest() { # <repo:tag>
  local url headers status
  url="http://$REGISTRY_HTTP/v2/caos/manifests/$1"
  headers=$(curl -sSI \
    -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
    -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
    "$url") || { echo "build-builtins: registry unreachable: $url" >&2; exit 1; }
  status=$(printf '%s\n' "$headers" | awk 'NR==1 {print $2}')
  case "$status" in
    200) printf '%s\n' "$headers" \
           | tr -d '\r' \
           | awk 'tolower($1)=="docker-content-digest:" {print $2}' ;;
    404) ;;
    *) echo "build-builtins: registry returned $status for $url" >&2; exit 1 ;;
  esac
}
tool_bin() { # <name> -> the path of bin/<name> among the built paths
  local p
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    if [ -x "$p/bin/$1" ]; then
      echo "$p/bin/$1"
      return 0
    fi
  done
  echo "build-builtins: no binary $1 in the built paths" >&2
  exit 1
}
caos_bin=$(tool_bin caos)

# The caos additions, staged as a git-docker LAYER TREE: /bin/caos (setuid), the
# user db, /usr/bin/env, /tmp. The server rebuilds the layer tar from this at
# convert time and applies the `<name>.caosmeta` sidecars for the modes git
# cannot record (setuid, sticky). Staged inside CLIENT because only git-tracked
# paths can be hashed here.
#
# (The core images get the same pieces from the root expression, as layers built
# by nix and a checked-in tree: stack/core-layers/static is the static half of
# this.)
ADD=$CLIENT/layer-additions
rm -rf "${ADD:?}"
mkdir -p "$ADD/bin" "$ADD/usr/bin" "$ADD/etc"
install -m 755 "$caos_bin" "$ADD/bin/caos"
# setuid needs no privilege here: the sidecar is data, and the SERVER (root)
# applies the mode when it rebuilds the layer.
printf '{"mode":"4755","uid":0,"gid":0}' > "$ADD/bin/caos.caosmeta"
# /usr/bin/env for env-shebang scripts; every streamed userland has /bin/env.
ln -s /bin/env "$ADD/usr/bin/env"
printf 'root:x:0:0:root:/root:/sbin/nologin\nworker:x:1000:1000:caos worker:/tmp:/sbin/nologin\n' \
  > "$ADD/etc/passwd"
printf 'root:x:0:\nworker:x:1000:\n' > "$ADD/etc/group"
# The world-writable /tmp. Its 1777 mode rides in a sidecar, and the DIRECTORY is
# carried by a keep-file because it must not be EMPTY: an empty directory is a
# legal git tree entry but does not survive a materialize -> read -> re-put round
# trip, so a worker that rebuilds a tree from the filesystem silently drops it.
mkdir -p "$ADD/tmp"
printf 'This file exists so that /tmp is a NON-EMPTY directory.\n\nAn empty directory is not representable in a git worktree and does not survive\na materialize -> read -> re-put round trip, so a worker that rebuilds a tree\nfrom the filesystem (deep-deps) drops it. /tmp must exist in the image, so it\ncarries this file. Its 1777 mode rides in the tmp.caosmeta sidecar.\n' \
  > "$ADD/tmp/.caos-keep"
printf '{"mode":"1777","uid":0,"gid":0}' > "$ADD/tmp.caosmeta"
git -C "$CLIENT" add layer-additions
additions_tree=$(git -C "$CLIENT" write-tree --prefix=layer-additions/)
bts "staged the additions"

# The clean image goes to the registry ONCE, keyed on its store path's own name
# (not the path we reached it by, which sits inside a per-build aggregate), so a
# rebuild of the workspace that leaves the image alone is a registry hit.
ctag="clean-nix-$(basename "$tarball" | sha1sum | cut -c1-12)"
digest=$(manifest_digest "$ctag")
if [ -n "$digest" ]; then
  echo "nix: registry hit for $ctag" >&2
else
  echo "nix: streaming clean image $ctag..." >&2
  # skopeo's docker-archive transport reads an uncompressed tar; nix hands us
  # .tar.gz.
  gunzip -c "$tarball" > "$WORK/nix.tar"
  skopeo --insecure-policy copy --dest-tls-verify=false \
    "docker-archive:$WORK/nix.tar" "docker://$REGISTRY_HTTP/caos:$ctag" >&2
  rm -f "$WORK/nix.tar"
  digest=$(manifest_digest "$ctag")
  [ -n "$digest" ] || { echo "build-builtins: no digest for pushed $ctag" >&2; exit 1; }
fi

img=$CLIENT/img-nix
rm -rf "${img:?}"
mkdir -p "$img"
# `base` is read by the SERVER, which pulls it with skopeo from inside its own
# container — so it carries the on-network registry name, not the host-published
# one the docker daemon uses.
printf 'docker://%s/caos@%s' "$REGISTRY_BASE_HOST" "$digest" > "$img/base"
# The clean image's own OCI config, verbatim: it already names the runner
# entrypoint and its PATH, because its flake said so.
skopeo --insecure-policy inspect --tls-verify=false --config \
  "docker://$REGISTRY_HTTP/caos:$ctag" > "$img/config.json"
git -C "$CLIENT" add img-nix
nix_delta=$(
  {
    git -C "$CLIENT" ls-tree "$(git -C "$CLIENT" write-tree --prefix=img-nix/)"
    printf '040000 tree %s\tlayer00\n' "$additions_tree"
  } | git -C "$CLIENT" mktree
)
echo "nix: git-docker delta $nix_delta over $ctag" >&2
bts "delta for nix"

# Pushed under `refs/caos/req/<hash>`, where every other pushed object in this
# system lands: content-addressed, so two publishers into one repo cannot
# collide, and the server's request-ref pruner already sweeps it.
#
# The server reads the image through the file CAOS_NIX_IMAGE_FILE names, per
# `nix:v1` job. Written whole and renamed, so a reader never sees half.
if [ -n "${CAOS_NIX_IMAGE_FILE:-}" ]; then
  git -C "$CLIENT" push -q --force caos "$nix_delta:refs/caos/req/$nix_delta"
  printf '%s\n' "$nix_delta" > "$CAOS_NIX_IMAGE_FILE.tmp"
  mv "$CAOS_NIX_IMAGE_FILE.tmp" "$CAOS_NIX_IMAGE_FILE"
  echo "nix primitive: $nix_delta" >&2
fi
bts "pushed the nix image"
