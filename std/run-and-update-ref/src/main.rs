//! Run one already-complete CAOS request, then append its task status to a
//! session ref without making the caller wait in a worker container.
//!
//! `Q = run-and-update-ref { subreq: R, target-ref: F }` has three positions:
//! start emits `run-request-then R` with this same Q (minus the call result) as
//! its callback; success and failure finishes make the result addressable,
//! append an async terminal transition containing that result OID to F, and
//! return R's result unchanged. When `child` is present, the finish position
//! instead validates the child's terminal session head and checkpoints it
//! on the parent before returning. A caught R failure is represented as a small
//! result tree because the current promise protocol can deliver a failure to a
//! callback but cannot ask that callback to rethrow it.
//!
//! A non-session caller can instead supply `result-ref`, constrained to
//! `refs/heads/caos-task-results/<subreq>`. The same callback publishes an
//! immutable result commit there, without requiring an async session record.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use worker_common::{
    arg, caos, caos_curry, cas_hash, forward, own_args_tree, read_arg, read_arg_opt,
    run_request_then_catching, run_worker, scratch, Arg,
};

mod refs;

fn main() -> ExitCode {
    run_worker("run-and-update-ref", run)
}

fn run() -> Result<(), String> {
    let has_result = Path::new(&arg("result")).exists();
    let has_error = Path::new(&arg("error")).exists();
    match (has_result, has_error) {
        (false, false) => start(),
        (true, false) => finish_success(),
        (false, true) => finish_failure(),
        (true, true) => Err("callback received both result and error".to_string()),
    }
}

fn start() -> Result<(), String> {
    let request = read_arg("subreq")?;
    refs::validate_hash(&request, "subreq")?;
    let result_ref = read_arg_opt("result-ref")?;
    match (read_arg_opt("target-ref")?, &result_ref) {
        (Some(target), None) => refs::validate_target_ref(&target)?,
        (None, Some(target)) => refs::validate_result_ref(target, &request)?,
        _ => return Err("exactly one of target-ref and result-ref is required".to_string()),
    }
    if let Some(child) = read_arg_opt("child")? {
        if result_ref.is_some() {
            return Err("child checkpoints require a target session ref".to_string());
        }
        refs::validate_child(&child)?;
    }

    // Curry the exact Q forward as the finish callback. `result`/`error` are
    // call-time args supplied later by the promise interpreter.
    let q = own_args_tree()?;
    let callback = caos_curry(Arg::Hash(&q), &[("task", Arg::Lit(&q))])?;
    run_request_then_catching(&request, Arg::Hash(&callback))
}

fn finish_success() -> Result<(), String> {
    let result_path = arg("result");
    let result = cas_hash(&result_path)?;
    refs::validate_hash(&result, "result")?;
    finish("complete", &result)?;
    // Exact pass-through, including tree/commit result kinds and without
    // loading potentially large output bytes.
    forward(&result_path, "/cas/out")
}

fn finish_failure() -> Result<(), String> {
    caos(["get", &arg("error")])?;
    let error =
        fs::read_to_string(arg("error")).map_err(|e| format!("reading subrequest failure: {e}"))?;
    let out = scratch("run-and-update-ref-failure")?;
    fs::write(out.join("status"), "failed\n")
        .map_err(|e| format!("writing failure status: {e}"))?;
    fs::write(out.join("error"), error).map_err(|e| format!("writing failure detail: {e}"))?;
    // Store the caught failure before publishing the terminal event. If this
    // process dies after the append but before returning, the event still names
    // the exact result a retry should converge on.
    let result_path = "/cas/terminal-result";
    caos(["put", worker_common::path(&out), result_path])?;
    let result = cas_hash(result_path)?;
    refs::validate_hash(&result, "failure result")?;
    finish("failed", &result)?;
    forward(result_path, "/cas/out")
}

fn finish(status: &str, result: &str) -> Result<(), String> {
    if let Some(target) = read_arg_opt("result-ref")? {
        let request = read_arg("subreq")?;
        refs::validate_result_ref(&target, &request)?;
        return publish_result(&target, &request, status, result);
    }
    let target_ref = read_arg("target-ref")?;
    if let Some(child) = read_arg_opt("child")? {
        let subrequest = read_arg("subreq")?;
        let relay = read_arg("task")?;
        return refs::append_child_terminal(&target_ref, &child, &subrequest, &relay, status);
    }
    let task = read_arg("task")?;
    refs::append_status(&target_ref, &task, status, result)
}

/// A caller without a session (a protocol listener, for example) still needs a
/// durable completion record. Keep the result by reference in an immutable
/// commit. Ref writes use the ordinary compare-and-swap API, not a new channel.
fn publish_result(target: &str, request: &str, status: &str, result: &str) -> Result<(), String> {
    refs::validate_hash(result, "result")?;
    if status != "complete" && status != "failed" {
        return Err("invalid result publication status".to_string());
    }
    let value = "/cas/task-result-value";
    caos(["get-hash", result, value])?;
    let directory = scratch("detached-task-result")?;
    worker_common::link(value, directory.join("result"))?;
    fs::write(directory.join("status"), status)
        .map_err(|_| "writing task result status")?;
    fs::write(directory.join("request"), request)
        .map_err(|_| "writing task request identity")?;
    caos(["put", worker_common::path(&directory), "/cas/task-result-tree"])?;
    let tree = cas_hash("/cas/task-result-tree")?;
    // Fixed metadata makes a repeated callback mint the exact same object.
    // This is a task-result commit, not a source-code or session-spine commit.
    let body = format!(
        "tree {tree}\nauthor CAOS task runner <task@caos> 0 +0000\n\
         committer CAOS task runner <task@caos> 0 +0000\n\nCAOS task result\n"
    );
    let commit_file = directory.join("commit");
    fs::write(&commit_file, body).map_err(|_| "writing task result commit")?;
    caos([
        "put-commit",
        worker_common::path(&commit_file),
        "/cas/task-result-commit",
    ])?;
    let commit = cas_hash("/cas/task-result-commit")?;
    let pushed = Command::new("caos")
        .args([
            "ref-set",
            target,
            "--new:@=/cas/task-result-commit",
            "--expected=absent",
        ])
        .output()
        .map_err(|_| "starting task result publication")?;
    if pushed.status.success() {
        return Ok(());
    }
    // A lost response or duplicate callback is successful only if the ref now
    // names this exact immutable result. Never force over a different result.
    let observed = "/cas/observed-task-result";
    let read = Command::new("caos")
        .args(["ref-get", target, observed])
        .output()
        .map_err(|_| "reconciling task result publication")?;
    if read.status.success() && cas_hash(observed)? == commit {
        return Ok(());
    }
    Err("task result publication failed or conflicts with an existing result".to_string())
}
