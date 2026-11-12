//! The Firecracker runtime: the same job, run in a microVM instead of a
//! container (`CAOS_RUNTIME=firecracker`).
//!
//! What runs is IDENTICAL to the docker path — the job's OCI image, entered at
//! `/bin/caos runner --job=<json>` with the image's own config env — so the two
//! runtimes can be benchmarked against each other and work can move between
//! them. Only the box around it differs:
//!
//! ```text
//! image_ref ──docker create/export──▶ rootfs.tar ──+ caos-fc-init──▶ mkfs.ext4 -d
//!                                                      (cached per image digest)
//! per job:  vda = that ext4, READ-ONLY and shared by every VM of the image
//!           vdb = a fresh sparse scratch ext4 (the container's writable layer)
//!           vdc = the job: argv, env, cwd, hostname, dns (JSON, NUL-padded)
//! ```
//!
//! `caos-fc-init` (`src/bin/caos-fc-init.rs`) is PID 1 in the guest. It builds
//! an overlayfs root from vda + vdb — the same copy-on-write shape as the
//! engine's overlay driver — mounts what an engine would (`/proc`, `/sys`,
//! devpts, `/dev/shm`), brings `lo` up, writes `/etc/hosts`, `/etc/resolv.conf`
//! and `/etc/hostname`, runs the job's argv, and prints [`EXIT_MARKER`] with the
//! exit status on the console before rebooting, which ends the firecracker
//! process. That marker is this runtime's "container exit status": a VM that
//! ends without one (kernel panic, firecracker error) is a failure, exactly like
//! a container dying before it reported.
//!
//! Networking is per SLOT: slot `n` attaches the pre-created tap
//! `<CAOS_FC_TAP_PREFIX><n>` and its guest takes `CAOS_FC_HOST_IP + 1 + n`. The
//! taps, the bridge and NAT are host setup that needs root, done once by
//! `dev/firecracker/setup-host`, never by runnerd — a runner that can create
//! network devices is a runner that can reconfigure the host.
//!
//! NOT supported, and refused by name rather than run degraded: images that
//! declare an engine-socket, volume or device grant. A VM has no socket to bind
//! in, volumes would be persistent disks, and the guest kernel has no FUSE. A
//! job on such an image would fail far away from the cause; this fails here.
//! `CAOS_GRANT_SYS_ADMIN` needs nothing: the guest's root is already root of
//! its own kernel.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::{image_grants, Config, Job, CAOS_BIN};

/// Printed by `caos-fc-init` on the console, followed by the decimal exit
/// status of the job's process, as the last thing before the guest reboots.
/// Shared with `src/bin/caos-fc-init.rs`, which spells it the same way.
pub const EXIT_MARKER: &str = "caos-fc-init: exit=";

/// Where `caos-fc-init` sits in every converted rootfs, and the empty directory
/// it mounts the scratch disk on. Both are added to the image's tarball at
/// conversion; nothing in an image may already use these names.
const INIT_PATH: &str = "caos-fc-init";
const STAGING_DIR: &str = ".caos-fc";

/// Bump when the conversion's OUTPUT changes shape (a new file injected, a
/// different mkfs layout), so cached rootfs images from an older runnerd are
/// rebuilt instead of reused. A changed INIT needs no bump: its hash is in the
/// cache key (`FcConfig::init_id`), because a rootfs carries its own copy of
/// the init and a rebuilt one would otherwise never reach a cached image.
const CONVERSION_VERSION: u32 = 1;

