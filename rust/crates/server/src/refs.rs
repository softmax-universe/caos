//! Named, mutable refs under `refs/heads/`: `GET /ref`, `POST /ref`,
//! `GET|POST /ref/wait`, and the wait behind `caos ref-then` (design/refs.md).
//!
//! Everything else in the store is addressed by content. These are the one
//! deliberate exception offered to workers and HTTP clients, for state that has
//! to outlive a request (std/actor keeps its state on one), for jobs that have
//! to signal each other (tests/sub-run), and for external nodes watching shared
//! refs without polling. They are ordinary git refs in the server's repository,
//! so `git ls-remote` and a receive-pack push see the same values.
//!
//! ONLY `refs/heads/` IS OFFERED. The rest of the namespace is the server's:
//! `refs/keys/` is the secret store, `refs/caos/res/` pins results and
//! `refs/caos/seed` once named the seed tree. Moving one of those would be
//! corrupting the server, not using a feature, so the endpoints refuse them by
//! name. Git requires a ref under `refs/heads/` to name a commit, so the values
//! are commits (`caos put-commit` mints one).
//!
//! A WRITE IS A COMPARE-AND-SWAP unless the caller says otherwise: it names
//! the value it expects to replace (`absent` for create-only), and git checks
//! that under the ref's lock. A stale expectation is a 409 naming the value
//! the ref actually has, so a caller that lost a race knows what it lost to.
//!
//! A WAIT HOLDS A SERVER THREAD, NOT A RUNNER SLOT. `ref-then` is a
//! continuation (the worker records it and exits, and the server's promise
//! interpreter blocks here); `GET /ref/wait` and `POST /ref/wait` offer the
//! same wait directly over HTTP so an external client can follow one or more
//! refs on a long-lived stream without dispatching a worker job. Writes through
//! `POST /ref` and pushes through `POST /git-receive-pack` wake waiters at
//! once; a write that bypassed the server is noticed by the once-a-second
//! recheck.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::{Config, HttpError};

/// The one namespace a worker or HTTP client may read, write and watch.
pub(crate) const REF_PREFIX: &str = "refs/heads/";

/// How a missing ref is spelled, in both directions: what `GET /ref` answers
/// for one, and what a caller passes to mean "I expect there to be none" or
/// "delete it". The same word `caos push-git --expected=absent` uses.
pub(crate) const ABSENT: &str = "absent";

/// What `GET /ref/wait` answers when `timeout` passes with the ref still at
/// `expected`: an ordinary 200 turnover, distinct from both `<oid>` and
/// `absent`, so an idle long-poll never reads as an HTTP failure.
pub(crate) const UNCHANGED: &str = "unchanged";

/// Git's spelling of "no old value" on an `update-ref` command line.
const ZEROS: &str = "0000000000000000000000000000000000000000";

/// The longest a `ref-then` or `/ref/wait` may wait. A day: long enough for any
/// human-paced handshake, short enough that a forgotten waiter does not hold a
/// server thread for ever.
pub(crate) const MAX_WAIT_SECS: u64 = 24 * 60 * 60;

/// Default wait for `GET|POST /ref/wait` when `timeout` is omitted: short
/// enough to turn over cleanly across relays and NAT idle windows, long enough
/// to keep an idle watcher quiet.
pub(crate) const DEFAULT_HTTP_WAIT_SECS: u64 = 60;

/// Most refs one `POST /ref/wait` call may watch at once: keeps the argv handed
/// to `git for-each-ref` bounded.
pub(crate) const MAX_WATCHED_REFS: usize = 256;

/// How often a waiter rereads its refs when nothing has woken it. Only a write
/// that bypassed [`set`] and `/git-receive-pack` needs this; writes through
/// either wake waiters immediately.
const RECHECK: Duration = Duration::from_secs(1);

/// Bumped on every write through [`set`] (and `/git-receive-pack`), so a waiter
/// can sleep until something has changed rather than rereading on a timer.
static GENERATION: Mutex<u64> = Mutex::new(0);
static CHANGED: Condvar = Condvar::new();

