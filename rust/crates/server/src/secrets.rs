//! Secrets during a run (SPEC.md, "Secrets"; design/secrets.md): what a
//! request's SecretReaderKeys hold, which images evaluation granted them to,
//! and injection at dispatch.
//!
//! A grant is decided where evaluation applies a directory's `.caos-expr`. A
//! pushed secret carries the trees it is granted to, so the decision is a
//! lookup: is this directory, or the image it evaluated to, one of them. A
//! granted image is marked with `secret-hash` and recorded; dispatch injects
//! only into a job whose ArgTree is a superset of a recorded image. The record
//! is what proves the image came from this server's evaluation: `secret-hash`
//! alone is visible to anyone who has seen a granted run.
//!
//! Records are kept in redis as well as here, so a restart does not forget
//! them and a cached walk needs nothing replayed. A record is a fact about a
//! scope that names every input to the grant, so no build's answer differs
//! and they are not namespaced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use caos_eval::EvalHost;
use caos_world::secrets::Reader;

use crate::compute::{list_append_many, list_read};
use crate::secret_store::Stored;
use crate::{Config, HttpError};

/// The secrets a run carries, fixed at admission and passed to every sub-run.
#[derive(Clone, Default)]
pub(crate) struct Context {
    stored: Arc<Vec<Stored>>,
    /// Every input a grant depends on: the stores' trees, which hold the
    /// approved trees themselves, and the session. A push or another session
    /// sees none of this run's records.
    scope: String,
    session: Option<String>,
    /// The session's head tree, when a `reader:@=` grant names it.
    head: Option<String>,
}

impl Context {
    /// Resolve a request's SecretReaderKeys (space-separated) and session.
    pub(crate) fn admit(
        config: &Config,
        readers: &str,
        session: Option<&str>,
    ) -> Result<Context, HttpError> {
        let keys: Vec<String> = readers.split_whitespace().map(str::to_string).collect();
        if keys.is_empty() {
            return Ok(Context::default());
        }
        let (stored, trees) =
            crate::secret_store::load(config, &keys).map_err(|e| HttpError::new(400, e))?;
        let session = session.filter(|c| !c.is_empty()).map(str::to_string);
        let mut head = None;
        for reader in stored.iter().flat_map(|s| &s.readers) {
            let Reader::Session { session: id, .. } = reader else {
                continue;
            };
            if Some(id) == session.as_ref() && head.is_none() {
                head = session_head(config, id).unwrap_or_else(|e| {
                    eprintln!("secrets: session {id}: {e}");
                    None
                });
            }
        }
        let named = session.as_deref().unwrap_or("");
        let scope = format!("{}|{named}", trees.join(","));
        Ok(Context {
            scope,
            stored: Arc::new(stored),
            session,
            head,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.stored.is_empty()
    }

    /// What a walk's result depends on besides its tree and path: `None` with
    /// no secrets, since then it depends on nothing more.
    pub(crate) fn walk_key(&self) -> Option<String> {
        (!self.is_empty()).then(|| format!("{}|{}", self.scope, self.head.as_deref().unwrap_or("")))
    }

    /// Whether `secret` is granted to the directory `node`, which evaluated to
    /// `value` (both tree oids).
    ///
    /// A pushed tree may name either. `secrets-push` sends the directory: it
    /// depends on the commit alone, where the image a tool builds also depends
    /// on the build of caos that built it and on what its own dependencies
    /// were granted. An image written by hand is honored all the same.
    fn grants(&self, config: &Config, secret: &Stored, node: &str, value: &str) -> bool {
        let mut trees = secret.trees.iter();
        if trees.any(|tree| tree == node || tree == value) {
            return true;
        }
        let Some(head) = self.head.as_deref() else {
            return false;
        };
        secret.readers.iter().any(|reader| match reader {
            Reader::Session { path, session } => {
                self.session.as_deref() == Some(session.as_str())
                    && crate::approved::session_approves(config, head, path, node)
            }
            Reader::Locator { .. } => false,
        })
    }
}

// ---- records ----------------------------------------------------------------

/// Bounds the in-memory map; forgetting only means a later redis read.
const MEMORY_CAP: usize = 1_000_000;

/// A granted image's entries and the names granted, by scope. In front of a
/// redis list per scope; a scope present here holds everything redis does.
type Record = (BTreeMap<String, String>, Vec<String>);
static RECORDS: Mutex<Option<HashMap<String, Vec<Record>>>> = Mutex::new(None);

fn records_key(scope: &str) -> String {
    format!("caos:secrets:records:{}", blob_oid(scope.as_bytes()))
}

/// The records of `scope`, loading them from redis the first time.
fn records(config: &Config, scope: &str) -> Vec<Record> {
    if let Some(hit) = RECORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|r| r.get(scope).cloned())
    {
        return hit;
    }
    let loaded: Vec<Record> = list_read(&config.redis_addr, &records_key(scope))
        .unwrap_or_else(|e| {
            eprintln!("secrets: reading the records of a scope: {e}");
            Vec::new()
        })
        .iter()
        .filter_map(|text| serde_json::from_str(text).ok())
        .collect();
    let mut guard = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= MEMORY_CAP {
        map.clear();
    }
    map.entry(scope.to_string()).or_insert(loaded).clone()
}

fn record_grant(config: &Config, scope: &str, record: Record) {
    if records(config, scope).iter().any(|(e, _)| *e == record.0) {
        return;
    }
    let text = serde_json::to_string(&record).expect("a record serializes");
    RECORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .entry(scope.to_string())
        .or_default()
        .push(record);
    if let Err(e) = list_append_many(&config.redis_addr, &[(records_key(scope), text)]) {
        eprintln!("secrets: recording a grant: {e}");
    }
}

// ---- granting ---------------------------------------------------------------

/// [`EvalHost::evaluated`] for the server: mark and record `value` when it, or
/// the directory `node` it was evaluated from, is a tree one of the run's
/// secrets is granted to.
pub(crate) fn evaluated(
    config: &Config,
    host: &dyn EvalHost,
    context: &Context,
    node: &str,
    value: (String, String),
) -> Result<(String, String), String> {
    if context.is_empty() {
        return Ok(value);
    }
    let granted: Vec<&Stored> = context
        .stored
        .iter()
        .filter(|secret| context.grants(config, secret, node, &value.1))
        .collect();
    if granted.is_empty() {
        return Ok(value);
    }
    let mut names: Vec<String> = granted.iter().map(|s| s.name.clone()).collect();
    if value.0 != "tree" {
        eprintln!(
            "secrets: {names:?} match a node that evaluates to a {}, which cannot carry a grant",
            value.0
        );
        return Ok(value);
    }
    // A value built on a granted image already carries that image's mark —
    // one `secret-hash` per ArgTree, so it is replaced by the union.
    let mut image = value.1.clone();
    let existing =
        crate::compute::image_entries(config, &image).map_err(|e| e.message().to_string())?;
    if existing.contains_key(caos_world::SECRET_HASH_ARG) {
        names.extend(
            records(config, &context.scope)
                .into_iter()
                .filter(|(recorded, _)| *recorded == existing)
                .flat_map(|(_, names)| names),
        );
        names.sort();
        names.dedup();
        image = crate::compute::unbind(config, &image, caos_world::SECRET_HASH_ARG)
            .map_err(|e| e.message().to_string())?;
    }
    let pairs: Vec<(&str, &str)> = context
        .stored
        .iter()
        .filter(|s| names.contains(&s.name))
        .map(|s| (s.name.as_str(), s.entropy.as_str()))
        .collect();
    let digest = blob_oid(&caos_world::secret_hash_material(&pairs));
    let entry = gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: caos_world::SECRET_HASH_ARG.into(),
        oid: host.post_object("blob", digest.as_bytes())?,
    };
    let marked = caos_eval::curry(host, &image, vec![entry])?.to_string();
    let entries =
        crate::compute::image_entries(config, &marked).map_err(|e| e.message().to_string())?;
    record_grant(config, &context.scope, (entries, names.clone()));
    eprintln!("secrets: granted {names:?} to {marked}");
    Ok(("tree".to_string(), marked))
}

