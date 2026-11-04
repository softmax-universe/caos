{
  description = "Standalone Delta protocol connector on CAOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          connector = import ./connector/package.nix {
            inherit pkgs;
            prelude = ./DEEP-DEPS/prelude;
          };
          root = pkgs.runCommand "caos-delta-connector-root" { } ''
            mkdir -p $out/tmp
            chmod 1777 $out/tmp
            install -m755 ${connector}/bin/connector $out/worker
          '';
        in
        {
          inherit connector;
          caosImage = pkgs.dockerTools.buildLayeredImage {
            name = "caos-delta-connector";
            tag = "latest";
            contents = [
              root
              pkgs.cacert
            ];
            config.Env = [
              "PATH=/bin"
              "TMPDIR=/tmp"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
            ];
            fakeRootCommands = ''
              mkdir -p tmp
              chmod 1777 tmp
            '';
          };
        };
    in
    {
      packages = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value = forSystem system;
          })
          [
            "x86_64-linux"
            "aarch64-linux"
          ]
      );
    };
}
