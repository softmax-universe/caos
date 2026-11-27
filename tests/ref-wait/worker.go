// tests/ref-wait — a WORKER test: no client, no repo.
//
// Exercises the HTTP ref surface (`GET /ref`, `POST /ref`, `GET /ref/wait`,
// `POST /ref/wait`) directly with net/http:
//   - immediate return when a ref already differs from `expected`
//   - `unchanged` / `{"changed":{}}` on timeout when nothing moves
//   - immediate condvar wakeup when a concurrent `POST /ref` moves a watched ref
//   - deletion (`absent`) delivered cleanly to a waiter
//   - multi-ref `POST /ref/wait` waking on one watched ref while ignoring an
//     unwatched ref's update
//   - strict 400 validation on malformed / unknown parameters and fields
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"regexp"
	"strings"
	"time"

	"caos/w"
)

var (
	server string
	client = &http.Client{Timeout: 2 * time.Minute}
)

func escape(s string) string {
	return strings.ReplaceAll(url.QueryEscape(s), "+", "%20")
}

func call(method, path, contentType, body string) (int, string) {
	var reader io.Reader
	if body != "" {
		reader = strings.NewReader(body)
	}
	req := w.Check(http.NewRequest(method, server+path, reader))
	if contentType != "" {
		req.Header.Set("Content-Type", contentType)
	}
	resp, err := client.Do(req)
	w.True(err == nil, "%s %s did not complete: %v", method, path, err)
	defer resp.Body.Close()
	text := string(w.Check(io.ReadAll(resp.Body)))
	return resp.StatusCode, strings.TrimSpace(text)
}

func mintCommit(nonce, label string) string {
	dir := "/tmp/empty-" + label
	w.Must(os.MkdirAll(dir, 0o755))
	w.Put(dir, "/cas/empty-"+label)
	body := fmt.Sprintf("tree %s\nauthor ref-wait <ref-wait@caos> 0 +0000\n"+
		"committer ref-wait <ref-wait@caos> 0 +0000\n\nref-wait %s %s\n",
		w.Hash("/cas/empty-"+label), nonce, label)
	commitFile := "/tmp/commit-" + label
	w.Must(os.WriteFile(commitFile, []byte(body), 0o644))
	casPath := "/cas/commit-" + label
	w.Caos("put-commit", commitFile, casPath)
	return w.Hash(casPath)
}

func setRef(name, value, expected string) (int, string) {
	payload := map[string]any{"name": name, "value": value}
	if expected != "" {
		payload["expected"] = expected
	}
	raw := string(w.Check(json.Marshal(payload)))
	return call("POST", "/ref", "application/json", raw)
}

