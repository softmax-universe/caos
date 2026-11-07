// Package w is the worker prelude: an ERROR POLICY over os and
// github.com/bitfield/script, not a replacement for either. Its whole job is
// to give a Go worker script the default `set -euo pipefail` gives a bash one
// — any failure ends the script, loudly, naming the thing that failed.
package w

import (
	"fmt"
	"os"
	"strings"

	"github.com/bitfield/script"
)

type failed struct{ msg string }

// infra is a failure of the ENVIRONMENT, not of the thing under test.
type infra struct{ msg string }

// InfraMarker opens the line Infra prints. dev/run-test looks for it in a
// failed test's error text and turns that test into a job error, uncached,
// rather than a FAIL verdict, which caches. It is the marker and not the exit
// code that carries the classification: a sub-run's failure reaches its caller
// as the callee's text, and only the marker survives that.
const InfraMarker = "RUN-TEST INFRA:"

// InfraExit is the exit status Infra ends the worker with: not 1, which is
// `True`'s, so the two read apart in a runner log as well.
const InfraExit = 70

// Out runs a pipe and returns its output, failing on error with the command's
// OWN output as the diagnostic. A sink returns the output ALONGSIDE the error
// — script interleaves stderr into the pipe — so the failing command's message
// is already in hand. Do NOT re-read the pipe to get it: it is closed by then
// and answers "io: read/write on closed pipe".
func Out(p *script.Pipe) string {
	s, err := p.String()
	if err != nil {
		panic(failed{fmt.Sprintf("%v: %s", err, strings.TrimSpace(s))})
	}
	return strings.TrimRight(s, "\n")
}

// Do runs a pipe for its effect.
func Do(p *script.Pipe) { Out(p) }

// Try runs a pipe WITHOUT failing, returning its output and exit status. This
// is how a script distinguishes ABSENT from BROKEN (AGENTS.md): a lookup whose
// "absent" answer is a non-zero exit must not die on it.
func Try(p *script.Pipe) (string, int) {
	s, _ := p.String()
	return strings.TrimRight(s, "\n"), p.ExitStatus()
}

// Check unwraps any (T, error) pair.
func Check[T any](v T, err error) T {
	if err != nil {
		panic(failed{err.Error()})
	}
	return v
}

// Must unwraps a bare error.
func Must(err error) {
	if err != nil {
		panic(failed{err.Error()})
	}
}

// True asserts a claim.
func True(ok bool, format string, a ...any) {
	if !ok {
		panic(failed{fmt.Sprintf(format, a...)})
	}
}

// Infra ends the worker as an INFRASTRUCTURE failure: something the test
// relied on (GitHub, the network, a runner) broke, so the test reached no
// verdict. Use it where a test can tell which broke; `True` and `Must` stay the
// test's own FAIL. Getting the split wrong costs a re-run, never a false green.
func Infra(format string, a ...any) {
	panic(infra{fmt.Sprintf(format, a...)})
}

// Step announces a group of claims on stderr; stdout belongs to the result.
func Step(heading string) { fmt.Fprintf(os.Stderr, "== %s ==\n", heading) }

// Report writes the worker's result to /cas/out.
func Report(body string) {
	const path = "/tmp/report"
	Must(os.WriteFile(path, []byte(body), 0o644))
	fmt.Fprint(os.Stderr, body)
	Do(script.Exec("caos put " + path + " /cas/out"))
}

// Main gives the script `set -e`'s default: a failed claim prints `FAIL:` and
// exits 1, an Infra prints InfraMarker and exits InfraExit.
func Main(body func()) {
	defer func() {
		r := recover()
		if r == nil {
			return
		}
		switch f := r.(type) {
		case failed:
			fmt.Fprintln(os.Stderr, "FAIL: "+f.msg)
			os.Exit(1)
		case infra:
			fmt.Fprintln(os.Stderr, InfraMarker+" "+f.msg)
			os.Exit(InfraExit)
		default:
			panic(r)
		}
	}()
	body()
}
