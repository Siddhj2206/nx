package nix

import (
	"bufio"
	"bytes"
	"os"
	"strings"
	"testing"

	"github.com/Siddhj2206/nx/internal/ui"
)

// The permanent lines a recorded fetch leaves behind, without a terminal.
func TestViewOfRealFetch(t *testing.T) {
	var out, errb bytes.Buffer
	v := newView(ui.Plain(&out, &errb), "Resolving")
	f, err := os.Open("../../testdata/fetch-mosh.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	var res Result
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		v.notes(v.tr.Line(sc.Bytes()), &res)
		v.draw() // no terminal: draws nothing
	}
	v.notes(v.tr.Finish(), &res)
	got := errb.String()
	for _, want := range []string{
		"  › Fetching 15 packages · 14.2 MiB download, 46.5 MiB unpacked\n",
		"    ↓ mosh                   1.4.0          324 KiB\n",
		"    ↓ pcsclite (lib)         2.5.2          39 KiB\n",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("missing %q in:\n%s", want, got)
		}
	}
	if strings.Contains(got, "\x1b") || strings.Contains(got, "error") {
		t.Errorf("plain output has escapes or errors:\n%s", got)
	}
	if len(res.Fetched) != 15 || out.Len() != 0 {
		t.Errorf("fetched %d, stdout %q", len(res.Fetched), out.String())
	}
}

func TestSourceLabel(t *testing.T) {
	if got := sourceLabel("https://github.com/NixOS/nixpkgs/archive/5d2b81a7a3be3e2f832879028e4d60abbf58e9a0.tar.gz"); got != "nixpkgs 5d2b81a" {
		t.Fatal(got)
	}
}
