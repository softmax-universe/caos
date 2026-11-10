#!/bin/bash
# usage: podlat.sh <dir> <container> <image>; time a trivial podman run every 5s inside <container>
d=$1 c=$2 img=$3
while [ ! -e "$d/stop" ]; do
  r=$(docker exec $c bash -c "P='podman --root /caos-images/store --runroot /caos-images/run'; s=\$(date +%s%N); \$P run --rm --network=host --entrypoint /bin/bash $img -c true; echo \$(( (\$(date +%s%N) - s)/1000000 ))" 2>&1 | tail -1)
  echo "$(date +%T) $r" >> $d/podlat.log
  sleep 5
done
