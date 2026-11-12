//! caos-fc-init: PID 1 of a Firecracker worker VM (see runnerd's
//! `src/firecracker.rs`, which injects this binary into every converted rootfs
//! and boots the kernel with `init=/caos-fc-init`).
//!
//! It does what a container engine does before it execs an entrypoint, then
//! gets out of the way:
//!
//! 1. root: overlayfs of `/dev/vda` (the image, read-only, shared by every VM
//!    of it) under `/dev/vdb` (this VM's scratch disk) — the engine's overlay
//!    driver, rebuilt in the guest. Staged on `/.caos-fc`, an empty directory
//!    the conversion adds, then moved onto `/` the way `switch_root` does.
//! 2. `/proc`, `/sys`, `/sys/fs/cgroup`, `/dev/pts`, `/dev/shm`, the
//!    `/dev/fd` and `/dev/std*` links, `lo` up, hostname, and fresh
//!    `/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname`.
//! 3. the job, from `/dev/vdc` (JSON: argv, env, cwd, uid, gid, hostname, ip,
//!    dns): run with exactly that environment, stdin `/dev/null`, stdout and
//!    stderr on the console.
//!
//! PID 1 stays this process rather than the job, so that it can REPORT the
//! job's exit status — the one thing an engine tells runnerd that a VM's
//! console would not. It reaps every orphan meanwhile. Once the job's process
//! exits it prints `caos-fc-init: exit=<status>` (the marker runnerd parses),
//! syncs, and reboots, which ends the firecracker process.
//!
//! Every failure ends the same way, with status 125 (docker's "the engine
//! could not run it"), so a broken guest is a failed job and never a hang.

use std::ffi::CString;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

const EXIT_MARKER: &str = "caos-fc-init: exit=";
const STAGE: &str = "/.caos-fc";

mod sys {
    use std::ffi::{c_char, c_int, c_ulong, c_void};
    extern "C" {
        pub fn mount(
            src: *const c_char,
            target: *const c_char,
            fstype: *const c_char,
            flags: c_ulong,
            data: *const c_void,
        ) -> c_int;
        pub fn chroot(path: *const c_char) -> c_int;
        pub fn sethostname(name: *const c_char, len: usize) -> c_int;
        pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
        pub fn kill(pid: c_int, sig: c_int) -> c_int;
        pub fn sync();
        pub fn reboot(cmd: c_int) -> c_int;
        pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        pub fn ioctl(fd: c_int, req: c_ulong, ...) -> c_int;
        pub fn close(fd: c_int) -> c_int;
    }
    pub const MS_RDONLY: c_ulong = 1;
    pub const MS_NOSUID: c_ulong = 2;
    pub const MS_NODEV: c_ulong = 4;
    pub const MS_NOEXEC: c_ulong = 8;
    pub const MS_MOVE: c_ulong = 8192;
    pub const RB_AUTOBOOT: c_int = 0x0123_4567;
    pub const SIGKILL: c_int = 9;
    pub const AF_INET: c_int = 2;
    pub const SOCK_DGRAM: c_int = 2;
    pub const SIOCGIFFLAGS: c_ulong = 0x8913;
    pub const SIOCSIFFLAGS: c_ulong = 0x8914;
    pub const IFF_UP: i16 = 1;
}

fn main() {
    let mut timing = Timing::default();
    let code = match run(&mut timing) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("caos-fc-init: {e}");
            125
        }
    };
    // Leading newline: the job's last line may not have ended with one. The
    // timings ride on the marker line so runnerd can split a VM's life into
    // boot and job without anything extra on the console.
    print!(
        "\n{EXIT_MARKER}{code} boot_ms={} job_ms={}\n",
        timing.boot_ms, timing.job_ms
    );
    let _ = std::io::stdout().flush();
    unsafe {
        sys::kill(-1, sys::SIGKILL);
        sys::sync();
        sys::reboot(sys::RB_AUTOBOOT);
    }
    // reboot only returns on failure; a PID 1 that exits panics the kernel,
    // which `panic=1` turns into the same reboot.
    std::process::exit(code);
}