/// What a ref holds: nothing, or an object id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    Absent,
    Oid(String),
}

impl Value {
    /// Parse [`ABSENT`] or a lowercase 40-character hash. The all-zeros hash is
    /// refused: it is git's private spelling of "none", and accepting it would
    /// give "absent" two names.
    pub(crate) fn parse(text: &str) -> Result<Value, String> {
        let text = text.trim();
        if text == ABSENT {
            return Ok(Value::Absent);
        }
        if text.len() == 40
            && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && text != ZEROS
        {
            return Ok(Value::Oid(text.to_string()));
        }
        Err(format!(
            "expected a lowercase 40-character hash or {ABSENT:?}, got {text:?}"
        ))
    }

    /// The value as `GET /ref` and `/ref/wait` print it.
    pub(crate) fn text(&self) -> &str {
        match self {
            Value::Absent => ABSENT,
            Value::Oid(oid) => oid,
        }
    }
}

/// What a write requires the ref to hold first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Expected {
    /// Anything: an unconditional write (`caos ref-set --force`).
    Any,
    /// Exactly this, or the write is refused with a 409.
    Is(Value),
}

/// Refuse a name outside [`REF_PREFIX`], or one git would not accept.
pub(crate) fn validate_name(name: &str) -> Result<(), HttpError> {
    if !name.starts_with(REF_PREFIX) || name.len() == REF_PREFIX.len() {
        return Err(HttpError::new(
            400,
            format!(
                "ref {name:?} must be under {REF_PREFIX}: the rest of the namespace is the \
                 server's own (refs/keys holds the secret store)"
            ),
        ));
    }
    let ok = Command::new("git")
        .args(["check-ref-format", name])
        .status()
        .map_err(|e| HttpError::new(500, format!("running git check-ref-format: {e}")))?
        .success();
    if !ok {
        return Err(HttpError::new(
            400,
            format!("{name:?} is not a valid ref name (git check-ref-format refuses it)"),
        ));
    }
    Ok(())
}