pub struct FcConfig {
    bin: String,
    kernel: PathBuf,
    init: PathBuf,
    /// Short content hash of `init`, part of every rootfs cache key.
    init_id: String,
    mkfs: String,
    state: PathBuf,
    /// This runnerd's own VM directory under `state/vms`, so concurrent
    /// runnerds sharing one state dir never touch each other's VMs.
    vms: PathBuf,
    host_ip: Ipv4Addr,
    prefix_len: u8,
    tap_prefix: String,
    vcpus: u32,
    mem_mib: u32,
    scratch_mib: u64,
    dns: Vec<String>,
    /// Address a worker may use for its own caching. Separate from the docker
    /// runtime's on purpose: that one names a host on the engine's network,
    /// which a VM cannot resolve. Unset offers none, which a worker must
    /// already tolerate.
    worker_redis_addr: Option<String>,
    /// Single-flight per image ref: a fan-out of jobs for one fresh image must
    /// convert it once, not once per slot (AGENTS.md, "check-then-act").
    converting: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

impl FcConfig {
    /// Read the runtime's configuration and check the host is ready for it.
    /// Every problem found here would otherwise surface as a failed JOB, far
    /// from its cause, so this refuses to start instead.
    pub fn from_env(slots: u32) -> Result<FcConfig, String> {
        let kernel = PathBuf::from(
            env_opt("CAOS_FC_KERNEL")
                .ok_or("CAOS_FC_KERNEL must name an uncompressed guest vmlinux")?,
        );
        if !kernel.is_file() {
            return Err(format!("CAOS_FC_KERNEL {} is not a file", kernel.display()));
        }
        let init = match env_opt("CAOS_FC_INIT") {
            Some(p) => PathBuf::from(p),
            None => std::env::current_exe()
                .map_err(|e| format!("locating runnerd: {e}"))?
                .with_file_name("caos-fc-init"),
        };
        if !init.is_file() {
            return Err(format!(
                "guest init {} not found (set CAOS_FC_INIT)",
                init.display()
            ));
        }
        let init_id = {
            use sha1::{Digest, Sha1};
            let bytes =
                std::fs::read(&init).map_err(|e| format!("reading {}: {e}", init.display()))?;
            Sha1::digest(&bytes)
                .iter()
                .take(6)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let state = PathBuf::from(env_opt("CAOS_FC_STATE").unwrap_or_else(|| {
            let base = env_opt("XDG_CACHE_HOME")
                .or_else(|| env_opt("HOME").map(|h| format!("{h}/.cache")))
                .unwrap_or_else(|| "/var/tmp".to_string());
            format!("{base}/caos-fc")
        }));
        let host_ip: Ipv4Addr = env_opt("CAOS_FC_HOST_IP")
            .unwrap_or_else(|| "10.231.0.1".to_string())
            .parse()
            .map_err(|e| format!("CAOS_FC_HOST_IP: {e}"))?;
        let prefix_len: u8 = env_opt("CAOS_FC_PREFIX_LEN")
            .unwrap_or_else(|| "24".to_string())
            .parse()
            .map_err(|e| format!("CAOS_FC_PREFIX_LEN: {e}"))?;
        let tap_prefix = env_opt("CAOS_FC_TAP_PREFIX").unwrap_or_else(|| "caosfc-tap".to_string());
        let parse_num = |key: &str, default: u64| -> Result<u64, String> {
            env_opt(key)
                .map(|v| v.parse::<u64>().map_err(|e| format!("{key}: {e}")))
                .unwrap_or(Ok(default))
        };
        let dns = match env_opt("CAOS_FC_DNS") {
            Some(list) => list
                .split([' ', ','])
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            None => host_nameservers(),
        };
        if dns.is_empty() {
            return Err("no non-loopback nameserver found on this host; set CAOS_FC_DNS".into());
        }
        // Every slot's guest address has to fit in the subnet, and every slot
        // needs its tap. Checked for ALL slots now, not when the last slot first
        // gets a job an hour into a run.
        for slot in 0..slots {
            guest_ip(host_ip, prefix_len, slot)?;
            let tap = format!("{tap_prefix}{slot}");
            if !Path::new("/sys/class/net").join(&tap).exists() {
                return Err(format!(
                    "tap {tap} does not exist; run dev/firecracker/setup-host \
                     (CAOS_RUNNER_SLOTS={slots} needs {tap_prefix}0..{})",
                    slots - 1
                ));
            }
        }
        let owner = format!("{}-{}", super::owner_id(), std::process::id());
        let vms = state.join("vms").join(&owner);
        std::fs::create_dir_all(&vms).map_err(|e| format!("creating {}: {e}", vms.display()))?;
        std::fs::create_dir_all(state.join("images"))
            .map_err(|e| format!("creating {}/images: {e}", state.display()))?;
        let config = FcConfig {
            bin: env_opt("CAOS_FC_BIN").unwrap_or_else(|| "firecracker".to_string()),
            mkfs: env_opt("CAOS_FC_MKFS").unwrap_or_else(|| "mkfs.ext4".to_string()),
            kernel,
            init,
            init_id,
            vms,
            host_ip,
            prefix_len,
            tap_prefix,
            vcpus: parse_num("CAOS_FC_VCPUS", 2)? as u32,
            mem_mib: parse_num("CAOS_FC_MEM_MIB", 2048)? as u32,
            scratch_mib: parse_num("CAOS_FC_SCRATCH_MIB", 16384)?,
            dns,
            worker_redis_addr: env_opt("CAOS_FC_WORKER_REDIS_ADDR"),
            converting: Mutex::new(HashMap::new()),
            state,
        };
        reap_dead_vm_dirs(&config.state.join("vms"));
        Ok(config)
    }

    pub fn host_ip(&self) -> Ipv4Addr {
        self.host_ip
    }

    pub fn describe(&self) -> String {
        format!(
            "firecracker {} vcpu/{} MiB, kernel {}, state {}, taps {}N, guests {}+1+N, dns {:?}",
            self.vcpus,
            self.mem_mib,
            self.kernel.display(),
            self.state.display(),
            self.tap_prefix,
            self.host_ip,
            self.dns
        )
    }
}

/// The nameservers a guest can use: the host's, minus loopback ones, which in a
/// guest are the guest. Prefers systemd-resolved's UPSTREAM list, because with
/// resolved the ordinary resolv.conf names only its 127.0.0.53 stub.
fn host_nameservers() -> Vec<String> {
    for path in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let found = parse_nameservers(&text);
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

fn parse_nameservers(resolv: &str) -> Vec<String> {
    resolv
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .map(str::trim)
        .filter(|ns| match ns.parse::<std::net::IpAddr>() {
            Ok(ip) => !ip.is_loopback(),
            Err(_) => false,
        })
        .map(str::to_string)
        .collect()
}

/// Slot `n`'s guest address: the host's address plus `1 + n`, which must stay
/// inside the host's subnet and short of its broadcast address.
pub fn guest_ip(host: Ipv4Addr, prefix_len: u8, slot: u32) -> Result<Ipv4Addr, String> {
    if !(8..=30).contains(&prefix_len) {
        return Err(format!(
            "prefix length /{prefix_len} is not usable for guests"
        ));
    }
    let mask = u32::MAX << (32 - prefix_len);
    let host_n = u32::from(host);
    let ip = host_n
        .checked_add(1 + slot)
        .ok_or("guest address overflows")?;
    let broadcast = (host_n & mask) | !mask;
    if ip & mask != host_n & mask || ip >= broadcast {
        return Err(format!(
            "slot {slot}'s guest address falls outside {host}/{prefix_len}"
        ));
    }
    Ok(Ipv4Addr::from(ip))
}

fn netmask(prefix_len: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::MAX << (32 - prefix_len))
}

/// The server URL a GUEST should dial. A loopback host in runnerd's own URL is
/// runnerd's host — which, from the guest, is the bridge address — so that is
/// rewritten; anything else is used as given.
pub fn guest_server_url(url: &str, host_ip: Ipv4Addr) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (hostport, None),
    };
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return url.to_string();
    }
    match port {
        Some(p) => format!("{scheme}://{host_ip}:{p}{path}"),
        None => format!("{scheme}://{host_ip}{path}"),
    }
}

