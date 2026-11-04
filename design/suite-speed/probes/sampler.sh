#!/bin/bash
# usage: sampler.sh <dir> ; runs until <dir>/stop exists
d=$1
while [ ! -e "$d/stop" ]; do
  { echo "=== $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"; top -bcn1 -w 230 | sed -n '3p;8,30p'; } >> "$d/topc.log"
  sleep 20
done
