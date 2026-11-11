//! Approved sets (design/secrets.md): the trees a reader approves.
//!
//! A reader is a set of root trees and a path, `(roots, dir)`. For each root
//! `T` it approves ONE tree, `eval(T, parent of dir)/name`: the directory a
//! walk from `T` has in hand when it is about to apply the tool's own
//! `.caos-expr`. A tool is granted when the directory being evaluated is one
//! of those trees, wherever the walk that reached it started.
//!
//! Nothing is evaluated here. A path is followed through values the server
//! has ALREADY computed ([`crate::eval_cache`]); a root whose path meets an
//! expression with no value yet is PARKED on that tree and followed again
//! when the eval cache records one ([`noted`]). So a set is never complete at
//! admission and is not a snapshot: it is shared by the process and read as
//! it stands at each check.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::eval_cache::Value;
use crate::Config;

/// One entry of a tree, as far as following a path needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Child {
    Tree(String),
    /// A gitlink, by the commit it names.
    Commit(String),
    Other,
}

/// What following a path reads: the object store and the eval cache. A trait
/// so the walk is tested without either.
pub(crate) trait Source {
    /// The entries of `tree` by name; `None` when the store holds no such tree.
    fn children(&self, tree: &str) -> Option<HashMap<String, Child>>;
    /// The tree of `commit`.
    fn commit_tree(&self, commit: &str) -> Option<String>;
    /// Every value the eval cache holds for `tree`, whatever the variant.
    fn values(&self, tree: &str) -> Vec<Value>;
}

/// What following a path from one root found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reach {
    /// The trees at the path. More than one when an expression on the way has
    /// more than one cached value (a salted run, another secret store).
    pub(crate) nodes: Vec<String>,
    /// Where it stopped: trees whose expression has no cached value, or that
    /// the store does not hold yet.
    pub(crate) parked: Vec<String>,
}

/// The path's components, as `caos_eval::eval_path` splits one.
pub(crate) fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty() && *c != ".")
}

/// Follow `dir` from `root` the way `caos_eval::eval_path` walks it, through
/// cached values only, and WITHOUT applying the last directory's expression:
/// the result is the directory a walk has in hand there.
///
/// A directory with a `.caos-expr` is entered through its values and never
/// raw. The raw directory is what the commit holds, not what a walk reaches,
/// and a tool evaluated without what its parents mount into it is a program
/// nobody approved.
pub(crate) fn reach(source: &dyn Source, root: &str, dir: &[&str]) -> Reach {
    let mut parked = Vec::new();
    let mut nodes = vec![root.to_string()];
    for (at, name) in dir.iter().enumerate() {
        let last = at + 1 == dir.len();
        let mut next: Vec<String> = Vec::new();
        for node in &nodes {
            for tree in entered(source, node, &mut parked) {
                let children = source.children(&tree).unwrap_or_default();
                let child = match children.get(*name) {
                    Some(Child::Tree(oid)) => Some(oid.clone()),
                    // A gitlink is entered only on the way: one that ends the
                    // path stays a commit, which evaluation does not apply.
                    Some(Child::Commit(commit)) if !last => source.commit_tree(commit),
                    _ => None,
                };
                match child {
                    Some(oid) if !next.contains(&oid) => next.push(oid),
                    _ => {}
                }
            }
        }
        nodes = next;
    }
    Reach { nodes, parked }
}

