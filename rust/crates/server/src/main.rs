//! caos server: storage and compute behind one endpoint.
//!
//! Storage (a tiny HTTP front-end over a git object database mounted at `/git`):
//!
//! * `GET  /object/<hash>` — return the serialized object (`<type> <size>\0…`).
//! * `HEAD /object/<hash>` — 200/404, no body: is it stored? (`put` prunes on it.)
//! * `POST /object/` — store the serialized object in the body, return its hash.
//!
//! Compute:
//!
//! * `GET /run?req=<hash>` — run the ArgTree `<hash>` (`req` is the
//!   query param's historical name; its value is the ArgTree hash) and return
//!   the hash of its result.
//! * `POST /start` (a bundled request, `POST /submit`'s body) and `POST
//!   /start/<verb>` (a `.caos-expr` line) — start a request and return its
//!   TICKET at once. Forming the request (a base's build, a locator's fetch)
//!   and running it happen on the ticket's own thread, recorded under it, so
//!   `GET /status/<ticket>` shows the evaluation as well as the run.
//!   `GET /result/<ticket>` says how it stands and `GET /wait/<ticket>` blocks
//!   for the answer. `/eval/<verb>` is a start followed by that wait.
//! * `POST /trace/child` — record, under an in-flight job, that it started work
//!   on another stack, so a reader descends into it (`status`).
//! * `POST /sub-run` — admit an exact detached child under an in-flight job's
//!   existing server-side run stack and secret store.
//! * `GET /ref?name=<ref>`, `POST /ref` — read a named mutable ref under
//!   `refs/heads/`, or move one with a compare-and-swap (see [`mod refs`]).
//!   `caos ref-then` waits on one, as a continuation.
//! * `GET /eval?root:<type>=<value>[&path=<p>]` — walk `.caos-expr` from a root
//!   down to a path and return what falls out as `"<type> <hash>"`. The root is
//!   a TYPED argument, named by the caller as everything else in caos is:
//!   `root:hash=<oid>` starts from an object this store holds, `root:@@=<locator>`
//!   from a pin, fetching the commit if the store lacks it (see [`mod locator`]).
//!   A client and a worker both ask here rather than following a pin themselves,
//!   which is what lets a server-side evaluation reach one at all.
//!
//! The server runs no workers itself. Dispatch is pull-based (see
//! `design/runner-protocol.md`): runners long-poll `POST /runner/poll` with
//! their required args, the server matches pending `/run` jobs against the
//! parked polls, and the runner posts the job's `"<type> <hash>"` back via
//! `POST /runner/result` (or a `promise` the compute half resolves — see
//! `design/map-then.md`). The generic runner minting fresh worker containers is
//! `caos-runnerd`, an ordinary poller with no required args.
//!
//! Compute results are cached in Redis (`CAOS_REDIS_ADDR`, default
//! `caos-redis:6379`): the key is the arg-tree hash — the ArgTree itself, which
//! carries the worker image, std and salt — the value the
//! result hash. A hit skips the worker entirely. Redis is best-effort — if
//! it's unreachable we log and run uncached.
//!
//! Git transport:
//!
//! * `GET  /info/refs?service=…`, `POST /git-upload-pack`, `POST /git-receive-pack`
//!   — git smart-HTTP over the same repo, so the caos client can use the server as
//!   a `caos` git remote (push objects up, fetch refs/results down). See
//!   [`mod git`]; it delegates to `git http-backend`.
//!
//! The halves live in [`mod storage`], [`mod compute`], [`mod runner`], and
//! [`mod git`]; this file is the entry point, the shared [`Config`]/[`HttpError`],
//! and the request router.

mod approved;
mod compute;
mod eval_cache;
mod git;
mod grant_history;
mod import;
mod locator;
mod loose;
mod mcp;
mod push;
mod refs;
mod remote_git;
mod runner;
mod secret_store;
mod secrets;
mod status;
mod storage;

use std::sync::Arc;
use std::time::Duration;

use tiny_http::{Method, Request, Response, Server, StatusCode};

/// Listen address; overridable for local runs outside the container. Binds the
/// IPv6 wildcard (dual-stack: also accepts IPv4) so runners can reach us over
/// IPv6-only networks too.
const DEFAULT_ADDR: &str = "[::]:80";

/// Where the git object database lives (the storage half, now in-process).
/// Override with `CAOS_GIT_DIR` (useful for local runs outside the container).
const DEFAULT_GIT_DIR: &str = "/git";

/// Registry base URL converted git-docker images are pushed to, reachable from
/// *this* container over the docker network. Override with
/// `CAOS_REGISTRY_PUSH_URL`.
const DEFAULT_REGISTRY_PUSH_URL: &str = "http://caos-registry:5000";

/// How the docker daemon that actually runs workers (runnerd's) refers to that
/// same registry — a published port on localhost, which docker treats as an
/// insecure registry, so no TLS/daemon config is needed. Override with
/// `CAOS_REGISTRY_PULL_HOST`.
const DEFAULT_REGISTRY_PULL_HOST: &str = "localhost:5000";

/// Redis (host:port) used to cache results. Override with `CAOS_REDIS_ADDR`.
const DEFAULT_REDIS_ADDR: &str = "caos-redis:6379";

/// Runtime configuration, read once from the environment at startup. Cloning
/// is cheap and lets admitted sub-runs outlive the request thread that launched
/// them while sharing the same repository handle.
#[derive(Clone)]
struct Config {
    registry_push_url: String,
    registry_pull_host: String,
    redis_addr: String,
    /// What this stack's cache keys are prefixed with, so several stacks can
    /// share one redis without answering each other's questions.
    ///
    /// THE ARGTREE IS NOT ENOUGH ON ITS OWN, and that is the point. It captures
    /// the image, std, salt and every arg — everything the WORK depends on —
    /// but not the caos that ran it. Where a server's behavior changes without
    /// any of those moving, a stale entry is served as a hit: CLAUDE.md records
    /// the observed case, a rebuilt `caos` binary leaving a seeded sentinel's
    /// key unmoved so redis hands back the previous deploy's worker image. Any
    /// value that identifies the build closes that, and folding it into the KEY
    /// beats invalidating entries — nothing has to be deleted, so nothing has to
    /// be deleted correctly.
    ///
    /// It is also what lets two stacks built from different trees run at once
    /// against one redis: their namespaces differ because their builds do, with
    /// no coordination and nothing host-specific in a key.
    ///
    /// Empty (the default) means no prefix at all, so a deployment that sets
    /// nothing keeps exactly the keys it has today.
    cache_namespace: String,
    /// Filesystem path to the git object database, passed to `git http-backend`
    /// as `GIT_PROJECT_ROOT` for the smart-HTTP transport (see [`mod git`]).
    git_dir: String,
    /// `secrets.git`, which nothing else in this server links to.
    secrets_git: String,
    /// The git object database, served directly (storage is now in-process).
    /// Thread-safe: each request thread takes a local handle via `to_thread_local`.
    repo: gix::ThreadSafeRepository,
}

/// The stack limit (`RLIMIT_STACK`, soft) this process runs with, and so
/// every git it spawns: a child's main stack is sized by the limit it inherits
/// at exec. Every push is now unpacked by `unpack-objects --strict`, whose fsck
/// walk recurses once per commit of a new chain, so a push of a long history
/// overflows git's default 8 MiB and dies with SIGSEGV. 1 GiB is address space,
/// not memory: only what a walk actually touches is ever mapped.
const GIT_STACK_BYTES: u64 = 1 << 30;