func nonceFor(salt string) string {
	raw := fmt.Sprintf("%s-%d-%d", salt, time.Now().UnixNano(), os.Getpid())
	return regexp.MustCompile(`[^A-Za-z0-9-]+`).ReplaceAllString(raw, "-")
}

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")

		salt := ""
		if w.Exists("/cas/args/test-salt") {
			salt = strings.TrimSpace(w.Read("/cas/args/test-salt"))
		}
		nonce := nonceFor(salt)
		ref := func(leaf string) string {
			return "refs/heads/tests/ref-wait/" + nonce + "/" + leaf
		}
		r1 := ref("one")
		r2 := ref("two")
		noise := ref("noise")

		defer func() {
			for _, r := range []string{r1, r2, noise} {
				_, _ = setRef(r, "absent", "")
			}
		}()

		cA := mintCommit(nonce, "a")
		cB := mintCommit(nonce, "b")

		w.Step("GET /ref/wait reports `unchanged` on timeout when nothing moves")
		code, body := call("GET", "/ref/wait?name="+escape(r1)+"&expected=absent&timeout=1", "", "")
		w.True(code == 200 && body == "unchanged", "expected 200 unchanged, got %d: %q", code, body)

		w.Step("GET /ref/wait wakes immediately when POST /ref moves the ref")
		done := make(chan error, 1)
		go func() {
			time.Sleep(120 * time.Millisecond)
			c, b := setRef(r1, cA, "absent")
			if c != 200 {
				done <- fmt.Errorf("POST /ref returned %d: %s", c, b)
				return
			}
			done <- nil
		}()
		started := time.Now()
		code, body = call("GET", "/ref/wait?name="+escape(r1)+"&expected=absent&timeout=30", "", "")
		w.Must(<-done)
		w.True(code == 200 && body == cA, "expected 200 %s, got %d: %q", cA, code, body)
		w.True(time.Since(started) < 5*time.Second, "wait took %v; expected immediate wakeup", time.Since(started))

		w.Step("GET /ref/wait returns immediately when the ref already differs from `expected`")
		started = time.Now()
		code, body = call("GET", "/ref/wait?name="+escape(r1)+"&expected=absent&timeout=30", "", "")
		w.True(code == 200 && body == cA, "expected immediate %s, got %d: %q", cA, code, body)
		w.True(time.Since(started) < 2*time.Second, "already-different wait blocked for %v", time.Since(started))

		w.Step("POST /ref/wait watches multiple refs on one stream and ignores unwatched writes")
		go func() {
			time.Sleep(80 * time.Millisecond)
			c, b := setRef(noise, cB, "absent")
			if c != 200 {
				done <- fmt.Errorf("noise POST /ref returned %d: %s", c, b)
				return
			}
			time.Sleep(100 * time.Millisecond)
			c, b = setRef(r2, cB, "absent")
			if c != 200 {
				done <- fmt.Errorf("r2 POST /ref returned %d: %s", c, b)
				return
			}
			done <- nil
		}()
		waitReq := string(w.Check(json.Marshal(map[string]any{
			"refs":    map[string]string{r1: cA, r2: "absent"},
			"timeout": 30,
		})))
		code, body = call("POST", "/ref/wait", "application/json", waitReq)
		w.Must(<-done)
		w.True(code == 200, "POST /ref/wait returned %d: %s", code, body)
		var parsed struct {
			Changed map[string]string `json:"changed"`
		}
		w.Must(json.Unmarshal([]byte(body), &parsed))
		w.True(len(parsed.Changed) == 1 && parsed.Changed[r2] == cB,
			"expected only %s=%s in changed map, got %v", r2, cB, parsed.Changed)

		w.Step("deleting a watched ref delivers `absent`")
		go func() {
			time.Sleep(100 * time.Millisecond)
			c, b := setRef(r1, "absent", cA)
			if c != 200 {
				done <- fmt.Errorf("delete POST /ref returned %d: %s", c, b)
				return
			}
			done <- nil
		}()
		code, body = call("GET", "/ref/wait?name="+escape(r1)+"&expected="+cA+"&timeout=30", "", "")
		w.Must(<-done)
		w.True(code == 200 && body == "absent", "expected 200 absent, got %d: %q", code, body)

		w.Step("malformed or unknown parameters and fields are refused with 400")
		for _, badQuery := range []string{
			"",
			"name=" + escape(r1) + "&expect=absent",
			"name=" + escape(r1) + "&name=" + escape(r2),
			"name=" + escape(r1) + "&timeout=0",
			"name=refs/keys/secret",
		} {
			c, b := call("GET", "/ref/wait?"+badQuery, "", "")
			w.True(c == 400, "GET /ref/wait?%s: expected 400, got %d: %s", badQuery, c, b)
		}
		for _, badBody := range []string{
			`{}`,
			`{"refs":{}}`,
			`{"refs":{"` + r1 + `":"absent"},"unknown":1}`,
			`{"refs":{"refs/keys/secret":"absent"}}`,
			`{"refs":{"` + r1 + `":"absent"},"timeout":0}`,
		} {
			c, b := call("POST", "/ref/wait", "application/json", badBody)
			w.True(c == 400, "POST /ref/wait %s: expected 400, got %d: %s", badBody, c, b)
		}

		w.Report("ref-wait: ALL PASS\n")
	})
}
