# dev/fc-test's WORKLOAD: the one definition its Docker image (./flake.nix) and
# its Firecracker appliance (the root flake's `caos-fc-test-appliance`, via
# nix/firecracker-appliance.nix) are both built from. That is what keeps the
# appliance's worker and image env identical to the base a job names
# (design/firecracker-runners.md, "Workload selection").
#
# A script interpreter like std/bash, on an image that is RESIDENT and stays
# WARM, so one appliance covers the acceptance checks: independent jobs on a
# warm VM, and successive resident messages. The knobs are short for a test,
# as in dev/resident-test.
{ pkgs }:
{
  contents = [
    pkgs.bash
    pkgs.coreutils
    pkgs.gnugrep
  ];
  worker = ./worker;
  env = [
    "PATH=/bin"
    "CAOS_RESIDENT=1"
    "CAOS_RESIDENT_POLL_MS=2000"
    "CAOS_RESIDENT_GRACE_SECS=2"
    "CAOS_RESIDENT_MAX_SECS=600"
    "CAOS_RUNNER_TTL_MS=30000"
  ];
}
