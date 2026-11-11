// tests/std-grep — a WORKER test of std/grep under the root contract
// (design/tool-contract.md): the scope is the object bound as `root` (a
// directory, a source tree's commit, or a file), matches come back as
// `path:linenum:line` relative to it, and every answer is a tree: `stdout`, and
// `failed` on a mistake.
package main

import (
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"caos/w"
)

var (
	server string
	client = &http.Client{Timeout: 10 * time.Minute}
	seq    int64
	// runSalt keys every tool call to THIS invocation (tests/std-files says
	// why it is not the pid).
	runSalt string
)

func run(name string, args ...string) string {
	out, err := exec.Command(name, args...).CombinedOutput()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, out)
	return strings.TrimSpace(string(out))
}

func write(path, text string) {
	w.Must(os.MkdirAll(filepath.Dir(path), 0o755))
	w.Must(os.WriteFile(path, []byte(text), 0o644))
}

// at is the object at a path inside a tree (through a source tree's code).
func at(tree, path string) string {
	dest := fmt.Sprintf("/cas/at-%d", atomic.AddInt64(&seq, 1))
	run("caos", "resolve", tree, path, dest)
	return run("caos", "hash", dest)
}

// grep runs the tool with `root` bound to an object (none for "") and returns
// its stdout and whether it failed.
func grep(root string, params ...string) (text string, failed bool) {
	n := atomic.AddInt64(&seq, 1)
	form := url.Values{}
	form.Set("base:hash", run("caos", "hash", "/cas/args/grep"))
	if root != "" {
		form.Set("root:hash", root)
	}
	form.Set("salt", fmt.Sprintf("std-grep-%s-%d", runSalt, n))
	for i := 0; i < len(params); i += 2 {
		form.Set(params[i], params[i+1])
	}
	resp, err := client.PostForm(server+"/eval/run", form)
	w.True(err == nil, "POST did not complete: %v", err)
	defer resp.Body.Close()
	body := strings.TrimSpace(string(w.Check(io.ReadAll(resp.Body))))
	w.True(resp.StatusCode == 200, "grep: HTTP %d: %s", resp.StatusCode, body)
	kind, hash, found := strings.Cut(body, " ")
	w.True(found && kind == "tree", "grep answered %q, not a result tree", body)
	dir := fmt.Sprintf("/cas/r-%d", n)
	run("caos", "get-hash", hash, dir)
	run("caos", "get", dir)
	run("caos", "get", dir+"/stdout")
	_, err = os.Lstat(dir + "/failed")
	return string(w.Check(os.ReadFile(dir + "/stdout"))), err == nil
}

// query is one grep call and, once run, its answer.
type query struct {
	root   string
	params []string
	text   string
	failed bool
}

// runAll runs every query at once. They are independent, and each is a few
// jobs deep (std/grep asks rgrep), so run one after another they were most of
// this test's time.
func runAll(qs ...*query) {
	var wg sync.WaitGroup
	for _, q := range qs {
		wg.Add(1)
		go func(q *query) {
			defer wg.Done()
			q.text, q.failed = grep(q.root, q.params...)
		}(q)
	}
	wg.Wait()
}

func q(root string, params ...string) *query { return &query{root: root, params: params} }

func ok(text string, failed bool) string {
	w.True(!failed, "grep failed: %s", text)
	return text
}

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")
		testSalt := ""
		if w.Exists("/cas/args/test-salt") {
			testSalt = strings.TrimSpace(w.Read("/cas/args/test-salt"))
		}
		runSalt = fmt.Sprintf("%s-%d-%d", testSalt, time.Now().UnixNano(), os.Getpid())

		w.Step("a tree with a source tree in it")
		write("/tmp/code/README.md", "one\ntwo\nthree\n")
		write("/tmp/code/src/lib.txt", "lib\ntwo libs\n")
		run("caos", "put", "/tmp/code", "/cas/code")
		tree := run("caos", "hash", "/cas/code")
		write("/tmp/commit", "tree "+tree+"\nauthor t <t@caos> 1 +0000\ncommitter t <t@caos> 1 +0000\n\nthe source tree\n")
		run("caos", "put-commit", "/tmp/commit", "/cas/commit")
		commit := run("caos", "hash", "/cas/commit")
		write("/tmp/ws/notes.txt", "hello\nworld\ntwo more\n")
		w.Must(os.MkdirAll("/tmp/ws/feature", 0o755))
		w.Must(os.Symlink("/cas/commit", "/tmp/ws/feature/dirty"))
		run("caos", "put", "/tmp/ws", "/cas/ws")
		ws := run("caos", "hash", "/cas/ws")

		matches := q(ws, "pattern", "two")
		inCommit := q(commit, "pattern", "two")
		inDir := q(at(ws, "feature/dirty/src"), "pattern", "two")
		inFile := q(at(ws, "notes.txt"), "pattern", "world")
		regex := q(commit, "pattern", "^(one|three)$")
		none := q(ws, "pattern", "zzz-not-there")
		badPattern := q(ws, "pattern", "(")
		noRoot := q("", "pattern", "x")
		noPattern := q(ws)
		runAll(matches, inCommit, inDir, inFile, regex, none, badPattern, noRoot, noPattern)

		w.Step("matches come back as path:line:text, through the source tree")
		text := ok(matches.text, matches.failed)
		want := "feature/dirty/README.md:2:two\nfeature/dirty/src/lib.txt:2:two libs\nnotes.txt:3:two more"
		w.True(text == want, "grep two:\n%s\nwant:\n%s", text, want)

		w.Step("the root narrows the search: a source tree, a directory, a file")
		text = ok(inCommit.text, inCommit.failed)
		w.True(text == "README.md:2:two\nsrc/lib.txt:2:two libs", "a source tree root: %q", text)
		text = ok(inDir.text, inDir.failed)
		w.True(text == "lib.txt:2:two libs", "a directory root: %q", text)
		text = ok(inFile.text, inFile.failed)
		w.True(text == "2:world", "a file root: %q", text)

		w.Step("a pattern is a regular expression")
		text = ok(regex.text, regex.failed)
		w.True(text == "README.md:1:one\nREADME.md:3:three", "alternation: %q", text)

		w.Step("no matches is an answer, not an error")
		text = ok(none.text, none.failed)
		w.True(text == "no matches", "no matches: %q", text)

		w.Step("mistakes are values the model can act on")
		w.True(badPattern.failed && strings.Contains(badPattern.text, "invalid pattern"),
			"a bad pattern: %v %q", badPattern.failed, badPattern.text)
		w.True(noRoot.failed && strings.Contains(noRoot.text, "nothing is at the root path"),
			"no root: %v %q", noRoot.failed, noRoot.text)
		w.True(noPattern.failed && strings.Contains(noPattern.text, "needs a string `pattern`"),
			"no pattern: %v %q", noPattern.failed, noPattern.text)
	})
}
