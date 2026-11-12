#!/usr/bin/env bash
# One job on the Firecracker test appliance (dev/fc-test), for
# dev/firecracker-check. It answers which machine ran it and leaves the answer
# as a FILE in a tree result, which must stay readable after the VM is gone.
#
#   boot=  the kernel's boot_id. Unique per VM boot, so it names the VM. (PID 1's
#          start time names a container, as in tests/resident, but every VM's
#          clock starts at zero, so in a VM it is the same number every time.)
#   kernel= the running kernel, which tells a VM from a container on the host
#   iso=   with --iso: what this job read back from /tmp/iso after writing its own
#          n there and sleeping. Another VM writing at the same time must not
#          change it.
#   --sleep=<s> holds the job open, for the crash and concurrency checks.
set -euo pipefail
arg() { caos get "/cas/args/$1" >/dev/null; cat "/cas/args/$1"; }
n=$(arg n)
if [ -e /cas/args/iso ]; then printf '%s' "$n" > /tmp/iso; fi
if [ -e /cas/args/sleep ]; then sleep "$(arg sleep)"; fi
iso=""
if [ -e /cas/args/iso ]; then iso=$(cat /tmp/iso); fi
mkdir -p /tmp/out
printf 'n=%s boot=%s kernel=%s iso=%s' "$n" "$(cat /proc/sys/kernel/random/boot_id)" \
  "$(cat /proc/sys/kernel/osrelease)" "$iso" > /tmp/out/answer
caos put /tmp/out /cas/out
