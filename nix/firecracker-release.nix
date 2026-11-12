# Firecracker and its jailer from the UPSTREAM release, for a runnerd
# Firecracker pool (design/firecracker-runners.md, "Deployment").
#
# Not nixpkgs' build: that one is linked against glibc, and the jailer copies
# the binary into each VM's chroot and execs it there, where the dynamic loader
# in /nix/store does not exist ("Failed to exec into Firecracker: No such file
# or directory"). Upstream releases are static musl binaries, which is what the
# jailer is built for. nixpkgs' static builds (pkgsStatic, pkgsMusl) are not in
# the binary cache, so they would mean a toolchain-sized source build.
#
# Pinned by the tarball's sha256, which is upstream's published
# firecracker-v<version>-x86_64.tgz.sha256.txt. x86_64 only for now: aarch64
# publishes its own tarball and sum.
{ pkgs }:
let
  version = "1.16.1";
in
assert pkgs.lib.assertMsg (pkgs.stdenv.hostPlatform.system == "x86_64-linux")
  "nix/firecracker-release.nix pins only the x86_64 release";
pkgs.stdenvNoCC.mkDerivation {
  pname = "firecracker-release";
  inherit version;
  src = pkgs.fetchurl {
    url = "https://github.com/firecracker-microvm/firecracker/releases/download/v${version}/firecracker-v${version}-x86_64.tgz";
    sha256 = "382a02a869e4d6d5cb14c40577f9545e8458021ea8b0b2d3fc10ec14d9c242e6";
  };
  dontConfigure = true;
  dontBuild = true;
  # Static binaries: nothing to patch, and patchelf would break them.
  dontPatchELF = true;
  dontStrip = true;
  installPhase = ''
    mkdir -p $out/bin
    install -m 755 firecracker-v${version}-x86_64 $out/bin/firecracker
    install -m 755 jailer-v${version}-x86_64 $out/bin/jailer
  '';
}
