#!/usr/bin/env bash
# One job on dev/warm-test: answer which container ran it. The runner is
# PID 1, so its start time names the container (tests/resident says why
# /etc/hostname cannot).
set -euo pipefail
caos get /cas/args/n
read -ra stat < /proc/1/stat
printf 'n=%s host=%s' "$(cat /cas/args/n)" "${stat[21]}" > /tmp/out
caos put /tmp/out /cas/out
