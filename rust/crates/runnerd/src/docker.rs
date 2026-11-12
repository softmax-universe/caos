//! The Docker backend: one worker container per slot (`docker run … --entrypoint
//! /bin/caos <image_ref> runner --job=<json>`), with the image's own grants.
//! See the crate docs for the shape; this module is everything about it that is
//! specific to a container engine.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;

use crate::{env_or, owner_id, worker_env, Backend, Common, Job, Outcome};

/// The caos binary inside every compute image, forced as the entrypoint.
const CAOS_BIN: &str = "/bin/caos";

/// Fixed in-container path where a granted engine socket is bind-mounted
/// (`CAOS_RUNNER_SOCKET`); advertised to the worker as `CAOS_ENGINE_SOCKET`.
/// Its HOST path rides alongside as `CAOS_ENGINE_SOCKET_HOST`, for a worker
/// that has to mount it into a container of its own — see `run_container`.
const ENGINE_SOCKET_PATH: &str = "/run/caos/engine.sock";

/// An image declares in its own config env that it hosts a caos stack and
/// needs the engine socket (design/test-stack-image.md). Same shape as the
/// `CAOS_WORKER_UID=0` root grant: the image states what containment it needs,
/// and the runner decides whether to honor it. Before this, every worker in
/// the pool got the socket because one image needed it.
const SOCKET_GRANT_ENV: &str = "CAOS_GRANT_ENGINE_SOCKET";

/// Declared the same way and read from the same place: the mountpoints an image
/// wants backed by persistent named volumes. See `ImageGrants::volumes`.
const VOLUME_GRANT_ENV: &str = "CAOS_GRANT_VOLUMES";

/// Declared the same way: this image needs CAP_SYS_ADMIN. The one caller is an
/// image that mounts a persistent store OVER its own — `mount --bind` is the
/// only way to replace a directory whose replacement lives on a different
/// filesystem (rename is EXDEV), and it needs the capability.
const SYS_ADMIN_GRANT_ENV: &str = "CAOS_GRANT_SYS_ADMIN";

/// Declared the same way: device nodes this image needs passed through.
///
/// The one caller wants `/dev/fuse`, because it runs a container engine of its
/// own and that engine's overlay driver is `fuse-overlayfs` here. Without the
/// device it falls back to `vfs`, which copies a whole rootfs per container
/// instead of layering — measured at 1.84s against 0.25s for three starts of a
/// 120 MB image, and the gap grows with image size.
const DEVICE_GRANT_ENV: &str = "CAOS_GRANT_DEVICES";

/// Label stamped on every worker container we start, so a restarted runnerd can
/// find and delete the containers its predecessor left behind. Its value is
/// `owner_id()`, not a constant: containers are not `--rm` any more, so the
/// reaper is what keeps a crash from leaking them.
const OWNER_LABEL: &str = "caos.runnerd.owner";

pub(crate) struct Config {
    network: String,
    docker_bin: String,
    /// Global args inserted *before* the `run` subcommand — e.g.
    /// `--remote --url unix://…` so an INNER runnerd delegates to an outer
    /// engine's API socket (the sibling/socket-delegation backend,
    /// design/cargo-workers.md phase 4) instead of running a nested runtime.
    docker_args: Vec<String>,
    /// The engine socket this runnerd can grant. It is bind-mounted only into
    /// worker containers whose IMAGE asks for it (`SOCKET_GRANT_ENV` in the
    /// image's own config env) — the socket is root-equivalent over this
    /// engine, so it is a per-image grant, declared by the image the same way
    /// `CAOS_WORKER_UID=0` declares its root grant. Unset here means no worker
    /// can be granted it at all, whatever the image claims.
    socket: Option<String>,
    /// Memo for `image_grants`: image ref -> what it declared. Refs are
    /// content-addressed, so a cached answer cannot go stale, and the same
    /// handful of images run over and over.
    grants: Mutex<HashMap<String, ImageGrants>>,
    /// Ceilings every worker container runs under. See [`Limits`].
    limits: Limits,
    /// Which images may be granted host-level power. See [`Trust`].
    trust: Trust,
}

