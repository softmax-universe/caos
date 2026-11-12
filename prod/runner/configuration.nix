# A runner-prod host: compute for caosd-prod, on another machine.
#
# Apply it with
#
#   sudo nixos-rebuild switch --refresh --flake github:Metta-AI/caos#runner-prod
#
# A bare rebuild works, as for the watchdog: nothing here is per-machine. The
# server is named by three tickets in Secrets Manager, not by an address, so
# every runner host is the same config and the server can move without one.
#
# THE SHAPE IS caosd-prod's, MINUS THE SERVER. There, one container on
# `caos-net` carries the names `caos-server`, `caos-registry` and `caos-redis`,
# runs runnerd, and runnerd's workers join the same network and use those names.
# Here one container does the same, except that behind each name is
# `iroh-vhost pin`, splicing to that service's endpoint on the server
# (stack/serve, CAOS_STACK_RUNNER_TICKETS):
#
#   caos-server:80        -> the server        (runnerd's polls, workers' API)
#   caos-registry:5000    -> the registry      (workers push here)
#   caos-redis:6379       -> redis             (workers' own cache)
#   127.0.0.1:5000 (host) -> the registry      (dockerd pulls `localhost:5000/...`)
#
# so neither runnerd, its workers, nor the image refs the server hands out can
# tell which machine they are on.
#
# runnerd has to be IN that container, not a host unit: it resolves
# CAOS_SERVER_URL to an address once and hands the address to every worker
# (rust/crates/runnerd, `as_address`), so it must resolve `caos-server` the way
# the workers do, through the network's DNS.

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.caos.runner;
  services = [
    "server"
    "registry"
    "redis"
  ];
  secretFile = service: "/run/caos-secrets/runner-ticket-${service}";

  # PID 1 of the container. Either member exiting takes the container down, and
  # the unit's Restart= brings it back: a runner whose gateway has died would
  # poll nothing and look idle, which is the failure worth being loud about.
  entrypoint = pkgs.writeShellScript "caos-runner" ''
    set -euo pipefail
    ${cfg.irohVhost}/bin/iroh-vhost pin \
      0.0.0.0:80=/tickets/server \
      0.0.0.0:5000=/tickets/registry \
      0.0.0.0:6379=/tickets/redis &
    gateway=$!
    ${cfg.runnerd}/bin/runnerd &
    runner=$!
    trap 'kill "$gateway" "$runner" 2>/dev/null; wait; exit 0' TERM INT
    status=0
    wait -n || status=$?
    echo "caos-runner: a member exited ($status); taking the container down" >&2
    exit 1
  '';

  image = pkgs.dockerTools.buildLayeredImage {
    name = "caos-runner";
    contents = [
      pkgs.bash
      pkgs.coreutils
      pkgs.cacert
      # runnerd starts workers with `docker run` on the host engine, through
      # the socket mounted below.
      (pkgs.docker-client.override {
        buildxSupport = false;
        composeSupport = false;
      })
    ];
    extraCommands = "mkdir -p tmp && chmod 1777 tmp";
    config = {
      Entrypoint = [ "${entrypoint}" ];
      Env = [
        "PATH=/bin"
        "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
        # The same values the stack gives its own runnerd on caosd-prod
        # (flake.nix, `caosd up`), because these names resolve to the same
        # services here.
        "CAOS_SERVER_URL=http://caos-server"
        "CAOS_WORKER_REDIS_ADDR=caos-redis:6379"
        "CAOS_DOCKER_NETWORK=caos-net"
        "CAOS_RUNNER_SOCKET=/var/run/docker.sock"
        "CAOS_RUNNER_SLOTS=${toString cfg.slots}"
      ];
    };
  };
  imageRef = "${image.imageName}:${image.imageTag}";

  start = pkgs.writeShellScript "caos-runner-start" ''
    set -euo pipefail
    if ! docker image inspect ${imageRef} >/dev/null 2>&1; then
      docker load < ${image}
    fi
    docker network inspect caos-net >/dev/null 2>&1 || docker network create caos-net >/dev/null
    docker rm -f caos-runner >/dev/null 2>&1 || true
    # Every worker on this engine is an orphan once the runner that started it
    # is gone: its runnerd's owner label names a container that no longer
    # exists, so no later runnerd reaps it. caosd up does the same, for the same
    # reason; here the whole engine is the runner's, so all of them go.
    mapfile -t orphans < <(docker ps -aq --filter label=caos.runnerd.owner)
    if [ "''${#orphans[@]}" -gt 0 ]; then
      echo "caos-runner: removing ''${#orphans[@]} orphaned worker(s)" >&2
      docker rm -f "''${orphans[@]}" >/dev/null
    fi
    exec docker run --name caos-runner \
      --network caos-net \
      --network-alias caos-server \
      --network-alias caos-registry \
      --network-alias caos-redis \
      -p 127.0.0.1:5000:5000 \
      -v /var/run/docker.sock:/var/run/docker.sock \
      ${lib.concatMapStringsSep " " (s: "-v ${secretFile s}:/tickets/${s}:ro") services} \
      ${imageRef}
  '';
