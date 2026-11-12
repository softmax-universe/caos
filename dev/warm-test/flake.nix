{
  # dev/warm-test: a script worker image whose runner stays WARM long enough
  # for a test to send it another job, for tests/warm-pool
  # (design/firecracker-runners.md, phase 0a). The default idle poll is 2s,
  # which a staged test's next stage does not reliably beat.
  #
  # Otherwise it is std/bash: ./worker is the same script interpreter.
  #
  #   CAOS_RUNNER_TTL_MS  a warm runner's idle poll
  description = "caos dev/warm-test — a script worker image whose runner stays warm for a minute, for tests/warm-pool";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "warm-test-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-warm-test";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs /worker.
          contents = [
            workerRoot
            pkgs.bash
            pkgs.coreutils
            pkgs.gnugrep
          ];
          config = {
            Env = [
              "PATH=/bin"
              "CAOS_RUNNER_TTL_MS=60000"
            ];
          };
        };
    in
    {
      packages = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value = {
              caosImage = forSystem system;
            };
          })
          [
            "x86_64-linux"
            "aarch64-linux"
          ]
      );
    };
}