/// Resource ceilings for a worker container, as `docker run` flags.
///
/// Capacity here is counted in SLOTS, and a slot says nothing about memory: one
/// runaway `cargo test` or an agent's bash tool could take the whole host, and
/// the host is where redis, the registry and the server also live (caosd is one
/// process group), so a worker that eats the machine takes the stack with it.
///
/// Each ceiling is an env var with three readings: unset means the DEFAULT
/// below, `0` (or `none`) means no limit, anything else is passed to the engine
/// verbatim (`CAOS_WORKER_MEMORY=16g`, `CAOS_WORKER_CPUS=4`,
/// `CAOS_WORKER_PIDS_LIMIT=4096`).
///
/// The defaults are OPPORTUNISTIC, an explicit value is not. A default applies
/// only where `docker info` reports the engine can enforce it (a Docker engine
/// on cgroup v2 says `MemoryLimit: true`, `PidsLimit: true`); the podman a dev
/// stack carries runs with `cgroups = "disabled"` (dev/devbox/flake.nix) and
/// reports neither, and a limit it cannot apply would fail every job rather
/// than bound one. A value the operator SET is passed through whatever the
/// engine says, so a misconfiguration fails loudly on the first job instead of
/// silently running unbounded.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Limits {
    /// `--memory`. Default: three quarters of the host's MemTotal, so no
    /// single worker can take the stack's own share.
    memory: Option<String>,
    /// `--cpus`. No default: a cap on CPU slows work without protecting
    /// anything, and the kernel already shares fairly.
    cpus: Option<String>,
    /// `--pids-limit`. Default 32768 (the classic pid_max): room for a dev
    /// stack's nested engine and its 48 workers, fatal for a fork bomb.
    pids: Option<String>,
}

/// Three-quarters of the host's memory, as a `--memory` value in MiB.
fn default_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib / 1024 * 3 / 4).filter(|mib| *mib > 0)
}

/// One ceiling's env var: `None` for "no limit", `Some(value)` to pass on.
/// `default` is used when the var is unset, and only when `supported`.
fn limit_from_env(name: &str, default: Option<String>, supported: bool) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim().to_string();
            if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(value)
            }
        }
        Err(_) => default.filter(|_| supported),
    }
}

/// What the engine says it can enforce: `(memory, pids)`. Docker's `info`
/// carries both as top-level booleans; anything else (podman's differs, and an
/// engine that cannot be asked) reads as "neither", which only withholds the
/// DEFAULTS — an explicit `CAOS_WORKER_*` still applies.
fn engine_limit_support(docker_bin: &str, docker_args: &[String]) -> (bool, bool) {
    let out = Command::new(docker_bin)
        .args(docker_args)
        .args(["info", "--format", "{{json .}}"])
        .output();
    let Ok(out) = out else { return (false, false) };
    if !out.status.success() {
        return (false, false);
    }
    let Ok(info) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return (false, false);
    };
    let flag = |name: &str| info.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
    (flag("MemoryLimit"), flag("PidsLimit"))
}

impl Limits {
    fn from_env(docker_bin: &str, docker_args: &[String]) -> Limits {
        let (memory_ok, pids_ok) = engine_limit_support(docker_bin, docker_args);
        Limits {
            memory: limit_from_env(
                "CAOS_WORKER_MEMORY",
                default_memory_mib().map(|mib| format!("{mib}m")),
                memory_ok,
            ),
            cpus: limit_from_env("CAOS_WORKER_CPUS", None, true),
            pids: limit_from_env("CAOS_WORKER_PIDS_LIMIT", Some("32768".to_string()), pids_ok),
        }
    }

    /// The `docker run` flags for these ceilings.
    fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(memory) = &self.memory {
            args.extend(["--memory".to_string(), memory.clone()]);
        }
        if let Some(cpus) = &self.cpus {
            args.extend(["--cpus".to_string(), cpus.clone()]);
        }
        if let Some(pids) = &self.pids {
            args.extend(["--pids-limit".to_string(), pids.clone()]);
        }
        args
    }

    fn describe(&self) -> String {
        let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "unlimited".to_string());
        format!(
            "memory {}, cpus {}, pids {}",
            show(&self.memory),
            show(&self.cpus),
            show(&self.pids)
        )
    }
}

