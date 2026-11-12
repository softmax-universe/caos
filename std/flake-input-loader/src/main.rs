//! std/flake-input-loader — mount a pinned flake input into a consumer's tree
//! (design/flake-inputs.md, "Consumer root").
//!
//! This is what lets a project that is NOT caos use caos' `std/*` without
//! vendoring or committing any of it. Its root `.caos-expr` is one line:
//!
//! ```text
//! run --base:@@=git+https://github.com/org/caos?rev=<sha>&dir=std/flake-input-loader --in:@=. --expr=$CAOS_EXPR --input=caos --input-tree:@@=git+https://github.com/org/caos?rev=<sha>&dir=std --output-path=caos-std
//! ```
//!
//! That is ONE line and has to be: `eval` splits on `text.lines()`, so a
//! trailing `\` is a token rather than a continuation. Wrapping it for
//! readability -- as this comment and design/flake-inputs.md both used to --
//! produces a tree whose `caos-std/` never resolves.
//!
//! The caller says WHICH input, WHAT of it to load, and WHERE to put it. This
//! worker checks the pin and does the splice.
//!
//! `--output-path=.` puts it HERE: the result is the input tree itself, in
//! place of `--in`. That is for an expression kept in a directory of its own
//! (`caos-std/`, holding the expression, `flake.nix` and `flake.lock`): the
//! loader's input is then those three files rather than a whole session tree,
//! so it is keyed on something that does not change from one call to the next.
//! The pin is checked first, exactly as for any other path.
//!
//! Two things are worth understanding about the shape:
//!
//! - **The locator appears twice, and must.** `--base` yields this worker's
//!   IMAGE; `--input-tree` yields the input's TREE. Passing the tree as an arg
//!   is what lets caos' own `:@@=` resolution do the fetching — the client
//!   resolves it before the request exists, so the ArgTree carries an oid and
//!   the URL never enters a cache key. The alternative would be this worker
//!   finding its own way to pull a subtree into the server's git repo.
//! - **`--expr=$CAOS_EXPR` is the only way to see the expression.** An
//!   expression is evaluated against its directory MINUS the directive
//!   (`strip_caos_expr`), so `--in:@=.` does NOT contain the `.caos-expr` and
//!   `--expr:@=.caos-expr` names a file that is not there. `$CAOS_EXPR` is the
//!   evaluator handing over the blob it is interpreting.
//!
//! What is checked, and what cannot be: the locators in the expression must
//! name the same repo and revision that `flake.lock` locks for `--input`, so a
//! tree cannot be evaluated against one caos while its lockfile declares
//! another (the `nix flake update`-without-regenerating drift). This compares
//! DECLARATIONS. No worker can verify that `--input-tree` actually came from
//! that revision — mapping a rev to a tree needs a fetch, and by the time a
//! worker runs, the locator is already an oid. That is a drift detector, not a
//! proof about the bytes.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use worker_common::{arg, caos, entries, file_name, link, path, read_arg, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("flake-input-loader", run)
}

