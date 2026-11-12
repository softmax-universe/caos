// tests/warm-pool — a WORKER test of a POOLED runner's warm reuse
// (design/firecracker-runners.md, phase 0a), on the dev stack's `test-pool`
// (dev/stack-up runs a runnerd requiring `required-pool=test-pool`).
//
// A warm runner's follow-up poll used to be `{base}` alone. Under a pooled
// host that is wrong both ways: the server's symmetric match rejects the
// pool's own jobs (they name `required-pool`, which the poll lacks), and it
// accepts same-base jobs from outside the pool. Covered:
//
//	start  job a, in the pool, starts a container
//	a      job b, in the pool, same base: the SAME container takes it
//	b      job c, same base, NO pool: a DIFFERENT container takes it, because
//	       the warm pooled one must not
//
// "Same container" is the runner's start time (/proc/1/stat), as in
// tests/resident. Every job carries this run's id and the test salt, so a
// cached answer never reaches a runner.
package main

import (
	"fmt"
	"math/rand"
	"os"
	"strings"
	"time"

	"caos/w"
)

const pool = "test-pool"

func next(stage string, extra ...string) string {
	return w.Caos(append([]string{"curry", "--base:@=/cas/args/base",
		"--worker1:@=/cas/args/worker1", "--stage=" + stage,
		"--test-salt:@=/cas/args/test-salt", "--warm:@=/cas/args/warm",
		"--job:@=/cas/args/job"}, extra...)...)
}

func arg(name string) string { return strings.TrimSpace(w.Read("/cas/args/" + name)) }

// job runs the job script on the warm image as job n of this run, in the pool
// or not, and continues at stage then with extra forwarded.
func job(n, run string, pooled bool, then string, extra ...string) {
	image := w.Caos("curry", "--base:@=/cas/args/warm", "--worker1:@=/cas/args/job")
	args := []string{"prepare-request", "--base:hash=" + image, "--n=" + n,
		"--run=" + run, "--test-salt:@=/cas/args/test-salt"}
	if pooled {
		args = append(args, "--required-pool="+pool)
	}
	request := w.Caos(args...)
	w.Caos("run-request-then", request,
		"--then:hash="+next(then, append([]string{"--run=" + run}, extra...)...))
}

// host is the container that answered job n, from its result.
func host(n string) string {
	r := w.Read("/cas/args/result")
	w.True(strings.HasPrefix(r, "n="+n+" host="), "job %s answered: %q", n, r)
	return strings.TrimPrefix(r, "n="+n+" host=")
}

func main() {
	w.Main(func() {
		stage := "start"
		if w.Exists("/cas/args/stage") {
			stage = arg("stage")
		}
		switch stage {
		case "start":
			run := fmt.Sprintf("warm-pool-%d-%d-%d", time.Now().UnixNano(), os.Getpid(), rand.Intn(32768))
			w.Step("job a, in " + pool)
			job("a", run, true, "a")

		case "a":
			a := host("a")
			w.Step("ok: a ran on " + a)
			w.Step("job b, in " + pool + ", same base: the warm runner takes it")
			job("b", arg("run"), true, "b", "--host-a="+a)

		case "b":
			a, b := arg("host-a"), host("b")
			w.True(b == a, "job b ran on %s, not on a's warm runner %s: the pooled warm runner did not keep its pool", b, a)
			w.Step("ok: b ran on a's warm runner " + b)
			w.Step("job c, same base, no pool: the warm pooled runner must not take it")
			job("c", arg("run"), false, "c", "--host-a="+a)

		case "c":
			a, c := arg("host-a"), host("c")
			w.True(c != a, "job c, outside the pool, ran on the pooled warm runner %s", a)
			w.Step("ok: c ran elsewhere, on " + c)
			w.Report("warm-pool: ALL PASS\n")

		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}