/// The trees a walk continues into from `node`: itself when it carries no
/// `.caos-expr`, else each value its expression has evaluated to here.
fn entered(source: &dyn Source, node: &str, parked: &mut Vec<String>) -> Vec<String> {
    let mut park = |parked: &mut Vec<String>| {
        if !parked.iter().any(|known| known == node) {
            parked.push(node.to_string());
        }
    };
    let Some(children) = source.children(node) else {
        park(parked);
        return Vec::new();
    };
    // As `caos_eval::eval_node`: a `.caos-expr` that is a directory is not a
    // directive.
    if matches!(children.get(".caos-expr"), None | Some(Child::Tree(_))) {
        return vec![node.to_string()];
    }
    let mut trees: Vec<String> = Vec::new();
    for (kind, oid) in source.values(node) {
        let tree = match kind.as_str() {
            "tree" => Some(oid),
            "commit" => source.commit_tree(&oid),
            _ => None,
        };
        match tree {
            Some(tree) if !trees.contains(&tree) => trees.push(tree),
            _ => {}
        }
    }
    if trees.is_empty() {
        park(parked);
    }
    trees
}

// ---- the process-wide state ---------------------------------------------------

/// A reader's approved set, shared by every run that carries the reader.
pub(crate) struct Set {
    roots: Arc<HashSet<String>>,
    dir: String,
    trees: Mutex<HashSet<String>>,
    /// XOR of the leading 64 bits of each tree's oid: changes exactly when
    /// the set gains a tree, and is the same on every server holding the
    /// same set.
    fingerprint: AtomicU64,
}

impl Set {
    pub(crate) fn contains(&self, tree: &str) -> bool {
        self.trees
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(tree)
    }

    /// What a cached walk is keyed on: a walk that found a tool ungranted must
    /// not answer once the set has grown to hold it.
    pub(crate) fn fingerprint(&self) -> String {
        format!("{:016x}", self.fingerprint.load(Ordering::Relaxed))
    }

    fn add(&self, trees: &[String]) {
        let mut held = self.trees.lock().unwrap_or_else(|e| e.into_inner());
        for tree in trees {
            if held.insert(tree.clone()) {
                self.fingerprint.fetch_xor(oid_bits(tree), Ordering::Relaxed);
            }
        }
    }
}

fn oid_bits(oid: &str) -> u64 {
    oid.get(..16)
        .and_then(|head| u64::from_str_radix(head, 16).ok())
        .unwrap_or(0)
}

/// A root and the path followed from it, normalized.
type Place = (String, String);

#[derive(Default)]
struct State {
    reached: HashMap<Place, Reach>,
    /// By the tree a place is parked on.
    parked: HashMap<String, HashSet<Place>>,
    /// By the address of the roots (`grant_history` hands out one `Arc` per
    /// commit range, and a set keeps its own alive) and the path.
    sets: HashMap<(usize, String), Arc<Set>>,
    refreshed: Option<Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Bounds what is remembered. A session reader adds places for every head of
/// its session; forgetting everything only means following paths again, and a
/// set a run already holds stays valid, if no longer added to.
const PLACES_CAP: usize = 200_000;

/// How often parked places are followed again unprompted. [`noted`] wakes a
/// place when THIS process records a value; a value another server process
/// wrote to the shared redis, or a tree that arrived by fetch, wakes nothing.
const REFRESH: Duration = Duration::from_secs(60);

/// The object store and the eval cache, with the values of some trees read
/// ahead in one round trip.
struct Store<'a> {
    config: &'a Config,
    repo: gix::Repository,
    ahead: HashMap<String, Vec<Value>>,
}

impl<'a> Store<'a> {
    fn ahead(config: &'a Config, trees: &[String]) -> Self {
        Store {
            config,
            repo: config.repo.to_thread_local(),
            ahead: crate::eval_cache::values_of(config, trees),
        }
    }
}