fn run() -> Result<(), String> {
    let input = read_arg("input")?;
    let output_path = read_arg_opt("output-path")?;
    let mount_spec = read_arg_opt("mounts")?;
    let expr = read_arg("expr")?;

    let in_dir = PathBuf::from(arg("in"));
    caos(["get", path(&in_dir)])?;

    // `flake.nix` is only checked for PRESENCE. Parsing it would need a nix
    // parser, and would buy nothing: `flake.lock` is nix's own resolved answer,
    // derived from `flake.nix`, and is what a build actually consumes — an
    // input that vanished from `flake.nix` is dropped from the lock too.
    if !in_dir.join("flake.nix").exists() {
        return Err("no flake.nix in --in; this loads a flake INPUT, so the consumer must be a flake"
            .to_string());
    }
    let lock_path = in_dir.join("flake.lock");
    if !lock_path.exists() {
        return Err("no flake.lock in --in; the input is not pinned, so there is nothing to check against"
            .to_string());
    }
    caos(["get", path(&lock_path)])?;
    let lock = std::fs::read_to_string(&lock_path).map_err(|e| format!("reading flake.lock: {e}"))?;

    let (url, rev) = locked_input(&lock, &input)?;
    check_expr(&expr, &input, &url, &rev)?;

    let out = scratch("loaded")?;
    let input_tree = PathBuf::from(arg("input-tree"));
    let mounts = match (output_path, mount_spec) {
        (Some(_), Some(_)) => {
            return Err("give --output-path or --mounts, not both".to_string());
        }
        (None, None) => {
            return Err(
                "say where the input goes: --output-path=<path>, or --mounts=<from>:<to>,…"
                    .to_string(),
            );
        }
        // `.` mounts the input AT the consumer's own directory: the result IS
        // the input tree, replacing the tree that was handed in. That is what
        // lets the expression live in a small directory of its own
        // (`caos-std/`) whose evaluation does not change as the rest of the
        // session does. Only a bare `.` means this; every other spelling that
        // names no component is still refused below.
        (Some(output_path), None) if output_path.trim() == "." => {
            let mounted = out.join("mounted");
            link(&input_tree, &mounted)?;
            return caos(["put", path(&mounted), "/cas/out"]);
        }
        (Some(output_path), None) => {
            let to = components(&output_path);
            if to.is_empty() {
                return Err(format!("--output-path={output_path:?} names no path"));
            }
            vec![(Vec::new(), to)]
        }
        (None, Some(spec)) => parse_mounts(&spec)?,
    };

    let mut sources = Vec::new();
    for (from, to) in &mounts {
        sources.push((to.as_slice(), descend(&input_tree, from)?));
    }
    let plan: Vec<(&[String], &Path)> = sources.iter().map(|(to, p)| (*to, p.as_path())).collect();
    splice(Some(in_dir.clone()), &out, &plan)?;
    record_mounts(&in_dir, &out, &mounts)?;
    caos(["put", path(&out), "/cas/out"])
}

/// The non-empty components of a `/`-separated path, `.` dropped.
fn components(p: &str) -> Vec<String> {
    p.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .map(String::from)
        .collect()
}

/// `--mounts=<from>:<to>,…` as `(from, to)` component lists. `<from>` is a
/// path in the input tree (`.` for all of it), `<to>` a path in the result.
/// Mounting several directories of ONE input in one call is what gives a
/// consumer `caos-core/` beside `caos-std/` from a single pin
/// (design/nix-bootstrap.md).
fn parse_mounts(spec: &str) -> Result<Vec<(Vec<String>, Vec<String>)>, String> {
    let mut mounts: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (from, to) = item
            .split_once(':')
            .ok_or_else(|| format!("--mounts entry {item:?} is not <from>:<to>"))?;
        if from.split('/').any(|c| c == "..") || to.split('/').any(|c| c == "..") {
            return Err(format!("--mounts entry {item:?} uses `..`"));
        }
        let to = components(to);
        if to.is_empty() {
            return Err(format!(
                "--mounts entry {item:?} names no destination (`.` is --output-path=.)"
            ));
        }
        for (_, other) in &mounts {
            let shorter = other.len().min(to.len());
            if other[..shorter] == to[..shorter] {
                return Err(format!(
                    "--mounts puts {} and {} one inside the other",
                    other.join("/"),
                    to.join("/")
                ));
            }
        }
        mounts.push((components(from), to));
    }
    if mounts.is_empty() {
        return Err("--mounts names nothing to mount".to_string());
    }
    Ok(mounts)
}

/// The `/cas` path of `from` inside the input tree, fetching each level on the
/// way down (and nothing below it).
fn descend(tree: &Path, from: &[String]) -> Result<PathBuf, String> {
    let mut at = tree.to_path_buf();
    for comp in from {
        caos(["get", path(&at)])?;
        at = at.join(comp);
        if !at.exists() {
            return Err(format!("the input has no {}", from.join("/")));
        }
    }
    Ok(at)
}