/// The secrets a job may read: those of every image evaluation granted in this
/// run's scope whose entries the job's ArgTree contains. Returns (name, value).
pub(crate) fn grant(
    config: &Config,
    context: &Context,
    arg_entries: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let names = granted_names(config, context, arg_entries);
    let out: Vec<(String, String)> = context
        .stored
        .iter()
        .filter(|s| names.contains(&s.name))
        .map(|s| (s.name.clone(), s.value.clone()))
        .collect();
    for (name, _) in &out {
        eprintln!("secret {name}: granted to this job");
    }
    out
}

/// The names [`grant`] would inject, sorted.
pub(crate) fn granted_names(
    config: &Config,
    context: &Context,
    arg_entries: &BTreeMap<String, String>,
) -> Vec<String> {
    if context.is_empty() {
        return Vec::new();
    }
    let mut names = HashSet::new();
    for (entries, granted) in records(config, &context.scope) {
        if entries.iter().all(|(k, v)| arg_entries.get(k) == Some(v)) {
            names.extend(granted);
        }
    }
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    names
}

/// The tree of the session's head commit, or `None` with no head yet.
fn session_head(config: &Config, session: &str) -> Result<Option<String>, String> {
    let repo = config.repo.to_thread_local();
    // Under its v4 name, or for a session recorded before the rename, v3.
    let names = session_protocol::v3::refs::head_refs(session)?;
    let Some((refname, mut reference)) = names.iter().find_map(|refname| {
        repo.find_reference(refname.as_str())
            .ok()
            .map(|reference| (refname, reference))
    }) else {
        return Ok(None);
    };
    let tree = reference
        .peel_to_commit()
        .map_err(|e| format!("{refname}: {e}"))?
        .tree_id()
        .map_err(|e| format!("{refname}: {e}"))?
        .to_string();
    Ok(Some(tree))
}

fn blob_oid(bytes: &[u8]) -> String {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .expect("hashing bytes")
        .to_string()
}