impl Source for Store<'_> {
    fn children(&self, tree: &str) -> Option<HashMap<String, Child>> {
        let oid = gix::ObjectId::from_hex(tree.as_bytes()).ok()?;
        let object = self.repo.find_object(oid).ok()?.try_into_tree().ok()?;
        let decoded = object.decode().ok()?;
        let children = decoded.entries.iter().map(|entry| {
            let child = if entry.mode.is_tree() {
                Child::Tree(entry.oid.to_string())
            } else if entry.mode.is_commit() {
                Child::Commit(entry.oid.to_string())
            } else {
                Child::Other
            };
            (entry.filename.to_string(), child)
        });
        Some(children.collect())
    }

    fn commit_tree(&self, commit: &str) -> Option<String> {
        let oid = gix::ObjectId::from_hex(commit.as_bytes()).ok()?;
        let object = self.repo.find_object(oid).ok()?.try_into_commit().ok()?;
        Some(object.tree_id().ok()?.to_string())
    }

    fn values(&self, tree: &str) -> Vec<Value> {
        match self.ahead.get(tree) {
            Some(values) => values.clone(),
            None => crate::eval_cache::values(self.config, tree),
        }
    }
}

fn forget_if_full(state: &mut State) {
    if state.reached.len() >= PLACES_CAP {
        *state = State::default();
    }
}

/// Follow `place` and remember what it reached: park it where it stopped, and
/// add what it found to every set it belongs to.
fn settle(state: &mut State, source: &dyn Source, place: &Place) {
    if let Some(old) = state.reached.remove(place) {
        for tree in &old.parked {
            let emptied = state.parked.get_mut(tree).is_some_and(|waiting| {
                waiting.remove(place);
                waiting.is_empty()
            });
            if emptied {
                state.parked.remove(tree);
            }
        }
    }
    let dir: Vec<&str> = components(&place.1).collect();
    let found = reach(source, &place.0, &dir);
    for tree in &found.parked {
        state
            .parked
            .entry(tree.clone())
            .or_default()
            .insert(place.clone());
    }
    for set in state.sets.values() {
        if set.dir == place.1 && set.roots.contains(&place.0) {
            set.add(&found.nodes);
        }
    }
    state.reached.insert(place.clone(), found);
}

/// Follow every parked place again, at most once per [`REFRESH`].
fn refresh(state: &mut State, config: &Config) {
    if state.refreshed.is_some_and(|at| at.elapsed() < REFRESH) {
        return;
    }
    state.refreshed = Some(Instant::now());
    let places: HashSet<Place> = state.parked.values().flatten().cloned().collect();
    if places.is_empty() {
        return;
    }
    let roots: Vec<String> = places.iter().map(|(root, _)| root.clone()).collect();
    let source = Store::ahead(config, &roots);
    for place in &places {
        settle(state, &source, place);
    }
}

/// The approved set of a `reader:@@=`: `dir` under each of `roots`.
///
/// The first call for a commit range follows the path from every root in it,
/// which is a tree lookup per root and one redis round trip for them all; a
/// range that grew by a commit follows only the new one.
pub(crate) fn locator_set(config: &Config, roots: &Arc<HashSet<String>>, dir: &str) -> Arc<Set> {
    let dir = components(dir).collect::<Vec<_>>().join("/");
    let id = (Arc::as_ptr(roots) as usize, dir.clone());
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(State::default);
    forget_if_full(state);
    if let Some(set) = state.sets.get(&id).cloned() {
        refresh(state, config);
        return set;
    }
    state.refreshed.get_or_insert_with(Instant::now);
    let places: Vec<Place> = roots.iter().map(|root| (root.clone(), dir.clone())).collect();
    let unknown: Vec<String> = places
        .iter()
        .filter(|place| !state.reached.contains_key(*place))
        .map(|(root, _)| root.clone())
        .collect();
    let source = Store::ahead(config, &unknown);
    let set = Arc::new(Set {
        roots: roots.clone(),
        dir,
        trees: Mutex::new(HashSet::new()),
        fingerprint: AtomicU64::new(0),
    });
    for place in &places {
        if !state.reached.contains_key(place) {
            settle(state, &source, place);
        }
        set.add(&state.reached[place].nodes);
    }
    state.sets.insert(id, set.clone());
    set
}