/// Where a VM's time went, as the guest saw it: `boot_ms` is the kernel's
/// uptime when the job's process started (kernel boot plus this init's setup),
/// `job_ms` that process's own lifetime.
#[derive(Default)]
struct Timing {
    boot_ms: u64,
    job_ms: u64,
}

fn uptime_ms() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|secs| (secs * 1000.0) as u64)
        .unwrap_or(0)
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no NUL in a mount argument")
}

fn mount(src: &str, target: &str, fstype: &str, flags: u64, data: &str) -> Result<(), String> {
    let (s, t, f, d) = (cstr(src), cstr(target), cstr(fstype), cstr(data));
    let rc = unsafe {
        sys::mount(
            s.as_ptr(),
            t.as_ptr(),
            if fstype.is_empty() {
                std::ptr::null()
            } else {
                f.as_ptr()
            },
            flags as _,
            if data.is_empty() {
                std::ptr::null()
            } else {
                d.as_ptr().cast()
            },
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "mount {src} on {target} ({fstype}): {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn mkdir(path: &str) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("mkdir {path}: {e}"))
}

/// Replace a file the image may carry — possibly as a symlink into its
/// read-only store — with our own content.
fn replace_file(path: &str, contents: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(path);
    std::fs::write(path, contents).map_err(|e| format!("writing {path}: {e}"))
}

struct Spec {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
    uid: Option<u32>,
    gid: Option<u32>,
    hostname: String,
    ip: String,
    dns: Vec<String>,
}

fn read_spec() -> Result<Spec, String> {
    let raw =
        std::fs::read("/dev/vdc").map_err(|e| format!("reading the job disk /dev/vdc: {e}"))?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let v: serde_json::Value =
        serde_json::from_slice(&raw[..end]).map_err(|e| format!("parsing the job disk: {e}"))?;
    let strings = |key: &str| -> Vec<String> {
        v[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let argv = strings("argv");
    if argv.is_empty() {
        return Err("the job disk has no argv".into());
    }
    Ok(Spec {
        argv,
        env: strings("env")
            .into_iter()
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
        cwd: v["cwd"].as_str().unwrap_or("/").to_string(),
        uid: v["uid"].as_u64().map(|u| u as u32),
        gid: v["gid"].as_u64().map(|g| g as u32),
        hostname: v["hostname"].as_str().unwrap_or("caos-worker").to_string(),
        ip: v["ip"].as_str().unwrap_or("").to_string(),
        dns: strings("dns"),
    })
}

fn run(timing: &mut Timing) -> Result<i32, String> {
    // The kernel mounts devtmpfs on /dev itself (DEVTMPFS_MOUNT) when the image
    // has a /dev; mounting again only answers EBUSY, which is fine.
    let _ = mount("devtmpfs", "/dev", "devtmpfs", 0, "");
    let spec = read_spec()?;

    // 1. The overlay root. Everything is staged on the scratch disk so that no
    //    mount is an ancestor of another one we later move.
    mount("/dev/vdb", STAGE, "ext4", 0, "")?;
    for d in ["upper", "work", "lower", "root"] {
        mkdir(&format!("{STAGE}/{d}"))?;
    }
    mount(
        "/dev/vda",
        &format!("{STAGE}/lower"),
        "ext4",
        sys::MS_RDONLY,
        "",
    )?;
    mount(
        "overlay",
        &format!("{STAGE}/root"),
        "overlay",
        0,
        &format!("lowerdir={STAGE}/lower,upperdir={STAGE}/upper,workdir={STAGE}/work"),
    )?;
    let root = format!("{STAGE}/root");
    mkdir(&format!("{root}/dev"))?;
    mount("/dev", &format!("{root}/dev"), "", sys::MS_MOVE, "")?;
    std::env::set_current_dir(&root).map_err(|e| format!("chdir {root}: {e}"))?;
    mount(".", "/", "", sys::MS_MOVE, "")?;
    if unsafe { sys::chroot(cstr(".").as_ptr()) } != 0 {
        return Err(format!("chroot: {}", std::io::Error::last_os_error()));
    }
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;

    // 2. What an engine mounts and writes before the entrypoint.
    let nosuid_nodev_noexec = sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC;
    for d in ["/proc", "/sys", "/dev/pts", "/dev/shm"] {
        mkdir(d)?;
    }
    mount("proc", "/proc", "proc", nosuid_nodev_noexec, "")?;
    mount("sysfs", "/sys", "sysfs", nosuid_nodev_noexec, "")?;
    // Best-effort: nothing a caos worker runs today needs cgroups, but an
    // engine would provide them.
    mkdir("/dev/mqueue")?;
    let _ = mount("mqueue", "/dev/mqueue", "mqueue", nosuid_nodev_noexec, "");
    let _ = mount(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        nosuid_nodev_noexec,
        "",
    );
    mount(
        "devpts",
        "/dev/pts",
        "devpts",
        sys::MS_NOSUID | sys::MS_NOEXEC,
        "newinstance,ptmxmode=0666,mode=0620,gid=5",
    )?;
    mount(
        "shm",
        "/dev/shm",
        "tmpfs",
        nosuid_nodev_noexec,
        "mode=1777,size=65536k",
    )?;
    for (link, target) in [
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ] {
        let _ = std::fs::remove_file(link);
        std::os::unix::fs::symlink(target, link).map_err(|e| format!("linking {link}: {e}"))?;
    }
    let _ = std::fs::remove_file("/dev/ptmx");
    std::os::unix::fs::symlink("pts/ptmx", "/dev/ptmx")
        .map_err(|e| format!("linking /dev/ptmx: {e}"))?;
    loopback_up()?;
    let name = cstr(&spec.hostname);
    if unsafe { sys::sethostname(name.as_ptr(), spec.hostname.len()) } != 0 {
        return Err(format!("sethostname: {}", std::io::Error::last_os_error()));
    }
    replace_file("/etc/hostname", &format!("{}\n", spec.hostname))?;
    let mut hosts = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n".to_string();
    if !spec.ip.is_empty() {
        hosts.push_str(&format!("{}\t{}\n", spec.ip, spec.hostname));
    }
    replace_file("/etc/hosts", &hosts)?;
    let resolv: String = spec
        .dns
        .iter()
        .map(|ns| format!("nameserver {ns}\n"))
        .collect();
    replace_file("/etc/resolv.conf", &resolv)?;

    // 3. The job.
    let mut command = Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spec.cwd)
        .stdin(Stdio::null());
    if let Some(gid) = spec.gid {
        command.gid(gid);
    }
    if let Some(uid) = spec.uid {
        command.uid(uid);
    }
    timing.boot_ms = uptime_ms();
    let started = std::time::Instant::now();
    let child = command
        .spawn()
        .map_err(|e| format!("starting {}: {e}", spec.argv[0]))?;
    let child_pid = child.id() as i32;
    // As PID 1, every orphan in the VM is ours to reap; the job's own status is
    // the one we keep.
    loop {
        let mut status = 0;
        let pid = unsafe { sys::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
        }
        if pid == child_pid {
            timing.job_ms = started.elapsed().as_millis() as u64;
            let code = if status & 0x7f == 0 {
                (status >> 8) & 0xff
            } else {
                128 + (status & 0x7f)
            };
            return Ok(code);
        }
    }
}

/// `ip link set lo up`, without an `ip` in the image: read the flags, add UP.
fn loopback_up() -> Result<(), String> {
    #[repr(C)]
    struct IfReq {
        name: [u8; 16],
        flags: i16,
        _pad: [u8; 22],
    }
    let mut req = IfReq {
        name: [0; 16],
        flags: 0,
        _pad: [0; 22],
    };
    req.name[..2].copy_from_slice(b"lo");
    unsafe {
        let fd = sys::socket(sys::AF_INET, sys::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(format!("socket: {}", std::io::Error::last_os_error()));
        }
        let ok = sys::ioctl(fd, sys::SIOCGIFFLAGS, &mut req as *mut IfReq) == 0 && {
            req.flags |= sys::IFF_UP;
            sys::ioctl(fd, sys::SIOCSIFFLAGS, &mut req as *mut IfReq) == 0
        };
        let err = std::io::Error::last_os_error();
        sys::close(fd);
        if !ok {
            return Err(format!("bringing lo up: {err}"));
        }
    }
    Ok(())
}