/// Set this process's soft stack limit to `bytes`, capped at the hard limit,
/// and return the value set. Async-signal-safe, so a `pre_exec` may call it.
fn set_stack_limit(bytes: u64) -> std::io::Result<u64> {
    #[repr(C)]
    struct Rlimit {
        current: u64,
        maximum: u64,
    }
    extern "C" {
        fn getrlimit(resource: std::ffi::c_int, limit: *mut Rlimit) -> std::ffi::c_int;
        fn setrlimit(resource: std::ffi::c_int, limit: *const Rlimit) -> std::ffi::c_int;
    }
    const RLIMIT_STACK: std::ffi::c_int = 3;
    let mut limit = Rlimit {
        current: 0,
        maximum: 0,
    };
    if unsafe { getrlimit(RLIMIT_STACK, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The hard limit reads as u64::MAX when unlimited.
    limit.current = bytes.min(limit.maximum);
    if unsafe { setrlimit(RLIMIT_STACK, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(limit.current)
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM`. This matters
/// in a container, where the daemon is PID 1: the kernel applies no default
/// disposition for these signals to PID 1, so without an explicit handler
/// `docker stop` (and `caosd down`'s Ctrl-C) would hang until the 10s `SIGKILL`.
fn install_termination_handlers() {
    // Async-signal-safe: we hold no state that needs flushing, so just exit.
    extern "C" fn terminate(_signum: std::ffi::c_int) {
        unsafe { exit_now(0) }
    }
    extern "C" {
        // libc, resolved against what std already links.
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

fn main() {
    // First, so every git this process ever runs inherits it.
    if let Err(error) = set_stack_limit(GIT_STACK_BYTES) {
        eprintln!("warning: cannot raise the stack limit for git: {error}");
    }
    // Run as the repository's `pre-receive` hook (see `loose::publish_quarantine`).
    if loose::is_hook_invocation() {
        std::process::exit(loose::publish_quarantine());
    }
    // `server doctor`: check the store by hand (`caosd doctor`). Nothing that
    // reads the whole store runs at start: every writer here is durable, so
    // what these find is not crash damage to repair on every boot. Read-only,
    // so it is safe beside a live server.
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        let git_dir = env_or("CAOS_GIT_DIR", DEFAULT_GIT_DIR);
        let mut healthy = true;
        if let Err(error) = verify_refs(&git_dir) {
            eprintln!("doctor: {git_dir}: broken refs; restore each from its reflog (logs/) or delete it: {error}");
            healthy = false;
        }
        if let Err(error) = verify_object_closure(&git_dir) {
            eprintln!("doctor: {git_dir}: object store has incomplete history; restore the missing objects: {error}");
            healthy = false;
        }
        if healthy {
            eprintln!("doctor: {git_dir}: every ref and every stored object's history is stored");
        }
        std::process::exit(if healthy { 0 } else { 1 });
    }
    install_termination_handlers();

    let addr = std::env::var("SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let git_dir = env_or("CAOS_GIT_DIR", DEFAULT_GIT_DIR);

    // This server's git settings live in ITS OWN ENVIRONMENT, never in the
    // repository's config (see `git_settings`). Several servers, of different
    // builds, share one repository (every test stack shares its host stack's),
    // so a setting written into the repo would apply to all of them. The one
    // exception is a repository this process creates (e.g. a fresh fly
    // Volume): nobody else uses it yet, and the protective settings written
    // into it also protect a person who runs git there by hand.
    if gix::open(&git_dir).is_err() {
        initialize_repo(&git_dir).unwrap_or_else(|error| {
            eprintln!("fatal: {error}");
            std::process::exit(1);
        });
    }
    let hooks = std::env::temp_dir().join(format!("caos-server-hooks-{}", std::process::id()));
    let hooks = loose::install_hooks(&hooks).unwrap_or_else(|error| {
        eprintln!("fatal: installing this server's git hooks: {error}");
        std::process::exit(1);
    });
    // Still single-threaded and with no child yet, so every git this process
    // runs against the store sees the settings from the start.
    let _ = GIT_ENV.set(settings_env(&git_settings(Some(&hooks))));

    // Clear what a previous crash left behind BEFORE opening the repo: gix
    // answers "is this object stored?" for a loose object with a path-exists
    // check, so an empty file has to be gone from disk before anything asks.
    let temporaries = loose::sweep_temporaries(&std::path::Path::new(&git_dir).join("objects"));
    if temporaries > 0 {
        eprintln!(
            "repair: {git_dir} had {temporaries} temporary object file(s) from a crash; removed"
        );
    }

    // Open the object database once as a thread-safe handle; each request thread
    // takes a cheap local handle from it (see `handle`).
    // Nothing writes packs any more (every transfer unpacks; see `mod loose`),
    // but a store from before that, or a seeded one, can still hold them. gix
    // fixes its slot capacity at open time; its client default (at least 32)
    // cannot accommodate many packs, so reserve its supported maximum. A store
    // that is still packed is migrated offline by exploding its packs into
    // loose objects (design/loose-object-store.md), never by a live repack:
    // see the shared-reader constraints above.
    // Given skips gix's initial scan, so diagnose an already-full store here.
    let packs = std::fs::read_dir(std::path::Path::new(&git_dir).join("objects/pack"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "idx"))
                .count()
        })
        .unwrap_or(0);
    if packs >= (1 << 15) - 1 {
        eprintln!("fatal: object store has {packs} packs; explode them into loose objects offline, preserving unreferenced CAS objects, before restarting (design/loose-object-store.md)");
        std::process::exit(1);
    }
    if packs > 0 {
        eprintln!("warning: object store has {packs} pack(s); nothing should write packs any more, so these predate the loose-object store; explode them offline (design/loose-object-store.md)");
    }
    let options = gix::open::Options::default()
        .object_store_slots(gix::odb::store::init::Slots::Given((1 << 15) - 1));
    let repo = match gix::open_opts(&git_dir, options) {
        Ok(repo) => repo.into_sync(),
        Err(err) => {
            eprintln!("fatal: cannot open git repo at {git_dir}: {err}");
            std::process::exit(1);
        }
    };

    let secrets_git = env_or("CAOS_SECRETS_GIT_DIR", &secret_store::default_dir(&git_dir));
    secret_store::init(&secrets_git).unwrap_or_else(|error| {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    });

    // Shared read-only across handler threads (one per request, see below).
    let config = Arc::new(Config {
        registry_push_url: env_or("CAOS_REGISTRY_PUSH_URL", DEFAULT_REGISTRY_PUSH_URL),
        registry_pull_host: env_or("CAOS_REGISTRY_PULL_HOST", DEFAULT_REGISTRY_PULL_HOST),
        redis_addr: env_or("CAOS_REDIS_ADDR", DEFAULT_REDIS_ADDR),
        cache_namespace: env_or("CAOS_CACHE_NAMESPACE", ""),
        git_dir,
        secrets_git,
        repo,
    });

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "caos-server listening on http://{addr} (storage + compute), \
         git repo {}, registry push {} / pull {}, redis {}",
        config.git_dir, config.registry_push_url, config.registry_pull_host, config.redis_addr,
    );

    spawn_request_ref_pruner(config.git_dir.clone());

    // One thread per request, not a serial loop: a worker fetches its inputs
    // from `/object` while its own `/run` request is still being served, a
    // runner's poll parks for its whole TTL, and several top-level runs may be
    // in flight at once. Threads are cheap here: each mostly blocks (compute
    // fans out its own threads for parallel promise maps — see
    // compute::resolve_promise).
    for request in server.incoming_requests() {
        let config = Arc::clone(&config);
        std::thread::Builder::new()
            .stack_size(compute::EVAL_STACK_BYTES)
            .spawn(move || {
                if let Err(err) = handle(config, request) {
                    // Only reachable if writing the response itself fails.
                    eprintln!("failed to send response: {err}");
                }
            })
            .expect("spawning a request thread");
    }
}

/// How long a required Git command waits out another process's lock, and how
/// often it retries — 5s in 50ms steps.
///
/// A SERVER REPO CAN HAVE A SECOND SERVER, and the settings above are asserted
/// on EVERY boot rather than only on a repo this process created, so two
/// servers coming up against one repo assert the same values at the same
/// instant. `git config` publishes an edit by taking `config.lock`, so the
/// loser gets `could not lock config file config: File exists` and — these
/// being *required* commands — died with `fatal:` before it ever listened. That
/// is the dev placement's ordinary case: several `dev/stack-up` stacks share
/// `/caos-dev/git` deliberately (their objects are content-addressed), and it
/// cost one of two concurrent stacks its whole bring-up, reported as "server
/// never came up" with the real message a file away.
///
/// Waited out rather than tolerated: the assertion still has to happen, and
/// what the loser is waiting for is a peer writing the very same value. Bounded
/// because a lock held longer than this is a STALE one — a crashed writer's
/// leftover `config.lock`, which no amount of patience clears and which a
/// person has to see.
const GIT_LOCK_WAIT_STEPS: u32 = 100;
const GIT_LOCK_WAIT_STEP: Duration = Duration::from_millis(50);

/// Does this failure say another process holds the lock on what we are editing?
///
/// git's lockfile layer reports it the same way for every ref, index and config
/// edit: it tried to create `<thing>.lock` and `File exists`. Matched on the
/// message because that is all `git config` gives us — its exit status for a
/// lock collision is the generic 255.
fn is_git_lock_contention(detail: &str) -> bool {
    detail.contains("File exists")
        && (detail.contains("could not lock") || detail.contains("Unable to create"))
}

/// Run one of the Git commands that establishes the server's storage
/// contract. Ignoring one of these failures lets the server start in a mode
/// where pushes, object fetches, or crash recovery are silently unsafe.
fn run_required_git(args: &[&str]) -> Result<(), String> {
    let command = format!(
        "git {}",
        args.iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut last = String::new();
    for attempt in 0..GIT_LOCK_WAIT_STEPS {
        let output = std::process::Command::new("git")
            .args(args)
            .output()
            .map_err(|error| format!("cannot run required command {command}: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        last = if detail.is_empty() {
            format!("required command {command} exited with {}", output.status)
        } else {
            format!(
                "required command {command} exited with {}: {detail}",
                output.status
            )
        };
        if !is_git_lock_contention(detail) {
            return Err(last);
        }
        if attempt == 0 {
            eprintln!("waiting for another process's git lock: {last}");
        }
        std::thread::sleep(GIT_LOCK_WAIT_STEP);
    }
    Err(format!(
        "{last} (still held after {}s — a crashed writer's stale lock file?)",
        GIT_LOCK_WAIT_STEPS as u64 * GIT_LOCK_WAIT_STEP.as_millis() as u64 / 1000
    ))
}

/// git against the store exactly as stored: no replacements, grafts, lazy
/// fetches or commit graph standing in for an object that is not there.
fn doctor_git(git_dir: &str) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .args([
            "-c",
            "core.commitGraph=false",
            "-c",
            "advice.graftFileDeprecated=false",
        ])
        .args(["-C", git_dir]);
    command
}

/// Whether every ref names a stored object. One that does not (an empty ref
/// file, or a name whose object is gone) makes `receive-pack` refuse every
/// push. `for-each-ref` reports an unreadable ref file on stderr and skips it;
/// a readable one naming a missing object it lists like any other, so each
/// listed object is looked up.
fn verify_refs(git_dir: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    let listed = doctor_git(git_dir)
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .output()
        .map_err(|error| format!("listing refs: {error}"))?;
    let mut problems = String::from_utf8_lossy(&listed.stderr).trim().to_string();
    if !listed.status.success() {
        return Err(problems);
    }
    let refs: Vec<(String, String)> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect();
    let mut check = doctor_git(git_dir)
        .args(["cat-file", "--batch-check=%(objectname)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("looking up ref targets: {error}"))?;
    // Fed from a thread: the answers fill stdout while the ids are still going in.
    let mut stdin = check.stdin.take().expect("piped stdin");
    let ids: String = refs.iter().map(|(id, _)| format!("{id}\n")).collect();
    let feeder = std::thread::spawn(move || stdin.write_all(ids.as_bytes()));
    let answered = check
        .wait_with_output()
        .map_err(|error| format!("looking up ref targets: {error}"))?;
    feeder
        .join()
        .expect("ref feeder")
        .map_err(|error| format!("looking up ref targets: {error}"))?;
    if !answered.status.success() {
        return Err(String::from_utf8_lossy(&answered.stderr).into_owned());
    }
    // One answer per id, in order: the id when stored, `<id> missing` when not.
    for ((id, name), answer) in refs
        .iter()
        .zip(String::from_utf8_lossy(&answered.stdout).lines())
    {
        if answer.ends_with(" missing") {
            problems.push_str(&format!("\n{name} names {id}, which is not stored"));
        }
    }
    let problems = problems.trim();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.to_string())
    }
}

/// Whether every stored object's history is stored: each object, referenced or
/// not, walked as a root. Older servers accepted individual commits without
/// their dependencies; the store's publication order keeps closure since
/// (design/loose-object-store.md). It reads every object, minutes on a
/// long-lived store, so it runs only on request (`server doctor`), never at
/// start.
fn verify_object_closure(git_dir: &str) -> Result<(), String> {
    match std::fs::read(std::path::Path::new(git_dir).join("shallow")) {
        Ok(bytes) if !bytes.is_empty() => {
            return Err("server repository must not be shallow".into())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    use std::process::Stdio;
    let git = || doctor_git(git_dir);
    // fsck --connectivity-only starts from refs and misses orphan commits.
    // Make every stored object a root, without reading every blob's payload.
    let mut objects = git()
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("enumerating stored objects: {error}"))?;
    let walk = git()
        .args([
            "rev-list",
            "--objects",
            "--quiet",
            "--missing=error",
            "--stdin",
        ])
        .stdin(objects.stdout.take().expect("piped object list"))
        .output();
    // Check both processes: a failed enumeration must not look like an empty
    // store, and a failed walk must not be hidden by a successful enumeration.
    let listed = objects
        .wait_with_output()
        .map_err(|error| format!("enumerating stored objects: {error}"))?;
    let walked = walk.map_err(|error| format!("checking object closure: {error}"))?;
    if !walked.status.success() || !listed.status.success() {
        return Err(format!(
            "{}{}",
            String::from_utf8_lossy(&listed.stderr),
            String::from_utf8_lossy(&walked.stderr)
        ));
    }
    Ok(())
}

/// `*.unpackLimit` meaning "always unpack": git keeps a transfer as a pack only
/// when it carries at least this many objects, and this is the largest value it
/// accepts.
const UNPACK_ALWAYS: &str = "2147483647";

/// `core.looseCompression`: [`loose::LEVEL`], so git's loose objects and the
/// server's are written alike (a unit test holds the two together).
const LOOSE_LEVEL: &str = "1";

/// The git settings this server runs with, as `(key, value)` pairs.
///
/// They reach git through this process's environment (`settings_env`, applied
/// with `git_env` to every git this server runs against the store, and from
/// `http-backend` to receive-pack, upload-pack and the hook), NEVER through the
/// repository's config: several servers of different builds share one
/// repository, and a setting written there would apply to all of them. Only a
/// repository this process creates gets them written in (`initialize_repo`,
/// with `hooks` = `None`). Environment settings outrank the repo's own.
///
/// `hooks` is this process's own hooks directory (`loose::install_hooks`).
fn git_settings(hooks: Option<&std::path::Path>) -> Vec<(&'static str, String)> {
    let mut settings = vec![
        // The server's protocol requirements. A repo can arrive from anywhere
        // (a test stack seeds one by `git init --bare` plus a fetch, and a
        // plain `init` sets none of these), and a missing `http.receivepack`
        // reads as every push coming back `403`, an auth problem in disguise.
        ("http.receivepack", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
        // Serve partial-clone (`--filter`) fetches: the std/merge worker
        // fetches the commit GRAPH only (`--filter=tree:0`) and lazily faults
        // in just the three trees `git merge-tree` needs.
        ("uploadpack.allowFilter", "true"),
        // Request refs are transient negotiation anchors, result refs a
        // server-written durability index, and a locator's pin-to-tree memo
        // one ref per pin. None belongs in fetch advertisements. All stay
        // visible to receive-pack negotiation: a client can form its next
        // request from a result object that exists only on the server, so
        // hiding that result makes send-pack try (and fail) to read it.
        ("uploadpack.hideRefs", "refs/caos/req/"),
        ("uploadpack.hideRefs", locator::TREE_REF_PREFIX),
        ("uploadpack.hideRefs", "refs/caos/res/"),
        // Validate every incoming object in git's quarantine, including
        // objects outside the updated refs.
        ("receive.fsckObjects", "true"),
        ("receive.shallowUpdate", "false"),
        // The store is loose objects only and is never packed
        // (design/loose-object-store.md, `mod loose`). Every transfer unpacks
        // into loose objects, however small or large; a push lands in git's
        // quarantine and the hook publishes it children first. git writes
        // them at the same zlib level as the server's own writer.
        ("receive.unpackLimit", UNPACK_ALWAYS),
        ("transfer.unpackLimit", UNPACK_ALWAYS),
        ("fetch.unpackLimit", UNPACK_ALWAYS),
        ("core.looseCompression", LOOSE_LEVEL),
        // Never let git rewrite this repo's object store behind our back.
        // receive-pack forks a background repack after a push; that rewrites
        // the store while a concurrent upload-pack streams from it (a client
        // sees `fetch-pack: invalid index-pack output`), and the gix handle
        // this server holds asserts when packs move under it, which tiny_http
        // turns into a body-less 500. THREE settings: `gc.auto 0` gates only
        // the `gc` task, and git 2.54's `geometric-repack` maintenance task
        // ignores every gc knob. A cruft repack is data loss, not just a
        // crash: nearly every object here is unreachable from a ref, so it
        // would drop whatever has not been touched in two weeks while redis
        // keeps handing out hashes that point at it.
        ("gc.auto", "0"),
        ("receive.autogc", "false"),
        ("maintenance.geometric-repack.enabled", "false"),
        // Survive an unclean shutdown. git's default publishes a loose object
        // or a ref by renaming a temp file into place without fsyncing, so a
        // crash can leave the right name holding zero bytes, a POISONED object
        // or ref that `receive-pack` then refuses every push over. `batch`
        // flushes once per batch of writes.
        ("core.fsync", "objects,reference"),
        ("core.fsyncMethod", "batch"),
        // Keep previous ref values, the way to restore a damaged ref by hand
        // (`caosd doctor` names any). Nothing expires them yet: until a
        // retention policy exists, keep the history rather than invent a window.
        ("core.logAllRefUpdates", "always"),
    ]
    .into_iter()
    .map(|(key, value)| (key, value.to_string()))
    .collect::<Vec<_>>();
    if let Some(hooks) = hooks {
        settings.push(("core.hooksPath", hooks.display().to_string()));
    }
    settings
}

/// `settings` as git's environment configuration (`GIT_CONFIG_COUNT` and its
/// numbered keys and values).
fn settings_env(settings: &[(&str, String)]) -> Vec<(String, String)> {
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), settings.len().to_string())];
    for (index, (key, value)) in settings.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key.to_string()));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value.clone()));
    }
    env
}

/// This server's git settings, set once at startup (`main`).
static GIT_ENV: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// The environment that carries this server's git settings: apply it
/// (`Command::envs`) to every git command run against the store.
pub(crate) fn git_env() -> impl Iterator<Item = (&'static str, &'static str)> {
    GIT_ENV
        .get()
        .into_iter()
        .flatten()
        .map(|(key, value)| (key.as_str(), value.as_str()))
}

/// Create the store's repository, with the settings written into its config:
/// this process is its only user so far, and the written settings also protect
/// a person who later runs git in it by hand. The config is then left
/// read-only as a marker that nothing should change it; git rewrites a config
/// by lock and rename whatever its mode, so the guarantee is that this server
/// never writes it again.
fn initialize_repo(git_dir: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    run_required_git(&["init", "-q", "--bare", git_dir])?;
    for (key, value) in git_settings(None) {
        run_required_git(&["-C", git_dir, "config", "--add", key, &value])?;
    }
    let config = std::path::Path::new(git_dir).join("config");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444))
        .map_err(|error| format!("making {} read-only: {error}", config.display()))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// An error that maps cleanly onto an HTTP status code + body.
#[derive(Debug)]
pub(crate) struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// Plain-data accessors, for callers that clone an error across threads
    /// (single-flight broadcasts one outcome to every waiter).
    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl From<std::io::Error> for HttpError {
    fn from(err: std::io::Error) -> Self {
        HttpError::new(500, format!("io error: {err}"))
    }
}

/// Dispatch a single request and send its response.
fn handle(config: Arc<Config>, mut request: Request) -> std::io::Result<()> {
    // Git smart-HTTP (the `caos` remote) is served by a separate CGI delegate that
    // sets its own status/headers, so it bypasses the `route` -> `from_data` path.
    let path = request.url().split('?').next().unwrap_or("").to_string();
    if git::is_git_path(&path) {
        return git::serve(&config, request);
    }
    // MCP over HTTP (design: the caos tool server lives here, not in each
    // harness's client). It sets its own status and content type, so like
    // git it bypasses the `route` -> `from_data` path.
    if path == "/mcp" {
        return serve_mcp(&config, request);
    }
    // The WORLD guard (design/test-stack-image.md): a caos client built for
    // the other world must not drive this stack. The dangerous crossing is
    // silent — a host client against the test stack passes until the tree
    // under test changes the client, and then the suite is exercising host
    // code where the tested code was the point. Requests without the header
    // pass: git smart-HTTP returned above, and health probes are plain curl.
    if let Some(client) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv(caos_world::WORLD_HEADER))
        .map(|header| header.value.as_str().to_string())
    {
        if client != caos_world::WORLD {
            return request.respond(
                Response::from_string(caos_world::mismatch(caos_world::WORLD, &client) + "\n")
                    .with_status_code(StatusCode(400)),
            );
        }
    }
    let mut headers = Vec::new();
    match route(&config, &mut request, &mut headers) {
        Ok(body) => {
            let mut response = Response::from_data(body);
            for (name, value) in headers {
                if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
                {
                    response.add_header(header);
                }
            }
            request.respond(response)
        }
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// `POST /mcp`: one JSON-RPC message in, its answer out (`202` with no body for
/// a notification). What the caller is — which step, which secret readers — is
/// in headers, because the endpoint keeps no connection state.
fn serve_mcp(config: &Arc<Config>, mut request: Request) -> std::io::Result<()> {
    let header = |request: &Request, name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    if *request.method() != Method::Post {
        return request.respond(
            Response::from_string("POST a JSON-RPC message\n").with_status_code(StatusCode(405)),
        );
    }
    let caller = mcp::Caller {
        step: header(&request, caos_world::secrets::STEP_HEADER).filter(|s| !s.trim().is_empty()),
        readers: header(&request, caos_world::secrets::READERS_HEADER).unwrap_or_default(),
        max_result_chars: header(&request, caos_world::secrets::MAX_RESULT_CHARS_HEADER)
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(mcp::DEFAULT_MAX_RESULT_CHARS),
    };
    let mut body = String::new();
    if let Err(error) = request.as_reader().read_to_string(&mut body) {
        return request.respond(
            Response::from_string(format!("reading request: {error}\n"))
                .with_status_code(StatusCode(400)),
        );
    }
    match mcp::handle(config, &caller, &body) {
        Some(message) => request.respond(
            Response::from_string(message.to_string()).with_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                    .expect("a static header"),
            ),
        ),
        None => request.respond(Response::empty(StatusCode(202))),
    }
}

/// The secrets a request's SecretReaderKeys hold, for its session.
/// Headers, never the ArgTree: a key is a credential.
fn secret_context(config: &Config, request: &Request) -> Result<secrets::Context, HttpError> {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    secrets::Context::admit(
        config,
        &header(caos_world::secrets::READERS_HEADER).unwrap_or_default(),
        header(caos_world::secrets::SESSION_HEADER)
            .or_else(|| header(caos_world::secrets::LEGACY_SESSION_HEADER))
            .as_deref(),
    )
}

/// An expression's parameters: the query string, plus a form-style body when
/// the request has one.
fn expression_params(request: &mut Request, query: &str) -> Result<String, HttpError> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    Ok(join_params(query, &body))
}

/// `query` and `body` as one parameter string. A body is
/// `application/x-www-form-urlencoded`, in which `+` is a space (`curl
/// --data-urlencode` writes every space that way), so it is spelled `%20` here;
/// a literal `+` is `%2B` in a form, and still decodes to one. The query string
/// keeps the convention the rest of the server's URLs have, where `+` is
/// itself.
fn join_params(query: &str, body: &str) -> String {
    let body = body.trim().replace('+', "%20");
    match (query.is_empty(), body.is_empty()) {
        (_, true) => query.to_string(),
        (true, false) => body,
        (false, false) => format!("{query}&{body}"),
    }
}

/// A private repository's token for a `:@@=` fetch — the header
/// `POST /git/import` takes, for the same reason: it is the caller's, scoped
/// to one URL, and never looked up from the calling job's secrets.
fn git_token(request: &Request) -> Result<Option<String>, HttpError> {
    let tokens: Vec<_> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv(git_locator::import::TOKEN_HEADER))
        .map(|h| h.value.as_str().to_owned())
        .collect();
    if tokens.len() > 1 {
        return Err(HttpError::new(400, "duplicate Git token header"));
    }
    Ok(tokens.into_iter().next())
}

/// Match the request to a handler and produce the response body. Serves the
/// storage endpoints (`/object*`), compute (`/run`, `/sub-run`), and the runner
/// protocol (`/runner/poll`, `/runner/result`).
fn route(
    config: &Arc<Config>,
    request: &mut Request,
    headers: &mut Vec<(&'static str, String)>,
) -> Result<Vec<u8>, HttpError> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    match request.method() {
        Method::Get if path == "/run" => {
            let secrets = secret_context(config, request)?;
            let run = compute::run(config, &query, &secrets)?;
            headers.push((
                compute::RUN_CACHE_HEADER,
                run.cache.as_header_value().to_string(),
            ));
            if let Some(image) = run.image {
                headers.push((compute::RUN_IMAGE_HEADER, image));
            }
            Ok(run.body)
        }
        Method::Get if path.starts_with("/status/") => {
            status::serve(config, path.trim_start_matches("/status/"), &query)
        }
        Method::Get if path == "/resolve-image" => compute::resolve_image_endpoint(config, &query),
        Method::Get if path == "/eval" => {
            let secrets = secret_context(config, request)?;
            let token = git_token(request)?;
            compute::eval_endpoint(config, &query, &secrets, token.as_deref())
        }
        // One `.caos-expr` line over HTTP: `/eval/run`, `/eval/curry`, and the
        // same for `/start/…`. The args ride in the query and, for a value too
        // big for one, in a POST body of the same `name:type=value&…` form.
        Method::Get | Method::Post if path.starts_with("/eval/") => {
            let params = expression_params(request, &query)?;
            let secrets = secret_context(config, request)?;
            compute::eval_expression_endpoint(
                config,
                path.trim_start_matches("/eval/"),
                &params,
                &secrets,
            )
        }
        // The same, for a request a client has bundled rather than spelled
        // (`POST /submit`'s body). `/wait/<ticket>` is what `/eval` does after
        // a start: it blocks for the ticket's answer.
        Method::Post if path == "/start" => {
            let secrets = secret_context(config, request)?;
            let token = git_token(request)?;
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            compute::start_bundle(config, &body, &secrets, token.as_deref())
        }
        Method::Get if path.starts_with("/wait/") => {
            compute::wait_ticket(config, path.trim_start_matches("/wait/"))
        }
        Method::Post if path.starts_with("/start/") => {
            let params = expression_params(request, &query)?;
            let secrets = secret_context(config, request)?;
            compute::start_expression(
                config,
                path.trim_start_matches("/start/"),
                &params,
                &secrets,
            )
        }
        Method::Get if path.starts_with("/result/") => {
            compute::expression_result(config, path.trim_start_matches("/result/"))
        }
        Method::Post if path == "/submit" => {
            let secrets = secret_context(config, request)?;
            let token = git_token(request)?;
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            compute::submit(config, &body, &secrets, token.as_deref())
        }
        // Named mutable refs under refs/heads/ (`caos ref-get` / `ref-set`):
        // read one here, move one with a compare-and-swap below. See
        // [`mod refs`]. Ahead of the catch-all GET, which serves /object/.
        Method::Get if path == "/ref" => refs::get_endpoint(config, &query),
        Method::Get => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => storage::get_object(config, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Head => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => storage::head_object(config, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Post if path == "/object/" || path == "/object" => {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            storage::post_object(config, &body)
        }
        Method::Post if path == "/ref" => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            refs::set_endpoint(config, &body)
        }
        Method::Post if path == "/git/import" => import::endpoint(config, request),
        Method::Post if path == "/git/push" => push::endpoint(config, request),
        Method::Post if path == "/secrets/push" => secret_store::push_endpoint(config, request),
        Method::Post if path == "/secrets/commits" => {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            grant_history::commits_endpoint(config, &body)
        }
        Method::Post if path == "/sub-run" => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            runner::sub_run(&body)
        }
        Method::Post if path == "/trace/child" => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            runner::trace_child(config, &body)
        }
        Method::Post
            if path == "/runner/poll" || path == "/runner/result" || path == "/runner/lease" =>
        {
            let authorization = request
                .headers()
                .iter()
                .find(|h| h.field.to_string().eq_ignore_ascii_case("authorization"))
                .map(|h| h.value.to_string());
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            if path == "/runner/poll" {
                runner::poll(authorization.as_deref(), &body)
            } else if path == "/runner/lease" {
                runner::lease(authorization.as_deref(), &body)
            } else {
                runner::result(authorization.as_deref(), &body)
            }
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}

/// How often stale request refs are swept, and how long one is kept.
const REQ_REF_SWEEP: std::time::Duration = std::time::Duration::from_secs(120);
const REQ_REF_KEEP: std::time::Duration = std::time::Duration::from_secs(600);

/// Sweep old `refs/caos/req/<hash>` refs in the background.
///
/// EVERY PUSHED OBJECT LEAVES ONE (`ensure_pushed`), so they accumulate without
/// bound — a single suite run adds about 1300 — and every push and fetch after
/// that pays for them, because git advertises the whole ref list on each
/// connection. Measured on this repo: 2560 refs is a 251 KB advertisement and a
/// 258ms push of one small object; at 1231 refs the same push is 181ms. That is
/// roughly 58ms per thousand refs, on every one of the hundreds of pushes a
/// suite makes, and it gets worse every run.
///
/// Safe to delete, on two independent grounds. Nothing READS them: the name is
/// written by `ensure_pushed` and never looked up (a result is fetched by hash,
/// or by the `refs/caos/res/<argTree>` the server pins). And they anchor
/// nothing: GC is off here precisely because almost every object is unreachable
/// from any ref, so these are not what keeps objects alive.
///
/// AGE, not a flush. Their one job is to be a negotiation base for the NEXT
/// push from the same client, seconds later, so that an edited tree ships only
/// its delta. Deleting a fresh one would make that client re-send a closure it
/// already sent, which is the cost this is trying to avoid.
///
/// `refs/caos/res/<argTree>` TOO, on the same two grounds and with the same
/// job. A result pin is never read back (design/caos-store-and-gc.md: a result
/// is fetched by hash, and sessions never query these), it keeps nothing alive
/// with GC off, and what it does do -- stay advertised to receive-pack, so a
/// client can push a request built on a result it never fetched -- is the
/// next push's business, seconds later. Left alone they were the largest set
/// of refs: one per top-level run, forever, and a dev stack's suite runs
/// slowed as they piled up (954s of test time at 2,215 refs, 713s at 432).
fn spawn_request_ref_pruner(git_dir: String) {
    std::thread::spawn(move || loop {
        std::thread::sleep(REQ_REF_SWEEP);
        let mut stale = String::new();
        let mut count = 0usize;
        for prefix in ["refs/caos/req", "refs/caos/res"] {
            let dir = std::path::Path::new(&git_dir).join(prefix);
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                // No directory yet (nothing pushed) is the ordinary case, not news.
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().map(|age| age > REQ_REF_KEEP).unwrap_or(false))
                    .unwrap_or(false);
                if !old {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    stale.push_str(&format!("delete {prefix}/{name}\n"));
                    count += 1;
                }
            }
        }
        if count == 0 {
            continue;
        }
        // Through git rather than by unlinking: it takes the same locks the
        // push path does, and it copes with a ref that has been packed since.
        let mut child = match std::process::Command::new("git")
            .envs(git_env())
            .args(["-C", &git_dir, "update-ref", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!("caos-server: pruning request refs: {e}");
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(stale.as_bytes());
        }
        match child.wait() {
            Ok(_) => eprintln!("caos-server: pruned {count} stale request/result ref(s)"),
            Err(e) => eprintln!("caos-server: pruning request refs: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    fn with_stdin(mut command: std::process::Command, input: &[u8]) -> std::process::Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap();
        output
    }

    /// What a push of a long new history costs git under `receive.fsckObjects`:
    /// `unpack-objects --strict` walks it recursively, a frame or more per
    /// commit. Measured: 10,000 commits fit git's default 8 MiB stack, 30,000
    /// overflow it (SIGSEGV), and 100,000 unpack in 6 s under
    /// [`super::GIT_STACK_BYTES`]. Both halves run, so the test fails if the
    /// chain ever stops reaching the overflow, not only if the fix stops working.
    #[test]
    fn a_long_commit_chain_unpacks_only_with_the_raised_stack() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        const COMMITS: u32 = 30_000;
        let base = std::env::temp_dir().join(format!("caos-stack-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let init = |name: &str| {
            let dir = base.join(name);
            let status = Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&dir)
                .status()
                .unwrap();
            assert!(status.success());
            dir
        };
        let source = init("source");
        let mut stream = String::new();
        for i in 1..=COMMITS {
            stream.push_str(&format!(
                "commit refs/heads/main\nmark :{i}\ncommitter a <a> {i} +0000\ndata 0\n"
            ));
            if i > 1 {
                stream.push_str(&format!("from :{}\n", i - 1));
            }
            stream.push('\n');
        }
        let mut import = Command::new("git");
        import
            .arg("--git-dir")
            .arg(&source)
            .args(["fast-import", "--quiet"]);
        let imported = with_stdin(import, stream.as_bytes());
        assert!(
            imported.status.success(),
            "{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        let mut pack = Command::new("git");
        pack.arg("--git-dir")
            .arg(&source)
            .args(["pack-objects", "--revs", "--stdout", "-q"]);
        let pack = with_stdin(pack, b"refs/heads/main\n");
        assert!(pack.status.success());
        let unpack = |name: &str, stack: u64| {
            let target = init(name);
            let mut command = Command::new("git");
            command
                .arg("--git-dir")
                .arg(&target)
                .args(["unpack-objects", "--strict", "-q"]);
            unsafe {
                command.pre_exec(move || super::set_stack_limit(stack).map(|_| ()));
            }
            with_stdin(command, &pack.stdout).status
        };
        let default = unpack("default", 8 << 20);
        assert!(
            !default.success(),
            "{COMMITS} commits no longer overflow 8 MiB"
        );
        let raised = unpack("raised", super::GIT_STACK_BYTES);
        assert!(raised.success(), "{raised:?} under the raised stack");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn git_writes_loose_objects_at_the_servers_level() {
        assert_eq!(super::LOOSE_LEVEL, super::loose::LEVEL.to_string());
    }

    #[test]
    fn a_form_body_decodes_plus_as_space_and_a_query_does_not() {
        use super::join_params;
        // curl --data-urlencode "cmd=echo written > out.txt"
        assert_eq!(
            join_params("a=1+2", "cmd=echo+written+%3E+out.txt&p=a%2Bb"),
            "a=1+2&cmd=echo%20written%20%3E%20out.txt&p=a%2Bb"
        );
        assert_eq!(join_params("a=1", ""), "a=1");
        assert_eq!(join_params("", " b=2 \n"), "b=2");
        assert_eq!(join_params("", ""), "");
    }

    use super::{git_settings, initialize_repo, run_required_git, settings_env};
    use std::process::Command;

    #[test]
    fn required_git_command_reports_nonzero_status() {
        let absent = std::env::temp_dir().join(format!(
            "caos-required-git-test-{}-absent",
            std::process::id()
        ));
        std::fs::remove_dir_all(&absent).ok();
        let error = run_required_git(&[
            "-C",
            absent.to_str().unwrap(),
            "config",
            "http.receivepack",
            "true",
        ])
        .unwrap_err();
        assert!(error.contains("required command git"), "{error}");
        assert!(error.contains("exited with"), "{error}");
    }

    #[test]
    fn server_owned_refs_are_hidden_from_fetch_but_available_for_push_negotiation() {
        let dir = std::env::temp_dir().join(format!("caos-hidden-ref-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let env = settings_env(&git_settings(None));
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .envs(env.iter().map(|(key, value)| (key, value)))
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        Command::new("git")
            .args(["init", "-q", "--bare", dir.to_str().unwrap()])
            .status()
            .unwrap();
        // An administrator's own value, in the repo's config, survives beside
        // the server's, which never touch the file.
        Command::new("git")
            .args(["-C", dir.to_str().unwrap(), "config", "--add"])
            .args(["uploadpack.hideRefs", "refs/private/"])
            .status()
            .unwrap();
        let config = std::fs::read(dir.join("config")).unwrap();
        let blob = git(&["-C", dir.to_str().unwrap(), "hash-object", "-w", "--stdin"]);
        let blob = blob.trim();
        for refname in [
            "refs/caos/req/request",
            "refs/caos/res/result",
            "refs/caos/v2/users/u-1/conversations/active/c-74657374",
            "refs/private/x",
            // A resolved locator's pin->tree memo (`mod locator`). One per pin, so
            // a server that has resolved a few hundred would advertise a few
            // hundred to every client that dials it.
            &format!("{}{}", crate::locator::TREE_REF_PREFIX, "a".repeat(40)),
        ] {
            git(&["-C", dir.to_str().unwrap(), "update-ref", refname, blob]);
        }

        let upload = git(&[
            "upload-pack",
            "--stateless-rpc",
            "--advertise-refs",
            dir.to_str().unwrap(),
        ]);
        assert!(!upload.contains("refs/caos/req/"));
        assert!(!upload.contains("refs/caos/res/"));
        assert!(!upload.contains(crate::locator::TREE_REF_PREFIX));
        assert!(!upload.contains("refs/private/"));
        assert!(upload.contains("refs/caos/v2/users/"));

        let receive = git(&[
            "receive-pack",
            "--stateless-rpc",
            "--advertise-refs",
            dir.to_str().unwrap(),
        ]);
        assert!(receive.contains("refs/caos/req/"));
        assert!(receive.contains("refs/caos/res/"));
        assert!(receive.contains("refs/caos/v2/users/"));

        // The settings reached git without the repository's config changing.
        assert_eq!(std::fs::read(dir.join("config")).unwrap(), config);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_created_repo_gets_the_settings_and_a_read_only_config() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("caos-init-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        initialize_repo(dir.to_str().unwrap()).unwrap();
        let mode = std::fs::metadata(dir.join("config"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o444);
        let get = |key: &str| {
            let output = Command::new("git")
                .args(["-C", dir.to_str().unwrap(), "config", "--get-all", key])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        assert_eq!(get("gc.auto").trim(), "0");
        assert_eq!(get("uploadpack.hideRefs").lines().count(), 3);
        // A process's own hooks directory is never written into a repo.
        assert!(get("core.hooksPath").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_hook_is_this_binary_under_the_hooks_name() {
        let dir = std::env::temp_dir().join(format!("caos-hooks-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let hooks = crate::loose::install_hooks(&dir).unwrap();
        // Twice: a restart replaces the link.
        crate::loose::install_hooks(&dir).unwrap();
        let target = std::fs::read_link(hooks.join("pre-receive")).unwrap();
        assert_eq!(target, std::env::current_exe().unwrap());
        let settings = git_settings(Some(&hooks));
        assert!(settings
            .iter()
            .any(|(key, value)| *key == "core.hooksPath" && value == &hooks.display().to_string()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    mod run_http_tests {
        use std::collections::HashMap;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};

        use gix::objs::tree::EntryKind;

        use crate::storage::{store_git_blob, store_git_tree};
        use crate::{compute, handle, initialize_repo, secret_store, Config};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        struct StubRedis {
            addr: String,
            kv: Arc<Mutex<HashMap<String, String>>>,
            stop: Arc<AtomicBool>,
        }

        impl StubRedis {
            fn start() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let addr = listener.local_addr().unwrap().to_string();
                let kv: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
                let lists: Arc<Mutex<HashMap<String, Vec<String>>>> =
                    Arc::new(Mutex::new(HashMap::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let kv_bg = Arc::clone(&kv);
                let lists_bg = Arc::clone(&lists);
                let stop_bg = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while let Ok((mut stream, _)) = listener.accept() {
                        if stop_bg.load(Ordering::Relaxed) {
                            break;
                        }
                        let kv = Arc::clone(&kv_bg);
                        let lists = Arc::clone(&lists_bg);
                        std::thread::spawn(move || {
                            let mut reader = BufReader::new(stream.try_clone().unwrap());
                            while let Some(args) = Self::read_command(&mut reader) {
                                let cmd = args.first().map(|s| s.to_ascii_uppercase());
                                match cmd.as_deref() {
                                    Some("GET") if args.len() == 2 => {
                                        let val = kv.lock().unwrap().get(&args[1]).cloned();
                                        match val {
                                            Some(v) => {
                                                let _ = write!(stream, "${}\r\n", v.len());
                                                let _ = stream.write_all(v.as_bytes());
                                                let _ = stream.write_all(b"\r\n");
                                            }
                                            None => {
                                                let _ = stream.write_all(b"$-1\r\n");
                                            }
                                        }
                                    }
                                    Some("SET") if args.len() >= 3 => {
                                        kv.lock().unwrap().insert(args[1].clone(), args[2].clone());
                                        let _ = stream.write_all(b"+OK\r\n");
                                    }
                                    Some("RPUSH") if args.len() >= 3 => {
                                        let mut guard = lists.lock().unwrap();
                                        let list = guard.entry(args[1].clone()).or_default();
                                        for item in &args[2..] {
                                            list.push(item.clone());
                                        }
                                        let _ = write!(stream, ":{}\r\n", list.len());
                                    }
                                    Some("LRANGE") if args.len() >= 4 => {
                                        let items = lists
                                            .lock()
                                            .unwrap()
                                            .get(&args[1])
                                            .cloned()
                                            .unwrap_or_default();
                                        let _ = write!(stream, "*{}\r\n", items.len());
                                        for item in items {
                                            let _ = write!(stream, "${}\r\n", item.len());
                                            let _ = stream.write_all(item.as_bytes());
                                            let _ = stream.write_all(b"\r\n");
                                        }
                                    }
                                    _ => {
                                        let _ = stream.write_all(b"-ERR unsupported\r\n");
                                    }
                                }
                            }
                        });
                    }
                });
                Self { addr, kv, stop }
            }

            fn read_command(reader: &mut BufReader<impl Read>) -> Option<Vec<String>> {
                let mut line = String::new();
                if reader.read_line(&mut line).ok()? == 0 {
                    return None;
                }
                let count: usize = line.trim().strip_prefix('*')?.parse().ok()?;
                let mut args = Vec::with_capacity(count);
                for _ in 0..count {
                    line.clear();
                    reader.read_line(&mut line).ok()?;
                    let len: usize = line.trim().strip_prefix('$')?.parse().ok()?;
                    let mut buf = vec![0u8; len + 2];
                    reader.read_exact(&mut buf).ok()?;
                    buf.truncate(len);
                    args.push(String::from_utf8(buf).ok()?);
                }
                Some(args)
            }

            fn get(&self, key: &str) -> Option<String> {
                self.kv.lock().unwrap().get(key).cloned()
            }

            fn set(&self, key: &str, val: &str) {
                self.kv
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), val.to_string());
            }
        }

        impl Drop for StubRedis {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Relaxed);
                let _ = std::net::TcpStream::connect(&self.addr);
            }
        }

        struct TestHarness {
            dir: PathBuf,
            config: Arc<Config>,
            redis: StubRedis,
            base_url: String,
            server: Arc<tiny_http::Server>,
        }

        impl TestHarness {
            fn start() -> Self {
                let n = SEQ.fetch_add(1, Ordering::Relaxed);
                let dir = std::env::temp_dir()
                    .join(format!("caos-run-http-test-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let git_dir = dir.join("git");
                initialize_repo(git_dir.to_str().unwrap()).unwrap();
                let secrets_git = dir.join("secrets.git");
                secret_store::init(secrets_git.to_str().unwrap()).unwrap();
                let repo = gix::open(&git_dir).unwrap().into_sync();
                let redis = StubRedis::start();
                let config = Arc::new(Config {
                    registry_push_url: "http://127.0.0.1:9".to_string(),
                    registry_pull_host: "localhost:5000".to_string(),
                    redis_addr: redis.addr.clone(),
                    cache_namespace: format!("ns-{n}"),
                    git_dir: git_dir.display().to_string(),
                    secrets_git: secrets_git.display().to_string(),
                    repo,
                });
                let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
                let base_url = format!("http://{}", server.server_addr().to_ip().unwrap());
                let server_bg = Arc::clone(&server);
                let config_bg = Arc::clone(&config);
                std::thread::spawn(move || {
                    for req in server_bg.incoming_requests() {
                        let cfg = Arc::clone(&config_bg);
                        std::thread::spawn(move || {
                            let _ = handle(cfg, req);
                        });
                    }
                });
                Self {
                    dir,
                    config,
                    redis,
                    base_url,
                    server,
                }
            }

            fn store_blob(&self, bytes: &[u8]) -> gix::ObjectId {
                store_git_blob(&self.config, bytes).unwrap()
            }

            fn store_pooled_arg_tree(
                &self,
                base_entry: gix::objs::tree::Entry,
                pool: &str,
            ) -> (String, String) {
                let pool_oid = self.store_blob(pool.as_bytes());
                let mut entries = vec![
                    base_entry,
                    gix::objs::tree::Entry {
                        mode: EntryKind::Blob.into(),
                        filename: caos_world::REQUIRED_POOL_ARG.as_bytes().to_vec().into(),
                        oid: pool_oid,
                    },
                ];
                entries.sort_by(|a, b| a.filename.cmp(&b.filename));
                let tree = store_git_tree(&self.config, entries).unwrap();
                (tree.to_string(), pool_oid.to_string())
            }

            /// Claim one job from `pool_oid` via `/runner/poll` and answer it via
            /// `/runner/result`, returning the claimed job JSON.
            fn serve_one_runner_job(
                &self,
                pool_oid: String,
                result_text: String,
            ) -> std::thread::JoinHandle<serde_json::Value> {
                let base_url = self.base_url.clone();
                std::thread::spawn(move || {
                    let poll_body = serde_json::json!({
                        "required": { caos_world::REQUIRED_POOL_ARG: pool_oid },
                        "ttl_ms": 5000,
                    })
                    .to_string();
                    let poll_resp = minreq::post(format!("{base_url}/runner/poll"))
                        .with_body(poll_body)
                        .send()
                        .unwrap();
                    assert_eq!(poll_resp.status_code, 200);
                    let parsed: serde_json::Value =
                        serde_json::from_slice(poll_resp.as_bytes()).unwrap();
                    let job = parsed["job"].clone();
                    assert!(job.is_object(), "expected claimed job, got {parsed}");
                    let result_body = serde_json::json!({
                        "req": job["req"],
                        "nonce": job["nonce"],
                        "ok": true,
                        "result": result_text,
                    })
                    .to_string();
                    let res_resp = minreq::post(format!("{base_url}/runner/result"))
                        .with_body(result_body)
                        .send()
                        .unwrap();
                    assert_eq!(res_resp.status_code, 200);
                    job
                })
            }

            fn result_cache_key(&self, arg_tree: &str) -> String {
                format!("caos:result:{}:{arg_tree}", self.config.cache_namespace)
            }
        }

        impl Drop for TestHarness {
            fn drop(&mut self) {
                self.server.unblock();
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        #[test]
        fn run_reports_cache_and_image_headers_and_nocache_bypasses_read_and_write() {
            let h = TestHarness::start();
            let image_digest = "a1".repeat(32);
            let docker_ref = format!("docker://example.com/worker@sha256:{image_digest}");
            let base_blob = h.store_blob(docker_ref.as_bytes());
            let (arg_tree, pool_oid) = h.store_pooled_arg_tree(
                gix::objs::tree::Entry {
                    mode: EntryKind::Blob.into(),
                    filename: b"base".to_vec().into(),
                    oid: base_blob,
                },
                "test-pool-nocache-1",
            );
            let out_1 = h.store_blob(b"first-output");
            let out_2 = h.store_blob(b"replay-output");

            // 1. Cold `/run`: dispatches to the runner, returns `miss` + resolved image,
            //    and caches `out_1` in Redis.
            let runner = h.serve_one_runner_job(pool_oid.clone(), format!("blob {out_1}"));
            let resp = minreq::get(format!("{}/run?req={arg_tree}", h.base_url))
                .send()
                .unwrap();
            let claimed = runner.join().unwrap();
            assert_eq!(claimed["req"].as_str(), Some(arg_tree.as_str()));
            assert_eq!(resp.status_code, 200);
            assert_eq!(resp.as_str().unwrap(), format!("blob {out_1}\n"));
            assert_eq!(
                resp.headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("miss")
            );
            assert_eq!(
                resp.headers
                    .get(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some(docker_ref.as_str())
            );
            assert_eq!(
                h.redis.get(&h.result_cache_key(&arg_tree)).as_deref(),
                Some(format!("blob {out_1}").as_str())
            );

            // 2. Warm `/run` with NO runner: answered from Redis with `hit` + resolved image.
            let hit = minreq::get(format!("{}/run?req={arg_tree}", h.base_url))
                .send()
                .unwrap();
            assert_eq!(hit.status_code, 200);
            assert_eq!(hit.as_str().unwrap(), format!("blob {out_1}\n"));
            assert_eq!(
                hit.headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("hit")
            );
            assert_eq!(
                hit.headers
                    .get(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some(docker_ref.as_str())
            );

            // 3. `/run?req=...&nocache=1`: genuinely dispatches the exact unsalted ArgTree
            //    to a runner (`miss`), and leaves the existing Redis entry untouched.
            let replay_runner = h.serve_one_runner_job(pool_oid.clone(), format!("blob {out_2}"));
            let replay = minreq::get(format!("{}/run?req={arg_tree}&nocache=1", h.base_url))
                .send()
                .unwrap();
            let replay_claimed = replay_runner.join().unwrap();
            assert_eq!(replay_claimed["req"].as_str(), Some(arg_tree.as_str()));
            assert_eq!(replay.status_code, 200);
            assert_eq!(replay.as_str().unwrap(), format!("blob {out_2}\n"));
            assert_eq!(
                replay
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("miss")
            );
            assert_eq!(
                replay
                    .headers
                    .get(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some(docker_ref.as_str())
            );
            assert_eq!(
                h.redis.get(&h.result_cache_key(&arg_tree)).as_deref(),
                Some(format!("blob {out_1}").as_str()),
                "nocache=1 must not overwrite an existing cached result"
            );

            // 4. `/run?req=...&nocache=1` on a cold ArgTree leaves Redis unpopulated.
            let (cold_tree, cold_pool) = h.store_pooled_arg_tree(
                gix::objs::tree::Entry {
                    mode: EntryKind::Blob.into(),
                    filename: b"base".to_vec().into(),
                    oid: base_blob,
                },
                "test-pool-nocache-cold",
            );
            let cold_runner = h.serve_one_runner_job(cold_pool, format!("blob {out_1}"));
            let cold_replay = minreq::get(format!("{}/run?req={cold_tree}&nocache=1", h.base_url))
                .send()
                .unwrap();
            cold_runner.join().unwrap();
            assert_eq!(cold_replay.status_code, 200);
            assert_eq!(
                cold_replay
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("miss")
            );
            assert!(
                h.redis.get(&h.result_cache_key(&cold_tree)).is_none(),
                "nocache=1 on a cold ArgTree must not populate Redis"
            );

            // 5. Malformed `nocache` parameter is rejected with 400.
            let bad = minreq::get(format!("{}/run?req={arg_tree}&nocache=bogus", h.base_url))
                .send()
                .unwrap();
            assert_eq!(bad.status_code, 400);
        }

        #[test]
        fn git_image_cache_hit_uses_cached_manifest_digest_without_cold_conversion() {
            let h = TestHarness::start();
            let cfg_blob = h.store_blob(b"{}");
            let image_tree = store_git_tree(
                &h.config,
                vec![gix::objs::tree::Entry {
                    mode: EntryKind::Blob.into(),
                    filename: b"config.json".to_vec().into(),
                    oid: cfg_blob,
                }],
            )
            .unwrap();
            let (arg_tree, _pool_oid) = h.store_pooled_arg_tree(
                gix::objs::tree::Entry {
                    mode: EntryKind::Tree.into(),
                    filename: b"base".to_vec().into(),
                    oid: image_tree,
                },
                "test-pool-git-image-hit",
            );
            let out = h.store_blob(b"cached-tree-result");
            h.redis
                .set(&h.result_cache_key(&arg_tree), &format!("blob {out}"));

            // When `caos:image:<hash>` is absent, the result hit still succeeds without
            // attempting a cold OCI conversion against the unreachable registry.
            let hit_unconverted = minreq::get(format!("{}/run?req={arg_tree}", h.base_url))
                .send()
                .unwrap();
            assert_eq!(hit_unconverted.status_code, 200);
            assert_eq!(hit_unconverted.as_str().unwrap(), format!("blob {out}\n"));
            assert_eq!(
                hit_unconverted
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("hit")
            );
            assert!(!hit_unconverted
                .headers
                .contains_key(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase()));

            // Once the converted manifest digest is in `caos:image:<hash>`, a hit
            // surfaces the full digest-pinned image reference.
            let manifest_digest = format!("sha256:{}", "d4".repeat(32));
            h.redis
                .set(&format!("caos:image:{image_tree}"), &manifest_digest);
            let hit_with_image = minreq::get(format!("{}/run?req={arg_tree}", h.base_url))
                .send()
                .unwrap();
            assert_eq!(hit_with_image.status_code, 200);
            assert_eq!(
                hit_with_image
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("hit")
            );
            assert_eq!(
                hit_with_image
                    .headers
                    .get(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some(format!("docker://localhost:5000/caos@{manifest_digest}").as_str())
            );
        }

        #[test]
        fn concurrent_nocache_does_not_prevent_normal_caller_from_caching() {
            let h = TestHarness::start();
            let image_digest = "e5".repeat(32);
            let docker_ref = format!("docker://example.com/worker@sha256:{image_digest}");
            let base_blob = h.store_blob(docker_ref.as_bytes());
            let (arg_tree, pool_oid) = h.store_pooled_arg_tree(
                gix::objs::tree::Entry {
                    mode: EntryKind::Blob.into(),
                    filename: b"base".to_vec().into(),
                    oid: base_blob,
                },
                "test-pool-concurrent-nocache",
            );
            let out = h.store_blob(b"deterministic-step");

            let r1 = h.serve_one_runner_job(pool_oid.clone(), format!("blob {out}"));
            let r2 = h.serve_one_runner_job(pool_oid, format!("blob {out}"));

            let url_normal = format!("{}/run?req={arg_tree}", h.base_url);
            let url_nocache = format!("{}/run?req={arg_tree}&nocache=1", h.base_url);
            let t_normal = std::thread::spawn(move || minreq::get(url_normal).send().unwrap());
            let t_nocache = std::thread::spawn(move || minreq::get(url_nocache).send().unwrap());

            let resp_normal = t_normal.join().unwrap();
            let resp_nocache = t_nocache.join().unwrap();
            r1.join().unwrap();
            r2.join().unwrap();

            assert_eq!(resp_normal.status_code, 200);
            assert_eq!(resp_nocache.status_code, 200);
            assert_eq!(
                resp_nocache
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("miss")
            );
            assert_eq!(
                h.redis.get(&h.result_cache_key(&arg_tree)).as_deref(),
                Some(format!("blob {out}").as_str())
            );
        }

        #[test]
        fn native_pool_worker_image_resolves_sentinel_stamps_miss_and_hit_and_rejects_pool_conflict(
        ) {
            let h = TestHarness::start();
            let pool_name = format!("unforged-test-{}", SEQ.fetch_add(1, Ordering::Relaxed));
            let sentinel = format!("docker://seeded-{pool_name}");
            let base_blob = h.store_blob(sentinel.as_bytes());
            let (arg_tree, pool_oid) = h.store_pooled_arg_tree(
                gix::objs::tree::Entry {
                    mode: EntryKind::Blob.into(),
                    filename: b"base".to_vec().into(),
                    oid: base_blob,
                },
                &pool_name,
            );
            let worker_img_v1 = format!("native@sha256:{}", "b2".repeat(32));
            let worker_img_v2 = format!("native@sha256:{}", "c3".repeat(32));
            let out_v1 = h.store_blob(b"native-out-v1");
            let out_v2 = h.store_blob(b"native-out-v2");

            // 1. Park a native runner advertising `worker_img_v1`: `/resolve-image` resolves
            //    BOTH `seeded-<pool>` and `docker://seeded-<pool>` to `worker_img_v1`!
            let base_url = h.base_url.clone();
            let p_oid = pool_oid.clone();
            let img_v1 = worker_img_v1.clone();
            let res_v1 = format!("blob {out_v1}");
            let runner_v1 = std::thread::spawn(move || {
                let poll_body = serde_json::json!({
                    "required": { caos_world::REQUIRED_POOL_ARG: p_oid },
                    "ttl_ms": 5000,
                    "worker_image": img_v1,
                })
                .to_string();
                let poll_resp = minreq::post(format!("{base_url}/runner/poll"))
                    .with_body(poll_body)
                    .send()
                    .unwrap();
                assert_eq!(poll_resp.status_code, 200);
                let parsed: serde_json::Value =
                    serde_json::from_slice(poll_resp.as_bytes()).unwrap();
                let job = parsed["job"].clone();
                let result_body = serde_json::json!({
                    "req": job["req"],
                    "nonce": job["nonce"],
                    "ok": true,
                    "result": res_v1,
                    "worker_image": img_v1,
                })
                .to_string();
                let r = minreq::post(format!("{base_url}/runner/result"))
                    .with_body(result_body)
                    .send()
                    .unwrap();
                assert_eq!(r.status_code, 200);
            });

            // Wait briefly for `runner_v1`'s poll to park, then verify `/resolve-image`!
            let mut resolved_body = String::new();
            for _ in 0..50 {
                let r = minreq::get(format!(
                    "{}/resolve-image?image=seeded-{pool_name}",
                    h.base_url
                ))
                .send()
                .unwrap();
                if r.status_code == 200 {
                    let s = r.as_str().unwrap().to_string();
                    if s == worker_img_v1 {
                        resolved_body = s;
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(resolved_body, worker_img_v1);

            // 2. Cold `/run`: returns `X-Caos-Cache: miss` and `X-Caos-Image: native@sha256:...`
            let miss_resp = minreq::get(format!("{}/run?req={arg_tree}", h.base_url))
                .send()
                .unwrap();
            runner_v1.join().unwrap();
            assert_eq!(miss_resp.status_code, 200);
            assert_eq!(miss_resp.as_str().unwrap(), format!("blob {out_v1}\n"));
            assert_eq!(
                miss_resp
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("miss")
            );
            assert_eq!(
                miss_resp
                    .headers
                    .get(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some(worker_img_v1.as_str())
            );

            // 3. Warm `/run` with ZERO runners parked: returns `X-Caos-Cache: hit` AND preserves
            //    `X-Caos-Image: native@sha256:...` from `caos:run-image:` (never raw sentinel)!
            let hit_resp = minreq::get(format!("{}/run?req={arg_tree}", h.base_url))
                .send()
                .unwrap();
            assert_eq!(hit_resp.status_code, 200);
            assert_eq!(hit_resp.as_str().unwrap(), format!("blob {out_v1}\n"));
            assert_eq!(
                hit_resp
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("hit")
            );
            assert_eq!(
                hit_resp
                    .headers
                    .get(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some(worker_img_v1.as_str())
            );

            // 4. `nocache=1` replay on an upgraded runner (`worker_img_v2`) returns `v2` receipt
            //    without overwriting cached `v1` result or image!
            let base_url2 = h.base_url.clone();
            let p_oid2 = pool_oid.clone();
            let img_v2 = worker_img_v2.clone();
            let res_v2 = format!("blob {out_v2}");
            let runner_v2 = std::thread::spawn(move || {
                let poll_body = serde_json::json!({
                    "required": { caos_world::REQUIRED_POOL_ARG: p_oid2 },
                    "ttl_ms": 5000,
                    "worker_image": img_v2,
                })
                .to_string();
                let poll_resp = minreq::post(format!("{base_url2}/runner/poll"))
                    .with_body(poll_body)
                    .send()
                    .unwrap();
                let parsed: serde_json::Value =
                    serde_json::from_slice(poll_resp.as_bytes()).unwrap();
                let job = parsed["job"].clone();
                let result_body = serde_json::json!({
                    "req": job["req"],
                    "nonce": job["nonce"],
                    "ok": true,
                    "result": res_v2,
                })
                .to_string();
                minreq::post(format!("{base_url2}/runner/result"))
                    .with_body(result_body)
                    .send()
                    .unwrap();
            });
            let nocache_resp = minreq::get(format!("{}/run?req={arg_tree}&nocache=1", h.base_url))
                .send()
                .unwrap();
            runner_v2.join().unwrap();
            assert_eq!(nocache_resp.status_code, 200);
            assert_eq!(nocache_resp.as_str().unwrap(), format!("blob {out_v2}\n"));
            assert_eq!(
                nocache_resp
                    .headers
                    .get(&compute::RUN_CACHE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some("miss")
            );
            assert_eq!(
                nocache_resp
                    .headers
                    .get(&compute::RUN_IMAGE_HEADER.to_ascii_lowercase())
                    .map(String::as_str),
                Some(worker_img_v2.as_str())
            );

            // 5. Two concurrent parked runners advertising different `worker_image`s in the same
            //    pool cause `/resolve-image` to fail closed with `409 Conflict`!
            let conflict_pool = format!("conflict-pool-{}", SEQ.fetch_add(1, Ordering::Relaxed));
            let conflict_oid = crate::runner::pool_oid(&conflict_pool);
            let spawn_idle_poll = |img: String| {
                let u = h.base_url.clone();
                let p = conflict_oid.clone();
                std::thread::spawn(move || {
                    let b = serde_json::json!({
                        "required": { caos_world::REQUIRED_POOL_ARG: p },
                        "ttl_ms": 600,
                        "worker_image": img,
                    })
                    .to_string();
                    let _ = minreq::post(format!("{u}/runner/poll")).with_body(b).send();
                })
            };
            let c1 = spawn_idle_poll(worker_img_v1.clone());
            let c2 = spawn_idle_poll(worker_img_v2.clone());
            let mut saw_409 = false;
            for _ in 0..40 {
                let r = minreq::get(format!(
                    "{}/resolve-image?image=seeded-{conflict_pool}",
                    h.base_url
                ))
                .send()
                .unwrap();
                if r.status_code == 409 {
                    saw_409 = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(
                saw_409,
                "conflicting live runner worker_image digests in one pool must fail closed with 409!"
            );
            c1.join().unwrap();
            c2.join().unwrap();

            // 6. Mutable / unpinned `worker_image` on `/runner/poll` is rejected with `400`!
            let bad_poll = minreq::post(format!("{}/runner/poll", h.base_url))
                .with_body(
                    serde_json::json!({
                        "required": {},
                        "ttl_ms": 50,
                        "worker_image": "ubuntu:latest",
                    })
                    .to_string(),
                )
                .send()
                .unwrap();
            assert_eq!(bad_poll.status_code, 400);
        }
    }
}
