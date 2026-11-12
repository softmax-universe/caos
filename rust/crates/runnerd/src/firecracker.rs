//! The Firecracker backend (design/firecracker-runners.md): one microVM per
//! slot, booted from a prebuilt appliance (`nix/firecracker-appliance.nix`).
//!
//! Per job, the host makes the VM's private resources:
//!
//! * a sparse copy of the appliance's root disk;
//! * a CONFIG disk: the job payload and the boot env (the shared
//!   [`crate::worker_env`], with the server address as the guest reaches it),
//!   one JSON object, NUL-padded. It is a disk and not the kernel command line
//!   because the payload carries credentials;
//! * a REPORT disk the guest's runner uses as stdout, for its claim records
//!   and the exit record `caos vm-init` appends (`caos_world::runner_lineage`);
//! * a tap device with a /30 per slot. The guest reaches the server through the
//!   host end of that link.
//!
//! It then boots Firecracker, waits for the guest's readiness line (a timeout
//! there is a BOOT failure, never a slow job), waits for the VM to exit, reads
//! the report, and deletes everything. The runner inside posts its own results
//! and polls for more work, exactly as in a container, so this backend adds no
//! protocol.
//!
//! Firecracker runs under its JAILER by default (`CAOS_FC_JAILER`, `none` to
//! opt out): each VM gets a chroot holding only its own files, and runs as a
//! uid of its slot's own, so one VM's process cannot open another's disks. The
//! kernel and initrd are copied into the state directory once, verified there,
//! and hard-linked into each chroot.
//!
//! What it refuses at startup, so nothing is discovered per job: no usable
//! `/dev/kvm`; appliance files whose digests do not match `appliance.json`; an
//! image env declaring a grant; and a required set without both `base` and
//! `required-pool`. A fixed appliance must claim only work it implements, and a
//! generic Firecracker pool would run any job's args with its one worker.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use caos_world::appliance::{BootConfig, READY_LINE};

use crate::{env_or, owner_id, worker_env, Backend, Common, Job, Outcome};

/// How much console output is kept to explain a failure.
const CONSOLE_TAIL: usize = 64 * 1024;
/// The report disk. Sparse, so its size costs nothing; one claim record is
/// about a hundred bytes, so this holds well over a hundred thousand.
const REPORT_BYTES: u64 = 16 << 20;
/// The config disk's minimum size; it is rounded up to whole 4 KiB blocks.
const CONFIG_MIN_BYTES: usize = 64 << 10;

pub(crate) struct Config {
    /// Absolute path: the jailer requires one.
    firecracker: String,
    /// The jailer's path, or `None` to run Firecracker directly.
    jailer: Option<String>,
    /// Slot `s`'s VMs run as uid and gid `jail_uid + s`.
    jail_uid: u32,
    appliance: PathBuf,
    state: PathBuf,
    vcpus: u32,
    mem_mib: u32,
    boot_timeout: Duration,
    /// First address of the /30 pool: slot `s` gets the `s`-th /30 after it.
    subnet: [u8; 4],
    /// The server URL as the guest must reach it; `None` = this host's end of
    /// the slot's tap link, at the port runnerd itself uses.
    guest_server_url: Option<String>,
    guest_redis_addr: Option<String>,
    dns: Option<String>,
}

impl Config {
    pub(crate) fn from_env() -> Result<Config, String> {
        let num = |key: &str, default: u32| -> Result<u32, String> {
            env_or(key, &default.to_string())
                .parse()
                .map_err(|e| format!("{key}: {e}"))
        };
        let subnet = env_or("CAOS_FC_SUBNET", "172.30.0.0");
        let subnet: std::net::Ipv4Addr = subnet
            .parse()
            .map_err(|e| format!("CAOS_FC_SUBNET {subnet:?}: {e}"))?;
        let nonempty = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let jailer = match env_or("CAOS_FC_JAILER", "jailer").as_str() {
            "none" => None,
            jailer => Some(on_path(jailer)?),
        };
        Ok(Config {
            firecracker: on_path(&env_or("CAOS_FC_BIN", "firecracker"))?,
            jailer,
            jail_uid: num("CAOS_FC_JAIL_UID", 30000)?,
            appliance: PathBuf::from(
                nonempty("CAOS_FC_APPLIANCE")
                    .ok_or("CAOS_FC_APPLIANCE (the appliance directory) is required")?,
            ),
            state: PathBuf::from(env_or("CAOS_FC_STATE", "/var/lib/caos-runnerd-fc")),
            vcpus: num("CAOS_FC_VCPUS", 1)?,
            mem_mib: num("CAOS_FC_MEM_MIB", 512)?,
            boot_timeout: Duration::from_secs(u64::from(num("CAOS_FC_BOOT_TIMEOUT_SECS", 20)?)),
            subnet: subnet.octets(),
            guest_server_url: nonempty("CAOS_FC_GUEST_SERVER_URL"),
            guest_redis_addr: nonempty("CAOS_FC_GUEST_REDIS_ADDR"),
            dns: nonempty("CAOS_FC_DNS"),
        })
    }

