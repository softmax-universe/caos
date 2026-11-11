{ pkgs, prelude }:

let
  preludeSource = builtins.path {
    path = prelude;
    name = "caos-go-prelude";
  };
  connectorSource = pkgs.lib.cleanSourceWith {
    src = ./.;
    filter =
      path: type:
      type == "regular"
      && (
        pkgs.lib.hasSuffix ".go" path
        || builtins.elem (builtins.baseNameOf path) [
          "go.mod"
          "go.sum"
        ]
      );
  };
in
pkgs.buildGoModule {
  pname = "caos-delta-connector";
  version = "0.1.0";
  src = pkgs.runCommand "caos-delta-connector-source" { } ''
    mkdir -p $out/integrations/delta/connector $out/std/go/prelude
    cp -RL "${preludeSource}/." "$out/std/go/prelude/"
    cp "${connectorSource}/go.mod" "${connectorSource}/go.sum" "$out/integrations/delta/connector/"
    cp "${connectorSource}/"*.go "$out/integrations/delta/connector/"
  '';
  modRoot = "integrations/delta/connector";
  vendorHash = "sha256-LY5Ks0+sY6OvXKKs6n+X/j/LCWZlSRPSfkyseo2JbrI=";
  subPackages = [ "." ];
  env.CGO_ENABLED = 0;
  meta.mainProgram = "connector";
}