/// Whether a `reader:@=` naming `path` in the session whose head tree is
/// `head` approves `node`.
///
/// A walk may start at the head, or at any tree along `path`: a tool is run
/// with its source tree as the root as often as with the session as the root.
/// So every split of `path` into a prefix descended raw from the head (a
/// source tree's gitlink entered) and a remainder is a place, with the tree
/// at the prefix as its root.
pub(crate) fn session_approves(config: &Config, head: &str, path: &str, node: &str) -> bool {
    let source = Store::ahead(config, &[]);
    let parts: Vec<&str> = components(path).collect();
    let mut places: Vec<Place> = Vec::new();
    let mut tree = head.to_string();
    for at in 0..=parts.len() {
        places.push((tree.clone(), parts[at..].join("/")));
        let Some(name) = parts.get(at) else {
            break;
        };
        let children = source.children(&tree).unwrap_or_default();
        let below = match children.get(*name) {
            Some(Child::Tree(oid)) => Some(oid.clone()),
            Some(Child::Commit(commit)) => source.commit_tree(commit),
            _ => None,
        };
        let Some(below) = below else {
            break;
        };
        tree = below;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(State::default);
    forget_if_full(state);
    for place in &places {
        if !state.reached.contains_key(place) {
            settle(state, &source, place);
        }
        if state.reached[place].nodes.iter().any(|at| at == node) {
            return true;
        }
    }
    false
}

/// The eval cache recorded a value for `tree`: follow again every place that
/// was parked on it. Called for every evaluated expression, so the common
/// case, nothing parked there, is one lookup.
pub(crate) fn noted(config: &Config, tree: &str) {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return;
    };
    let Some(waiting) = state.parked.get(tree) else {
        return;
    };
    let places: Vec<Place> = waiting.iter().cloned().collect();
    let source = Store::ahead(config, &[]);
    for place in &places {
        settle(state, &source, place);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Trees by name, commits by name, and the eval cache, all in memory.
    #[derive(Default)]
    struct Memory {
        trees: HashMap<String, HashMap<String, Child>>,
        commits: HashMap<String, String>,
        values: RefCell<HashMap<String, Vec<Value>>>,
    }

    impl Memory {
        fn tree(&mut self, name: &str, children: &[(&str, Child)]) {
            let children = children
                .iter()
                .map(|(child, kind)| (child.to_string(), kind.clone()))
                .collect();
            self.trees.insert(name.to_string(), children);
        }
        fn evaluates(&self, tree: &str, to: &str) {
            self.values
                .borrow_mut()
                .entry(tree.to_string())
                .or_default()
                .push(("tree".to_string(), to.to_string()));
        }
    }

    impl Source for Memory {
        fn children(&self, tree: &str) -> Option<HashMap<String, Child>> {
            self.trees.get(tree).cloned()
        }
        fn commit_tree(&self, commit: &str) -> Option<String> {
            self.commits.get(commit).cloned()
        }
        fn values(&self, tree: &str) -> Vec<Value> {
            self.values.borrow().get(tree).cloned().unwrap_or_default()
        }
    }

    fn tree(name: &str) -> Child {
        Child::Tree(name.to_string())
    }

    /// caos in miniature: a root expression that reshapes every tool, and one
    /// in `std`.
    fn repo() -> Memory {
        let mut memory = Memory::default();
        memory.tree("root", &[(".caos-expr", Child::Other), ("std", tree("std-raw"))]);
        memory.tree("std-raw", &[("tool", tree("tool-raw"))]);
        memory.tree("tool-raw", &[(".caos-expr", Child::Other)]);
        memory.tree("root-value", &[("std", tree("std-deep"))]);
        memory.tree(
            "std-deep",
            &[(".caos-expr", Child::Other), ("tool", tree("tool-half"))],
        );
        memory.tree("std-value", &[("tool", tree("tool-deep"))]);
        memory.tree("tool-deep", &[(".caos-expr", Child::Other)]);
        memory
    }

    #[test]
    fn an_unevaluated_root_is_parked_and_approves_nothing() {
        let memory = repo();
        let found = reach(&memory, "root", &["std", "tool"]);
        assert_eq!(found.nodes, Vec::<String>::new());
        assert_eq!(found.parked, vec!["root".to_string()]);
    }

    #[test]
    fn the_approved_tree_is_the_one_every_parent_has_reshaped() {
        let memory = repo();
        memory.evaluates("root", "root-value");
        // The root is evaluated and `std` is not: parked one level down, and
        // neither the committed tool nor the half-evaluated one is approved.
        let found = reach(&memory, "root", &["std", "tool"]);
        assert_eq!(found.nodes, Vec::<String>::new());
        assert_eq!(found.parked, vec!["std-deep".to_string()]);

        memory.evaluates("std-deep", "std-value");
        let found = reach(&memory, "root", &["std", "tool"]);
        assert_eq!(found.nodes, vec!["tool-deep".to_string()]);
        assert_eq!(found.parked, Vec::<String>::new());
        // The tool's own expression is not applied: a mount of `std` is
        // approved as the evaluated `std`, not as what a tool in it builds.
        let found = reach(&memory, "root", &["std"]);
        assert_eq!(found.nodes, vec!["std-deep".to_string()]);
    }

    #[test]
    fn every_cached_value_of_a_parent_is_followed() {
        let mut memory = repo();
        memory.tree("root-salted", &[("std", tree("std-other"))]);
        memory.tree("std-other", &[("tool", tree("tool-other"))]);
        memory.evaluates("root", "root-value");
        memory.evaluates("root", "root-salted");
        memory.evaluates("std-deep", "std-value");
        let mut found = reach(&memory, "root", &["std", "tool"]).nodes;
        found.sort();
        assert_eq!(found, vec!["tool-deep".to_string(), "tool-other".to_string()]);
    }

    #[test]
    fn without_expressions_the_committed_directory_is_approved() {
        let mut memory = Memory::default();
        memory.tree("root", &[("a", tree("a"))]);
        memory.tree("a", &[("tool", tree("tool"))]);
        let found = reach(&memory, "root", &["a", "tool"]);
        assert_eq!(found.nodes, vec!["tool".to_string()]);
        assert_eq!(reach(&memory, "root", &[]).nodes, vec!["root".to_string()]);
        assert_eq!(reach(&memory, "root", &["missing"]).nodes, Vec::<String>::new());
    }

    #[test]
    fn a_gitlink_is_entered_on_the_way_and_not_at_the_end() {
        let mut memory = Memory::default();
        memory.tree("root", &[("src", Child::Commit("commit".to_string()))]);
        memory.commits.insert("commit".to_string(), "src".to_string());
        memory.tree("src", &[("tool", tree("tool"))]);
        assert_eq!(reach(&memory, "root", &["src", "tool"]).nodes, vec!["tool".to_string()]);
        assert_eq!(reach(&memory, "root", &["src"]).nodes, Vec::<String>::new());
    }

    #[test]
    fn a_root_the_store_does_not_hold_is_parked_on_itself() {
        let memory = Memory::default();
        let found = reach(&memory, "absent", &["std", "tool"]);
        assert_eq!(found.nodes, Vec::<String>::new());
        assert_eq!(found.parked, vec!["absent".to_string()]);
    }

    #[test]
    fn a_set_fingerprint_moves_only_when_the_set_grows() {
        let set = Set {
            roots: Arc::new(HashSet::new()),
            dir: String::new(),
            trees: Mutex::new(HashSet::new()),
            fingerprint: AtomicU64::new(0),
        };
        let empty = set.fingerprint();
        set.add(&["00000000000000010000000000000000deadbeef".to_string()]);
        let one = set.fingerprint();
        assert_ne!(empty, one);
        set.add(&["00000000000000010000000000000000deadbeef".to_string()]);
        assert_eq!(set.fingerprint(), one);
        assert!(set.contains("00000000000000010000000000000000deadbeef"));
        assert!(!set.contains("absent"));
    }
}
