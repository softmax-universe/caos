// Evaluating connector builds its image and runs its Go auth, wire, transport,
// and credential-file tests in Nix. No real account or sign-in is used here.
package main

import (
	"os"
	"strings"

	"caos/w"
)

func next(stage string, extra ...string) string {
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--publisher:@=/cas/args/publisher", "--stage=" + stage}
	if w.Exists("/cas/args/test-salt") {
		args = append(args, "--test-salt:@=/cas/args/test-salt")
	}
	return w.Caos(append(args, extra...)...)
}

func verifyPublishedResult(request, status string) string {
	w.Caos("ref-get", "refs/heads/caos-task-results/"+request, "/cas/published-result")
	commit := w.Hash("/cas/published-result")
	w.Caos("resolve", commit, "status", "/cas/published-status")
	w.True(w.Read("/cas/published-status") == status, "task published the wrong terminal status")
	w.Caos("resolve", commit, "request", "/cas/published-request")
	w.True(w.Read("/cas/published-request") == request, "published another request's result")
	w.Caos("resolve", commit, "result", "/cas/published-output")
	w.True(w.Hash("/cas/published-output") == w.Hash("/cas/args/result"), "published a different result object")
	w.Get("/cas/published-output")
	if status == "complete" {
		w.True(w.Read("/cas/published-output/stdout") == "detached agent fixture\n", "task result contents changed")
	} else {
		w.True(strings.TrimSpace(w.Read("/cas/published-output/status")) == "failed", "failure became a successful result")
		w.True(w.Exists("/cas/published-output/error"), "failure diagnostic was lost")
	}
	return commit
}

func main() {
	w.Main(func() {
		stage := ""
		if w.Exists("/cas/args/stage") {
			stage = w.Read("/cas/args/stage")
		}
		switch stage {
		case "publisher-child":
			directory := w.Check(os.MkdirTemp("", "detached-agent-fixture-"))
			w.Must(os.WriteFile(directory+"/stdout", []byte("detached agent fixture\n"), 0600))
			w.Put(directory, "/cas/out")
			return
		case "publisher-failure":
			w.True(false, "expected detached task failure")
			return
		case "verify-error":
			const result = "/cas/args/result"
			w.Get(result)
			w.True(w.Exists(result+"/failed"), "unsupported operation was not a failed result")
			w.True(strings.Contains(w.Read(result+"/stdout"), "unsupported Delta worker operation"),
				"unsupported operation did not return its diagnostic")
			child := []string{"prepare-request", "--base:@=/cas/args/base",
				"--worker1:@=/cas/args/worker1", "--stage=publisher-child"}
			if w.Exists("/cas/args/test-salt") {
				child = append(child, "--test-salt:@=/cas/args/test-salt")
			}
			request := w.Caos(child...)
			task := w.Caos("prepare-request", "--base:@=/cas/args/publisher", "--subreq="+request,
				"--result-ref=refs/heads/caos-task-results/"+request)
			// Native detached admission followed by a continuation joins the same
			// immutable task without holding a worker slot while it finishes.
			w.True(w.Caos("sub-run", task) == "request "+task, "detached task was not admitted")
			w.Caos("run-request-then", task, "--then:hash="+next("verify-publisher", "--request="+request))
			return
		case "verify-publisher":
			request := w.Read("/cas/args/request")
			commit := verifyPublishedResult(request, "complete")
			// A different callback for the same R must reconcile its already
			// published result, not overwrite or fail on an ordinary duplicate.
			repeated := w.Caos("prepare-request", "--base:@=/cas/args/publisher", "--subreq="+request,
				"--result-ref=refs/heads/caos-task-results/"+request, "--repeat=1")
			w.Caos("run-request-then", repeated,
				"--then:hash="+next("verify-repeat", "--request="+request, "--published="+commit))
			return
		case "verify-repeat":
			commit := verifyPublishedResult(w.Read("/cas/args/request"), "complete")
			w.True(commit == w.Read("/cas/args/published"), "duplicate callback changed the result commit")
			child := []string{"prepare-request", "--base:@=/cas/args/base",
				"--worker1:@=/cas/args/worker1", "--stage=publisher-failure"}
			if w.Exists("/cas/args/test-salt") {
				child = append(child, "--test-salt:@=/cas/args/test-salt")
			}
			request := w.Caos(child...)
			task := w.Caos("prepare-request", "--base:@=/cas/args/publisher", "--subreq="+request,
				"--result-ref=refs/heads/caos-task-results/"+request)
			w.Caos("run-request-then", task, "--then:hash="+next("verify-failed-publisher", "--request="+request))
			return
		case "verify-failed-publisher":
			request := w.Read("/cas/args/request")
			verifyPublishedResult(request, "failed")
			invalid := w.Caos("prepare-request", "--base:@=/cas/args/publisher", "--subreq="+request,
				"--result-ref=refs/heads/rejected-task-"+request)
			w.Caos("run-request-then", invalid, "--catch",
				"--then:hash="+next("verify-rejected-target"))
			return
		case "verify-rejected-target":
			w.True(w.Exists("/cas/args/error"), "publisher accepted a non-task result target")
			w.True(strings.Contains(w.Read("/cas/args/error"), "result-ref must"), "publisher failed for an unrelated reason")
			w.Report("delta-protocol: connector/unit tests built; detached success, failure, duplicate and invalid-target cases passed\n")
			return
		case "":
		default:
			w.True(false, "unknown test stage")
		}
		w.Step("build the standalone Delta connector and run its unit tests")
		const image = "/cas/args/connector"
		w.Get(image)
		w.True(w.Check(os.Stat(image)).IsDir(), "connector did not evaluate to an image")
		w.True(w.Exists(image+"/.caos-curry"), "connector image is not curried")
		w.True(w.Exists(image+"/base"), "connector image has no built base")
		salt := ""
		if w.Exists("/cas/args/test-salt") {
			salt = strings.TrimSpace(w.Read("/cas/args/test-salt"))
		}
		// Unsupported-op dispatch returns before reading any credential. This
		// stays account/network independent even when a caller has reader keys.
		request := w.Caos("prepare-request", "--base:@="+image, "--op=unsupported",
			"--request-id=delta-protocol-errors-"+salt)
		w.Caos("run-request-then", request, "--then:hash="+next("verify-error"))
	})
}
