//! The eval cache (design/secrets.md, "The eval cache"): what the `.caos-expr`
//! at the root of a tree evaluated to, so a walk applies each expression once
//! and not once per walk.
//!
//! `caos-eval` asks its host for this at every `.caos-expr`
//! (`MemoKind::Node`), keyed on the tree alone, because an expression reads
//! only its own tree. What the tree cannot say is added here as a VARIANT:
//!
//! - the salt, which re-keys every job the expression runs;
//! - the secrets walk key, since a value that embeds a granted tool carries
//!   that tool's `secret-hash`;
//! - the build of caos, through the same namespace job results use, since a
//!   value is assembled from job results.
//!
//! An entry is a redis hash per tree with a field per variant, not a key per
//! `(tree, variant)`: the secrets approved set asks for EVERY value a tree has
//! had, whoever evaluated it, and a hash answers that in one read.
//!
//! Only a successful evaluation is stored. Expressions pin every locator by
//! commit (`locator::resolve_root` refuses anything else), so nothing a stored
//! value was computed from can move under it.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::Config;

/// `(kind, oid)`, as `caos-eval` passes a value.
pub(crate) type Value = (String, String);

/// Bounds the in-memory copy; forgetting only means a later redis read.
const MEMORY_CAP: usize = 200_000;

/// In front of redis, by `(redis key, variant)`. The key carries the
/// namespace, so two configs in one process (tests) cannot answer each other.
static MEMORY: Mutex<Option<HashMap<(String, String), Value>>> = Mutex::new(None);

fn key(config: &Config, tree: &str) -> String {
    if config.cache_namespace.is_empty() {
        format!("caos:eval:{tree}")
    } else {
        format!("caos:eval:{}:{tree}", config.cache_namespace)
    }
}

/// The hash field a walk reads and writes: its salt and, under secrets, its
/// walk key. A fixed-length digest, because a walk key holds tree oids, a
/// session id and commit shas, and is as long as its store has readers.
pub(crate) fn variant(salt: &str, walk_key: Option<&str>) -> String {
    let Some(walk_key) = walk_key else {
        if salt.is_empty() {
            return "plain".to_string();
        }
        return digest(&format!("salt\u{0}{salt}"));
    };
    digest(&format!("secrets\u{0}{salt}\u{0}{walk_key}"))
}

fn digest(text: &str) -> String {
    let bytes = text.as_bytes();
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .expect("hashing bytes")
        .to_string()
}

fn encode(value: &Value) -> String {
    format!("{} {}", value.0, value.1)
}

fn decode(text: &str) -> Option<Value> {
    let (kind, oid) = text.split_once(' ')?;
    Some((kind.to_string(), oid.to_string()))
}

fn remember(id: (String, String), value: Value) {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= MEMORY_CAP {
        map.clear();
    }
    map.insert(id, value);
}

/// What `tree`'s expression evaluated to under `variant`, if this server has
/// evaluated it. Redis is best-effort, as for job results: an unreachable one
/// means the expression is evaluated again.
pub(crate) fn get(config: &Config, tree: &str, variant: &str) -> Option<Value> {
    let id = (key(config, tree), variant.to_string());
    if let Some(hit) = MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(&id).cloned())
    {
        return Some(hit);
    }
    let stored = match crate::compute::hash_get(&config.redis_addr, &id.0, variant) {
        Ok(stored) => stored?,
        Err(e) => {
            eprintln!("eval cache: reading {tree}: {e}");
            return None;
        }
    };
    let value = decode(&stored)?;
    remember(id, value.clone());
    Some(value)
}

/// Record that `tree`'s expression evaluated to `value` under `variant`.
pub(crate) fn put(config: &Config, tree: &str, variant: &str, value: &Value) {
    let id = (key(config, tree), variant.to_string());
    if let Err(e) = crate::compute::hash_set(&config.redis_addr, &id.0, variant, &encode(value)) {
        eprintln!("eval cache: recording {tree}: {e}");
    }
    remember(id, value.clone());
}

/// Every value this server has recorded for `tree`, whatever the variant.
pub(crate) fn values(config: &Config, tree: &str) -> Vec<Value> {
    let mut read = values_of(config, &[tree.to_string()]);
    read.remove(tree).unwrap_or_default()
}

/// [`values`] for many trees in one round trip. Every tree asked for is in
/// the answer, with no values when it has none or redis could not be read:
/// the caller follows thousands of roots, and must not fall back to asking
/// for each.
pub(crate) fn values_of(config: &Config, trees: &[String]) -> HashMap<String, Vec<Value>> {
    if trees.is_empty() {
        return HashMap::new();
    }
    let keys: Vec<String> = trees.iter().map(|tree| key(config, tree)).collect();
    let read = crate::compute::hash_values_many(&config.redis_addr, &keys);
    let mut read = read.unwrap_or_else(|e| {
        eprintln!("eval cache: reading {} tree(s): {e}", trees.len());
        Vec::new()
    });
    read.resize(trees.len(), Vec::new());
    let mut values = HashMap::new();
    for (tree, texts) in trees.iter().zip(read) {
        let decoded: Vec<Value> = texts.iter().filter_map(|text| decode(text)).collect();
        values.insert(tree.clone(), decoded);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variant_tells_salts_and_walk_keys_apart() {
        assert_eq!(variant("", None), "plain");
        let salted = variant("s", None);
        let keyed = variant("", Some("trees|session|pins|"));
        let both = variant("s", Some("trees|session|pins|"));
        let all = ["plain", &salted, &keyed, &both];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // A salt cannot imitate a walk key, nor a walk key a salt.
        assert_ne!(variant("a\u{0}b", None), variant("a", Some("b")));
        assert_eq!(salted, variant("s", None));
    }

    #[test]
    fn a_value_survives_its_encoding() {
        let oid = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string();
        let value = ("tree".to_string(), oid);
        assert_eq!(decode(&encode(&value)), Some(value));
        assert_eq!(decode("malformed"), None);
    }
}
