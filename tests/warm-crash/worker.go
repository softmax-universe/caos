// tests/warm-crash — a WORKER test of the crash backstop for a warm runner's
// LATER job (design/firecracker-runners.md, phase 0b).
//
// dev/warm-crash-test's runner stays warm after its first job and dies, without
// posting, as soon as it claims its second. Only the first job's nonce passed
// through runnerd, so before claim records runnerd's backstop could only fail
// that (already settled) job, and the second job's caller waited forever:
// claimed jobs have no deadline. Covered:
//
//	start   job a runs on a fresh container and succeeds
//	first   …then job b, sent right after for the same base, is taken by
//	        that warm runner, which dies holding it
//	second  …and b FAILS, naming the dead container, instead of hanging
//
// Both jobs carry this run's id and the test salt: a cached answer never
// reaches a runner.
package main

import (
	"fmt"
	"math/rand"
	"os"
	"strings"
	"time"

	"caos/w"
)

func next(stage string, extra ...string) string {
	return w.Caos(append([]string{"curry", "--base:@=/cas/args/base",
		"--worker1:@=/cas/args/worker1", "--stage=" + stage,
		"--test-salt:@=/cas/args/test-salt", "--crashy:@=/cas/args/crashy",
		"--job:@=/cas/args/job"}, extra...)...)
}

func arg(name string) string { return strings.TrimSpace(w.Read("/cas/args/" + name)) }

// job runs the job script on the crashy image as job n of this run, and
// continues at stage then.
func job(n, run, then string, catch bool) {
	image := w.Caos("curry", "--base:@=/cas/args/crashy", "--worker1:@=/cas/args/job")
	request := w.Caos("prepare-request", "--base:hash="+image, "--n="+n,
		"--run="+run, "--test-salt:@=/cas/args/test-salt")
	args := []string{"run-request-then", request, "--then:hash=" + next(then, "--run="+run)}
	if catch {
		args = append(args, "--catch")
	}
	w.Caos(args...)
}

func main() {
	w.Main(func() {
		stage := "start"
		if w.Exists("/cas/args/stage") {
			stage = arg("stage")
		}
		switch stage {
		case "start":
			run := fmt.Sprintf("warm-crash-%d-%d-%d", time.Now().UnixNano(), os.Getpid(), rand.Intn(32768))
			w.Step("job a, on a fresh container")
			job("a", run, "first", false)

		case "first":
			w.Get("/cas/args/result")
			r := w.Read("/cas/args/result")
			w.True(strings.HasPrefix(r, "n=a "), "job a answered: %q", r)
			w.Step("ok: " + r)
			w.Step("job b, for the warm runner that will die holding it")
			job("b", arg("run"), "second", true)

		case "second":
			if !w.Exists("/cas/args/error") {
				// b ran to completion, so it did not land on the warm runner
				// (a fresh one would only have claimed once).
				w.True(false, "job b succeeded instead of dying with its runner: %s", w.Read("/cas/args/result"))
			}
			e := w.Read("/cas/args/error")
			w.True(strings.Contains(e, "worker container exited"),
				"job b failed, but not by the backstop: %s", e)
			w.Step("ok: job b failed: " + strings.SplitN(e, "\n", 2)[0])
			w.Report("warm-crash: ALL PASS\n")

		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}