/// Which images this runner will grant host-level power to: the engine socket,
/// CAP_SYS_ADMIN (with seccomp and AppArmor off), and device passthrough. Each
/// is root-equivalent on the host, or close to it.
///
/// A grant is DECLARED by the image's author, in the image's own config env,
/// and that is a statement of need, not of trust: anyone who can publish an
/// image to this server can declare one. Whether the declaration is honored is
/// this runner's decision, made from `CAOS_RUNNER_TRUSTED_IMAGES`:
///
/// - unset: every declared grant is honored (how it always was — right for a
///   single-tenant host, and logged as such per image so it is a choice);
/// - `none`: no image is granted any of the three, whatever it declares;
/// - a space- or comma-separated list of `sha256:<digest>` (or full refs):
///   only those images are. A digest is the image's content, so the list
///   names exactly the images someone looked at.
///
/// `CAOS_GRANT_VOLUMES` is outside this gate on purpose: runnerd derives the
/// volume NAMES, so a volume grant can reach only this deployment's own
/// storage, never the host's.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Trust {
    Everyone,
    Only(Vec<String>),
}

impl Trust {
    fn from_env() -> Trust {
        match std::env::var("CAOS_RUNNER_TRUSTED_IMAGES") {
            Ok(list) if !list.trim().is_empty() => Trust::Only(
                list.split(|c: char| c.is_whitespace() || c == ',')
                    .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
                    .map(str::to_string)
                    .collect(),
            ),
            _ => Trust::Everyone,
        }
    }

    /// Is `image_ref` one this runner grants host-level power to? A listed
    /// entry matches the whole ref or its digest (the part after `@`).
    fn allows(&self, image_ref: &str) -> bool {
        match self {
            Trust::Everyone => true,
            Trust::Only(list) => {
                let digest = image_ref.rsplit_once('@').map(|(_, d)| d);
                list.iter()
                    .any(|entry| entry == image_ref || Some(entry.as_str()) == digest)
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Trust::Everyone => "granting every image what it declares".to_string(),
            Trust::Only(list) if list.is_empty() => "granting no image host power".to_string(),
            Trust::Only(list) => format!("granting host power to {} trusted image(s)", list.len()),
        }
    }
}

impl Config {
    pub(crate) fn from_env() -> Config {
        let docker_bin = env_or("CAOS_DOCKER_BIN", "docker");
        let docker_args: Vec<String> = std::env::var("CAOS_DOCKER_ARGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let limits = Limits::from_env(&docker_bin, &docker_args);
        Config {
            network: env_or("CAOS_DOCKER_NETWORK", "caos-net"),
            docker_bin,
            docker_args,
            grants: Mutex::new(HashMap::new()),
            socket: std::env::var("CAOS_RUNNER_SOCKET")
                .ok()
                .filter(|s| !s.is_empty()),
            limits,
            trust: Trust::from_env(),
        }
    }
}

impl Backend for Config {
    fn describe(&self) -> String {
        format!(
            "network {}, limits ({}), {}",
            self.network,
            self.limits.describe(),
            self.trust.describe()
        )
    }

    fn reap_leftovers(&self) {
        reap_leftovers(self);
    }

