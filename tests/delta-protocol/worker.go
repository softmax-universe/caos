// Evaluating connector builds its image and runs its Go auth, wire, transport,
// and credential-file tests in Nix. No real account or sign-in is used here.
package main

import (
	"os"
	"strings"

	"caos/w"
)

func main() {
	w.Main(func() {
		if w.Exists("/cas/args/stage") {
			const result = "/cas/args/result"
			w.Get(result)
			w.True(w.Exists(result+"/failed"), "unsupported operation was not a failed result")
			w.True(strings.Contains(w.Read(result+"/stdout"), "unsupported Delta worker operation"),
				"unsupported operation did not return its diagnostic")
			w.Report("delta-protocol: image/unit tests built; expected errors return failed values\n")
			return
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
		next := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
			"--stage=verify-error"}
		if w.Exists("/cas/args/test-salt") {
			next = append(next, "--test-salt:@=/cas/args/test-salt")
		}
		then := w.Caos(next...)
		w.Caos("run-request-then", request, "--then:hash="+then)
	})
}
