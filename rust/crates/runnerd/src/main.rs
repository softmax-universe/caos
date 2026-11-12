//! caos-runnerd: the generic runner — the host agent that mints fresh worker
//! containers (see `design/runner-protocol.md`).
//!
//! Each of its slots long-polls the server's `POST /runner/poll` with the
//! required args from `CAOS_RUNNER_REQUIRE` — *none* by default, so the generic
//! pool matches any job that does not itself demand a pool. On a job it runs
//!
//! ```text
//! docker run --name caos-worker-<nonce> --label caos.runnerd.owner=<id> \
//!     --network <net> -e CAOS_SERVER_URL=<url> [-e CAOS_WORKER_REDIS_ADDR=<a>] \
//!     --entrypoint /bin/caos <image_ref> runner --job=<json>
//! ```
//!
//! and waits for the container to exit, then removes it itself (`docker rm -f`)
//! — NOT `--rm`, which would make the CLI wait on `condition=removed` and pin a
//! core inside podman; see `run_container`. The container owns the job from here:
//! it posts the result itself, then polls for more work for its image (that's
//! what makes it a warm runner) — this slot doesn't poll again until the
//! container dies, so each slot is exactly one machine's worth of capacity.
//! runnerd is only the crash backstop: a container that exits nonzero may have
//! died before posting, so runnerd posts a failure result with the captured
//! log (harmlessly answered 410 if the container already reported).
//!
//! Forcing `--entrypoint /bin/caos` means any image carrying the `caos` binary
//! and a `/worker` works as a compute image, regardless of its own configured
//! entrypoint/command.
//!
//! Configuration (env): `CAOS_SERVER_URL` (default `http://caos-server`; also
//! injected into the containers), `CAOS_RUNNER_TOKEN` (bearer token, if the
//! server requires one), `CAOS_RUNNER_SLOTS` (default 8), `CAOS_DOCKER_NETWORK`
//! (default `caos-net`), `CAOS_DOCKER_BIN` (default `docker`),
//! `CAOS_DOCKER_ARGS` (global flags before `run`, e.g.
//! `--remote --url unix://…` for the socket-delegation backend),
//! `CAOS_RUNNER_SOCKET` (an engine socket, granted only to images that declare
//! `CAOS_GRANT_ENGINE_SOCKET=1` in their own config env; `CAOS_GRANT_VOLUMES`
//! is declared the same way and backs the listed mountpoints with persistent
//! named volumes), `CAOS_RUNNER_TRUSTED_IMAGES` (which images may be granted
//! host-level power — the socket, CAP_SYS_ADMIN, devices — by digest; unset
//! trusts every image's own declaration, `none` trusts no one; see
//! `docker::Trust`), `CAOS_WORKER_MEMORY` / `CAOS_WORKER_CPUS` /
//! `CAOS_WORKER_PIDS_LIMIT` (ceilings on every worker container; see
//! `docker::Limits` for the defaults and `0` to lift one), and
//! `CAOS_WORKER_REDIS_ADDR` (a redis a worker may use for its own caching,
//! injected into the containers; unset means none is offered).
//!
//! The redis address is the same two-value shape as `CAOS_SERVER_URL`, for the
//! same reason: where workers share the stack's netns it is loopback, and where
//! they get their own netns on a bridge it has to be a name that resolves
//! there. `stack/serve` supplies whichever the placement needs.

mod docker;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Which runnerd owns a container. The hostname is this runnerd's container id
/// under docker/podman — stable across process restarts inside one container,
/// and distinct from any other runnerd sharing the same engine (an inner
/// runnerd delegating over `--remote` is a different container). Falls back to
/// a literal, which only costs the reaper precision, never correctness: it only
/// ever deletes containers already in `exited`.
pub(crate) fn owner_id() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// How long each generic poll hangs. Purely a reconnect cadence — a generic
/// runner never idles out, it just polls again.
const POLL_TTL: Duration = Duration::from_secs(60);

/// Backoff after a failed poll (server down or restarting).
const POLL_RETRY: Duration = Duration::from_secs(2);