    fn run(&self, common: &Common, slot: u32, job: &Job) -> Outcome {
        run_container(self, common, slot, job)
    }
}

/// What an image asks for in its own config env. Both are read from the IMAGE,
/// never from a job or its args, so a caller cannot talk its way into either —
/// only the image's author can, and an image is content-addressed.
#[derive(Clone, Default)]
struct ImageGrants {
    /// `CAOS_GRANT_ENGINE_SOCKET=1`.
    socket: bool,
    /// `CAOS_GRANT_VOLUMES=<abs path> [<abs path>…]` — mountpoints this image
    /// wants backed by persistent named volumes.
    ///
    /// NOT EXCLUSIVE, and that is the whole design rather than an omission.
    /// What lands in them — a nix store, a git object database — is
    /// content-addressed and safe for concurrent writers, so several workers
    /// share one volume rather than queueing for it. The one thing that is NOT
    /// safe that way is redis, which is why a dev stack points at an existing
    /// redis instead of running its own on a shared directory: two servers on
    /// one dbdir both start, neither locks, and they interleave one AOF between
    /// two disagreeing datasets (measured).
    ///
    /// The image names PATHS; runnerd names the volumes. So an image cannot
    /// address another deployment's storage, and nothing about the host's
    /// filesystem layout has to reach a worker.
    volumes: Vec<String>,
    /// `CAOS_GRANT_DEVICES=<abs path> [<abs path>…]` — device nodes to pass
    /// through. Validated like a mountpoint; runnerd decides nothing about what
    /// they are, only that the image asked by absolute traversal-free path.
    devices: Vec<String>,
    /// `CAOS_GRANT_SYS_ADMIN=1` — add CAP_SYS_ADMIN.
    ///
    /// Asked for by exactly one thing: an image that mounts a persistent store
    /// over its own with `mount --bind`. Nothing else can do that job. A
    /// volume and a container's overlay are different filesystems (measured:
    /// device 178 against 37), so `rename` and `renameat2(RENAME_EXCHANGE)` are
    /// both EXDEV, and a symlink swap cannot be atomic — the `ln` that would
    /// make it has its ELF interpreter under the very path being replaced, so
    /// it cannot even start once the old one is gone.
    ///
    /// SMALLER THAN IT SOUNDS, in this company. It is CAP_SYS_ADMIN inside the
    /// container's user namespace, and the image that asks for it already holds
    /// the engine socket, which is root-equivalent on the HOST. This does not
    /// widen what that image can reach; it lets it arrange its own filesystem.
    sys_admin: bool,
}

/// The volume backing `path` for this deployment. Derived, never taken from the
/// image: a mountpoint is a claim about the container's own filesystem, which is
/// safe for an image to make, while a volume NAME is a claim about shared
/// storage, which is not.
fn volume_name(path: &str) -> String {
    let slug: String = path
        .trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("caos-vol-{slug}")
}

/// Is this a mountpoint an image may ask for? Absolute, no traversal, no room
/// for a second `-v` argument to hide in.
fn valid_mountpoint(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && !path.contains("..")
        && !path.contains(char::is_whitespace)
        && !path.contains(':')
}

/// Does this image ASK for the engine socket (`SOCKET_GRANT_ENV` in its config
/// env)? Read from the image itself, not from the job or its args, so a caller
/// cannot talk its way into the grant — only the image's author can, and the
/// image is content-addressed.
///
/// `image inspect` needs the image present, and `run` is what normally pulls
/// it, so a miss pulls first. Both answers are memoized per ref: a wrong
/// answer here is a silent loss of containment, so the failure mode is chosen
/// deliberately — anything that goes wrong reads as "no grant".
fn image_grants(config: &Config, image_ref: &str) -> ImageGrants {
    if let Some(known) = config.grants.lock().expect("grant memo").get(image_ref) {
        return known.clone();
    }
    let inspect = |args: &[&str]| {
        Command::new(&config.docker_bin)
            .args(&config.docker_args)
            .args(args)
            .output()
    };
    let mut out = inspect(&[
        "image",
        "inspect",
        "--format",
        "{{range .Config.Env}}{{println .}}{{end}}",
        image_ref,
    ]);
    if !matches!(&out, Ok(o) if o.status.success()) {
        // Not pulled yet: `run` would have fetched it, so fetch it here.
        let _ = inspect(&["pull", image_ref]);
        out = inspect(&[
            "image",
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            image_ref,
        ]);
    }
    let mut grants = ImageGrants::default();
    match &out {
        Ok(o) if o.status.success() => {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let line = line.trim();
                if line == format!("{SOCKET_GRANT_ENV}=1") {
                    grants.socket = true;
                } else if line == format!("{SYS_ADMIN_GRANT_ENV}=1") {
                    grants.sys_admin = true;
                } else if let Some(list) = line.strip_prefix(&format!("{DEVICE_GRANT_ENV}=")) {
                    for path in list.split_whitespace() {
                        if valid_mountpoint(path) {
                            grants.devices.push(path.to_string());
                        } else {
                            eprintln!("caos-runnerd: {image_ref} asks for device {path:?}, which is not an absolute traversal-free path; ignoring it");
                        }
                    }
                } else if let Some(list) = line.strip_prefix(&format!("{VOLUME_GRANT_ENV}=")) {
                    for path in list.split_whitespace() {
                        if valid_mountpoint(path) {
                            grants.volumes.push(path.to_string());
                        } else {
                            eprintln!(
                                "caos-runnerd: {image_ref} asks for mountpoint {path:?}, \
                                 which is not an absolute traversal-free path; ignoring it"
                            );
                        }
                    }
                }
            }
        }
        // The wrong answer here is a silent loss of containment, so anything
        // that goes wrong reads as "nothing granted".
        _ => eprintln!("caos-runnerd: cannot inspect {image_ref}; granting it nothing"),
    }
    // The trust decision, applied where the grants are decided so no later
    // reader can forget it: an untrusted image's host-level declarations are
    // dropped here, and the memo below remembers the DECISION, not the claim.
    let asks_host_power = grants.socket || grants.sys_admin || !grants.devices.is_empty();
    if asks_host_power && !config.trust.allows(image_ref) {
        eprintln!(
            "caos-runnerd: {image_ref} declares host-level grants ({}) but is not in \
             CAOS_RUNNER_TRUSTED_IMAGES; refusing them",
            [
                grants.socket.then_some(SOCKET_GRANT_ENV),
                grants.sys_admin.then_some(SYS_ADMIN_GRANT_ENV),
                (!grants.devices.is_empty()).then_some(DEVICE_GRANT_ENV),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
        );
        grants.socket = false;
        grants.sys_admin = false;
        grants.devices.clear();
    } else if asks_host_power && config.trust == Trust::Everyone {
        eprintln!(
            "caos-runnerd: {image_ref} declares host-level grants and CAOS_RUNNER_TRUSTED_IMAGES \
             is unset, so its own declaration is trusted"
        );
    }
    if grants.socket {
        eprintln!("caos-runnerd: {image_ref} declares {SOCKET_GRANT_ENV}; granting engine socket");
    }
    if !grants.volumes.is_empty() {
        eprintln!(
            "caos-runnerd: {image_ref} declares {VOLUME_GRANT_ENV}; granting {}",
            grants.volumes.join(" ")
        );
    }
    config
        .grants
        .lock()
        .expect("grant memo")
        .insert(image_ref.to_string(), grants.clone());
    grants
}

/// Run the job's container and wait it out. The container posts its own
/// results; the caller backstops a crash from the [`Outcome`] (nonzero exit
/// means it may never have reported).
///
/// Deliberately NOT `--rm`: an *attached* `docker run --rm` waits with
/// `condition=removed`, i.e. `POST /containers/<id>/wait?condition=removed`,
/// and podman's compat handler spins a core on that call for as long as the
/// container is stopped-but-not-yet-removed. Podman also loses auto-removals
/// outright (containers sit in `Exited` with `AutoRemove=true` forever), which
/// turns that spin into a permanent one. So we take exit-wait only — which the
/// engine answers from an event, not a poll — and delete the container
/// ourselves afterwards.
fn run_container(config: &Config, common: &Common, slot: u32, job: &Job) -> Outcome {
    eprintln!(
        "runnerd slot {slot}: arg_tree {} -> container ({})",
        job.arg_tree, job.image_ref
    );
    let name = format!("caos-worker-{}", job.nonce);
    let mut command = Command::new(&config.docker_bin);
    command
        .args(&config.docker_args) // global flags (e.g. --remote --url …) precede `run`
        .arg("run")
        .args(["--name", &name])
        // Our mark for the crash reaper — see `reap_leftovers`. Scoped to this
        // runnerd, because an INNER runnerd delegating to the outer engine
        // (`--remote --url …`) shares a container namespace with the outer one,
        // and must not sweep its siblings' containers.
        .args(["--label", &format!("{OWNER_LABEL}={}", owner_id())])
        .args(["--network", &config.network])
        // The log reaches us through the attach stream (`output()` below), so
        // the engine's own copy is a second one nobody reads: conmon wrote it to
        // the store's disk at ~19 MB/s in a suite run, fsyncing as it went, on
        // the volume whose fsync latency is what every container start waits on.
        .args(["--log-driver", "none"])
        .args(config.limits.args());
    // One inspect answers both grants, memoized together.
    let grants = image_grants(config, &job.image_ref);
    if let Some(sock) = &config.socket {
        if grants.socket {
            // Hand THIS worker the engine socket so its own inner runnerd can
            // delegate sibling containers to this engine (phase 4). Bind it at
            // a fixed in-container path and advertise that path, so a worker
            // that only TALKS to the engine never needs to know the host
            // socket's location.
            //
            // …and the host location too, for the one worker that cannot use
            // the fixed path: a bind mount's source is resolved on the HOST, so
            // a worker CREATING a container that itself needs the engine can
            // only name the socket as the host sees it. That is how a shared
            // test stack gets started as a detached sibling
            // (design/faster-tests.md).
            //
            // Not a second grant, and not a wider one: it sits inside the same
            // per-image gate, and a holder of this socket can already read the
            // host path straight out of the engine by inspecting itself. This
            // hands it over rather than making it do the archaeology.
            command
                .args(["-v", &format!("{sock}:{ENGINE_SOCKET_PATH}")])
                .args(["-e", &format!("CAOS_ENGINE_SOCKET={ENGINE_SOCKET_PATH}")])
                .args(["-e", &format!("CAOS_ENGINE_SOCKET_HOST={sock}")]);
        }
    }
    // Persistent storage the image asked for. Named volumes rather than bind
    // mounts, deliberately: the engine owns the name, so nothing about the
    // host's filesystem has to be known by — or reach — a worker. It also seeds
    // itself, because mounting a FRESH named volume over a path that has content
    // in the image copies that content in (verified), which is what lets a nix
    // store survive here without any bootstrap step.
    if grants.sys_admin {
        command.args(["--cap-add", "SYS_ADMIN"]);
        // A nested container engine needs more than the capability: pivot_root
        // and keyctl are blocked by the default seccomp profile even with
        // SYS_ADMIN, so crun dies with "OCI permission denied" before the
        // container starts. Unconfined is the standard answer for a container
        // that runs its own engine — and this grant already implies that trust.
        command.args(["--security-opt", "seccomp=unconfined"]);
        // Same story one layer up: on hosts with AppArmor (Ubuntu's docker
        // ships the `docker-default` profile) `mount` is denied regardless of
        // the capability, so the bind over /nix fails with EPERM. Docker
        // Desktop and OrbStack have no AppArmor, which is why this only bites
        // on a Linux box.
        command.args(["--security-opt", "apparmor=unconfined"]);
    }
    for dev in &grants.devices {
        command.args(["--device", dev]);
    }
    for path in &grants.volumes {
        command.args(["-v", &format!("{}:{path}", volume_name(path))]);
    }
    // What every runner is told, whatever starts it (`worker_env`).
    for (name, value) in worker_env(common) {
        command.args(["-e", &format!("{name}={value}")]);
    }
    let out = command
        .args(["--entrypoint", CAOS_BIN])
        .arg(&job.image_ref)
        .arg("runner")
        .arg(format!("--job={}", job.payload))
        .output();
    // Delete it ourselves, now that we hold its exit status and its log. A
    // plain `rm -f` is one DELETE the engine answers synchronously — none of
    // the auto-remove machinery, and nothing polls.
    remove_container(config, &name);
    match out {
        Ok(out) => {
            // Relay the container's log (the runner relays its workers' output
            // to its stderr) so it survives the container's removal. NOT its
            // stdout: that carries only claim records, whose nonces stay out of
            // our log.
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let failure = (!out.status.success()).then(|| {
                (
                    format!("worker container exited with {}", out.status),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )
            });
            Outcome { failure, stdout }
        }
        Err(e) => Outcome {
            failure: Some((format!("running {}: {e}", config.docker_bin), String::new())),
            stdout: String::new(),
        },
    }
}

/// Delete one worker container. Best-effort and quiet: a container that is
/// already gone is the normal case for the reaper, and a removal that fails is
/// a leaked container, not a failed job — the job's result is already settled
/// by the time we get here.
fn remove_container(config: &Config, name_or_id: &str) {
    let out = Command::new(&config.docker_bin)
        .args(&config.docker_args)
        .args(["rm", "-f", name_or_id])
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!(
            "caos-runnerd: removing {name_or_id}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("caos-runnerd: removing {name_or_id}: {e}"),
    }
}

/// Delete the exited worker containers this runnerd's predecessor left behind.
/// Without `--rm` the engine no longer cleans up after a runnerd that died
/// mid-run, so this is the replacement — run once at startup, before any slot
/// claims a job.
///
/// `status=exited` is load-bearing as well as safe: our own live workers are
/// still running, and so are any siblings that happen to share the label.
fn reap_leftovers(config: &Config) {
    let filter = format!("label={OWNER_LABEL}={}", owner_id());
    let out = Command::new(&config.docker_bin)
        .args(&config.docker_args)
        .args([
            "ps",
            "-aq",
            "--filter",
            "status=exited",
            "--filter",
            &filter,
        ])
        .output();
    let listed = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            eprintln!(
                "caos-runnerd: listing leftover containers: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            eprintln!("caos-runnerd: listing leftover containers: {e}");
            return;
        }
    };
    let ids: Vec<&str> = listed.split_whitespace().collect();
    if ids.is_empty() {
        return;
    }
    eprintln!("caos-runnerd: reaping {} leftover container(s)", ids.len());
    for id in ids {
        remove_container(config, id);
    }
}

#[cfg(test)]
mod limits_and_trust_tests {
    use super::*;

