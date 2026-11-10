# Brief: does missing TRIM through LUKS make this machine's writes slow?

You are investigating the storage stack of the host where caos's test suite
runs. The question is whether TRIM (discard) reaches the SSD, and if not,
whether enabling it would cut the write and fsync latency we measured. You are
to **diagnose and propose**. Do not change any host configuration yourself:
every fix here needs a reboot, a LUKS passphrase or a security decision, and
those belong to the owner.

## Why we are asking

caos's test suite (`result/bin/caos-cli run-tool caos-test` from
`~/Projects/caos0`) runs about 1,900 short-lived containers per run, each
creating and deleting files. Under that load the disk was the bottleneck:

- `nvme0n1` averaged **27 ms per write** at 146 MB/s and about 3,500
  writes/s. A healthy SN850X should be well under 1 ms.
- A 300 KB write + fsync + rename on the btrfs (`design/suite-speed/probes/fsyncprobe.py`)
  took a **6 ms median, 4.6 s p90 and 20.8 s max** under load.
- After code changes that removed most fsyncs, 5-second windows still showed
  a median of 8 ms per write, a p90 of 30 ms and a max of 168 ms.

The full investigation is in `design/suite-speed/NOTES.md`. That has its own
leads. In particular, the caos server calls `syncfs` on every push, which
flushes the whole filesystem. **That is not your task**, but it means caos's
own load is not a neutral benchmark. Use the synthetic tests below for
before/after comparisons, and treat suite numbers as secondary.

## What is already known

Read from inside a dev container, so verify on the host:

| Layer | Observation |
|---|---|
| SSD | `nvme0n1`, WD_BLACK SN850X 1000GB, `queue/discard_max_bytes` = 2199023255040 (supports discard) |
| Partition | `nvme0n1p2` |
| LUKS | `/dev/mapper/crypted` = `dm-0`, **`queue/discard_max_bytes` = 0: discards are not passed through** |
| Filesystem | btrfs `c1d0d350-bfbd-4434-b8f0-0c874bc5f4c8`, mounted `rw,relatime,ssd,space_cache=v2` (no `discard`) |
| Space | 931 GB, 728 GB used (84%). btrfs data 844 GB allocated / 703 GB used; metadata 43 GB / 12 GB |
| Who uses it | `$HOME`, `/tmp`, `/var`, the podman volumes (`~/.local/share/containers/storage/volumes/caos-vol-*`), and so all container layers |

The theory: with discard blocked at dm-crypt and no working `fstrim`, the SSD
is never told which blocks are free. After heavy churn it treats nearly all of
its NAND as live. Its dynamic SLC cache shrinks and its garbage collection gets
expensive, so write latency rises. This theory is **unproven**. Your job is to
confirm or refute it.

## Questions to answer

1. **Where exactly is discard blocked?** Check each layer on the host:
   - `lsblk --discard` (DISC-GRAN / DISC-MAX per layer).
   - `sudo dmsetup table crypted`: look for `allow_discards` in the crypt
     target's optional parameters.
   - `sudo cryptsetup status crypted`: look for `flags: discards`.
   - How the volume is opened. The `crypted` name and the nix tooling suggest
     NixOS. If so, check `boot.initrd.luks.devices.crypted.allowDiscards` in the
     system config (`/etc/nixos` or the flake it comes from). Otherwise check
     `/etc/crypttab` for `discard`, or the kernel cmdline (`/proc/cmdline`) for
     `rd.luks.options=discard`.
   - The btrfs mount options for `discard` or `discard=async`.
2. **Is any TRIM happening at all?**
   - `systemctl status fstrim.timer`, `systemctl list-timers | grep -i trim`,
     `journalctl -u fstrim`. On NixOS, `services.fstrim.enable`.
   - `sudo fstrim -v /` is harmless to run. With discards blocked it should
     fail or trim 0 bytes, which is itself evidence. Record the output.
3. **What does the drive itself report?** `sudo nvme smart-log /dev/nvme0`
   (from `nixpkgs#nvme-cli`): percentage used, data units written, available
   spare, temperature, and any critical warnings. Thermal throttling would also
   produce slow writes, so record the temperature under load too.
4. **Does write latency match the theory?** Measure on a file in the btrfs,
   never on a raw device:
   - `fsyncprobe.py`, idle: `nix run nixpkgs#python3 --
     design/suite-speed/probes/fsyncprobe.py <dir on the btrfs> 50`.
   - A sustained-write test that would exhaust a small SLC cache, for example
     `nix run nixpkgs#fio -- --name=sustained --directory=<dir> --size=20G
     --rw=write --bs=1M --ioengine=libaio --direct=1 --runtime=120 --time_based`,
     watching throughput over time. A cliff after a few GB points to SLC cache
     exhaustion. Then
     `fio --name=fsync --directory=<dir> --size=1G --rw=randwrite --bs=4k
     --fsync=1 --runtime=60 --time_based` for fsync latency percentiles.
   - Make sure about 25 GB is free before the sustained test, and delete the
     fio files afterwards.
5. **Propose the change and its trade-off.** Typically:
   - Allow discards on the LUKS mapping (NixOS: `allowDiscards = true`, applied
     at the next boot; or once, live, with
     `sudo cryptsetup refresh --allow-discards --persistent crypted`, which
     asks for the passphrase).
   - Then either `services.fstrim.enable = true` (weekly) or btrfs
     `discard=async`. Recommend one and say why. For this churn-heavy
     workload, `discard=async` is probably reasonable, but justify it.
   - State the security trade-off plainly: with TRIM enabled, someone holding
     the raw disk can see which encrypted blocks are unused, which reveals usage
     patterns and the filesystem type, but no contents. The owner decides
     whether that matters.
6. **If the owner approves and applies it**, run one `sudo fstrim -v /`
   (expect it to trim on the order of 100+ GB the first time), then repeat
   step 4's measurements, idle and under load, and compare.

## Constraints

- Do not change crypttab, NixOS config, mount options or LUKS flags yourself.
  Present the exact change and let the owner apply it.
- Do not run benchmarks against `/dev/nvme0n1*` or `/dev/mapper/crypted`
  directly: file-level tests only. No `btrfs balance`, no `nvme format`, nothing
  that writes to the device outside the filesystem.
- Do not fill the disk: check free space before any sustained write test.
- Benchmarks are only comparable on a quiet machine. Note whether the caos stack
  or a suite run was active (`docker ps` shows `caos-stack` and `caos-worker-*`
  containers; here `docker` is rootless podman).
- Many of these commands need root on the host. If you are running inside the
  caos dev container, `/sys` is readable but `dmsetup` and `cryptsetup` are not
  available: ask the owner for a host shell, or for the outputs.

## What to hand back

A short report with:
- each layer's discard state, with the command output as evidence
- the fstrim situation
- the SMART summary
- the idle and loaded latency numbers
- the proposed change, including the exact config lines and the security trade-off

If a fix was applied, add before/after numbers from the same tests. Say plainly
what you could not verify.
