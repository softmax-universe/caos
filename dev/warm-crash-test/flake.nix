{
  # dev/warm-crash-test: a script worker image whose RUNNER dies holding a job
  # it claimed itself, for tests/warm-crash (design/firecracker-runners.md,
  # phase 0b). Its runner stays warm long enough to take a second job, and
  # exits without posting the moment it claims it — the case where the host's
  # crash backstop only knew the first job's nonce, and the second job's caller
  # waited forever.
  #
  # Otherwise it is std/bash: ./worker is the same script interpreter.
  #
  #   CAOS_RUNNER_TTL_MS                  a warm runner's idle poll; long, so the
  #                                       test's second job reaches this runner
  #   CAOS_RUNNER_TEST_EXIT_AFTER_CLAIMS  test-only: exit on the second claim
  description = "caos dev/warm-crash-test — a script worker image whose warm runner dies on its second job, for tests/warm-crash";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "warm-crash-test-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-warm-crash-test";
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
              "CAOS_RUNNER_TEST_EXIT_AFTER_CLAIMS=2"
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
