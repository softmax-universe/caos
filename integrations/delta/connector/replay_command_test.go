package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/vmihailenco/msgpack/v5"
)

func TestOfflineReplayCommandKeepsDataPrivateAndDoesNotOverwrite(t *testing.T) {
	id, timeline := replayID(1, 1), replayID(0, 1)
	head, _ := id.wireValue()
	timelineWire, _ := timeline.wireValue()
	metadata, err := encodeWire([]any{timelineWire, []any{head}})
	if err != nil {
		t.Fatal(err)
	}
	snapshot := historySnapshot{
		Timeline: timeline, Timelines: []msgpack.RawMessage{metadata},
		Download: historyDownload{CoveredBatchCount: 1, Batches: []historyDeltaBatch{
			{ID: id, Payloads: []wireVariant{replayEdit(t, "abc", 0, 0, 3)}},
		}},
	}
	dir := t.TempDir()
	input, output := filepath.Join(dir, "capture.json"), filepath.Join(dir, "report.json")
	if err := writePrivateJSON(input, snapshot); err != nil {
		t.Fatal(err)
	}
	before, err := os.ReadFile(input)
	if err != nil {
		t.Fatal(err)
	}
	args := []string{"replay-history", "--snapshot-file=" + input, "--output-file=" + output}
	if err := run(context.Background(), args); err != nil {
		t.Fatal(err)
	}
	var report struct {
		Summary    replaySummary
		ThreadText string
	}
	if err := readPrivateJSON(output, &report); err != nil {
		t.Fatal(err)
	}
	if report.ThreadText != "abc" || report.Summary.ThreadTextBytes != 3 || report.Summary.Batches != 1 {
		t.Fatalf("wrong replay report: %+v", report)
	}
	info, err := os.Stat(output)
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatalf("replay output is not private: %v", err)
	}
	if err := run(context.Background(), args); err == nil {
		t.Fatal("existing replay report was overwritten")
	}
	after, err := os.ReadFile(input)
	if err != nil || !bytes.Equal(before, after) {
		t.Fatal("offline replay modified its input")
	}
}