    #[test]
    fn a_limit_is_flags_and_none_is_no_flags() {
        let limits = Limits {
            memory: Some("4g".to_string()),
            cpus: None,
            pids: Some("512".to_string()),
        };
        assert_eq!(limits.args(), ["--memory", "4g", "--pids-limit", "512"]);
        assert!(Limits::default().args().is_empty());
    }

    #[test]
    fn a_set_value_applies_whatever_the_engine_says_and_zero_lifts_it() {
        // No engine support: the default is withheld, but a set value is not.
        assert_eq!(
            limit_from_env("CAOS_TEST_LIMIT_UNSET", Some("1g".into()), false),
            None
        );
        assert_eq!(
            limit_from_env("CAOS_TEST_LIMIT_UNSET", Some("1g".into()), true),
            Some("1g".to_string())
        );
        std::env::set_var("CAOS_TEST_LIMIT_SET", "2g");
        assert_eq!(
            limit_from_env("CAOS_TEST_LIMIT_SET", Some("1g".into()), false),
            Some("2g".to_string())
        );
        for lifted in ["0", "none", "NONE", ""] {
            std::env::set_var("CAOS_TEST_LIMIT_LIFTED", lifted);
            assert_eq!(
                limit_from_env("CAOS_TEST_LIMIT_LIFTED", Some("1g".into()), true),
                None
            );
        }
    }