    /// Everything that would otherwise fail per job, checked once.
    pub(crate) fn validate(&self, common: &Common) -> Result<(), String> {
        for key in ["base", caos_world::REQUIRED_POOL_ARG] {
            if !common.required.contains_key(key) {
                return Err(format!(
                    "a Firecracker pool must require `{key}` (CAOS_RUNNER_REQUIRE): its appliance \
                     implements one base, and must claim nothing else"
                ));
            }
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map_err(|e| format!("/dev/kvm is not usable: {e}"))?;
        let manifest_path = self.appliance.join("appliance.json");
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path)
                .map_err(|e| format!("{}: {e}", manifest_path.display()))?,
        )
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
        std::fs::create_dir_all(self.boot_files())
            .map_err(|e| format!("{}: {e}", self.boot_files().display()))?;
        for file in ["vmlinux", "initrd", "rootfs"] {
            let want = manifest[file]
                .as_str()
                .ok_or_else(|| format!("{}: no {file} digest", manifest_path.display()))?;
            // The kernel and initrd every VM boots are OUR copies, verified
            // here: hard-linkable into a chroot (the store is another
            // filesystem), and what was checked is what boots.
            let path = if file == "rootfs" {
                self.appliance_file(file)
            } else {
                let copy = self.boot_files().join(file);
                run(Command::new("cp")
                    .arg("-f")
                    .arg(self.appliance_file(file))
                    .arg(&copy))?;
                run(Command::new("chmod").arg("0444").arg(&copy))?;
                copy
            };
            let got = sha256(&path)?;
            if got != want {
                return Err(format!(
                    "{}: sha256 {got}, but appliance.json pins {want}",
                    path.display()
                ));
            }
        }
        let env = manifest["image_env"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(grant) = env
            .iter()
            .filter_map(serde_json::Value::as_str)
            .find(|e| e.starts_with("CAOS_GRANT_"))
        {
            return Err(format!(
                "the appliance's image env declares {grant}, which a VM cannot honor"
            ));
        }
        for bin in std::iter::once(&self.firecracker).chain(&self.jailer) {
            let version = Command::new(bin)
                .arg("--version")
                .output()
                .map_err(|e| format!("running {bin}: {e}"))?;
            if !version.status.success() {
                return Err(format!("{bin} --version failed"));
            }
        }
        match &self.jailer {
            Some(_) => {
                if !Path::new(&self.firecracker)
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("firecracker"))
                {
                    return Err(format!(
                        "the jailer only execs a file named *firecracker*, not {}",
                        self.firecracker
                    ));
                }
            }
            None => eprintln!(
                "caos-runnerd: CAOS_FC_JAILER=none: VMs run WITHOUT the jailer, as this \
                 process's user and in its filesystem"
            ),
        }
        for dir in [self.vms(), self.jails()] {
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(())
    }

    fn appliance_file(&self, file: &str) -> PathBuf {
        self.appliance.join(match file {
            "rootfs" => "rootfs.ext4",
            other => other,
        })
    }

    /// This runnerd's state lives under a directory named for it, so a
    /// restarted runnerd reaps only its own predecessor's.
    fn owned_state(&self) -> PathBuf {
        self.state.join(owner_id())
    }

    /// One directory per VM: its pid, tap, console, and (unjailed) its files.
    fn vms(&self) -> PathBuf {
        self.owned_state().join("vms")
    }

