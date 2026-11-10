#!/bin/bash
# usage: disklat.sh <dir>: every 5s, nvme write MB/s, writes/s, avg ms per write
d=$1
p=$(awk '$3=="nvme0n1"{print $10, $8, $11}' /proc/diskstats)
while [ ! -e "$d/stop" ]; do
  sleep 5
  c=$(awk '$3=="nvme0n1"{print $10, $8, $11}' /proc/diskstats)
  echo "$(date +%T) $p $c" | awk '{printf "%s %.0fMB/s %dw/s %.1fms\n", $1, ($5-$2)*512/1e6/5, ($6-$3)/5, ($7-$4)/(($6-$3)+1)}' >> $d/disk.log
  p=$c
done