    #[test]
    fn trust_matches_a_digest_or_a_whole_ref() {
        let digest = "sha256:0123456789abcdef";
        let image = format!("localhost:5000/caos/images@{digest}");
        let only = Trust::Only(vec![digest.to_string()]);
        assert!(only.allows(&image));
        assert!(!only.allows("localhost:5000/caos/images@sha256:ffff"));
        assert!(Trust::Only(vec![image.clone()]).allows(&image));
        assert!(!Trust::Only(vec![]).allows(&image));
        assert!(Trust::Everyone.allows(&image));
    }

    #[test]
    fn trust_from_env_reads_unset_none_and_a_list() {
        std::env::remove_var("CAOS_RUNNER_TRUSTED_IMAGES");
        assert_eq!(Trust::from_env(), Trust::Everyone);
        std::env::set_var("CAOS_RUNNER_TRUSTED_IMAGES", "none");
        assert_eq!(Trust::from_env(), Trust::Only(vec![]));
        std::env::set_var(
            "CAOS_RUNNER_TRUSTED_IMAGES",
            "sha256:aa, sha256:bb sha256:cc",
        );
        assert_eq!(
            Trust::from_env(),
            Trust::Only(vec![
                "sha256:aa".into(),
                "sha256:bb".into(),
                "sha256:cc".into()
            ])
        );
        std::env::remove_var("CAOS_RUNNER_TRUSTED_IMAGES");
    }
}