    /// The jailer's chroot base: `<jails>/firecracker/<id>/root`.
    fn jails(&self) -> PathBuf {
        self.owned_state().join("jail")
    }

    /// The verified kernel and initrd copies VMs boot.
    fn boot_files(&self) -> PathBuf {
        self.owned_state().join("appliance")
    }

    /// The slot's /30: (host end, guest end).
    fn link(&self, slot: u32) -> ([u8; 4], [u8; 4]) {
        let base = u32::from_be_bytes(self.subnet) + slot * 4;
        ((base + 1).to_be_bytes(), (base + 2).to_be_bytes())
    }
}

impl Backend for Config {
    fn describe(&self) -> String {
        format!("firecracker appliance {}", self.appliance.display())
    }

    fn reap_leftovers(&self) {
        let Ok(entries) = std::fs::read_dir(self.vms()) else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let pid: Option<u32> = std::fs::read_to_string(dir.join("pid"))
                .ok()
                .and_then(|p| p.trim().parse().ok());
            // A live VM is left alone: its runner may still be finishing a job.
            if pid.is_some_and(vm_is_alive) {
                eprintln!(
                    "caos-runnerd: {} is still running; leaving it",
                    dir.display()
                );
                continue;
            }
            eprintln!("caos-runnerd: reaping leftover VM state {}", dir.display());
            remove_vm(&dir);
        }
    }

    fn run(&self, common: &Common, slot: u32, job: &Job) -> Outcome {
        let vm = Vm::new(self, slot, &job.nonce);
        let outcome = self.boot(common, slot, job, &vm);
        // The VM has exited (or never started) by now: its storage goes only
        // after its process.
        remove_vm(&vm.dir);
        outcome
    }
}

