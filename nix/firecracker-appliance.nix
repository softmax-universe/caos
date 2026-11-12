# A Firecracker runner APPLIANCE (design/firecracker-runners.md): the kernel,
# initrd and root disk a runnerd Firecracker pool boots, one VM per job slot.
#
# `workload` is the same attrset its Docker base is built from
# (dev/fc-test/workload.nix): `contents` (packages for the userland), `worker`
# (the /worker program) and `env` (the image config env, `KEY=VALUE`). One
# definition for both is what keeps an appliance from disagreeing with the base
# it claims: the image env the runner reads (CAOS_RESIDENT, CAOS_RUNNER_TTL_MS,
# the worker uid, …) is baked into /etc/caos/image-env from the same list the
# Docker image config gets.
#
# Grants (CAOS_GRANT_*) are refused here, at build time: none of them has a VM
# translation, and a workload that declares one must not quietly run without it.
#
# Output:
#   vmlinux         the kernel (nixpkgs' stock kernel; Firecracker boots the ELF)
#   initrd          busybox + the virtio/ext4 modules + ./appliance-init.sh
#   rootfs.ext4     the root disk; runnerd boots a private COPY per VM
#   appliance.json  what runnerd checks at startup: the files' sha256 digests
#                   and the image env
{
  pkgs,
  # The static `caos` binary for this world (crates/caos-world): the guest's
  # runner and PID 1 (`caos vm-init`). It must be the world of the server the
  # pool serves.
  caos,
  name,
  workload,
  # Root disk size. /cas and /tmp live on it, so it bounds what one job can
  # materialize. Sparse: unused space costs nothing in the store or the copy.
  diskMiB ? 2048,
}:
let
  lib = pkgs.lib;
  kernel = pkgs.linuxPackages.kernel;
  grants = builtins.filter (e: lib.hasPrefix "CAOS_GRANT_" e) workload.env;

  # Built-ins in the stock kernel config are not enough: virtio and ext4 are
  # modules, so the initrd loads them. Decompressed at build time, so busybox's
  # modprobe loads them without retrying.
  moduleClosure = pkgs.makeModulesClosure {
    kernel = kernel.modules;
    firmware = pkgs.runCommand "no-firmware" { } "mkdir -p $out/lib/firmware";
    rootModules = [
      "virtio_mmio"
      "virtio_blk"
      "virtio_net"
      "ext4"
    ];
  };
  modules = pkgs.runCommand "${name}-modules" { nativeBuildInputs = [ pkgs.kmod pkgs.xz ]; } ''
    mkdir -p $out/lib
    cp -RL ${moduleClosure}/lib/modules $out/lib/
    chmod -R u+w $out
    find $out -name '*.ko.xz' -exec xz -d {} +
    depmod -b $out ${kernel.modDirVersion}
  '';

  initrd = pkgs.makeInitrdNG {
    name = "${name}-initrd";
    compressor = "gzip";
    contents = [
      {
        source = ./appliance-init.sh;
        target = "/init";
      }
      {
        source = "${pkgs.pkgsStatic.busybox}/bin/busybox";
        target = "/bin/busybox";
      }
      {
        source = "${modules}/lib/modules";
        target = "/lib/modules";
      }
    ];
  };

  userland = pkgs.buildEnv {
    name = "${name}-userland";
    paths = workload.contents;
    pathsToLink = [ "/bin" "/lib" "/share" "/etc" ];
  };

  rootfs = pkgs.runCommand "${name}-rootfs.ext4"
    {
      nativeBuildInputs = [ pkgs.e2fsprogs pkgs.fakeroot ];
      closure = pkgs.closureInfo { rootPaths = [ userland workload.worker ]; };
    }
    ''
      root=$PWD/root
      mkdir -p $root/nix/store $root/etc/caos $root/usr/bin $root/cas $root/proc \
        $root/sys $root/dev $root/run $root/tmp $root/var/tmp
      for p in $(cat $closure/store-paths); do cp -a "$p" $root/nix/store/; done
      # The userland's top level as real directories of links into the store,
      # the way dockerTools lays out `contents`.
      for d in bin lib share etc; do
        if [ -d ${userland}/$d ]; then
          mkdir -p $root/$d
          cp -a ${userland}/$d/. $root/$d/
        fi
      done
      chmod -R u+w $root/etc $root/bin
      # The caos additions build-builtins.sh layers onto every Docker image.
      # 755 here: the sandbox refuses setuid, and fakeroot records it below.
      install -m 755 ${caos}/bin/caos $root/bin/caos
      ln -sf /bin/env $root/usr/bin/env
      printf 'root:x:0:0:root:/root:/sbin/nologin\nworker:x:1000:1000:caos worker:/tmp:/sbin/nologin\n' > $root/etc/passwd
      printf 'root:x:0:\nworker:x:1000:\n' > $root/etc/group
      install -m 755 ${workload.worker} $root/worker
      printf '%s\n' ${lib.escapeShellArgs workload.env} > $root/etc/caos/image-env
      # Everything root-owned (the build runs as an unprivileged user, and the
      # worker must not own its own binaries), /tmp sticky, /bin/caos setuid.
      fakeroot sh -c "
        chown -R 0:0 $root
        chmod 1777 $root/tmp $root/var/tmp
        chmod 4755 $root/bin/caos
        mkfs.ext4 -q -L caos-root -d $root -E root_owner=0:0 -T default $out ${toString diskMiB}M
      "
    '';
in
assert lib.assertMsg (grants == [ ])
  "${name}: a Firecracker appliance cannot honor ${toString grants}; none has a VM translation";
pkgs.runCommand name { } ''
  mkdir -p $out
  ln -s ${kernel.dev}/vmlinux $out/vmlinux
  ln -s ${initrd}/initrd $out/initrd
  ln -s ${rootfs} $out/rootfs.ext4
  sum() { sha256sum "$1" | cut -d' ' -f1; }
  cat > $out/appliance.json <<JSON
  {
    "name": "${name}",
    "vmlinux": "$(sum ${kernel.dev}/vmlinux)",
    "initrd": "$(sum ${initrd}/initrd)",
    "rootfs": "$(sum ${rootfs})",
    "image_env": ${builtins.toJSON workload.env}
  }
  JSON
''
