#!/bin/busybox sh
# The initrd /init of a Firecracker runner appliance
# (nix/firecracker-appliance.nix). Loads the modules the stock kernel keeps
# out of its image, brings up eth0 from the kernel command line, mounts the
# VM's private root disk, and hands PID 1 to `caos vm-init`.
#
# Kernel command line (runnerd's Firecracker backend sets these):
#   caos.ip=<addr>/<prefix>  this guest's address on its tap link
#   caos.gw=<addr>           the host's end of the link
#   caos.dns=<addr>          optional resolver, written to /etc/resolv.conf
#
# Nothing secret is on the command line: /proc/cmdline is readable by the
# worker. The job and its credentials come from the config disk, read by
# `caos vm-init`.
#
# Any failure here powers the VM off (reboot=k makes Firecracker exit) without
# the readiness line, which the host reports as a boot failure.
set -eu
b=/bin/busybox
die() { echo "appliance-init: $*"; $b sync; $b reboot -f; }

$b mkdir -p /proc /sys /dev /newroot
$b mount -t proc proc /proc
$b mount -t sysfs sysfs /sys
$b mount -t devtmpfs dev /dev

for m in virtio_mmio virtio_blk virtio_net ext4; do
  $b modprobe "$m" || die "loading $m"
done

ip="" gw="" dns=""
for word in $($b cat /proc/cmdline); do
  case "$word" in
    caos.ip=*) ip=${word#caos.ip=} ;;
    caos.gw=*) gw=${word#caos.gw=} ;;
    caos.dns=*) dns=${word#caos.dns=} ;;
  esac
done

# The root disk can take a moment to appear after its driver loads.
for _ in $($b seq 1 50); do
  if [ -b /dev/vda ]; then break; fi
  $b sleep 0.1
done
[ -b /dev/vda ] || die "no root disk /dev/vda"
$b mount -t ext4 /dev/vda /newroot || die "mounting /dev/vda"

$b ip link set lo up
if [ -n "$ip" ]; then
  $b ip link set eth0 up || die "no eth0"
  $b ip addr add "$ip" dev eth0 || die "addressing eth0"
  if [ -n "$gw" ]; then
    $b ip route add default via "$gw" || die "routing via $gw"
  fi
fi
if [ -n "$dns" ]; then
  echo "nameserver $dns" > /newroot/etc/resolv.conf
fi

# vm-init mounts its own /proc, /sys and /dev in the new root.
$b umount /proc /sys
$b umount /dev || true
exec $b switch_root /newroot /bin/caos vm-init
