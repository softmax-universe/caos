//! What a `reader:@=<path> session=<id>` approves (design/secrets.md).
//!
//! A `reader:@@=` needs nothing from here: `secrets-push` evaluates the tools
//! it names and pushes their trees, and the check is a lookup in that list. A
//! session reader cannot be pushed that way. It names a path in a session's
//! head, which moves with every edit, so what it approves is worked out when
//! it is asked: the directory a walk from the head has in hand at `path` when
//! it is about to apply the tool's own `.caos-expr`.
//!
//! Nothing is evaluated to find that directory. The path is followed through
//! values the eval cache already holds ([`crate::eval_cache`]), which the
//! walk now asking has just put there: it evaluated each parent on its way to
//! the tool.

use std::collections::HashMap;

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

/// The path's components, as `caos_eval::eval_path` splits one.
pub(crate) fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty() && *c != ".")
}

fn push_new(list: &mut Vec<String>, item: &str) {
    if !list.iter().any(|known| known == item) {
        list.push(item.to_string());
    }
}

/// Follow `dir` from `root` the way `caos_eval::eval_path` walks it, through
/// cached values only, and WITHOUT applying the last directory's expression:
/// the result is the directory a walk has in hand there. More than one when
/// an expression on the way has more than one cached value (a salted run,
/// another secret store); none when one has no value yet.
///
/// A directory with a `.caos-expr` is entered through its values and never
/// raw. The raw directory is what the commit holds, not what a walk reaches,
/// and a tool evaluated without what its parents mount into it is a program
/// nobody approved.
pub(crate) fn reach(source: &dyn Source, root: &str, dir: &[&str]) -> Vec<String> {
    let mut nodes = vec![root.to_string()];
    for (at, name) in dir.iter().enumerate() {
        let last = at + 1 == dir.len();
        let mut next: Vec<String> = Vec::new();
        for node in &nodes {
            for tree in entered(source, node) {
                let children = source.children(&tree).unwrap_or_default();
                let child = match children.get(*name) {
                    Some(Child::Tree(oid)) => Some(oid.clone()),
                    // A gitlink is entered only on the way: one that ends the
                    // path stays a commit, which evaluation does not apply.
                    Some(Child::Commit(commit)) if !last => source.commit_tree(commit),
                    _ => None,
                };
                if let Some(oid) = child {
                    push_new(&mut next, &oid);
                }
            }
        }
        nodes = next;
    }
    nodes
}

/// The trees a walk continues into from `node`: itself when it carries no
/// `.caos-expr`, else each value its expression has evaluated to here.
fn entered(source: &dyn Source, node: &str) -> Vec<String> {
    let Some(children) = source.children(node) else {
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
        if let Some(tree) = tree {
            push_new(&mut trees, &tree);
        }
    }
    trees
}

/// The server's object store and eval cache.
struct Store<'a> {
    config: &'a Config,
    repo: gix::Repository,
}

impl Source for Store<'_> {
    fn children(&self, tree: &str) -> Option<HashMap<String, Child>> {
        let oid = gix::ObjectId::from_hex(tree.as_bytes()).ok()?;
        let object = self.repo.find_object(oid).ok()?.try_into_tree().ok()?;
        let decoded = object.decode().ok()?;
        let mut children = HashMap::new();
        for entry in &decoded.entries {
            let child = if entry.mode.is_tree() {
                Child::Tree(entry.oid.to_string())
            } else if entry.mode.is_commit() {
                Child::Commit(entry.oid.to_string())
            } else {
                Child::Other
            };
            children.insert(entry.filename.to_string(), child);
        }
        Some(children)
    }

    fn commit_tree(&self, commit: &str) -> Option<String> {
        let oid = gix::ObjectId::from_hex(commit.as_bytes()).ok()?;
        let object = self.repo.find_object(oid).ok()?.try_into_commit().ok()?;
        let tree = object.tree_id().ok()?;
        Some(tree.to_string())
    }

    fn values(&self, tree: &str) -> Vec<Value> {
        crate::eval_cache::values(self.config, tree)
    }
}

