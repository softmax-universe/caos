//! `.caos-expr` evaluation, from the client (design/caos-expr.md).
//!
//! A `.caos-expr` file makes the directory it sits in *evaluable*: instead of
//! being taken verbatim, the directory's contents are computed by running the
//! expression the file holds. The WALK lives in the shared `caos-eval` crate,
//! and the client never runs it: it pushes the tree and asks the server
//! (SPEC, "Submitting work"). A walk from here would cost a round trip per
//! node, and evaluation is where the server grants secrets.
//!
//! The grammar, the here-string form, the `$CAOS_EXPR` binding and the
//! worker-vs-data rule are documented on `caos_eval` itself.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{percent_encode, request_compute_url_with, Secrets, Transport};

/// The shared walk's kind/mode helper, re-exported so the rest of the client
/// keeps naming it through `eval::` — the same function the server uses.
pub(crate) use caos_eval::mode_of_kind;

/// A process-wide memo whose keys are CONTENT.
///
/// Every key stored through this is built from object hashes — a tree and a path
/// within it, a commit sha — so an entry cannot go stale: the same key names the
/// same bytes for the life of the world. That is the whole license for a global
/// here. A cache keyed on a NAME would need invalidation; this one has nothing
/// to invalidate, and a long-lived process (the TUI) accumulates one small entry
/// per distinct object it evaluated.
///
/// The lock is held across the map access and never across the work, so two
/// threads racing on a cold key both compute and both insert — the same answer,
/// because the key is the content. That costs a duplicated round trip once;
/// holding it across the evaluation would serialize every evaluation in the
/// process behind whichever one is dispatching a run.
pub(crate) struct Memo<V>(OnceLock<Mutex<HashMap<String, V>>>);

impl<V: Clone> Memo<V> {
    pub(crate) const fn new() -> Self {
        Self(OnceLock::new())
    }

    fn map(&self) -> &Mutex<HashMap<String, V>> {
        self.0.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn get(&self, key: &str) -> Option<V> {
        // A poisoned lock means some OTHER thread panicked; the map is still
        // consistent, because nothing fallible runs while it is held. Taking the
        // inner map is therefore recovery, not a swallowed error.
        self.map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    pub(crate) fn put(&self, key: String, value: V) {
        self.map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value);
    }
}

/// The object `path` evaluates to under `start_tree`, as `(kind, oid)`:
/// `start_tree` is pushed and the server walks it.
pub(crate) fn eval_path(
    t: &dyn Transport,
    start_tree: &str,
    path: &str,
    secrets: &Secrets,
) -> Result<(String, String), String> {
    t.ensure_pushed(start_tree)?;
    request_compute_url_with(
        &t.server_url()?,
        &format!("/eval?root:hash={start_tree}&path={}", percent_encode(path)),
        secrets,
        trace_headers(),
    )
}

/// The headers that say which trace node the server should record an
/// evaluation's dispatches under, so that what a job had to BUILD in order to
/// evaluate a tree (the test daemon evaluating `dev/run-tests` builds the whole
/// core) is in that job's trace, not an unlinked record no one reads.
///
/// A worker-side `caos` presents its job nonce, which its own server resolves.
/// A client that is not a worker has none; if `CAOS_TRACE_PARENT` names a trace
/// node (a worker got it from `caos job-req` and passed it on), it presents that
/// instead, which a server honors only when started with `CAOS_TRACE_PARENTS=yes`
/// (a dev stack: its evaluations belong to a job on another server). With neither
/// there is nothing to say, and nothing is sent.
pub(crate) fn trace_headers() -> Vec<(&'static str, String)> {
    if let Some(nonce) = crate::runner::job_nonce() {
        return vec![(caos_world::TRACE_NONCE_HEADER, nonce)];
    }
    match std::env::var("CAOS_TRACE_PARENT") {
        Ok(parent)
            if parent.len() == 40
                && parent
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')) =>
        {
            vec![(caos_world::TRACE_PARENT_HEADER, parent)]
        }
        _ => Vec::new(),
    }
}

/// `eval-path [--tree=<oid>] <path>` — evaluate the `.caos-expr` files from the
/// root of the tree down to `<path>` and print the resulting object's
/// `"<kind> <hash>"`. With no `--tree`, the tracked source tree is the start
/// (dirty edits included, like `run-tool`'s `--root:@=.`).
pub fn cli_eval_path(t: &dyn Transport, tree: Option<&str>, path: &str) -> Result<(), String> {
    let start = match tree {
        Some(oid) => {
            let (kind, _) = t.get_object(oid)?;
            if kind != "tree" {
                return Err(format!("--tree={oid} is a {kind}, not a tree"));
            }
            oid.to_string()
        }
        None => {
            let (_, oid) = t
                .ingest_path(".")?
                .ok_or_else(|| "this client cannot ingest the source tree".to_string())?;
            oid.to_string()
        }
    };
    let (kind, hash) = eval_path(t, &start, path, &Secrets::current())?;
    println!("{kind} {hash}");
    Ok(())
}

/// The walk run in-process over a test transport, which has no server.
#[cfg(test)]
pub(crate) fn eval_path_locally(
    t: &dyn Transport,
    start_tree: &str,
    path: &str,
) -> Result<(String, String), String> {
    use gix::objs::tree::Entry;

    struct Local<'a>(&'a dyn Transport);
    impl caos_eval::EvalHost for Local<'_> {
        fn get_object(&self, oid: &str) -> Result<(String, Vec<u8>), String> {
            self.0.get_object(oid)
        }
        fn post_object(&self, kind: &str, bytes: &[u8]) -> Result<gix::ObjectId, String> {
            super::post_object(self.0, kind, bytes)
        }
        fn fetch_tree_entries(&self, tree: &str) -> Result<Option<Vec<Entry>>, String> {
            super::fetch_tree_entries(self.0, tree)
        }
        fn post_tree(&self, entries: Vec<Entry>) -> Result<gix::ObjectId, String> {
            super::post_tree(self.0, entries)
        }
        fn dispatch(&self, image: &str, entries: Vec<Entry>) -> Result<(String, String), String> {
            let arg_tree = super::assemble_arg_tree(self.0, image, entries)?;
            super::request_compute(&self.0.server_url()?, &arg_tree, &Secrets::default())
        }
    }
    caos_eval::eval_path(&Local(t), start_tree, path)
}
