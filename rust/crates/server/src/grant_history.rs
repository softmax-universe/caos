//! The commits a `reader:@@=` covers (SPEC.md, "Secrets"): the locator's
//! `rev` and its first-parent ancestors, back to `since`, or the newest
//! [`DEFAULT_COMMITS`] with no `since`.
//!
//! Only commits are needed here, so a repository this server does not already
//! hold is fetched with `--filter=tree:0` into a staging repo of its own,
//! never into the object database, whose closure check refuses a partial
//! history. Each commit's tree is fetched when it is evaluated
//! ([`crate::approved`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use git_locator::GitRef;

use crate::Config;

/// How many commits a reader with no `since=` covers. Each is evaluated when
/// the secret is pushed, so this is what a push costs; a session pinned to an
/// older commit than these needs a `since=` that reaches it.
pub(crate) const DEFAULT_COMMITS: usize = 20;

/// The commits of `git_ref`'s range, newest first.
pub(crate) fn commits(
    config: &Config,
    git_ref: &GitRef,
    since: Option<&str>,
) -> Result<Vec<String>, String> {
    let rev = git_ref
        .rev
        .clone()
        .ok_or("a reader:@@= locator must pin rev=<sha>")?;
    if let Ok(commits) = first_parent_commits(&config.git_dir, &rev, since) {
        return Ok(commits);
    }
    let staging = staging_repo(config, &git_ref.fetch_url())?;
    git(
        &staging,
        &["fetch", "-q", "--filter=tree:0", "origin", &rev],
    )?;
    first_parent_commits(&staging.to_string_lossy(), &rev, since)
}

/// A grant's locator with any `ref=` replaced by the commit the branch names
/// now. `git+caos://` is this server, so its refs are read here; any other URL
/// is asked with `ls-remote`, remembered for [`REF_TTL`].
pub(crate) fn pin(config: &Config, locator: &str) -> Result<String, String> {
    let (url, query) = locator.split_once('?').unwrap_or((locator, ""));
    let mut name = None;
    let mut rest = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("ref", value)) => name = Some(value),
            _ => rest.push(pair.to_string()),
        }
    }
    let Some(name) = name else {
        return Ok(locator.to_string());
    };
    let rev = resolve_branch(config, url, name)?;
    rest.push(format!("rev={rev}"));
    Ok(format!("{url}?{}", rest.join("&")))
}

/// How long a remote branch's answer is trusted.
const REF_TTL: std::time::Duration = std::time::Duration::from_secs(60);

type Resolved = HashMap<(String, String), (std::time::Instant, String)>;
static BRANCHES: Mutex<Option<Resolved>> = Mutex::new(None);

fn resolve_branch(config: &Config, url: &str, name: &str) -> Result<String, String> {
    if url.starts_with("git+caos://") {
        let out = git(
            Path::new(&config.git_dir),
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{name}^{{commit}}"),
            ],
        )
        .map_err(|_| format!("{name} names no commit on this server"))?;
        return Ok(out.trim().to_string());
    }
    let key = (url.to_string(), name.to_string());
    if let Some((at, rev)) = BRANCHES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&key).cloned())
    {
        if at.elapsed() < REF_TTL {
            return Ok(rev);
        }
    }
    let fetch_url = url.strip_prefix("git+").unwrap_or(url);
    let fetch_url = match fetch_url.strip_prefix("github:") {
        Some(repo) => format!("https://github.com/{repo}"),
        None => fetch_url.to_string(),
    };
    let out = git(Path::new(&config.git_dir), &["ls-remote", &fetch_url, name])?;
    let rev = out
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| format!("{fetch_url} has no {name}"))?
        .to_string();
    BRANCHES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, (std::time::Instant::now(), rev.clone()));
    Ok(rev)
}

/// `rev` and its first-parent ancestors, newest first: back to and including
/// `since`, or [`DEFAULT_COMMITS`] of them. First-parent, so a branch forked
/// before `since` and merged after it does not bring back the code `since`
/// excluded.
fn first_parent_commits(dir: &str, rev: &str, since: Option<&str>) -> Result<Vec<String>, String> {
    let dir = Path::new(dir);
    let range = match since {
        Some(since) => {
            git(dir, &["merge-base", "--is-ancestor", since, rev])
                .map_err(|_| format!("since={since} is not an ancestor of {rev}"))?;
            format!("^{since}")
        }
        None => format!("--max-count={DEFAULT_COMMITS}"),
    };
    let args = ["log", "--first-parent", "--format=%H", &range, rev];
    let mut commits: Vec<String> = git(dir, &args)?.lines().map(str::to_string).collect();
    if let Some(since) = since {
        commits.push(since.to_string());
    }
    Ok(commits)
}

/// A bare repo per URL, beside `secrets.git`, set up as a treeless partial
/// clone of it.
fn staging_repo(config: &Config, url: &str) -> Result<PathBuf, String> {
    let digest =
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, url.as_bytes())
            .map_err(|e| format!("hashing {url}: {e}"))?;
    let dir = Path::new(&config.secrets_git)
        .with_file_name("grant-history")
        .join(format!("{digest}.git"));
    if !dir.join("HEAD").is_file() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        git(&dir, &["init", "-q", "--bare"])?;
        git(&dir, &["remote", "add", "origin", url])?;
        git(&dir, &["config", "remote.origin.promisor", "true"])?;
        git(
            &dir,
            &["config", "remote.origin.partialclonefilter", "tree:0"],
        )?;
        git(&dir, &["config", "extensions.partialClone", "origin"])?;
    }
    Ok(dir)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("running git {args:?}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_parent_back_to_since() {
        let dir = std::env::temp_dir().join(format!(
            "caos-grant-history-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| git(&dir, args).unwrap();
        run(&["init", "-q", "-b", "main"]);
        let commit = |msg: &str| {
            std::fs::write(dir.join("f"), msg).unwrap();
            run(&["add", "f"]);
            run(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                msg,
            ]);
            run(&["rev-parse", "HEAD"]).trim().to_string()
        };
        let a = commit("a");
        run(&["checkout", "-q", "-b", "side"]);
        let side = commit("side");
        run(&["checkout", "-q", "main"]);
        let b = commit("b");
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "merge",
            "-q",
            "--no-edit",
            "-X",
            "ours",
            "side",
        ]);
        let c = run(&["rev-parse", "HEAD"]).trim().to_string();

        let d = dir.to_str().unwrap();
        let back_to_b = first_parent_commits(d, &c, Some(&b)).unwrap();
        assert_eq!(back_to_b, [c.as_str(), b.as_str()]);
        // With no `since`, the newest few; a merged branch is not first-parent.
        let newest = first_parent_commits(d, &c, None).unwrap();
        assert_eq!(newest, [c.as_str(), b.as_str(), a.as_str()]);
        assert!(first_parent_commits(d, &b, Some(&side)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