/// Whether a `reader:@=` naming `path` in the session whose head tree is
/// `head` approves `node`.
///
/// A walk may start at the head, or at any tree along `path`: a tool is run
/// with its source tree as the root as often as with the session as the root.
/// So every split of `path` into a prefix descended raw from the head (a
/// source tree's gitlink entered) and a remainder is tried, following the
/// remainder from the tree at the prefix.
pub(crate) fn session_approves(config: &Config, head: &str, path: &str, node: &str) -> bool {
    let source = Store {
        config,
        repo: config.repo.to_thread_local(),
    };
    let parts: Vec<&str> = components(path).collect();
    let mut tree = head.to_string();
    for at in 0..=parts.len() {
        let reached = reach(&source, &tree, &parts[at..]);
        if reached.iter().any(|found| found == node) {
            return true;
        }
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
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Trees, commits and the eval cache, in memory and by made-up names.
    #[derive(Default)]
    struct Memory {
        trees: HashMap<String, HashMap<String, Child>>,
        commits: HashMap<String, String>,
        values: RefCell<HashMap<String, Vec<Value>>>,
    }

    impl Memory {
        fn tree(&mut self, name: &str, children: &[(&str, Child)]) {
            let mut entries = HashMap::new();
            for (child, kind) in children {
                entries.insert(child.to_string(), kind.clone());
            }
            self.trees.insert(name.to_string(), entries);
        }
        fn evaluates(&self, tree: &str, to: &str) {
            let mut values = self.values.borrow_mut();
            let known = values.entry(tree.to_string()).or_default();
            known.push(("tree".to_string(), to.to_string()));
        }
        fn at(&self, root: &str, dir: &[&str]) -> Vec<String> {
            let mut nodes = reach(self, root, dir);
            nodes.sort();
            nodes
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
            let values = self.values.borrow();
            values.get(tree).cloned().unwrap_or_default()
        }
    }

    fn expr() -> (&'static str, Child) {
        (".caos-expr", Child::Other)
    }

    fn sub(name: &'static str, tree: &str) -> (&'static str, Child) {
        (name, Child::Tree(tree.to_string()))
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    /// caos in miniature: a root expression that reshapes every tool, and one
    /// in `std`.
    fn repo() -> Memory {
        let mut memory = Memory::default();
        memory.tree("root", &[expr(), sub("std", "std-raw")]);
        memory.tree("std-raw", &[sub("tool", "tool-raw")]);
        memory.tree("tool-raw", &[expr()]);
        memory.tree("root-value", &[sub("std", "std-deep")]);
        memory.tree("std-deep", &[expr(), sub("tool", "tool-half")]);
        memory.tree("std-value", &[sub("tool", "tool-deep")]);
        memory.tree("tool-deep", &[expr()]);
        memory
    }

    #[test]
    fn an_unevaluated_root_approves_nothing() {
        let memory = repo();
        assert_eq!(memory.at("root", &["std", "tool"]), names(&[]));
    }

    #[test]
    fn the_approved_tree_is_the_one_every_parent_has_reshaped() {
        let memory = repo();
        memory.evaluates("root", "root-value");
        // The root is evaluated and `std` is not: neither the committed tool
        // nor the half-evaluated one is approved.
        assert_eq!(memory.at("root", &["std", "tool"]), names(&[]));

        memory.evaluates("std-deep", "std-value");
        let found = memory.at("root", &["std", "tool"]);
        assert_eq!(found, names(&["tool-deep"]));
        // The last directory's own expression is not applied: `std` is
        // approved as the directory a walk holds, not as what it builds.
        assert_eq!(memory.at("root", &["std"]), names(&["std-deep"]));
    }

    #[test]
    fn every_cached_value_of_a_parent_is_followed() {
        let mut memory = repo();
        memory.tree("root-salted", &[sub("std", "std-other")]);
        memory.tree("std-other", &[sub("tool", "tool-other")]);
        memory.evaluates("root", "root-value");
        memory.evaluates("root", "root-salted");
        memory.evaluates("std-deep", "std-value");
        let found = memory.at("root", &["std", "tool"]);
        assert_eq!(found, names(&["tool-deep", "tool-other"]));
    }

    #[test]
    fn without_expressions_the_committed_directory_is_approved() {
        let mut memory = Memory::default();
        memory.tree("root", &[sub("a", "a")]);
        memory.tree("a", &[sub("tool", "tool")]);
        assert_eq!(memory.at("root", &["a", "tool"]), names(&["tool"]));
        assert_eq!(memory.at("root", &[]), names(&["root"]));
        assert_eq!(memory.at("root", &["missing"]), names(&[]));
    }

    #[test]
    fn a_gitlink_is_entered_on_the_way_and_not_at_the_end() {
        let mut memory = Memory::default();
        let link = Child::Commit("commit".to_string());
        memory.tree("root", &[("src", link)]);
        memory.tree("src", &[sub("tool", "tool")]);
        let (commit, tree) = ("commit".to_string(), "src".to_string());
        memory.commits.insert(commit, tree);
        assert_eq!(memory.at("root", &["src", "tool"]), names(&["tool"]));
        assert_eq!(memory.at("root", &["src"]), names(&[]));
    }
}
