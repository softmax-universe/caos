//! caos-worker-deep-deps: restructure a tree so that each directory's declared
//! dependencies live, recursively deepened, inside it.
//!
//! Any directory may carry a `DEPS` file listing one dependency per line as
//!
//! ```text
//! <path> <name>
//! ```
//!
//! where `<path>` is relative to the DEPS file's OWN directory (so `../foo`
//! reaches out of the subtree) and `<name>` is what the dependency is mounted
//! under. The output mirrors the input tree, but every directory is "deepened":
//! its `DEPS` is replaced by a `DEEP-DEPS/` subtree whose children are the named
//! dependencies, each ITSELF deepened.
//!
//! A dependency may be a FILE as well as a directory. There is nothing under a
//! file to walk and no `DEPS` it could carry, so it is mounted as-is — which is
//! what lets a package name a workspace manifest or a shared lock beside the
//! directories it needs, instead of forcing every such thing into a directory
//! of its own.
//!
//! ONE RUN, one pass — do not split it back into per-directory jobs. Recursing
//! through `map_then` (a job to enumerate each directory, another to rebuild it)
//! costs ~2N containers for N directories, and buys nothing: the enumerating
//! job has to carry the WHOLE TREE to resolve relative deps, so it is keyed on
//! the whole tree and re-runs for every directory on any edit — paying N
//! container spawns to reach a cache that same edit just invalidated.
//!
//! Deepening is tree rewriting: no compiling, no network, nothing but reading a
//! materialized tree and staging symlinks. If this ever gets slow on a very
//! large tree, make the ONE pass incremental.
//!
//! Sharing is by absolute path: a dependency reached from two places is deepened
//! once and staged twice (as symlinks, so no bytes move), and `caos put` gives
//! the identical subtree one hash. That is what makes bootstrap's hand-deepen
//! (`build-builtins.sh`) reproducible — it must match this byte for byte, or the
//! seeded keys stop matching the keys resolution forms.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use worker_common::{arg, caos, entries, file_name, link, path, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("deep-deps", run)
}

/// The file at the tree's root listing the paths in it that were MOUNTED —
/// grafted in as finished values by the evaluator's `graft` or by
/// `std/flake-input-loader` (`caos_eval::MOUNTS_FILE`, same name and format:
/// one path per line, relative to the root).
const MOUNTS_FILE: &str = ".caos-mounts";

fn run() -> Result<(), String> {
    let input = arg("in");
    let mounts = fetch(&input)?;

    let staged = scratch("out")?;
    let mut memo: HashMap<String, PathBuf> = HashMap::new();
    let mut pass = Pass {
        base: &input,
        mounts: &mounts,
        memo: &mut memo,
        visiting: Vec::new(),
    };
    pass.deepen("", &staged)?;
    caos(["put", path(&staged), "/cas/out"])
}

/// Materialize what this pass reads, and return the tree's mounts.
///
/// A MOUNT IS A FINISHED TREE (design/nix-bootstrap.md): it was evaluated —
/// deepened — where it came from, so every `DEEP-DEPS/` in it is already filled
/// and its `DEPS` name paths that only mean something THERE (std's
/// `../../rust/crates/…`, from inside a consumer's `caos-std/`). So it is never
/// walked, never deepened again, and never fetched: it is carried across by
/// reference. That also keeps this pass from materializing a mounted core's
/// image deltas, which are hundreds of MB and have no `DEPS` to find.
///
/// Without a `.caos-mounts` the whole tree is fetched in one recursive round
/// trip, as before.
fn fetch(input: &str) -> Result<Vec<String>, String> {
    caos(["get", input])?;
    let list = Path::new(input).join(MOUNTS_FILE);
    if !list.exists() {
        caos(["get", "-r", input])?;
        return Ok(Vec::new());
    }
    caos(["get", path(&list)])?;
    let text = fs::read_to_string(&list).map_err(|e| format!("reading {MOUNTS_FILE}: {e}"))?;
    let mut mounts = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mount = normalize(line)?;
        if mount.is_empty() {
            return Err(format!("{MOUNTS_FILE} lists the root itself"));
        }
        mounts.push(mount);
    }
    fetch_except(input, "", &mounts)?;
    Ok(mounts)
}

