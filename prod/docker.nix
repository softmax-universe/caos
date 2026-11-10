# Docker as every caos host that runs workers has it: caosd-prod (whose stack
# also runs a runner) and the prod/runner hosts. Not in base.nix, because the
# watchdog must not run docker (base.nix says why). A role adds where the
# engine keeps its data.

{ pkgs, ... }:

{
  # Container-level metrics (restarts, OOM kills, per-container CPU/memory)
  # for the runner pool. The agent reads the socket, so it joins the group.
  services.datadog-agent.checks.docker = {
    init_config = { };
    instances = [ { url = "unix://var/run/docker.sock"; } ];
  };
  users.users.datadog.extraGroups = [ "docker" ];

  virtualisation.docker = {
    enable = true;
    # The `docker` alias still resolves to 28.5.2, which nixpkgs marks
    # unmaintained; an unpinned config refuses to evaluate at all.
    package = pkgs.docker_29;
    # Docker's built-in pool is 172.17.0.0/12 in /16s, spanning
    # 172.16-172.31 -- so its 16th network is 172.31.0.0/16, exactly the
    # CIDR of every EC2 *default* VPC. caos's runner pool can allocate that
    # many. Move the pool somewhere nothing routes, and exhaustion then
    # fails a network create loudly instead of eating the VPC route.
    daemon.settings.default-address-pools = [
      {
        base = "10.200.0.0/14";
        size = 24;
      }
    ];
  };
}