/// Write the result's `.caos-mounts`: whatever `--in` already listed, plus every
/// destination this call mounted. A mounted tree is FINISHED — it was
/// evaluated (deepened) where it came from — and this is how a later transform
/// of the consumer's tree (deep-deps) knows not to walk into it.
fn record_mounts(
    in_dir: &Path,
    out: &Path,
    mounts: &[(Vec<String>, Vec<String>)],
) -> Result<(), String> {
    let mut listed = std::collections::BTreeSet::new();
    let existing = in_dir.join(MOUNTS_FILE);
    if existing.exists() {
        caos(["get", path(&existing)])?;
        let text = std::fs::read_to_string(&existing)
            .map_err(|e| format!("reading {MOUNTS_FILE}: {e}"))?;
        listed.extend(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from));
    }
    listed.extend(mounts.iter().map(|(_, to)| to.join("/")));
    let at = out.join(MOUNTS_FILE);
    if at.symlink_metadata().is_ok() {
        std::fs::remove_file(&at).map_err(|e| format!("replacing {MOUNTS_FILE}: {e}"))?;
    }
    let body: String = listed.iter().map(|m| format!("{m}\n")).collect();
    std::fs::write(&at, body).map_err(|e| format!("writing {MOUNTS_FILE}: {e}"))
}

/// Build `dst` as `src` with each `(path, tree)` of `plan` spliced in.
///
/// Every sibling is carried across BY REFERENCE: `caos put` resolves a symlink
/// into `/cas` to the content's recorded hash, so an untouched subtree is never
/// read, copied or re-uploaded however large it is. Only the directories along
/// the mount paths are materialized, one level at a time.
fn splice(src: Option<PathBuf>, dst: &Path, plan: &[(&[String], &Path)]) -> Result<(), String> {
    let heads: std::collections::BTreeSet<&str> =
        plan.iter().map(|(to, _)| to[0].as_str()).collect();
    let mut present = std::collections::BTreeSet::new();
    if let Some(src) = &src {
        caos(["get", path(src)])?;
        for entry in entries(path(src))? {
            let name = file_name(&entry);
            if heads.contains(name.as_str()) {
                present.insert(name);
                continue; // replaced (a leaf) or merged into (below)
            }
            link(&entry, dst.join(&name))?;
        }
    }

    for head in heads {
        let here: Vec<&(&[String], &Path)> =
            plan.iter().filter(|(to, _)| to[0] == head).collect();
        if let Some((_, tree)) = here.iter().find(|(to, _)| to.len() == 1) {
            // Refuse to overwrite. The mount point is meant to be absent from
            // the tree (gitignored in the consumer), so a collision means the
            // caller aimed at something real — silently replacing it would lose
            // it.
            if present.contains(head) {
                return Err(format!(
                    "mounting would overwrite {head:?}, which already exists in --in"
                ));
            }
            link(tree, dst.join(head))?;
            continue;
        }
        let next_dst = dst.join(head);
        std::fs::create_dir(&next_dst)
            .map_err(|e| format!("creating {}: {e}", next_dst.display()))?;
        let next_src = match src.as_ref().map(|s| s.join(head)) {
            Some(p) if p.is_dir() => Some(p),
            Some(p) if p.exists() => {
                return Err(format!(
                    "a mount path descends through {}, which is a file",
                    file_name(&p)
                ))
            }
            _ => None, // a fresh directory the consumer's tree does not have
        };
        let rest: Vec<(&[String], &Path)> = here.iter().map(|(to, t)| (&to[1..], *t)).collect();
        splice(next_src, &next_dst, &rest)?;
    }
    Ok(())
}

/// The `(url, rev)` `flake.lock` locks for the root flake's `input`.
///
/// The node KEY is not the input name in general — `nodes.<root>.inputs.<name>`
/// maps one to the other — so this follows that indirection rather than
/// guessing.
fn locked_input(lock: &str, input: &str) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(lock).map_err(|e| format!("flake.lock is not JSON: {e}"))?;
    let root = v.get("root").and_then(|r| r.as_str()).unwrap_or("root");
    let node_name = v
        .get("nodes")
        .and_then(|n| n.get(root))
        .and_then(|n| n.get("inputs"))
        .and_then(|i| i.get(input))
        .ok_or_else(|| format!("flake.lock has no input {input:?} on its root node"))?;
    let node_name = node_name.as_str().ok_or_else(|| {
        format!("flake.lock input {input:?} is a `follows` path, which carries no lock of its own")
    })?;
    let locked = v
        .get("nodes")
        .and_then(|n| n.get(node_name))
        .and_then(|n| n.get("locked"))
        .ok_or_else(|| format!("flake.lock node {node_name:?} has no `locked` section"))?;
    let field = |k: &str| -> Result<String, String> {
        locked
            .get(k)
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("flake.lock input {input:?} has no `{k}`"))
    };
    let rev = locked.get("rev").and_then(|r| r.as_str()).ok_or_else(|| {
        format!("flake.lock input {input:?} has no `rev` — it is not pinned to a commit")
    })?;
    let url = match locked.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "github" => format!("https://github.com/{}/{}", field("owner")?, field("repo")?),
        "gitlab" => format!("https://gitlab.com/{}/{}", field("owner")?, field("repo")?),
        "git" => field("url")?,
        other => {
            return Err(format!(
                "flake.lock input {input:?} has type {other:?}, which carries no URL to compare \
                 against a locator"
            ))
        }
    };
    Ok((normalize_url(&url), rev.to_string()))
}

