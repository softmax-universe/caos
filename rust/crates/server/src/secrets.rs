//! Secrets during a run (SPEC.md, "Secrets"; design/secrets.md): what a
//! request's SecretReaderKeys hold, which images evaluation granted them to,
//! and injection at dispatch.
//!
//! A grant is decided where evaluation applies a directory's `.caos-expr`: the
//! directory's tree is looked up in each reader's APPROVED SET
//! ([`crate::approved`]). A granted image is marked with `secret-hash` and
//! recorded; dispatch injects only into a job whose ArgTree is a superset of a
//! recorded image. The record is what proves the image came from this
//! server's evaluation: `secret-hash` alone is visible to anyone who has seen
//! a granted run.
//!
//! Records are kept in redis as well as here, so a restart does not forget
//! them and a cached walk needs nothing replayed. A record is a fact about a
//! scope that names every input to the grant, so no build's answer differs
//! and they are not namespaced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use caos_eval::EvalHost;
use caos_world::secrets::Reader;

use crate::approved::Set;
use crate::compute::{list_append_many, list_read};
use crate::secret_store::Stored;
use crate::{Config, HttpError};

/// The secrets a run carries, fixed at admission and passed to every sub-run.
#[derive(Clone, Default)]
pub(crate) struct Context {
    stored: Arc<Vec<Stored>>,
    /// Each `reader:@@=` with its approved set, for the commit range the
    /// locator names at admission: a `ref=` is resolved once, so one run sees
    /// one answer to where the branch is. A reader that could not be resolved
    /// is absent, which grants nothing. The set itself is shared with every
    /// other run and grows as the server evaluates (design/secrets.md, "The
    /// set is live").
    sets: Arc<Vec<(Reader, Arc<Set>)>>,
    /// Every input a grant record depends on, the stores' trees, the session
    /// and the pinned commits, so a push, a moved branch or another session
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
        let mut pins: HashMap<String, Option<String>> = HashMap::new();
        let mut sets: Vec<(Reader, Arc<Set>)> = Vec::new();
        let mut unfinished: Vec<String> = Vec::new();
        let mut head = None;
        for reader in stored.iter().flat_map(|s| &s.readers) {
            match reader {
                Reader::Locator { locator, since } => {
                    let pinned = pins.entry(locator.clone()).or_insert_with(|| {
                        let pinned = crate::grant_history::pin(config, locator);
                        pinned
                            .map_err(|e| eprintln!("secrets: {locator}: {e}; it grants nothing"))
                            .ok()
                    });
                    let Some(pinned) = pinned else {
                        continue;
                    };
                    if sets.iter().any(|(known, _)| known == reader) {
                        continue;
                    }
                    match approved_set(config, pinned, since.as_deref()) {
                        Ok((set, more)) => {
                            sets.push((reader.clone(), set));
                            unfinished.extend(more);
                        }
                        Err(e) => eprintln!(
                            "secrets: a reader could not be checked, so it grants nothing: {e}"
                        ),
                    }
                }
                Reader::Session { session: id, .. }
                    if Some(id) == session.as_ref() && head.is_none() =>
                {
                    head = session_head(config, id).unwrap_or_else(|e| {
                        eprintln!("secrets: session {id}: {e}");
                        None
                    });
                }
                _ => {}
            }
        }
        // A reader's path that stops at a directory under a commit already in
        // use is finished before the run starts, so its set holds the tool
        // (design/secrets.md, "Finishing a path").
        crate::compute::finish_paths(config, &unfinished);
        let mut pinned: Vec<&str> = pins.values().flatten().map(String::as_str).collect();
        pinned.sort();
        Ok(Context {
            scope: format!(
                "{}|{}|{}",
                trees.join(","),
                session.as_deref().unwrap_or(""),
                pinned.join(",")
            ),
            stored: Arc::new(stored),
            sets: Arc::new(sets),
            session,
            head,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.stored.is_empty()
    }

    /// What a walk's result depends on besides its tree and path: `None` with
    /// no secrets, since then it depends on nothing more.
    ///
    /// The approved sets are in it by fingerprint. A set grows as the server
    /// evaluates, and a walk cached while a tool's tree was not yet in the set
    /// must not go on answering once it is.
    pub(crate) fn walk_key(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut sets: Vec<String> = Vec::new();
        for (_, set) in self.sets.iter() {
            sets.push(set.fingerprint());
        }
        let head = self.head.as_deref().unwrap_or("");
        Some(format!("{}|{head}|{}", self.scope, sets.join(",")))
    }

    /// Whether `reader` approves the directory `node` (a tree oid): it is in
    /// the reader's approved set.
    fn approves(&self, config: &Config, reader: &Reader, node: &str) -> bool {
        match reader {
            Reader::Locator { .. } => {
                let mut sets = self.sets.iter();
                sets.any(|(known, set)| known == reader && set.contains(node))
            }
            Reader::Session { path, session } => {
                if self.session.as_deref() != Some(session.as_str()) {
                    return false;
                }
                let Some(head) = self.head.as_deref() else {
                    return false;
                };
                crate::approved::session_approves(config, head, path, node)
            }
        }
    }
}

/// A set, and the directories to evaluate so its paths can be finished.
type Approved = (Arc<Set>, Vec<String>);

/// The approved set of a `reader:@@=` whose locator is pinned to a commit.
fn approved_set(config: &Config, pinned: &str, since: Option<&str>) -> Result<Approved, String> {
    let git_ref = git_locator::parse_git_ref(pinned)?;
    let roots = crate::grant_history::allowed_roots(config, &git_ref, since)?;
    let dir = git_ref.dir.as_deref().unwrap_or("");
    Ok(crate::approved::locator_set(config, &roots, dir))
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

/// [`EvalHost::evaluated`] for the server: mark and record `value` when the
/// directory it was evaluated from, `node`, is in the approved set of a
/// reader of the run's secrets.
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
    let approved = |secret: &&Stored| {
        let mut readers = secret.readers.iter();
        readers.any(|reader| context.approves(config, reader, node))
    };
    let granted: Vec<&Stored> = context.stored.iter().filter(approved).collect();
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