impl Config {
    fn boot(&self, common: &Common, slot: u32, job: &Job, vm: &Vm) -> Outcome {
        let dir = vm.dir.as_path();
        let fail = |error: String, log: String| Outcome {
            failure: Some((error, log)),
            stdout: String::new(),
        };
        let tap = tap_name(&job.nonce);
        let (host, guest) = self.link(slot);
        let host_ip = dotted(host);
        if let Err(e) = self.prepare(common, job, vm, &tap, &host_ip) {
            return fail(format!("preparing the VM: {e}"), String::new());
        }
        let mut boot_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off quiet caos.ip={}/30 caos.gw={host_ip}",
            dotted(guest)
        );
        if let Some(dns) = &self.dns {
            boot_args.push_str(&format!(" caos.dns={dns}"));
        }
        let vm_config = serde_json::json!({
            "boot-source": {
                "kernel_image_path": vm.seen("vmlinux"),
                "initrd_path": vm.seen("initrd"),
                "boot_args": boot_args,
            },
            "drives": [
                {"drive_id": "root", "path_on_host": vm.seen("rootfs.ext4"),
                 "is_root_device": false, "is_read_only": false},
                {"drive_id": "config", "path_on_host": vm.seen("config"),
                 "is_root_device": false, "is_read_only": true},
                {"drive_id": "report", "path_on_host": vm.seen("report"),
                 "is_root_device": false, "is_read_only": false},
            ],
            "network-interfaces": [
                {"iface_id": "eth0", "host_dev_name": tap, "guest_mac": guest_mac(slot)},
            ],
            "machine-config": {"vcpu_count": self.vcpus, "mem_size_mib": self.mem_mib},
        });
        let vm_json = vm.files.join("vm.json");
        if let Err(e) = std::fs::write(&vm_json, vm_config.to_string()) {
            return fail(format!("writing {}: {e}", vm_json.display()), String::new());
        }
        // The console goes to a FILE, not a pipe to us. A pipe ties the VM to
        // this process: when a runnerd restart broke it, Firecracker hung in
        // the guest's shutdown instead of exiting, and a VM that outlives its
        // supervisor must still be able to end. We follow the file instead.
        let console_path = dir.join("console.log");
        let console_file =
            match std::fs::File::create(&console_path).and_then(|f| Ok((f.try_clone()?, f))) {
                Ok(files) => files,
                Err(e) => {
                    return fail(
                        format!("creating {}: {e}", console_path.display()),
                        String::new(),
                    )
                }
            };
        let mut command = match &self.jailer {
            None => Command::new(&self.firecracker),
            Some(jailer) => {
                let mut command = Command::new(jailer);
                command
                    .arg("--id")
                    .arg(&job.nonce)
                    .arg("--exec-file")
                    .arg(&self.firecracker)
                    .arg("--uid")
                    .arg(vm.uid.to_string())
                    .arg("--gid")
                    .arg(vm.uid.to_string())
                    .arg("--chroot-base-dir")
                    .arg(self.jails())
                    .args(["--resource-limit", "no-file=4096"])
                    .arg("--");
                command
            }
        };
        let mut child = match command
            .arg("--no-api")
            .arg("--config-file")
            .arg(vm.seen("vm.json"))
            .stdin(Stdio::null())
            .stdout(console_file.0)
            .stderr(console_file.1)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return fail(format!("starting {}: {e}", self.firecracker), String::new()),
        };
        let _ = std::fs::write(dir.join("pid"), child.id().to_string());
        let console: Arc<Mutex<String>> = Arc::default();
        let (ready_tx, ready_rx) = mpsc::channel();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let follower = follow(console_path, slot, &console, ready_tx, &done);
        let ready = ready_rx.recv_timeout(self.boot_timeout).is_ok();
        if !ready {
            // Either the VM already died, or it is hung before its runner
            // started. Neither ran the job, so this is a boot failure.
            let _ = child.kill();
        }
        let status = child.wait();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = follower.join();
        let console = console.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if !ready {
            return fail(
                format!(
                    "the VM did not start its runner within {}s (boot failure)",
                    self.boot_timeout.as_secs()
                ),
                console,
            );
        }
        let report = read_report(&vm.files.join("report"));
        let failure = match (caos_world::runner_lineage::exit_status(&report), &status) {
            (Some(0), _) => None,
            (Some(code), _) => Some(format!("the VM's runner exited with {code}")),
            (None, Ok(status)) => Some(format!(
                "the VM ended ({status}) before its runner reported an exit"
            )),
            (None, Err(e)) => Some(format!("waiting for {}: {e}", self.firecracker)),
        };
        Outcome {
            failure: failure.map(|error| (error, console)),
            stdout: report,
        }
    }

    /// The VM's private resources: root disk copy, config and report disks,
    /// tap. Each is recorded in `dir` as it is made, so the cleanup (here, or a
    /// later runnerd's reaper) finds everything.
    fn prepare(
        &self,
        common: &Common,
        job: &Job,
        vm: &Vm,
        tap: &str,
        host_ip: &str,
    ) -> Result<(), String> {
        let dir = vm.dir.as_path();
        remove_vm(dir);
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if self.jailer.is_some() {
            // Recorded before it exists, so a crash between here and the jail
            // still leaves the reaper a pointer to it.
            std::fs::write(
                dir.join("chroot"),
                vm.files
                    .parent()
                    .unwrap_or(&vm.files)
                    .as_os_str()
                    .as_encoded_bytes(),
            )
            .map_err(|e| format!("recording the chroot: {e}"))?;
        }
        std::fs::create_dir_all(&vm.files).map_err(|e| format!("{}: {e}", vm.files.display()))?;
        // Hard links to the verified copies, jailed or not: one code path, and
        // what boots is what was checked.
        for file in ["vmlinux", "initrd"] {
            std::fs::hard_link(self.boot_files().join(file), vm.files.join(file))
                .map_err(|e| format!("linking {file} into {}: {e}", vm.files.display()))?;
        }
        let rootfs = vm.files.join("rootfs.ext4");
        run(Command::new("cp")
            .arg("--sparse=always")
            .arg(self.appliance_file("rootfs"))
            .arg(&rootfs))?;
        run(Command::new("chmod").arg("u+w").arg(&rootfs))?;

        let mut env: std::collections::BTreeMap<String, String> =
            worker_env(common).into_iter().collect();
        env.insert(
            "CAOS_SERVER_URL".to_string(),
            match &self.guest_server_url {
                Some(url) => url.clone(),
                None => with_host(&common.server_url, host_ip),
            },
        );
        match &self.guest_redis_addr {
            Some(addr) => env.insert("CAOS_WORKER_REDIS_ADDR".to_string(), addr.clone()),
            // The host's address is the host's: an absent variable means no
            // cache is offered, which a worker must tolerate.
            None => env.remove("CAOS_WORKER_REDIS_ADDR"),
        };
        let mut config = BootConfig {
            job: job.payload.clone(),
            env,
        }
        .encode()
        .into_bytes();
        let size = config.len().max(CONFIG_MIN_BYTES).div_ceil(4096) * 4096;
        config.resize(size, 0);
        write_private(&vm.files.join("config"), &config)?;
        std::fs::File::create(vm.files.join("report"))
            .and_then(|f| f.set_len(REPORT_BYTES))
            .map_err(|e| format!("creating the report disk: {e}"))?;
        if self.jailer.is_some() {
            // The VM's own files are its uid's; the shared kernel and initrd
            // stay root's and read-only.
            for file in ["rootfs.ext4", "config", "report"] {
                std::os::unix::fs::chown(vm.files.join(file), Some(vm.uid), Some(vm.uid))
                    .map_err(|e| format!("chown {file}: {e}"))?;
            }
        }

        std::fs::write(dir.join("tap"), tap).map_err(|e| format!("recording the tap: {e}"))?;
        // Owned by the VM's uid, so a jailed Firecracker can attach to it
        // without CAP_NET_ADMIN.
        let uid = vm.uid.to_string();
        run(Command::new("ip").args([
            "tuntap", "add", "dev", tap, "mode", "tap", "user", &uid, "group", &uid,
        ]))?;
        run(Command::new("ip").args(["addr", "add", &format!("{host_ip}/30"), "dev", tap]))?;
        run(Command::new("ip").args(["link", "set", tap, "up"]))?;
        Ok(())
    }
}

