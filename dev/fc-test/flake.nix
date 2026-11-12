{
  # dev/fc-test: the Docker image of the Firecracker test workload
  # (./workload.nix). It is the BASE that jobs for a Firecracker pool name, and
  # the server resolves it as for any job. The pool's appliance is built from
  # the same workload.nix by the root flake, so the two cannot disagree about
  # the worker or the image env.
  description = "caos dev/fc-test — the Docker base of the Firecracker test appliance";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workload = import ./workload.nix { inherit pkgs; };
          workerRoot = pkgs.runCommand "fc-test-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${workload.worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-fc-test";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs /worker.
          contents = [ workerRoot ] ++ workload.contents;
          config.Env = workload.env;
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