/// What every backend shares: where the server is, how to authenticate, which
/// jobs this process accepts, and how many it runs at once.
pub(crate) struct Common {
    server_url: String,
    /// Address (host:port) of a redis a worker may use for its OWN caching,
    /// injected as `CAOS_WORKER_REDIS_ADDR` into every worker container. This
    /// is a courtesy, not a contract: unset means no worker is offered one, so
    /// a worker must treat its absence as "no cache available" and still do the
    /// work, never as a reason to fail.
    worker_redis_addr: Option<String>,
    token: Option<String>,
    /// Args this runner REQUIRES a job to carry, name → git blob oid of the
    /// value, from `CAOS_RUNNER_REQUIRE` (`name=value` pairs, whitespace or
    /// comma separated). Empty for the generic pool.
    ///
    /// The server's rendezvous is symmetric: this says which jobs the runner
    /// will accept, and a job's own `required*` args say which runners it will
    /// accept. So a dedicated pool needs BOTH halves — this process requiring
    /// a pool arg, and the job carrying it — and neither alone changes
    /// anything: a runner requiring it simply never matches ordinary work,
    /// which is the point.
    required: std::collections::BTreeMap<String, String>,
    slots: u32,
}

/// One way of running a claimed job to completion. The poll loop, the required
/// set and the crash backstop are shared, so a backend only starts the job's
/// runner, waits for it, and cleans up after it (design/firecracker-runners.md,
/// phase 0c).
pub(crate) trait Backend: Send + Sync {
    /// For the startup line: what this backend runs workers on.
    fn describe(&self) -> String;
    /// Delete what a previous runnerd of this deployment left behind. Called
    /// once at startup, before any slot claims a job. Never touches a live
    /// worker or another deployment's.
    fn reap_leftovers(&self);
    /// Start the job's runner, wait for it to exit, and remove it. The runner
    /// posts its own results; the [`Outcome`] is only for the backstop.
    fn run(&self, common: &Common, slot: u32, job: &Job) -> Outcome;
}

/// How a runner's life ended, as far as the backstop cares.
pub(crate) struct Outcome {
    /// `(error, log)` if the runner died in a way that may have left a job
    /// unsettled; `None` for a clean exit.
    pub(crate) failure: Option<(String, String)>,
    /// The runner's stdout: its claim records (`caos_world::runner_lineage`).
    pub(crate) stdout: String,
}

/// The environment every runner is started with, whatever starts it. The
/// Docker backend passes it as `-e`; a VM backend puts it in boot config.
///
/// The redis address is passed only when set, so an unset one reaches the
/// runner as an ABSENT env var rather than an empty one it might dial. The
/// required set goes down so the runner's follow-up polls, and their lineage,
/// keep this slot's constraints: a pooled slot's warm runner must stay in the
/// pool.
pub(crate) fn worker_env(common: &Common) -> Vec<(String, String)> {
    let mut env = vec![("CAOS_SERVER_URL".to_string(), common.server_url.clone())];
    if let Some(addr) = &common.worker_redis_addr {
        env.push(("CAOS_WORKER_REDIS_ADDR".to_string(), addr.clone()));
    }
    env.push((
        caos_world::runner_lineage::PARENT_REQUIRED_ENV.to_string(),
        caos_world::runner_lineage::encode_required(&common.required),
    ));
    env
}

