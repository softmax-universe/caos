//! `caos vm-init`: PID 1 of a Firecracker runner appliance
//! (design/firecracker-runners.md, "Appliance boot contract").
//!
//! The appliance's initrd loads the virtio and ext4 modules, configures `eth0`
//! from the kernel command line, mounts the VM's private root disk, and
//! `switch_root`s here. This then:
//!
//! 1. mounts the pseudo-filesystems the runner and worker expect;
//! 2. reads the per-VM boot config from the raw CONFIG disk (`/dev/vdb`): the
//!    initial job payload and the boot env (server URL, redis address, the
//!    host's required set). It comes from a disk and not the kernel command
//!    line, because `/proc/cmdline` is readable by the worker and the payload
//!    carries the runner token and the job's secrets;
//! 3. runs `caos runner --job=<payload>` with the image env baked into the
//!    appliance (`/etc/caos/image-env`) plus the boot env, and with its stdout
//!    on the raw REPORT disk (`/dev/vdc`). That is where the claim records go
//!    (`caos_world::runner_lineage`), and the host reads it after the VM exits;
//! 4. reaps orphans until the runner exits, appends the exit record to the
//!    report, syncs, and reboots. Under `reboot=k` a guest reboot makes
//!    Firecracker exit, so the host sees the VM end exactly when its runner does.
//!
//! The console carries the runner's stderr (its workers' masked output), plus
//! one readiness line, [`READY_LINE`], which the host waits for to tell a boot
//! failure from a slow job.
//!
//! `/bin/caos` is setuid root, so this REFUSES unless it is PID 1 with a real
//! uid of 0. A worker can be neither.

use std::fs::File;

use caos_world::appliance::{BootConfig, READY_LINE};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

const CONFIG_DEVICE: &str = "/dev/vdb";
const REPORT_DEVICE: &str = "/dev/vdc";
/// `KEY=VALUE` lines baked in by the appliance build, from the same definition
/// as the matching Docker base's image config (`nix/firecracker-appliance.nix`).
const IMAGE_ENV_FILE: &str = "/etc/caos/image-env";

/// Parse `/etc/caos/image-env`: `KEY=VALUE` per line; blank lines and `#`
/// comments ignored.
pub fn parse_image_env(text: &str) -> Result<Vec<(String, String)>, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
            _ => Err(format!("{IMAGE_ENV_FILE}: not KEY=VALUE: {l:?}")),
        })
        .collect()
}

extern "C" {
    fn getpid() -> i32;
    fn getuid() -> u32;
    fn mount(
        source: *const std::ffi::c_char,
        target: *const std::ffi::c_char,
        fstype: *const std::ffi::c_char,
        flags: std::ffi::c_ulong,
        data: *const std::ffi::c_void,
    ) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn sync();
    fn reboot(cmd: i32) -> i32;
}

/// `caos vm-init`. Never returns: it ends the VM.
pub fn run() -> Result<(), String> {
    // SAFETY: plain syscalls with no arguments.
    if unsafe { getpid() } != 1 || unsafe { getuid() } != 0 {
        return Err("vm-init runs only as PID 1, as root, inside a runner appliance".to_string());
    }
    let mut report = None;
    let code = match boot_and_run(&mut report) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("caos-vm-init: {error}");
            // No runner ran, so there is no report but this one: the host,
            // having seen no READY_LINE, reports a boot failure.
            -1
        }
    };
    // Through OUR dup of the runner's stdout: the two share one file offset,
    // so this lands after the last claim record. (O_APPEND on a raw block
    // device would not: it wrote at offset 0, over the claims.)
    let report = report.or_else(|| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(REPORT_DEVICE)
            .ok()
    });
    if let Some(mut report) = report {
        let _ = writeln!(report, "{}", caos_world::runner_lineage::exit_record(code));
        let _ = report.sync_all();
    }
    // SAFETY: flush, then LINUX_REBOOT_CMD_RESTART. Nothing is left to protect.
    unsafe {
        sync();
        reboot(0x0123_4567);
    }
    // reboot(2) does not return on success; if it does, PID 1 must not exit.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Everything up to the runner's exit. Returns its exit code, 128 + signal for