/// Fetch the directory at `rel` and everything below it EXCEPT the mounts: in
/// one recursive round trip where no mount lies below, else level by level
/// down to the mounts' parents.
fn fetch_except(base: &str, rel: &str, mounts: &[String]) -> Result<(), String> {
    let dir = full(base, rel);
    if !mounts.iter().any(|m| strictly_under(m, rel)) {
        return caos(["get", "-r", &dir]);
    }
    caos(["get", &dir])?;
    for entry in entries(&dir)? {
        let child = join(rel, &file_name(&entry));
        if mounts.contains(&child) {
            continue;
        }
        let meta =
            fs::symlink_metadata(&entry).map_err(|e| format!("stat {}: {e}", entry.display()))?;
        if meta.is_dir() {
            fetch_except(base, &child, mounts)?;
        } else if file_name(&entry) == "DEPS" {
            caos(["get", path(&entry)])?; // the one file this pass reads
        }
    }
    Ok(())
}

/// The mount `p` is at or below, if any.
fn mount_of<'m>(mounts: &'m [String], p: &str) -> Option<&'m String> {
    mounts.iter().find(|m| p == m.as_str() || strictly_under(p, m))
}

/// Whether `p` lies strictly below the directory `dir` (`""` is the root).
fn strictly_under(p: &str, dir: &str) -> bool {
    if dir.is_empty() {
        !p.is_empty()
    } else {
        p.len() > dir.len() && p.starts_with(dir) && p.as_bytes()[dir.len()] == b'/'
    }
}

fn full(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Deepen the directory at `rel` within the materialized tree at `base`, staging
/// the result at `into`: its own files (minus `DEPS`), its sub-directories
/// deepened in place, and its `DEPS` targets deepened under `DEEP-DEPS/`.
///
/// `memo` maps an absolute tree path to a staged directory already deepened, so
/// a dependency reached twice is computed once. `visiting` is the current
/// dependency chain, and it is what catches a cycle — one pass makes no server
/// round trip, so there is no run-cycle detection to fall back on. `mounts`
/// are carried across by reference and never walked ([`fetch`]).
struct Pass<'a> {
    base: &'a str,
    mounts: &'a [String],
    memo: &'a mut HashMap<String, PathBuf>,
    visiting: Vec<String>,
}

fn deepen(pass: &mut Pass, rel: &str, into: &Path) -> Result<(), String> {
    let base = pass.base;
    let dir = full(base, rel);

    let mut deps_file: Option<PathBuf> = None;
    let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries(&dir)? {
        let name = file_name(&entry);
        let meta =
            fs::symlink_metadata(&entry).map_err(|e| format!("stat {}: {e}", entry.display()))?;
        if self.mounts.contains(&join(rel, &name)) {
            link(&entry, into.join(&name))?; // a mount: finished, carried by reference
        } else if meta.is_dir() {
            subdirs.push((name, entry));
        } else if name == "DEPS" {
            deps_file = Some(entry); // replaced by DEEP-DEPS, never an own file
        } else {
            link(&entry, into.join(&name))?; // a plain file, kept as-is
        }
    }

    // Sub-directories stay at their own names.
    for (name, _) in &subdirs {
        let child = into.join(name);
        fs::create_dir_all(&child).map_err(|e| format!("creating {}: {e}", child.display()))?;
        self.deepen(&join(rel, name), &child)?;
    }

    let Some(deps) = deps_file else {
        return Ok(());
    };
    let parsed = parse_deps(&deps)?;
    if parsed.is_empty() {
        // A DEPS of nothing but comments drops the file and adds NO `DEEP-DEPS`.
        // Creating an empty one would be a real (empty-tree) entry in the output
        // and would diverge from bootstrap's hand-deepen.
        return Ok(());
    }
    let dd = into.join("DEEP-DEPS");
    fs::create_dir_all(&dd).map_err(|e| format!("creating {}: {e}", dd.display()))?;
    for (dep_path, mount) in parsed {
        let target = normalize(&join(rel, &dep_path))?;
        let at = dd.join(&mount);
        if at.exists() {
            return Err(format!(
                "directory {rel:?} declares two deps mounted as {mount:?}"
            ));
        }
        // A dependency INSIDE a mount (`../../caos-core/rustc`) is mounted as
        // the finished object it is: never deepened again, and nothing under
        // it fetched beyond the levels needed to name it.
        if let Some(mount) = mount_of(pass.mounts, &target) {
            let target_path = reach_into_mount(base, mount, &target)?;
            link(&target_path, &at)?;
            continue;
        }
        // A FILE dependency is mounted, not deepened: there is nothing under it
        // to walk and no `DEPS` it could carry, so the mount IS the file. This
        // is what lets a package name a workspace manifest (`../../Cargo.toml
        // Cargo.toml`) or a shared lock beside the directories it needs, rather
        // than forcing every such thing to live in a directory of its own.
        //
        // `symlink_metadata` on the staged entry says "symlink" for every plain
        // file — that is how a materialized tree represents one — so this asks
        // `metadata`, which follows the link into /cas and answers about the
        // content. A missing target errors here rather than further down in
        // `entries`, where it would read as an empty directory.
        let target_path = format!("{base}/{target}");
        let meta = fs::metadata(&target_path)
            .map_err(|e| format!("dependency {target:?} of {rel:?}: {e}"))?;
        if !meta.is_dir() {
            link(&target_path, &at)?;
            continue;
        }
        // Already deepened somewhere else: re-stage that result rather than
        // walking it again. NOT a symlink to it — `caos put` resolves a symlink
        // into /cas to its recorded hash, but one pointing at a staging
        // directory is recorded AS A SYMLINK, which would put a link where the
        // subtree belongs. Copying the staged structure (all symlinks, so no
        // content moves) yields the identical tree, and identical trees are one
        // object.
        if let Some(done) = pass.memo.get(&target).cloned() {
            copy_staged(&done, &at)?;
            continue;
        }
        if pass.visiting.contains(&target) {
            return Err(format!(
                "dependency cycle: {} -> {target:?}",
                pass.visiting.join(" -> ")
            ));
        }
        pass.visiting.push(target.clone());
        fs::create_dir_all(&at).map_err(|e| format!("creating {}: {e}", at.display()))?;
        deepen(pass, &target, &at)?;
        pass.visiting.pop();
        pass.memo.insert(target, at);
    }
    Ok(())
}