in
{
  imports = [
    ../base.nix
    ../docker.nix
  ];

  options.caos.runner = {
    runnerd = lib.mkOption {
      type = lib.types.package;
      description = "The runnerd build (bin/runnerd). Supplied by the flake so the host runs THIS revision.";
    };
    irohVhost = lib.mkOption {
      type = lib.types.package;
      description = "The iroh-vhost build whose `pin` reaches the server's services.";
    };
    slots = lib.mkOption {
      type = lib.types.ints.positive;
      default = 8;
      description = "Concurrent workers on this host (CAOS_RUNNER_SLOTS); caosd-prod's own pool is 8.";
    };
  };

  config = {
    # The tickets, from `caosd runner-ticket <service>` on the server, put in
    # Secrets Manager by hand once (prod/README.md).
    caos.secrets = lib.listToAttrs (
      map (s: lib.nameValuePair "runner-ticket-${s}" { secretId = "caos/runner-ticket/${s}"; }) services
    );

    # EVERY RUNNER HOST IS NAMED runner-prod (one config), so the agent must
    # not report that name, or Datadog folds all of them into one host and
    # the per-host monitors see one. It reports the instance id instead,
    # written before it starts; `role:runner-prod` still groups them.
    services.datadog-agent.hostname = lib.mkForce null;
    services.datadog-agent.extraConfig.hostname_file = "/run/caos-instance-id";
    systemd.services.caos-instance-id = {
      description = "Write this instance's EC2 id for the Datadog agent's hostname";
      wantedBy = [ "multi-user.target" ];
      before = [ "datadog-agent.service" ];
      requiredBy = [ "datadog-agent.service" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = [ pkgs.curl ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        token=$(curl -sf -X PUT http://169.254.169.254/latest/api/token \
          -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
        curl -sf -H "X-aws-ec2-metadata-token: $token" \
          http://169.254.169.254/latest/meta-data/instance-id > /run/caos-instance-id
      '';
    };

    caos.datadog.units = [
      "caos-runner.service"
      "docker.service"
      "amazon-ssm-agent.service"
    ];

    systemd.services.caos-runner = {
      description = "caos runner: runnerd and its gateway to caosd-prod";
      wantedBy = [ "multi-user.target" ];
      after = [
        "docker.service"
        "caos-secrets.service"
        "network-online.target"
      ];
      requires = [
        "docker.service"
        "caos-secrets.service"
      ];
      wants = [ "network-online.target" ];
      path = [ config.virtualisation.docker.package ];
      serviceConfig = {
        ExecStart = start;
        ExecStop = "${config.virtualisation.docker.package}/bin/docker stop caos-runner";
        Restart = "always";
        RestartSec = 10;
        # The first start loads the image.
        TimeoutStartSec = "10min";
      };
      # A runner that cannot start is retried forever, as caosd is.
      unitConfig.StartLimitIntervalSec = 0;
    };

    users.users.ssm-user.extraGroups = [ "docker" ];
  };
}
