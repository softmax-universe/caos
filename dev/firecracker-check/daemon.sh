#!/usr/bin/env bash
# A RESIDENT worker for dev/firecracker-check (dev/fc-test declares
# CAOS_RESIDENT=1): one message per job, all in ONE process, each reply naming
# the VM (boot_id), the process (token) and the job count. `stop` is answered
# and ends the daemon, so its VM shuts down.
set -euo pipefail
boot=$(cat /proc/sys/kernel/random/boot_id)
token="$$-$RANDOM-$(date +%s%N)"
n=0
while true; do
  caos get /cas/args/in >/dev/null
  n=$((n + 1))
  printf 'in=%s boot=%s token=%s n=%s' "$(cat /cas/args/in)" "$boot" "$token" "$n" > /tmp/out
  caos put /tmp/out /cas/out
  if [ "$(cat /cas/args/in)" = stop ]; then exit 0; fi
  if caos next; then continue; fi
  rc=$?
  if [ "$rc" -eq 10 ]; then exit 0; fi
  exit "$rc"
done
