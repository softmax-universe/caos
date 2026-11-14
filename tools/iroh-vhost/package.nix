# One derivation for both flakes that build it: this directory's own, and the
# caos root flake, whose prod/runner hosts run `iroh-vhost pin`.
{ rustPlatform }:
rustPlatform.buildRustPackage {
  pname = "iroh-vhost";
  version = "0.1.0";
  src = ./.;
  cargoLock.lockFile = ./Cargo.lock;
  meta.mainProgram = "iroh-vhost";
}