/// Current values of `names`, read in one `git for-each-ref` call.
///
/// `for-each-ref` rather than `show-ref --verify` or `rev-parse`: it answers a
/// missing ref with no output and success, so ABSENT and BROKEN cannot be
/// confused (AGENTS.md), and it never guesses at a short name. Its pattern
/// also matches refs BELOW a requested name, so only exact keys seeded into
/// `values` are filled in.
pub(crate) fn read_many(
    git_dir: &str,
    names: &[&str],
) -> Result<BTreeMap<String, Value>, HttpError> {
    let mut values: BTreeMap<String, Value> = names
        .iter()
        .map(|&name| (name.to_string(), Value::Absent))
        .collect();
    if values.is_empty() {
        return Ok(values);
    }
    let output = Command::new("git")
        .args([
            "-C",
            git_dir,
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "--",
        ])
        .args(values.keys())
        .output()
        .map_err(|e| HttpError::new(500, format!("running git for-each-ref: {e}")))?;
    if !output.status.success() {
        return Err(HttpError::new(
            500,
            format!(
                "reading refs: git for-each-ref exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((oid, refname)) = line.split_once(' ') {
            if let Some(slot) = values.get_mut(refname) {
                *slot = Value::parse(oid)
                    .map_err(|e| HttpError::new(500, format!("{refname}: {e}")))?;
            }
        }
    }
    Ok(values)
}

/// The ref's current value.
pub(crate) fn read(git_dir: &str, name: &str) -> Result<Value, HttpError> {
    Ok(read_many(git_dir, &[name])?
        .remove(name)
        .unwrap_or(Value::Absent))
}

/// Move `name` to `new` (deleting it for [`Value::Absent`]) if it holds what
/// `expected` says. A stale expectation is a 409 that names the current value.
pub(crate) fn set(
    git_dir: &str,
    name: &str,
    new: &Value,
    expected: &Expected,
) -> Result<(), HttpError> {
    if let Value::Oid(oid) = new {
        let kind = object_kind(git_dir, oid)?;
        if kind != "commit" {
            return Err(HttpError::new(
                400,
                format!(
                    "{oid} is a {kind}: a ref under {REF_PREFIX} names a commit \
                     (`caos put-commit` mints one)"
                ),
            ));
        }
    }

    // Deleting a ref that is already gone needs no git at all, and `update-ref
    // -d` has no way to say "expect it to be absent".
    if *new == Value::Absent {
        let current = read(git_dir, name)?;
        if current == Value::Absent {
            return match expected {
                Expected::Is(Value::Oid(want)) => Err(conflict(name, &current, want)),
                _ => Ok(()),
            };
        }
        if *expected == Expected::Is(Value::Absent) {
            return Err(conflict(name, &current, ABSENT));
        }
    }

    let old = match expected {
        Expected::Any => None,
        Expected::Is(Value::Absent) => Some(ZEROS.to_string()),
        Expected::Is(Value::Oid(oid)) => Some(oid.clone()),
    };
    let mut args: Vec<String> = vec!["-C".into(), git_dir.into(), "update-ref".into()];
    match new {
        Value::Absent => args.extend(["-d".into(), name.into()]),
        Value::Oid(oid) => args.extend([name.into(), oid.clone()]),
    }
    args.extend(old);

    // A concurrent writer holding the ref's lock makes update-ref fail at once
    // ("Unable to create ... .lock: File exists"). That is not a lost race —
    // nothing has been compared yet — so wait it out briefly, as
    // `pin_result_in` does, and only then decide what the failure means.
    const ATTEMPTS: usize = 8;
    let mut stderr = String::new();
    for attempt in 0..ATTEMPTS {
        let output = Command::new("git")
            .args(&args)
            .output()
            .map_err(|e| HttpError::new(500, format!("running git update-ref: {e}")))?;
        if output.status.success() {
            notify();
            return Ok(());
        }
        stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !stderr.contains(".lock") {
            break;
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Refused. If the ref no longer holds what the caller expected, that is
    // the answer whatever git's wording; otherwise it is a real failure.
    if let Expected::Is(want) = expected {
        let current = read(git_dir, name)?;
        if current != *want {
            return Err(conflict(name, &current, want.text()));
        }
    }
    Err(HttpError::new(
        500,
        format!(
            "moving {name} to {}: git update-ref failed: {stderr}",
            new.text()
        ),
    ))
}

fn conflict(name: &str, current: &Value, expected: &str) -> HttpError {
    HttpError::new(
        409,
        format!("ref {name} is {}, not {expected}", current.text()),
    )
}

fn object_kind(git_dir: &str, oid: &str) -> Result<String, HttpError> {
    let output = Command::new("git")
        .args(["-C", git_dir, "cat-file", "-t", oid])
        .output()
        .map_err(|e| HttpError::new(500, format!("running git cat-file: {e}")))?;
    if !output.status.success() {
        return Err(HttpError::new(
            400,
            format!("{oid} is not in the store: put it before pointing a ref at it"),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Wake every waiter: something in the repository's refs may have changed.
pub(crate) fn notify() {
    let mut generation = GENERATION.lock().unwrap_or_else(|e| e.into_inner());
    *generation = generation.wrapping_add(1);
    CHANGED.notify_all();
}

/// Block until at least one `(name, expected)` entry in `targets` holds
/// something other than `expected`, and return every entry in `targets` that
/// differs; return an empty map once `timeout` has passed with all of them
/// unchanged.
///
/// The generation is taken BEFORE each read, so a write landing between the
/// read and the sleep moves it and the sleep returns at once: there is no
/// window in which a change can be missed until the next recheck.
pub(crate) fn wait_for_changes(
    git_dir: &str,
    targets: &[(String, Value)],
    timeout: Duration,
) -> Result<BTreeMap<String, Value>, HttpError> {
    if targets.is_empty() {
        return Ok(BTreeMap::new());
    }
    let names: Vec<&str> = targets.iter().map(|(name, _)| name.as_str()).collect();
    let deadline = Instant::now() + timeout;
    loop {
        let seen = *GENERATION.lock().unwrap_or_else(|e| e.into_inner());
        let current = read_many(git_dir, &names)?;
        let mut changed = BTreeMap::new();
        for (name, from) in targets {
            let now = current.get(name).cloned().unwrap_or(Value::Absent);
            if now != *from {
                changed.insert(name.clone(), now);
            }
        }
        if !changed.is_empty() {
            return Ok(changed);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(changed);
        }
        let guard = GENERATION.lock().unwrap_or_else(|e| e.into_inner());
        let _ = CHANGED
            .wait_timeout_while(guard, left.min(RECHECK), |generation| *generation == seen)
            .unwrap_or_else(|e| e.into_inner());
    }
}

/// Block until `name` holds something other than `from`, and return what it
/// holds; fail once `timeout` has passed with it unchanged (`caos ref-then`).
pub(crate) fn wait_for_change(
    git_dir: &str,
    name: &str,
    from: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let mut changed = wait_for_changes(git_dir, &[(name.to_string(), from.clone())], timeout)
        .map_err(|e| e.message().to_string())?;
    match changed.remove(name) {
        Some(now) => Ok(now),
        None => Err(format!(
            "ref {name} was still {} after {}s",
            from.text(),
            timeout.as_secs()
        )),
    }
}

fn check_timeout_secs(seconds: u64, where_from: &str) -> Result<Duration, HttpError> {
    if (1..=MAX_WAIT_SECS).contains(&seconds) {
        Ok(Duration::from_secs(seconds))
    } else {
        Err(HttpError::new(
            400,
            format!(
                "{where_from}: timeout must be whole seconds from 1 to {MAX_WAIT_SECS}, got {seconds}"
            ),
        ))
    }
}

/// `GET /ref?name=<ref>` — the ref's value, `<oid>` or `absent`, and a newline.
pub(crate) fn get_endpoint(config: &Config, query: &str) -> Result<Vec<u8>, HttpError> {
    let name = crate::compute::query_param(query, "name")
        .ok_or_else(|| HttpError::new(400, "GET /ref needs ?name=<ref>"))?;
    validate_name(&name)?;
    Ok(format!("{}\n", read(&config.git_dir, &name)?.text()).into_bytes())
}

/// `POST /ref` with `{"name": <ref>, "value": <oid|absent>, "expected":
/// <oid|absent>}` — a compare-and-swap. Leaving `expected` out makes the write
/// unconditional; the CLI asks for `--force` to say so.
pub(crate) fn set_endpoint(config: &Config, body: &str) -> Result<Vec<u8>, HttpError> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HttpError::new(400, format!("POST /ref wants a JSON object: {e}")))?;
    let field = |key: &str| json.get(key).and_then(|v| v.as_str());
    let name = field("name").ok_or_else(|| HttpError::new(400, "POST /ref needs \"name\""))?;
    validate_name(name)?;
    let value = field("value").ok_or_else(|| HttpError::new(400, "POST /ref needs \"value\""))?;
    let new = Value::parse(value).map_err(|e| HttpError::new(400, format!("value: {e}")))?;
    let expected = match json.get("expected") {
        None | Some(serde_json::Value::Null) => Expected::Any,
        Some(serde_json::Value::String(text)) => Expected::Is(
            Value::parse(text).map_err(|e| HttpError::new(400, format!("expected: {e}")))?,
        ),
        Some(other) => {
            return Err(HttpError::new(
                400,
                format!("expected must be a string, got {other}"),
            ))
        }
    };
    set(&config.git_dir, name, &new, &expected)?;
    Ok(format!("{}\n", new.text()).into_bytes())
}

/// `GET /ref/wait?name=<ref>[&expected=<oid|absent>][&timeout=<secs>]` — block
/// until `<ref>` holds something other than `expected` (default `absent`) and
/// answer `<oid>\n` or `absent\n`, or `unchanged\n` once `timeout` (default
/// 60s, at most a day) passes with no change.
///
/// Unknown or duplicate parameters are refused rather than ignored: a typo in
/// `expected` would otherwise wait against `absent` and return at once on every
/// call, turning a long-poll into a tight spin.
pub(crate) fn wait_get_endpoint(config: &Config, query: &str) -> Result<Vec<u8>, HttpError> {
    let mut name: Option<String> = None;
    let mut expected: Option<Value> = None;
    let mut timeout: Option<Duration> = None;

    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let Some((raw_k, raw_v)) = pair.split_once('=') else {
            return Err(HttpError::new(
                400,
                format!("GET /ref/wait: parameter {pair:?} is missing `=`"),
            ));
        };
        let key = crate::compute::percent_decode(raw_k);
        let val = crate::compute::percent_decode(raw_v);
        match key.as_str() {
            "name" => {
                if name.replace(val).is_some() {
                    return Err(HttpError::new(400, "GET /ref/wait: duplicate `name`"));
                }
            }
            "expected" => {
                let parsed = Value::parse(&val)
                    .map_err(|e| HttpError::new(400, format!("GET /ref/wait: expected: {e}")))?;
                if expected.replace(parsed).is_some() {
                    return Err(HttpError::new(400, "GET /ref/wait: duplicate `expected`"));
                }
            }
            "timeout" => {
                let secs = val.parse::<u64>().map_err(|_| {
                    HttpError::new(
                        400,
                        format!(
                            "GET /ref/wait: timeout must be whole seconds from 1 to {MAX_WAIT_SECS}, got {val:?}"
                        ),
                    )
                })?;
                let dur = check_timeout_secs(secs, "GET /ref/wait")?;
                if timeout.replace(dur).is_some() {
                    return Err(HttpError::new(400, "GET /ref/wait: duplicate `timeout`"));
                }
            }
            other => {
                return Err(HttpError::new(
                    400,
                    format!(
                        "GET /ref/wait: unknown parameter {other:?} (takes `name`, `expected`, `timeout`)"
                    ),
                ))
            }
        }
    }

    let name = name.ok_or_else(|| HttpError::new(400, "GET /ref/wait needs ?name=<ref>"))?;
    validate_name(&name)?;
    let from = expected.unwrap_or(Value::Absent);
    let dur = timeout.unwrap_or(Duration::from_secs(DEFAULT_HTTP_WAIT_SECS));

    let mut changed = wait_for_changes(&config.git_dir, &[(name.clone(), from)], dur)?;
    let answer = match changed.remove(&name) {
        Some(now) => now.text().to_string(),
        None => UNCHANGED.to_string(),
    };
    Ok(format!("{answer}\n").into_bytes())
}

/// `POST /ref/wait` with `{"refs": {"<ref>": "<oid|absent>", ...}, "timeout":
/// <secs>}` — block on one thread until at least one named ref differs from its
/// expected value, and answer `{"changed": {"<ref>": "<oid|absent>", ...}}\n`
/// (`{"changed": {}}\n` when `timeout` passes with none changed).
pub(crate) fn wait_post_endpoint(config: &Config, body: &str) -> Result<Vec<u8>, HttpError> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HttpError::new(400, format!("POST /ref/wait wants a JSON object: {e}")))?;
    let Some(top) = json.as_object() else {
        return Err(HttpError::new(400, "POST /ref/wait wants a JSON object"));
    };
    for key in top.keys() {
        if key != "refs" && key != "timeout" {
            return Err(HttpError::new(
                400,
                format!("POST /ref/wait: unknown field {key:?} (takes \"refs\" and \"timeout\")"),
            ));
        }
    }

    let Some(refs_val) = top.get("refs") else {
        return Err(HttpError::new(400, "POST /ref/wait needs \"refs\""));
    };
    let Some(refs_map) = refs_val.as_object() else {
        return Err(HttpError::new(
            400,
            "POST /ref/wait: \"refs\" must be a JSON object mapping ref names to expected values",
        ));
    };
    if refs_map.is_empty() {
        return Err(HttpError::new(
            400,
            "POST /ref/wait: \"refs\" must name at least one ref",
        ));
    }
    if refs_map.len() > MAX_WATCHED_REFS {
        return Err(HttpError::new(
            400,
            format!(
                "POST /ref/wait: \"refs\" names {} refs, at most {MAX_WATCHED_REFS} allowed",
                refs_map.len()
            ),
        ));
    }

    let mut targets = Vec::with_capacity(refs_map.len());
    for (name, exp_val) in refs_map {
        validate_name(name)?;
        let Some(exp_str) = exp_val.as_str() else {
            return Err(HttpError::new(
                400,
                format!(
                    "POST /ref/wait: expected value for {name:?} must be a string, got {exp_val}"
                ),
            ));
        };
        let from = Value::parse(exp_str)
            .map_err(|e| HttpError::new(400, format!("POST /ref/wait: {name}: {e}")))?;
        targets.push((name.clone(), from));
    }

    let dur = match top.get("timeout") {
        None | Some(serde_json::Value::Null) => Duration::from_secs(DEFAULT_HTTP_WAIT_SECS),
        Some(other) => {
            let Some(secs) = other.as_u64() else {
                return Err(HttpError::new(
                    400,
                    format!(
                        "POST /ref/wait: timeout must be whole seconds from 1 to {MAX_WAIT_SECS}, got {other}"
                    ),
                ));
            };
            check_timeout_secs(secs, "POST /ref/wait")?
        }
    };

    let changed = wait_for_changes(&config.git_dir, &targets, dur)?;
    let changed_json: serde_json::Map<String, serde_json::Value> = changed
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v.text().to_string())))
        .collect();
    let reply = serde_json::json!({ "changed": changed_json });
    Ok(format!("{reply}\n").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare repository holding one empty-tree commit per label.
    struct Repo {
        dir: std::path::PathBuf,
    }

    impl Repo {
        fn new(tag: &str) -> Repo {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("caos-refs-{tag}-{}-{nanos}", std::process::id()));
            let status = Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&dir)
                .status()
                .unwrap();
            assert!(status.success());
            Repo { dir }
        }

        fn path(&self) -> &str {
            self.dir.to_str().unwrap()
        }

        fn config(&self) -> Config {
            let repo = gix::open(self.path()).unwrap().into_sync();
            Config {
                registry_push_url: String::new(),
                registry_pull_host: String::new(),
                redis_addr: String::new(),
                cache_namespace: String::new(),
                git_dir: self.path().to_string(),
                secrets_git: String::new(),
                repo,
            }
        }

        fn git(&self, args: &[&str], stdin: &str) -> String {
            use std::io::Write;
            let mut child = Command::new("git")
                .args(["-C", self.path()])
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@caos")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@caos")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "git {args:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        fn commit(&self, label: &str) -> Value {
            let tree = self.git(&["mktree"], "");
            Value::Oid(self.git(&["commit-tree", &tree], label))
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const REF: &str = "refs/heads/tests/refs/x";

    #[test]
    fn a_missing_ref_reads_as_absent_and_a_ref_below_it_does_not_count() {
        let repo = Repo::new("absent");
        assert_eq!(read(repo.path(), REF).unwrap(), Value::Absent);
        let c = repo.commit("below");
        set(repo.path(), &format!("{REF}y"), &c, &Expected::Any).unwrap();
        set(
            repo.path(),
            "refs/heads/tests/refs/x2/y",
            &c,
            &Expected::Any,
        )
        .unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), Value::Absent);
        let many = read_many(repo.path(), &[REF, &format!("{REF}y")]).unwrap();
        assert_eq!(many.get(REF), Some(&Value::Absent));
        assert_eq!(many.get(&format!("{REF}y")), Some(&c));
    }

    #[test]
    fn create_only_and_compare_and_swap() {
        let repo = Repo::new("cas");
        let (a, b) = (repo.commit("a"), repo.commit("b"));
        let absent = Expected::Is(Value::Absent);

        set(repo.path(), REF, &a, &absent).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), a);

        // Create-only again: the ref exists now, and the 409 says what it holds.
        let lost = set(repo.path(), REF, &b, &absent).unwrap_err();
        assert_eq!(lost.status, 409);
        assert!(lost.message().contains(a.text()), "{}", lost.message());

        // A stale expectation is refused and moves nothing.
        let stale = set(repo.path(), REF, &b, &Expected::Is(b.clone())).unwrap_err();
        assert_eq!(stale.status, 409);
        assert_eq!(read(repo.path(), REF).unwrap(), a);

        set(repo.path(), REF, &b, &Expected::Is(a.clone())).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), b);

        // Delete, compare-and-swap style, then again: already gone is fine
        // unless a value was expected.
        set(repo.path(), REF, &Value::Absent, &Expected::Is(b.clone())).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), Value::Absent);
        set(repo.path(), REF, &Value::Absent, &absent).unwrap();
        assert_eq!(
            set(repo.path(), REF, &Value::Absent, &Expected::Is(b.clone()))
                .unwrap_err()
                .status,
            409
        );

        // --force overwrites whatever is there.
        set(repo.path(), REF, &a, &Expected::Any).unwrap();
        set(repo.path(), REF, &b, &Expected::Any).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), b);
    }

    #[test]
    fn a_ref_names_a_stored_commit() {
        let repo = Repo::new("kinds");
        let tree = repo.git(&["mktree"], "");
        let not_commit = set(repo.path(), REF, &Value::Oid(tree), &Expected::Any).unwrap_err();
        assert_eq!(not_commit.status, 400);
        assert!(
            not_commit.message().contains("is a tree"),
            "{}",
            not_commit.message()
        );

        let missing = Value::Oid("1".repeat(40));
        assert_eq!(
            set(repo.path(), REF, &missing, &Expected::Any)
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn only_refs_heads_is_offered() {
        for name in ["refs/keys/abc", "refs/caos/res/abc", "HEAD", "refs/heads/"] {
            assert_eq!(validate_name(name).unwrap_err().status, 400, "{name}");
        }
        assert_eq!(validate_name("refs/heads/a..b").unwrap_err().status, 400);
        validate_name("refs/heads/actors/counter").unwrap();
    }

    #[test]
    fn values_parse_strictly() {
        assert_eq!(Value::parse(" absent\n").unwrap(), Value::Absent);
        assert!(Value::parse(&"0".repeat(40)).is_err());
        assert!(Value::parse(&"A".repeat(40)).is_err());
        assert!(Value::parse("abc").is_err());
    }

    #[test]
    fn a_wait_returns_when_a_write_lands_and_times_out_when_none_does() {
        let repo = Repo::new("wait");
        let a = repo.commit("a");

        let timed_out =
            wait_for_change(repo.path(), REF, &Value::Absent, Duration::from_millis(200))
                .unwrap_err();
        assert!(timed_out.contains("was still absent"), "{timed_out}");

        let started = Instant::now();
        let path = repo.path().to_string();
        let writer = {
            let a = a.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                set(&path, REF, &a, &Expected::Is(Value::Absent)).unwrap();
            })
        };
        let seen =
            wait_for_change(repo.path(), REF, &Value::Absent, Duration::from_secs(30)).unwrap();
        writer.join().unwrap();
        assert_eq!(seen, a);
        // Woken by the write, not by the once-a-second recheck.
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );

        // Already different: no wait at all.
        assert_eq!(
            wait_for_change(repo.path(), REF, &Value::Absent, Duration::from_secs(30)).unwrap(),
            a
        );
    }

    #[test]
    fn http_get_ref_wait_wakes_on_change_reports_unchanged_and_rejects_bad_params() {
        let repo = Repo::new("http-get-wait");
        let cfg = repo.config();
        let a = repo.commit("a");

        // Timeout with nothing moving reports `unchanged\n` (200), not an error.
        let idle = wait_get_endpoint(&cfg, &format!("name={REF}&timeout=1")).unwrap();
        assert_eq!(idle, b"unchanged\n");

        // Concurrent write wakes the waiter immediately.
        let started = Instant::now();
        let path = repo.path().to_string();
        let writer = {
            let a = a.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                set(&path, REF, &a, &Expected::Is(Value::Absent)).unwrap();
            })
        };
        let woke =
            wait_get_endpoint(&cfg, &format!("name={REF}&expected=absent&timeout=30")).unwrap();
        writer.join().unwrap();
        assert_eq!(woke, format!("{}\n", a.text()).into_bytes());
        assert!(started.elapsed() < Duration::from_millis(900));

        // Already different from `expected`: returns at once.
        let immediate =
            wait_get_endpoint(&cfg, &format!("name={REF}&expected=absent&timeout=30")).unwrap();
        assert_eq!(immediate, format!("{}\n", a.text()).into_bytes());

        // Deletion while waiting from commit `a` reports `absent\n`.
        let path = repo.path().to_string();
        let deleter = {
            let a = a.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                set(&path, REF, &Value::Absent, &Expected::Is(a)).unwrap();
            })
        };
        let deleted = wait_get_endpoint(
            &cfg,
            &format!("name={REF}&expected={}&timeout=30", a.text()),
        )
        .unwrap();
        deleter.join().unwrap();
        assert_eq!(deleted, b"absent\n");

        // Caller mistakes are 400s, including typos that would otherwise spin.
        for bad in [
            "",
            &format!("name={REF}&expect=absent"),
            &format!("name={REF}&name={REF}"),
            &format!("name={REF}&expected=absent&expected=absent"),
            &format!("name={REF}&timeout=0"),
            &format!("name={REF}&timeout=999999"),
            &format!("name={REF}&bare"),
            "name=refs/keys/secret",
        ] {
            assert_eq!(
                wait_get_endpoint(&cfg, bad).unwrap_err().status,
                400,
                "query {bad:?}"
            );
        }
    }

    #[test]
    fn http_post_ref_wait_watches_multiple_refs_at_once() {
        let repo = Repo::new("http-post-wait");
        let cfg = repo.config();
        let (a, b) = (repo.commit("a"), repo.commit("b"));
        let r1 = "refs/heads/tests/refs/one";
        let r2 = "refs/heads/tests/refs/two";
        let unwatched = "refs/heads/tests/refs/other";

        set(repo.path(), r1, &a, &Expected::Is(Value::Absent)).unwrap();

        // Moving an unwatched ref wakes the condvar, and the waiter goes back to
        // sleep until a watched ref moves or timeout expires.
        let path = repo.path().to_string();
        let noise = {
            let b = b.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                set(&path, unwatched, &b, &Expected::Is(Value::Absent)).unwrap();
            })
        };
        let body = serde_json::json!({
            "refs": { r1: a.text(), r2: "absent" },
            "timeout": 1,
        })
        .to_string();
        let none_changed: serde_json::Value =
            serde_json::from_slice(&wait_post_endpoint(&cfg, &body).unwrap()).unwrap();
        noise.join().unwrap();
        assert_eq!(none_changed, serde_json::json!({ "changed": {} }));

        // Moving `r2` wakes the multi-ref wait and reports only the changed ref.
        let started = Instant::now();
        let path = repo.path().to_string();
        let writer = {
            let b = b.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                set(&path, r2, &b, &Expected::Is(Value::Absent)).unwrap();
            })
        };
        let body = serde_json::json!({
            "refs": { r1: a.text(), r2: "absent" },
            "timeout": 30,
        })
        .to_string();
        let woke: serde_json::Value =
            serde_json::from_slice(&wait_post_endpoint(&cfg, &body).unwrap()).unwrap();
        writer.join().unwrap();
        assert_eq!(woke, serde_json::json!({ "changed": { r2: b.text() } }));
        assert!(started.elapsed() < Duration::from_millis(900));

        // Validation: empty refs, unknown fields, non-string expectations, bad ref names.
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "refs": {} }),
            serde_json::json!({ "refs": { r1: "absent" }, "extra": 1 }),
            serde_json::json!({ "refs": { r1: 123 } }),
            serde_json::json!({ "refs": { "refs/keys/x": "absent" } }),
            serde_json::json!({ "refs": { r1: "absent" }, "timeout": 0 }),
        ] {
            assert_eq!(
                wait_post_endpoint(&cfg, &bad.to_string())
                    .unwrap_err()
                    .status,
                400,
                "body {bad}"
            );
        }
    }
}