/// a signal, as a shell would.
fn boot_and_run(report_out: &mut Option<File>) -> Result<i32, String> {
    mount_pseudo_filesystems();
    for dir in ["/tmp", "/var/tmp"] {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {dir}: {e}"))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777))
            .map_err(|e| format!("chmod {dir}: {e}"))?;
    }
    let mut raw = Vec::new();
    File::open(CONFIG_DEVICE)
        .and_then(|mut f| f.read_to_end(&mut raw))
        .map_err(|e| format!("reading the boot config ({CONFIG_DEVICE}): {e}"))?;
    let config = BootConfig::decode(&raw)?;
    let image_env = match std::fs::read_to_string(IMAGE_ENV_FILE) {
        Ok(text) => parse_image_env(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("reading {IMAGE_ENV_FILE}: {e}")),
    };
    // The report is opened WRITE from offset 0 and handed to the runner as
    // stdout, so the claim records start at the device's beginning. We keep a
    // dup, which shares the offset, for the exit record.
    let report = std::fs::OpenOptions::new()
        .write(true)
        .open(REPORT_DEVICE)
        .map_err(|e| format!("opening the report ({REPORT_DEVICE}): {e}"))?;
    *report_out = Some(
        report
            .try_clone()
            .map_err(|e| format!("duplicating the report: {e}"))?,
    );
    let mut child = Command::new("/bin/caos")
        .arg("runner")
        .arg(format!("--job={}", config.job))
        .env_clear()
        .envs(image_env)
        .envs(&config.env)
        .stdin(Stdio::null())
        .stdout(report)
        .spawn()
        .map_err(|e| format!("starting the runner: {e}"))?;
    eprintln!("{READY_LINE}");
    let runner = child.id() as i32;
    // As PID 1 we inherit every orphan, so reap them all and stop at the
    // runner. `child.wait()` would reap only the runner and leave zombies.
    loop {
        let mut status = 0;
        // SAFETY: waitpid(-1) with a valid status pointer.
        let pid = unsafe { waitpid(-1, &mut status, 0) };
        if pid == runner {
            let status = std::process::ExitStatus::from_raw(status);
            std::mem::forget(child);
            return Ok(status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)));
        }
        if pid < 0 {
            // ECHILD with the runner never seen: should not happen.
            return child
                .wait()
                .map(|s| s.code().unwrap_or(-1))
                .map_err(|e| format!("waiting for the runner: {e}"));
        }
    }
}

/// Mount what a Linux userland expects. An already-mounted target (the
/// initrd may have moved one in) is fine, so failures are reported, not fatal:
/// the runner fails loudly on whatever is really missing.
fn mount_pseudo_filesystems() {
    let mounts: [(&str, &str, &str, &str); 6] = [
        ("proc", "/proc", "proc", ""),
        ("sysfs", "/sys", "sysfs", ""),
        ("devtmpfs", "/dev", "devtmpfs", ""),
        ("devpts", "/dev/pts", "devpts", "mode=0620,ptmxmode=0666"),
        ("tmpfs", "/dev/shm", "tmpfs", "mode=1777"),
        ("tmpfs", "/run", "tmpfs", "mode=0755"),
    ];
    for (source, target, fstype, data) in mounts {
        let _ = std::fs::create_dir_all(target);
        let c = |s: &str| std::ffi::CString::new(s).expect("no NUL in a constant");
        let (source_c, target_c, fstype_c, data_c) = (c(source), c(target), c(fstype), c(data));
        // SAFETY: valid NUL-terminated strings that outlive the call.
        let rc = unsafe {
            mount(
                source_c.as_ptr(),
                target_c.as_ptr(),
                fstype_c.as_ptr(),
                0,
                data_c.as_ptr().cast(),
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(16) {
                // 16 = EBUSY: already mounted there.
                eprintln!("caos-vm-init: mounting {target}: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_env_is_key_value_lines() {
        let env = parse_image_env("# baked\nPATH=/bin\n\nCAOS_RESIDENT=1\nX=a=b\n").unwrap();
        assert_eq!(
            env,
            vec![
                ("PATH".to_string(), "/bin".to_string()),
                ("CAOS_RESIDENT".to_string(), "1".to_string()),
                ("X".to_string(), "a=b".to_string()),
            ]
        );
        assert!(parse_image_env("nonsense\n").is_err());
    }
}