/// The `/cas` path of `target`, which lies at or under `mount`: fetches the
/// levels between them (one directory each) and nothing else.
fn reach_into_mount(base: &str, mount: &str, target: &str) -> Result<String, String> {
    let mut at = mount.to_string();
    for comp in target[mount.len()..].split('/').filter(|c| !c.is_empty()) {
        caos(["get", &full(base, &at)])?;
        at = join(&at, comp);
    }
    let p = full(base, target);
    fs::symlink_metadata(&p)
        .map_err(|e| format!("dependency {target:?}, inside the mount {mount:?}: {e}"))?;
    Ok(p)
}

// ---- helpers -------------------------------------------------------------------

/// The `(path, name)` pairs in a `DEPS` file: one per non-empty, non-`#` line,
/// each two whitespace-separated fields.
fn parse_deps(deps: &Path) -> Result<Vec<(String, String)>, String> {
    let text = fs::read_to_string(deps).map_err(|e| format!("reading DEPS: {e}"))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(dep_path), Some(name), None) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(format!(
                "malformed DEPS line {line:?}: expected `<path> <name>`"
            ));
        };
        out.push((dep_path.to_string(), name.to_string()));
    }
    Ok(out)
}

/// Join a tree-relative directory `rel` with a further `comp` (a name or a
/// relative dep path), leaving normalization to [`normalize`].
fn join(rel: &str, comp: &str) -> String {
    if rel.is_empty() {
        comp.to_string()
    } else {
        format!("{rel}/{comp}")
    }
}

/// Resolve `.`/`..` in a `/`-separated path lexically, erroring if it escapes
/// the tree root.
fn normalize(p: &str) -> Result<String, String> {
    let mut stack: Vec<&str> = Vec::new();
    for comp in p.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack
                    .pop()
                    .ok_or_else(|| format!("dep path {p:?} escapes the tree root"))?;
            }
            c => stack.push(c),
        }
    }
    Ok(stack.join("/"))
}

/// Recreate a staged directory at `to`: directories are made, symlinks are
/// remade with the same target. Everything a staged tree holds is one or the
/// other (a plain file is staged as a symlink into `/cas`), so this reproduces
/// it exactly while moving no content.
fn copy_staged(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("creating {}: {e}", to.display()))?;
    for entry in entries(path(from))? {
        let name = file_name(&entry);
        let meta =
            fs::symlink_metadata(&entry).map_err(|e| format!("stat {}: {e}", entry.display()))?;
        if meta.is_symlink() {
            let target =
                fs::read_link(&entry).map_err(|e| format!("readlink {}: {e}", entry.display()))?;
            link(&target, to.join(&name))?;
        } else if meta.is_dir() {
            copy_staged(&entry, &to.join(&name))?;
        } else {
            return Err(format!("unexpected staged entry {}", entry.display()));
        }
    }
    Ok(())
}
