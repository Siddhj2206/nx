package nixlog

import (
	"bufio"
	"os"
	"strings"
	"testing"
)

func feed(t *testing.T, tr *Tracker, path string) []Note {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	var notes []Note
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	for sc.Scan() {
		notes = append(notes, tr.Line(sc.Bytes())...)
	}
	return append(notes, tr.Finish()...)
}

func kinds(notes []Note, k NoteKind) []Note {
	var out []Note
	for _, n := range notes {
		if n.Kind == k {
			out = append(out, n)
		}
	}
	return out
}

// A real `nix profile add` of tk, lua and mosh: fifteen paths from the cache.
func TestRealFetch(t *testing.T) {
	tr := NewTracker()
	notes := feed(t, tr, "../../testdata/fetch-mosh.jsonl")

	plans := kinds(notes, NotePlan)
	if len(plans) != 1 {
		t.Fatalf("plans = %d, want 1", len(plans))
	}
	p := plans[0].Plan
	if len(p.Fetch) != 15 || p.DownloadSize != "14.2 MiB" || p.UnpackedSize != "46.5 MiB" {
		t.Fatalf("plan = %d paths, %s, %s", len(p.Fetch), p.DownloadSize, p.UnpackedSize)
	}

	fetched := kinds(notes, NoteFetched)
	if len(fetched) != 15 {
		t.Fatalf("fetched = %d, want 15", len(fetched))
	}
	var sum int64
	for _, n := range fetched {
		if n.Download.Size == 0 || n.Download.Unpacked == 0 {
			t.Errorf("%s: size %d, unpacked %d", n.Download.Name.Name, n.Download.Size, n.Download.Unpacked)
		}
		sum += n.Download.Size
	}
	tot := tr.Totals()
	if tot.DownloadSize == 0 || tot.Downloaded != sum {
		t.Errorf("totals = %+v, sum of downloads %d", tot, sum)
	}
	if tot.Paths != 15 || tot.PathsDone != 15 {
		t.Errorf("paths %d/%d", tot.PathsDone, tot.Paths)
	}
	if len(tr.Downloads()) != 0 {
		t.Errorf("%d downloads still active", len(tr.Downloads()))
	}
	// The ignored eval-cache error is Nix's business, not the user's.
	if e := kinds(notes, NoteError); len(e) != 0 {
		t.Errorf("errors = %+v", e)
	}
}

func TestFixtureBuildAndErrors(t *testing.T) {
	tr := NewTracker()
	notes := feed(t, tr, "../../testdata/nix-internal-log.jsonl")
	if b := kinds(notes, NoteBuilding); len(b) != 1 || b[0].Build.Name.Name != "lolcat" {
		t.Fatalf("building = %+v", b)
	}
	e := kinds(notes, NoteError)
	if len(e) != 1 || e[0].Text != "builder for lolcat failed" {
		t.Fatalf("errors = %+v", e)
	}
	if r := kinds(notes, NoteRaw); len(r) != 1 || !strings.HasPrefix(r[0].Text, "a plain line") {
		t.Fatalf("raw = %+v", r)
	}
	f := kinds(notes, NoteFetched)
	if len(f) != 1 || f[0].Download.Name.Name != "figlet" || f[0].Download.Unpacked != 6156320 {
		t.Fatalf("fetched = %+v", f)
	}
}

func TestRetryNamesThePackage(t *testing.T) {
	tr := NewTracker()
	lines := []string{
		`@nix {"action":"start","fields":["/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0","https://cache.nixos.org"],"id":1,"level":0,"parent":0,"text":"","type":108}`,
		`@nix {"action":"start","fields":["/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0","https://cache.nixos.org","local"],"id":2,"level":3,"parent":1,"text":"","type":100}`,
		`@nix {"action":"start","fields":["https://cache.nixos.org/nar/abc.nar.zst"],"id":3,"level":4,"parent":2,"text":"","type":101}`,
		`@nix {"action":"msg","level":0,"msg":"\u001b[35;1mwarning:\u001b[0m unable to download 'https://cache.nixos.org/nar/abc.nar.zst': HTTP error 200 (curl error: Stream error in the HTTP/2 framing layer); retrying in 268 ms"}`,
	}
	var notes []Note
	for _, l := range lines {
		notes = append(notes, tr.Line([]byte(l))...)
	}
	if len(notes) != 1 || notes[0].Kind != NoteRetry {
		t.Fatalf("notes = %+v", notes)
	}
	if notes[0].Download.Name.Name != "ripgrep" || notes[0].Text != "Stream error in the HTTP/2 framing layer" {
		t.Fatalf("retry = %+v %q", notes[0].Download.Name, notes[0].Text)
	}
}

func TestWarningAtErrorLevel(t *testing.T) {
	tr := NewTracker()
	n := tr.Line([]byte(`@nix {"action":"msg","level":0,"msg":"warning: something odd"}`))
	if len(n) != 1 || n[0].Kind != NoteWarning || n[0].Text != "something odd" {
		t.Fatalf("%+v", n)
	}
}

func TestParseSize(t *testing.T) {
	if got := ParseSize("14.2 MiB"); got != 14889779 {
		t.Fatal(got)
	}
}

func TestCondenseUnfree(t *testing.T) {
	msg := "… in the condition of the assert statement\n         at «github:NixOS/nixpkgs/x»/lib/customisation.nix:421:13:\n\n       error: Refusing to evaluate package 'unrar-7.3.2' in /nix/store/xxf-source/pkgs/by-name/un/unrar/package.nix:58 because it has an unfree license (‘unfreeRedistributable’)\n       a) To temporarily allow unfree packages\n\n more"
	if got := Condense(msg); got != "unrar-7.3.2 has an unfree license (unfreeRedistributable)" {
		t.Fatal(got)
	}
	if got := Condense("attribute 'nope' missing\n\n       at x"); got != "attribute 'nope' missing" {
		t.Fatal(got)
	}
}

func TestPlansAnnouncedSeparately(t *testing.T) {
	tr := NewTracker()
	var notes []Note
	for _, l := range []string{
		`@nix {"action":"msg","level":3,"msg":"these 2 derivations will be built:"}`,
		`@nix {"action":"msg","level":3,"msg":"  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source.drv"}`,
		`@nix {"action":"msg","level":3,"msg":"  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab-unrar-7.3.2.drv"}`,
		`@nix {"action":"msg","level":3,"msg":"this path will be fetched (1.0 MiB download, 2.0 MiB unpacked):"}`,
		`@nix {"action":"msg","level":3,"msg":"  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac-curl-8.23.0"}`,
	} {
		notes = append(notes, tr.Line([]byte(l))...)
	}
	notes = append(notes, tr.Finish()...)
	if len(notes) != 2 || len(notes[0].Plan.Build) != 2 || len(notes[0].Plan.Fetch) != 0 ||
		len(notes[1].Plan.Fetch) != 1 || len(notes[1].Plan.Build) != 0 {
		t.Fatalf("%+v", notes)
	}
	if p := tr.PlanSoFar(); len(p.Build) != 2 || len(p.Fetch) != 1 {
		t.Fatalf("whole plan %+v", p)
	}
}