/// Remove VM directories left by runnerds that are no longer running. A VM dir
/// is named `<owner>-<pid>`; its firecrackers die with their runnerd
/// (`PR_SET_PDEATHSIG`), so a dir whose pid is gone holds nothing live.
fn reap_dead_vm_dirs(vms_root: &Path) {
    let me = format!("{}-", super::owner_id());
    let Ok(entries) = std::fs::read_dir(vms_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(&me) else {
            continue; // another host's runnerd sharing this state dir
        };
        if pid == std::process::id().to_string() || Path::new("/proc").join(pid).exists() {
            continue;
        }
        eprintln!(
            "caos-runnerd: removing leftover VM dir {}",
            entry.path().display()
        );
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// A converted image: its rootfs, and the config an engine would have applied.
struct Prepared {
    rootfs: PathBuf,
    env: Vec<String>,
    cwd: String,
    user: Option<(u32, u32)>,
}

/// Run one job in a microVM and wait it out. Same contract as the docker path's
/// `run_container`: `None` when the guest's process exited 0, otherwise the
/// error and log the crash backstop posts.
pub fn run(config: &Config, fc: &FcConfig, slot: u32, job: &Job) -> Option<(String, String)> {
    eprintln!(
        "runnerd slot {slot}: arg_tree {} -> firecracker ({})",
        job.arg_tree, job.image_ref
    );
    let started = Instant::now();
    let prepared = match prepare_image(config, fc, &job.image_ref) {
        Ok(p) => p,
        Err(e) => {
            return Some((
                format!("firecracker: preparing {}: {e}", job.image_ref),
                String::new(),
            ))
        }
    };
    let prepared_ms = started.elapsed().as_millis();
    let vm_dir = fc.vms.join(&job.nonce);
    let mut split = Vec::new();
    let result = boot(config, fc, slot, job, &prepared, &vm_dir, &mut split);
    let _ = std::fs::remove_dir_all(&vm_dir);
    // One line per VM with the split a benchmark wants: conversion (a cache
    // hit after an image's first job), host-side setup, the guest's own boot
    // and job times, and the VM's whole life. The job's time includes a warm
    // runner's idle poll before it gives up (CAOS_RUNNER_TTL_MS) — the same
    // wait a container pays.
    let split: String = split.iter().map(|(k, v)| format!(" {k}={v}")).collect();
    eprintln!(
        "runnerd slot {slot}: arg_tree {}: firecracker prepare_ms={prepared_ms}{split} total_ms={}",
        job.arg_tree,
        started.elapsed().as_millis() - prepared_ms
    );
    result
}

fn boot(
    config: &Config,
    fc: &FcConfig,
    slot: u32,
    job: &Job,
    image: &Prepared,
    dir: &Path,
    split: &mut Vec<(String, String)>,
) -> Option<(String, String)> {
    let setup = Instant::now();
    let fail = |e: String| Some((format!("firecracker: {e}"), String::new()));
    if let Err(e) = std::fs::create_dir_all(dir) {
        return fail(format!("creating {}: {e}", dir.display()));
    }
    let scratch = dir.join("scratch.ext4");
    if let Err(e) = scratch_disk(fc, &scratch) {
        return fail(e);
    }
    let guest = match guest_ip(fc.host_ip, fc.prefix_len, slot) {
        Ok(ip) => ip,
        Err(e) => return fail(e),
    };
    let hostname = format!("caos-worker-{}", short(&job.nonce));
    let mut env = image.env.clone();
    let mut set = |kv: String| {
        let key = kv.split('=').next().unwrap_or_default().to_string();
        env.retain(|e| e.split('=').next() != Some(key.as_str()));
        env.push(kv);
    };
    // What `docker run` adds on top of the image's config env, in the order it
    // applies them: its defaults first, then the `-e` flags.
    if !image.env.iter().any(|e| e.starts_with("PATH=")) {
        set("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    }
    if !image.env.iter().any(|e| e.starts_with("HOME=")) {
        set("HOME=/root".into());
    }
    set(format!("HOSTNAME={hostname}"));
    if let Some(addr) = &fc.worker_redis_addr {
        set(format!("CAOS_WORKER_REDIS_ADDR={addr}"));
    }
    set(format!(
        "CAOS_SERVER_URL={}",
        guest_server_url(&config.worker_server_url, fc.host_ip)
    ));
    let spec = serde_json::json!({
        "argv": [CAOS_BIN, "runner", format!("--job={}", job.payload)],
        "env": env,
        "cwd": image.cwd,
        "uid": image.user.map(|u| u.0),
        "gid": image.user.map(|u| u.1),
        "hostname": hostname,
        "ip": guest.to_string(),
        "dns": fc.dns,
    });
    let job_disk = dir.join("job.json");
    if let Err(e) = write_padded(&job_disk, spec.to_string().as_bytes()) {
        return fail(format!("writing {}: {e}", job_disk.display()));
    }
    let boot_args = format!(
        "console=ttyS0 reboot=k panic=1 pci=off quiet loglevel=1 \
         i8042.noaux i8042.nomux i8042.dumbkbd random.trust_cpu=on \
         init=/{INIT_PATH} ip={guest}::{}:{}::eth0:off",
        fc.host_ip,
        netmask(fc.prefix_len),
    );
    let vm = serde_json::json!({
        "boot-source": {
            "kernel_image_path": fc.kernel,
            "boot_args": boot_args,
        },
        // Order is the guest's device order: vda (root, given first by
        // firecracker), vdb, vdc — caos-fc-init relies on it.
        "drives": [
            {"drive_id": "rootfs", "path_on_host": image.rootfs,
             "is_root_device": true, "is_read_only": true},
            {"drive_id": "scratch", "path_on_host": scratch,
             "is_root_device": false, "is_read_only": false},
            {"drive_id": "job", "path_on_host": job_disk,
             "is_root_device": false, "is_read_only": true},
        ],
        "machine-config": {"vcpu_count": fc.vcpus, "mem_size_mib": fc.mem_mib},
        "network-interfaces": [{
            "iface_id": "eth0",
            "guest_mac": guest_mac(guest),
            "host_dev_name": format!("{}{slot}", fc.tap_prefix),
        }],
    });
    let vm_config = dir.join("vm.json");
    if let Err(e) = std::fs::write(&vm_config, vm.to_string()) {
        return fail(format!("writing {}: {e}", vm_config.display()));
    }
    let log_path = dir.join("firecracker.log");
    if let Err(e) = std::fs::write(&log_path, "") {
        return fail(format!("creating {}: {e}", log_path.display()));
    }
    let mut command = Command::new(&fc.bin);
    command
        .arg("--no-api")
        .arg("--id")
        .arg(short(&job.nonce))
        .arg("--config-file")
        .arg(&vm_config)
        .arg("--log-path")
        .arg(&log_path)
        .args(["--level", "Warning"])
        .stdin(Stdio::null());
    // The VM must not outlive this slot — the stand-in for the engine owning
    // the container. A runnerd that dies (or is stopped: its handlers `_exit`)
    // takes its VMs with it instead of leaking them onto the host's CPUs.
    unsafe {
        command.pre_exec(|| {
            extern "C" {
                fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
            }
            const PR_SET_PDEATHSIG: i32 = 1;
            const SIGKILL: u64 = 9;
            prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0);
            Ok(())
        });
    }
    split.push(("setup_ms".into(), setup.elapsed().as_millis().to_string()));
    let out = match command.output() {
        Ok(out) => out,
        Err(e) => return fail(format!("running {}: {e}", fc.bin)),
    };
    let console = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let (log, exit) = split_exit_marker(&console);
    // Relay the guest's console like the docker path relays the container's.
    eprint!("{log}");
    if let Some(exit) = &exit {
        split.extend(exit.timings.iter().cloned());
    }
    match exit.map(|e| e.code) {
        Some(0) => None,
        Some(code) => Some((format!("worker vm exited with exit status: {code}"), log)),
        None => {
            let vmm = std::fs::read_to_string(&log_path).unwrap_or_default();
            Some((
                format!(
                    "worker vm ended without reporting an exit status (firecracker {})",
                    out.status
                ),
                format!(
                    "{log}\n--- firecracker log ---\n{vmm}{}",
                    String::from_utf8_lossy(&out.stderr)
                ),
            ))
        }
    }
}

/// What init reported on its marker line.
#[derive(Debug, PartialEq)]
pub struct Exit {
    pub code: i32,
    /// `key=value` pairs after the status (`boot_ms`, `job_ms`), as given.
    pub timings: Vec<(String, String)>,
}

/// Separate the guest's console output from the exit marker init prints last.
/// The LAST marker wins: a job's own output can contain the string.
pub fn split_exit_marker(console: &str) -> (String, Option<Exit>) {
    let Some(at) = console.rfind(EXIT_MARKER) else {
        return (console.to_string(), None);
    };
    let line = console[at + EXIT_MARKER.len()..]
        .lines()
        .next()
        .unwrap_or("");
    let mut words = line.split_whitespace();
    let Some(Ok(code)) = words.next().map(str::parse::<i32>) else {
        return (console.to_string(), None);
    };
    let timings = words
        .filter_map(|w| {
            w.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    (console[..at].to_string(), Some(Exit { code, timings }))
}

/// A locally administered unicast MAC derived from the guest's address, so a
/// slot's interface keeps one MAC across VMs.
fn guest_mac(ip: Ipv4Addr) -> String {
    let [a, b, c, d] = ip.octets();
    format!("06:00:{a:02x}:{b:02x}:{c:02x}:{d:02x}")
}

fn short(nonce: &str) -> &str {
    &nonce[..nonce.len().min(12)]
}

/// A block device's size is whole sectors; bytes past the last full one cannot
/// be read by the guest, so pad with the NULs init strips.
fn write_padded(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut data = bytes.to_vec();
    data.resize(bytes.len().div_ceil(4096).max(1) * 4096, 0);
    std::fs::write(path, data)
}

/// A fresh, empty scratch disk for one VM: a sparse copy of a template made
/// once per size. Sparse both ways, so a 16 GiB scratch costs the few MiB of
/// metadata mkfs wrote, plus whatever the job actually writes.
fn scratch_disk(fc: &FcConfig, dest: &Path) -> Result<(), String> {
    let template = fc.state.join(format!(
        "scratch-{}m-v{CONVERSION_VERSION}.ext4",
        fc.scratch_mib
    ));
    if !template.exists() {
        let tmp = tmp_sibling(&template);
        run_tool(
            Command::new(&fc.mkfs)
                .args(["-q", "-F", "-t", "ext4", "-L", "caos-scratch"])
                // root_owner: mkfs otherwise gives the root directory to whoever ran
                // it, and the overlay's upper dir lives under it.
                .args(["-E", "root_owner=0:0,lazy_itable_init=1,nodiscard"])
                .arg(&tmp)
                .arg(format!("{}M", fc.scratch_mib)),
        )?;
        std::fs::rename(&tmp, &template)
            .map_err(|e| format!("installing {}: {e}", template.display()))?;
    }
    run_tool(
        Command::new("cp")
            .arg("--sparse=always")
            .arg(&template)
            .arg(dest),
    )
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    path.with_file_name(name)
}

/// Run a host tool to completion; a failure carries its stderr.
fn run_tool(command: &mut Command) -> Result<(), String> {
    let out = command
        .output()
        .map_err(|e| format!("running {:?}: {e}", command.get_program()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{:?} failed ({}): {}",
            command.get_program(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run the configured docker CLI (with its global args) for its stdout.
fn docker(config: &Config, args: &[&str]) -> Result<String, String> {
    let out = Command::new(&config.docker_bin)
        .args(&config.docker_args)
        .args(args)
        .output()
        .map_err(|e| format!("running {}: {e}", config.docker_bin))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{} {} failed: {}",
            config.docker_bin,
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The cache directory for an image ref. Only a DIGEST ref names content, so
/// only a digest ref is cached; a tag gets a fresh conversion every time,
/// because a stale rootfs would be the wrong program running silently.
fn cache_key(image_ref: &str, init_id: &str) -> Option<String> {
    let digest = image_ref.split_once("@sha256:")?.1;
    if digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("{digest}-v{CONVERSION_VERSION}-init{init_id}"))
    } else {
        None
    }
}

/// Convert an OCI image into a bootable rootfs, or find the conversion already
/// done. Single-flight per ref within this process, and atomic (temp dir +
/// rename) across processes sharing the state dir.
fn prepare_image(config: &Config, fc: &FcConfig, image_ref: &str) -> Result<Prepared, String> {
    let grants = image_grants(config, image_ref);
    let mut refused = Vec::new();
    if grants.socket {
        refused.push("the engine socket".to_string());
    }
    if !grants.volumes.is_empty() {
        refused.push(format!("volumes {}", grants.volumes.join(" ")));
    }
    if !grants.devices.is_empty() {
        refused.push(format!("devices {}", grants.devices.join(" ")));
    }
    if !refused.is_empty() {
        return Err(format!(
            "the image declares grants the firecracker runtime cannot provide: {}",
            refused.join(", ")
        ));
    }
    let key = cache_key(image_ref, &fc.init_id);
    let lock = {
        let mut map = fc.converting.lock().expect("conversion locks");
        Arc::clone(map.entry(image_ref.to_string()).or_default())
    };
    let _held = lock.lock().expect("conversion lock");
    let dir = match &key {
        Some(k) => fc.state.join("images").join(k),
        None => fc.vms.join(format!("image-{}", std::process::id())),
    };
    // Re-read after acquiring: a peer slot may have converted it meanwhile.
    if key.is_none() || !dir.join("rootfs.ext4").exists() {
        let started = Instant::now();
        convert(config, fc, image_ref, &dir)?;
        eprintln!(
            "caos-runnerd: converted {image_ref} to a firecracker rootfs in {}ms",
            started.elapsed().as_millis()
        );
    }
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("config.json"))
            .map_err(|e| format!("reading image config: {e}"))?,
    )
    .map_err(|e| format!("parsing image config: {e}"))?;
    let env = meta["Env"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd = meta["WorkingDir"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("/")
        .to_string();
    let user = parse_user(meta["User"].as_str().unwrap_or(""))?;
    Ok(Prepared {
        rootfs: dir.join("rootfs.ext4"),
        env,
        cwd,
        user,
    })
}

/// An image's `User`, as the uid/gid the guest's process runs under. Numeric
/// only: a name would need the image's passwd, and no caos image sets one —
/// so a name is refused rather than guessed.
fn parse_user(user: &str) -> Result<Option<(u32, u32)>, String> {
    if user.is_empty() {
        return Ok(None);
    }
    let (u, g) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    let uid: u32 = u.parse().map_err(|_| {
        format!(
            "image User {user:?} is not numeric; the firecracker runtime does not resolve names"
        )
    })?;
    let gid: u32 = match g {
        Some(g) => g
            .parse()
            .map_err(|_| format!("image User {user:?} has a non-numeric group"))?,
        None => uid,
    };
    Ok(Some((uid, gid)))
}

fn convert(config: &Config, fc: &FcConfig, image_ref: &str, dir: &Path) -> Result<(), String> {
    let tmp = tmp_sibling(dir);
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
    let result = (|| {
        // The engine has the image (or pulls it, as `run` would), and gives us
        // both halves: the config an engine applies, and the flattened rootfs.
        let inspect = |config: &Config| {
            docker(
                config,
                &[
                    "image",
                    "inspect",
                    "--format",
                    "{{json .Config}}",
                    image_ref,
                ],
            )
        };
        let meta = match inspect(config) {
            Ok(m) => m,
            Err(_) => {
                docker(config, &["pull", "-q", image_ref])?;
                inspect(config)?
            }
        };
        std::fs::write(tmp.join("config.json"), meta.trim())
            .map_err(|e| format!("writing config: {e}"))?;
        let id = docker(config, &["create", "--entrypoint", CAOS_BIN, image_ref])?
            .trim()
            .to_string();
        let tar = tmp.join("rootfs.tar");
        let exported = docker(config, &["export", "-o", &tar.to_string_lossy(), &id]);
        let _ = docker(config, &["rm", "-f", &id]);
        exported?;
        // Add the guest init and its staging mountpoint, owned by root like
        // everything else the export carries.
        let stage = tmp.join("stage");
        std::fs::create_dir_all(stage.join(STAGING_DIR)).map_err(|e| format!("staging: {e}"))?;
        std::fs::copy(&fc.init, stage.join(INIT_PATH)).map_err(|e| format!("copying init: {e}"))?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            stage.join(INIT_PATH),
            std::fs::Permissions::from_mode(0o755),
        )
        .map_err(|e| format!("chmod init: {e}"))?;
        std::fs::set_permissions(
            stage.join(STAGING_DIR),
            std::fs::Permissions::from_mode(0o755),
        )
        .map_err(|e| format!("chmod staging: {e}"))?;
        run_tool(
            Command::new("tar")
                .args([
                    "--append",
                    "--owner=0",
                    "--group=0",
                    "--numeric-owner",
                    "-f",
                ])
                .arg(&tar)
                .arg("-C")
                .arg(&stage)
                .args([INIT_PATH, STAGING_DIR]),
        )?;
        // mkfs reads the TARBALL directly (e2fsprogs >= 1.47.1), which keeps
        // the tar's ownership and modes — setuid-root /bin/caos included —
        // without runnerd being root or unpacking anything. No journal: this
        // filesystem is only ever mounted read-only.
        let tar_len = std::fs::metadata(&tar)
            .map_err(|e| format!("stat tar: {e}"))?
            .len();
        let size_mib = tar_len / (1 << 20) * 5 / 4 + 256;
        let rootfs = tmp.join("rootfs.ext4");
        run_tool(
            Command::new(&fc.mkfs)
                .args([
                    "-q",
                    "-F",
                    "-t",
                    "ext4",
                    "-L",
                    "caos-root",
                    "-m",
                    "0",
                    "-O",
                    "^has_journal",
                ])
                .args(["-E", "root_owner=0:0", "-d"])
                .arg(&tar)
                .arg(&rootfs)
                .arg(format!("{size_mib}M")),
        )?;
        std::fs::remove_file(&tar).map_err(|e| format!("removing tar: {e}"))?;
        let _ = std::fs::remove_dir_all(&stage);
        Ok::<(), String>(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&tmp, dir).map_err(|e| format!("installing {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_addresses_stay_in_the_subnet() {
        let host: Ipv4Addr = "10.231.0.1".parse().unwrap();
        assert_eq!(guest_ip(host, 24, 0).unwrap().to_string(), "10.231.0.2");
        assert_eq!(guest_ip(host, 24, 252).unwrap().to_string(), "10.231.0.254");
        assert!(guest_ip(host, 24, 253).is_err(), "broadcast is not a guest");
        assert!(guest_ip(host, 31, 0).is_err());
    }

    #[test]
    fn loopback_server_urls_point_at_the_host() {
        let host: Ipv4Addr = "10.231.0.1".parse().unwrap();
        assert_eq!(
            guest_server_url("http://127.0.0.1:9090", host),
            "http://10.231.0.1:9090"
        );
        assert_eq!(
            guest_server_url("http://localhost:9090/x", host),
            "http://10.231.0.1:9090/x"
        );
        assert_eq!(
            guest_server_url("http://localhost", host),
            "http://10.231.0.1"
        );
        assert_eq!(
            guest_server_url("http://192.168.1.5:9090", host),
            "http://192.168.1.5:9090"
        );
    }

    #[test]
    fn loopback_nameservers_are_skipped() {
        let text = "nameserver 127.0.0.53\noptions edns0\nnameserver 10.64.0.1\n nameserver ::1\nnameserver 1.1.1.1\n";
        assert_eq!(parse_nameservers(text), ["10.64.0.1", "1.1.1.1"]);
    }

    #[test]
    fn the_last_exit_marker_wins() {
        let console = format!(
            "hello\n{EXIT_MARKER}7 from the job\nbye\n{EXIT_MARKER}3 boot_ms=120 job_ms=5\n"
        );
        let (log, exit) = split_exit_marker(&console);
        let exit = exit.unwrap();
        assert_eq!(exit.code, 3);
        assert_eq!(
            exit.timings,
            [
                ("boot_ms".into(), "120".into()),
                ("job_ms".into(), "5".into())
            ]
        );
        assert!(log.ends_with("bye\n"));
        assert_eq!(split_exit_marker("panic\n").1, None);
        assert_eq!(split_exit_marker(&format!("{EXIT_MARKER}oops\n")).1, None);
    }

    #[test]
    fn only_digest_refs_are_cached() {
        let d = "a".repeat(64);
        let at = format!("localhost:5000/caos@sha256:{d}");
        assert!(cache_key(&at, "i1").is_some());
        assert_ne!(
            cache_key(&at, "i1"),
            cache_key(&at, "i2"),
            "a new init is a new rootfs"
        );
        assert!(cache_key("localhost:5000/caos:latest", "i1").is_none());
    }

    #[test]
    fn users_are_numeric() {
        assert_eq!(parse_user("").unwrap(), None);
        assert_eq!(parse_user("1000").unwrap(), Some((1000, 1000)));
        assert_eq!(parse_user("1000:100").unwrap(), Some((1000, 100)));
        assert!(parse_user("nobody").is_err());
    }
}
