#!/bin/bash
# What a worker sees of its box, printed so two runtimes can be diffed:
#
#   caos-cli run --base:@=std/bash --worker1:@=dev/firecracker/probe.sh \
#     --required-pool=firecracker --probe=$(date +%s%N)
#
# Exits 3 on purpose, so the run also shows how each runtime reports a failure.
echo "kernel: $(cat /proc/version | cut -d' ' -f1-3)"
echo "pid1: $(tr '\0' ' ' < /proc/1/cmdline)"
echo "hostname: $(cat /etc/hostname) uid: $(id -u)"
echo "server: $CAOS_SERVER_URL"
echo "mounts: $(cut -d' ' -f2,3 /proc/mounts | tr '\n' ';')"
echo "write-root: $(echo ok > /probe-file && cat /probe-file)"
echo "dns+egress: $(timeout 5 bash -c 'exec 3<>/dev/tcp/github.com/443 && echo reachable' 2>&1)"
echo "devfd: $(cat <(echo procsubst-ok))"
exit 3