/// Where one VM's things are.
struct Vm {
    /// Our record of it: pid, tap, console, and the chroot's path if jailed.
    dir: PathBuf,
    /// Where its disks, kernel and config live: `dir` unjailed, else the
    /// chroot's root.
    files: PathBuf,
    jailed: bool,
    uid: u32,
}

impl Vm {
    fn new(config: &Config, slot: u32, nonce: &str) -> Vm {
        let dir = config.vms().join(nonce);
        let (files, jailed) = match config.jailer {
            // The jailer names the chroot after the exec file's basename.
            Some(_) => {
                let exec = Path::new(&config.firecracker)
                    .file_name()
                    .map_or_else(|| "firecracker".into(), |n| n.to_os_string());
                (config.jails().join(exec).join(nonce).join("root"), true)
            }
            None => (dir.clone(), false),
        };
        Vm {
            dir,
            files,
            jailed,
            uid: config.jail_uid + slot,
        }
    }

    /// `name` as Firecracker sees it: inside the chroot, or by its real path.
    fn seen(&self, name: &str) -> PathBuf {
        if self.jailed {
            Path::new("/").join(name)
        } else {
            self.files.join(name)
        }
    }
}

/// Delete a VM's tap, its chroot (if it had one) and its record. Only for a
/// VM whose process has exited.
fn remove_vm(dir: &Path) {
    if let Ok(tap) = std::fs::read_to_string(dir.join("tap")) {
        delete_tap(tap.trim());
    }
    if let Ok(chroot) = std::fs::read_to_string(dir.join("chroot")) {
        let chroot = chroot.trim();
        if !chroot.is_empty() {
            let _ = std::fs::remove_dir_all(chroot);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// `bin` as an absolute path: itself if it has a slash, else found on PATH.
fn on_path(bin: &str) -> Result<String, String> {
    if bin.contains('/') {
        return Ok(bin.to_string());
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{bin} is not on PATH"))
}

/// Follow the VM's console file until `done` and then to its end: relay each
/// line to our stderr (the console carries the runner's stderr, so this is the
/// job's log), keep a bounded tail, and signal when the guest's readiness line
/// goes past.
fn follow(
    path: PathBuf,
    slot: u32,
    console: &Arc<Mutex<String>>,
    ready: mpsc::Sender<()>,
    done: &Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let console = Arc::clone(console);
    let done = Arc::clone(done);
    std::thread::spawn(move || {
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            // Checked BEFORE reading, so the read after `done` drains the rest.
            let finished = done.load(std::sync::atomic::Ordering::SeqCst);
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => {
                    if finished {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) if !line.ends_with(b"\n") && !finished => continue,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\r']);
            // Firecracker's own MMIO-probe noise is not the job's log.
            if !text.contains("MissingAddressRange") {
                eprintln!("runnerd slot {slot} vm: {text}");
                if text.contains(READY_LINE) {
                    let _ = ready.send(());
                }
                let mut tail = console.lock().unwrap_or_else(|p| p.into_inner());
                tail.push_str(text);
                tail.push('\n');
                if tail.len() > CONSOLE_TAIL {
                    let cut = tail.len() - CONSOLE_TAIL;
                    let cut = (cut..tail.len())
                        .find(|&i| tail.is_char_boundary(i))
                        .unwrap_or(cut);
                    tail.drain(..cut);
                }
            }
            line.clear();
        }
    })
}

/// Is `pid` a running Firecracker? Not merely a present pid: an orphaned VM
/// that has exited stays a zombie until something reaps it, and a pid can be
/// reused by an unrelated process after a restart.
fn vm_is_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) state …`; comm may contain spaces, so split at the last ')'.
    let Some((head, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let comm = head.split_once('(').map_or("", |(_, c)| c);
    let state = rest.split_whitespace().next().unwrap_or("");
    comm.starts_with("firecracker") && state != "Z" && state != "X"
}

/// The report disk's text: everything before the first NUL.
fn read_report(path: &Path) -> String {
    let raw = std::fs::read(path).unwrap_or_default();
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// A tap device name for this job. Kernel limit is 15 bytes; the nonce is
/// unique per job, so a leftover live VM never collides with a new one.
fn tap_name(nonce: &str) -> String {
    let id: String = nonce
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(10)
        .collect();
    format!("cfc{id}")
}

fn guest_mac(slot: u32) -> String {
    let [_, a, b, c] = slot.to_be_bytes();
    format!("06:00:ac:{a:02x}:{b:02x}:{c:02x}")
}

fn dotted(ip: [u8; 4]) -> String {
    std::net::Ipv4Addr::from(ip).to_string()
}

/// `url` with its host replaced by `host`, keeping scheme, port and path.
fn with_host(url: &str, host: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return format!("http://{host}");
    };
    let (hostport, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let port = hostport
        .rsplit_once(':')
        .filter(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .map(|(_, p)| format!(":{p}"))
        .unwrap_or_default();
    format!("{scheme}://{host}{port}{path}")
}

fn delete_tap(tap: &str) {
    if !tap.is_empty() {
        let _ = Command::new("ip").args(["link", "del", tap]).output();
    }
}

/// Write a file only its owner can read: the config disk carries the runner
/// token and the job's secrets.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

fn sha256(path: &Path) -> Result<String, String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| format!("running sha256sum: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sha256sum {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

fn run(command: &mut Command) -> Result<(), String> {
    let out = command
        .output()
        .map_err(|e| format!("running {command:?}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{command:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guest_reaches_the_server_at_the_host_end_of_its_link() {
        assert_eq!(
            with_host("http://127.0.0.1", "172.30.0.1"),
            "http://172.30.0.1"
        );
        assert_eq!(
            with_host("http://caos-server:9090/x", "172.30.0.5"),
            "http://172.30.0.5:9090/x"
        );
    }

    #[test]
    fn each_slot_gets_its_own_slash_30() {
        let config = Config {
            firecracker: String::new(),
            jailer: None,
            jail_uid: 30000,
            appliance: PathBuf::new(),
            state: PathBuf::new(),
            vcpus: 1,
            mem_mib: 512,
            boot_timeout: Duration::from_secs(1),
            subnet: [172, 30, 0, 0],
            guest_server_url: None,
            guest_redis_addr: None,
            dns: None,
        };
        assert_eq!(config.link(0), ([172, 30, 0, 1], [172, 30, 0, 2]));
        assert_eq!(config.link(1), ([172, 30, 0, 5], [172, 30, 0, 6]));
        assert_eq!(config.link(64), ([172, 30, 1, 1], [172, 30, 1, 2]));
    }

    #[test]
    fn tap_names_fit_the_kernel_limit() {
        let name = tap_name("0123456789abcdef0123456789abcdef");
        assert!(name.len() <= 15, "{name}");
        assert_eq!(name, "cfc0123456789");
    }
}