pub(crate) fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM` — as PID 1
/// in a container the kernel applies no default disposition, so without these
/// `docker stop` would hang until the 10s `SIGKILL`.
fn install_termination_handlers() {
    extern "C" fn terminate(_signum: std::ffi::c_int) {
        unsafe { exit_now(0) }
    }
    extern "C" {
        fn signal(signum: std::ffi::c_int, handler: extern "C" fn(std::ffi::c_int)) -> usize;
        #[link_name = "_exit"]
        fn exit_now(code: std::ffi::c_int) -> !;
    }
    const SIGINT: std::ffi::c_int = 2;
    const SIGTERM: std::ffi::c_int = 15;
    unsafe {
        signal(SIGINT, terminate);
        signal(SIGTERM, terminate);
    }
}

/// Resolve the server URL's host to an ADDRESS, once, and hand that on.
///
/// A name costs about a second per lookup inside a worker's netns — the
/// container resolver is asked for each search-domain permutation, A and AAAA —
/// and git makes roughly ten lookups per operation. Measured from a worker:
/// `git ls-remote http://caos-server` 10022ms, the same by address 50ms, and a
/// single `curl` 1046ms against 41ms. Every worker pays that on every push and
/// every fetch, so it is the largest fixed cost in a test job.
///
/// Resolved HERE because this is the one place it can be paid once: runnerd
/// outlives every worker, and hands the result to all of them. It also fixes
/// runnerd's own long-polling, which would otherwise pay a lookup per poll.
///
/// IPv4 by preference — the AAAA half is what makes these lookups slow, and a
/// v6 literal would need bracketing in the URL. An unresolvable name is left
/// exactly as it was: this is a shortcut, never the thing that decides whether
/// the server is reachable.
fn as_address(url: &str) -> String {
    use std::net::ToSocketAddrs;
    let resolved = (|| {
        let (scheme, rest) = url.split_once("://")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (host, port_text) = match hostport.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
            _ => (hostport, None),
        };
        if host.parse::<std::net::IpAddr>().is_ok() {
            return None;
        }
        let port: u16 = port_text.and_then(|p| p.parse().ok()).unwrap_or(80);
        let ip = (host, port)
            .to_socket_addrs()
            .ok()?
            .find(|a| a.is_ipv4())?
            .ip();
        Some(match port_text {
            Some(p) => format!("{scheme}://{ip}:{p}{path}"),
            None => format!("{scheme}://{ip}{path}"),
        })
    })();
    match resolved {
        Some(addr) => {
            eprintln!("caos-runnerd: serving workers {url} as {addr}");
            addr
        }
        None => url.to_string(),
    }
}