/// Check the expression's `:@@=` locators against what `flake.lock` locks.
///
/// Only locators naming THIS input's URL are checked, so a consumer pinning
/// several inputs runs the loader once per input without the others tripping
/// it. At least one must match: an expression that never names the locked URL
/// is the drift this exists to catch.
fn check_expr(expr: &str, input: &str, url: &str, rev: &str) -> Result<(), String> {
    let found = locators(expr);
    let mine: Vec<&Locator> = found.iter().filter(|l| l.url == url).collect();
    if mine.is_empty() {
        let seen: Vec<&str> = found.iter().map(|l| l.url.as_str()).collect();
        return Err(format!(
            "flake.lock locks {input:?} at {url}, but no `:@@=` locator in the expression names \
             it (the expression names: {})",
            if seen.is_empty() {
                "nothing".to_string()
            } else {
                seen.join(", ")
            }
        ));
    }
    for loc in mine {
        match &loc.rev {
            None => return Err(format!("locator {:?} pins no rev", loc.raw)),
            Some(r) if r != rev => {
                return Err(format!(
                    "{input:?} is locked at {rev} in flake.lock, but the expression pins {r} \
                     ({:?}) — regenerate the expression, or re-lock the input",
                    loc.raw
                ))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// One `--name:@@=<value>` occurrence in an expression.
struct Locator {
    url: String,
    rev: Option<String>,
    raw: String,
}

/// Every `:@@=` locator in an expression, in order. Full-line `#` comments are
/// skipped, matching the evaluator's own rule, so a commented-out locator is
/// not checked.
fn locators(expr: &str) -> Vec<Locator> {
    let mut found = Vec::new();
    for line in expr.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        for token in line.split_whitespace() {
            let Some(body) = token.strip_prefix("--") else {
                continue;
            };
            // `split_once('=')` takes the FIRST `=`, so a `?rev=…&dir=…` query
            // survives whole in the value.
            let Some((key, value)) = body.split_once('=') else {
                continue;
            };
            if !key.ends_with(":@@") {
                continue;
            }
            let (url, query) = value.split_once('?').unwrap_or((value, ""));
            let rev = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("rev="))
                .map(str::to_string);
            found.push(Locator {
                url: normalize_locator_url(url),
                rev,
                raw: value.to_string(),
            });
        }
    }
    found
}

/// A locator's URL in the same shape [`locked_input`] produces: nix's `git+`
/// transport prefix comes off and its host shorthands expand, because git and
/// the lockfile spell the same repo differently.
fn normalize_locator_url(url: &str) -> String {
    let url = url.strip_prefix("git+").unwrap_or(url);
    if let Some(rest) = url.strip_prefix("github:") {
        return normalize_url(&format!("https://github.com/{rest}"));
    }
    if let Some(rest) = url.strip_prefix("gitlab:") {
        return normalize_url(&format!("https://gitlab.com/{rest}"));
    }
    normalize_url(url)
}

/// Trailing `/` and `.git` are not part of a repo's identity; comparing without
/// them keeps `…/caos`, `…/caos/` and `…/caos.git` from reading as three repos.
fn normalize_url(url: &str) -> String {
    let url = url.trim_end_matches('/');
    url.strip_suffix(".git").unwrap_or(url).to_string()
}
