// Evaluating connector builds its image and runs its Go auth, wire, transport,
// and credential-file tests in Nix. No real account or sign-in is used here.
package main

import (
	"os"

	"caos/w"
)

func main() {
	w.Main(func() {
		w.Step("build the standalone Delta connector and run its unit tests")
		const image = "/cas/args/connector"
		w.Get(image)
		w.True(w.Check(os.Stat(image)).IsDir(), "connector did not evaluate to an image")
		w.True(w.Exists(image+"/.caos-curry"), "connector image is not curried")
		w.True(w.Exists(image+"/base"), "connector image has no built base")
		w.Report("delta-protocol: connector image and Go unit tests built successfully\n")
	})
}