fn main() {
    install_termination_handlers();
    let common = Arc::new(Common {
        server_url: as_address(&env_or("CAOS_SERVER_URL", "http://caos-server")),
        worker_redis_addr: std::env::var("CAOS_WORKER_REDIS_ADDR")
            .ok()
            .filter(|s| !s.is_empty()),
        token: std::env::var("CAOS_RUNNER_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        slots: env_or("CAOS_RUNNER_SLOTS", "8").parse().unwrap_or(8),
        required: required_args(&env_or("CAOS_RUNNER_REQUIRE", "")),
    });
    // A backend that cannot work is refused HERE, before any slot claims a
    // job: a misconfigured pool that starts and fails every job is far worse
    // than one that does not start.
    let backend: Arc<dyn Backend> = match env_or("CAOS_RUNNER_BACKEND", "docker").as_str() {
        "docker" => Arc::new(docker::Config::from_env()),
        other => {
            eprintln!("caos-runnerd: unknown CAOS_RUNNER_BACKEND {other:?} (known: docker)");
            std::process::exit(2);
        }
    };
    // The required set is part of this line because it is what decides which
    // jobs this process can ever see, and `stack/serve` greps this line ("slots,
    // server") to know runnerd is up.
    eprintln!(
        "caos-runnerd: {} slots, server {}, {}, requiring {:?}",
        common.slots,
        common.server_url,
        backend.describe(),
        common.required
    );
    backend.reap_leftovers();
    let mut threads = Vec::new();
    for slot in 1..common.slots {
        let common = Arc::clone(&common);
        let backend = Arc::clone(&backend);
        threads.push(std::thread::spawn(move || {
            slot_loop(&common, &*backend, slot)
        }));
    }
    slot_loop(&common, &*backend, 0);
}

/// One slot: poll for a job, run its container, wait for the container to die,
/// poll again. The container (a warm runner) may serve many jobs before dying;
/// this slot stays parked on `wait` the whole time — one poll per slot lineage.
fn slot_loop(common: &Common, backend: &dyn Backend, slot: u32) {
    loop {
        match poll(common) {
            Ok(Some(job)) => run_job(common, backend, slot, &job),
            Ok(None) => {} // idle (or evicted, which we ignore): poll again
            Err(e) => {
                eprintln!("runnerd slot {slot}: poll failed: {e}");
                std::thread::sleep(POLL_RETRY);
            }
        }
    }
}

/// A claimed job: the fields runnerd itself needs, plus the payload verbatim to
/// hand the container. (`req` is the wire field name; its value is the ArgTree
/// hash.)
pub(crate) struct Job {
    arg_tree: String,
    nonce: String,
    image_ref: String,
    payload: String,
}

/// A git blob's object id: sha1 over `blob <len>\0` then the content. The
/// server's rendezvous compares OIDS, and a job's arg value reaches it as the
/// blob of that value — so a runner requiring `required-pool=seeded` has to
/// name the same object the ArgTree does, not the string.
fn blob_oid(value: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", value.len()).as_bytes());
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Parse `CAOS_RUNNER_REQUIRE` — `name=value` pairs, whitespace or comma
/// separated — into the required set. `name=value` names the BLOB of `value`
/// (`required-pool=seeded`); `name:hash=<oid>` names an object by its id, which
/// is how a pool names its `base`, an image TREE (the same `:hash=` form the
/// caos CLI binds args with). A pair with no `=`, an empty name, or a `:hash=`
/// that is not a 40-hex oid is dropped with a warning rather than failing the
/// process: a runner that starts and matches nothing is far easier to diagnose
/// than one that does not start.
fn required_args(spec: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in spec.split([' ', '\t', '\n', ',']).filter(|s| !s.is_empty()) {
        match pair.split_once('=') {
            Some((name, oid)) if name.ends_with(":hash") => {
                let name = name.trim_end_matches(":hash");
                let is_oid = oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit());
                if name.is_empty() || !is_oid {
                    eprintln!(
                        "caos-runnerd: ignoring malformed CAOS_RUNNER_REQUIRE entry {pair:?}"
                    );
                } else {
                    out.insert(name.to_string(), oid.to_ascii_lowercase());
                }
            }
            Some((name, value)) if !name.is_empty() => {
                out.insert(name.to_string(), blob_oid(value));
            }
            _ => eprintln!("caos-runnerd: ignoring malformed CAOS_RUNNER_REQUIRE entry {pair:?}"),
        }
    }
    out
}

/// One long-poll. `Some(job)` to run; `None` on idle/evicted.
fn poll(config: &Common) -> Result<Option<Job>, String> {
    let body = serde_json::json!({
        "required": config.required,
        "lineage": [],
        "ttl_ms": POLL_TTL.as_millis() as u64,
    });
    let url = format!("{}/runner/poll", config.server_url.trim_end_matches('/'));
    let mut req = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
        .with_timeout(POLL_TTL.as_secs() + 15)
        .with_body(body.to_string());
    if let Some(token) = &config.token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().map_err(|e| format!("POST {url}: {e}"))?;
    if resp.status_code != 200 {
        return Err(format!(
            "poll failed ({}): {}",
            resp.status_code,
            resp.as_str().unwrap_or("")
        ));
    }
    let v: serde_json::Value = serde_json::from_str(resp.as_str().unwrap_or(""))
        .map_err(|e| format!("invalid poll reply: {e}"))?;
    let Some(job) = v.get("job").filter(|j| j.is_object()) else {
        return Ok(None);
    };
    let field = |k: &str| job[k].as_str().unwrap_or_default().to_string();
    let parsed = Job {
        arg_tree: field("req"),
        nonce: field("nonce"),
        image_ref: field("image_ref"),
        payload: job.to_string(),
    };
    if parsed.arg_tree.is_empty() || parsed.nonce.is_empty() || parsed.image_ref.is_empty() {
        return Err(format!(
            "job missing req/nonce/image_ref: {}",
            parsed.payload
        ));
    }
    Ok(Some(parsed))
}

/// Run one claimed job on the backend, then backstop its runner's death: post
/// a failure for every job it may have left unsettled (410 if it did report).
fn run_job(common: &Common, backend: &dyn Backend, slot: u32, job: &Job) {
    eprintln!(
        "runnerd slot {slot}: arg_tree {} -> {} ({})",
        job.arg_tree,
        backend.describe(),
        job.image_ref
    );
    let outcome = backend.run(common, slot, job);
    if let Some((error, log)) = outcome.failure {
        eprintln!("runnerd slot {slot}: arg_tree {}: {error}", job.arg_tree);
        for (arg_tree, nonce) in backstop_targets(job, &outcome.stdout) {
            if let Err(e) = post_failure(common, &arg_tree, &nonce, &error, &log) {
                eprintln!("runnerd slot {slot}: reporting failure for {arg_tree}: {e}");
            }
        }
    }
}

/// The jobs a dead runner may have left unsettled: the one we handed it, and
/// the last one it recorded claiming (`caos_world::runner_lineage`), which for
/// a warm runner is the job it actually died in. Every job claimed before the
/// last one was posted before the next was claimed. A target that was already
/// settled answers 410, which is harmless. The first job is always included,
/// because a runner can die before writing its first record.
fn backstop_targets(job: &Job, stdout: &str) -> Vec<(String, String)> {
    let mut targets = vec![(job.arg_tree.clone(), job.nonce.clone())];
    if let Some(last) = caos_world::runner_lineage::last_claim(stdout) {
        if last.1 != job.nonce {
            targets.push(last);
        }
    }
    targets
}

/// The crash backstop: report a container that died without (necessarily)
/// posting. A 410 means it did post before dying — the job is settled.
fn post_failure(
    config: &Common,
    arg_tree: &str,
    nonce: &str,
    error: &str,
    log: &str,
) -> Result<(), String> {
    // Keep only the tail of a big log: the failure is usually at the end, and
    // the message lands in an error string a client will read.
    let tail: String = log
        .lines()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let body = serde_json::json!({
        "req": arg_tree, "nonce": nonce, "ok": false, "error": error, "log": tail,
    });
    let url = format!("{}/runner/result", config.server_url.trim_end_matches('/'));
    let mut req = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
        .with_timeout(30)
        .with_body(body.to_string());
    if let Some(token) = &config.token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().map_err(|e| format!("POST {url}: {e}"))?;
    match resp.status_code {
        200 | 410 => Ok(()),
        code => Err(format!(
            "result post failed ({code}): {}",
            resp.as_str().unwrap_or("")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caos_world::runner_lineage::claim_record;

    fn job(arg_tree: &str, nonce: &str) -> Job {
        Job {
            arg_tree: arg_tree.to_string(),
            nonce: nonce.to_string(),
            image_ref: "img".to_string(),
            payload: "{}".to_string(),
        }
    }

    #[test]
    fn required_names_a_blob_by_value_or_an_object_by_hash() {
        let oid = "2fc39c5d1021c6cb1d149ab94e40837d2a6b0b66";
        let required = required_args(&format!("required-pool=fc base:hash={oid} bad:hash=xyz"));
        assert_eq!(
            required,
            BTreeMap::from([
                ("base".to_string(), oid.to_string()),
                ("required-pool".to_string(), blob_oid("fc")),
            ])
        );
    }

    #[test]
    fn a_warm_runner_crash_fails_the_job_it_was_running() {
        let stdout = format!(
            "{}\n{}\n",
            claim_record("r1", "n1"),
            claim_record("r2", "n2")
        );
        assert_eq!(
            backstop_targets(&job("r1", "n1"), &stdout),
            vec![("r1".into(), "n1".into()), ("r2".into(), "n2".into())]
        );
    }

    #[test]
    fn a_first_job_crash_is_reported_once_with_or_without_a_record() {
        let only = vec![("r1".to_string(), "n1".to_string())];
        assert_eq!(backstop_targets(&job("r1", "n1"), ""), only);
        let stdout = format!("{}\n", claim_record("r1", "n1"));
        assert_eq!(backstop_targets(&job("r1", "n1"), &stdout), only);
    }
}
