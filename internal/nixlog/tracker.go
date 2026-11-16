package nixlog

import (
	"regexp"
	"strconv"
	"strings"
	"time"

	"github.com/Siddhj2206/nx/internal/storepath"
)

// Download is one store path coming from a binary cache.
type Download struct {
	Path     string
	Name     storepath.Name
	Done     int64 // compressed bytes received
	Size     int64 // compressed bytes expected, 0 until known
	Unpacked int64 // NAR size, 0 until known
	Started  time.Time
	Retries  int
}

// Build is one derivation being built locally.
type Build struct {
	Drv     string
	Name    storepath.Name
	Phase   string
	Line    string // last log line
	Started time.Time
}

// Transfer is a download that isn't a store path from a cache, such as the
// nixpkgs source tarball.
type Transfer struct {
	URL     string
	Done    int64
	Size    int64
	Started time.Time
}

// Totals are the whole operation's numbers so far.
type Totals struct {
	DownloadSize int64 // compressed bytes Nix expects to fetch
	UnpackedSize int64 // bytes they unpack to
	Downloaded   int64 // compressed bytes received so far
	Paths        int   // store paths to copy
	PathsDone    int
	Builds       int
	BuildsDone   int
}

// NoteKind says what a Note reports.
type NoteKind int

const (
	NotePlan     NoteKind = iota // Nix has worked out what it will fetch and build
	NoteFetched                  // a download finished and is in the store
	NoteBuilding                 // a build started
	NoteBuilt                    // a build finished
	NoteSource                   // a source download (not a store path) finished
	NoteRetry                    // a download failed and Nix is retrying it
	NoteError
	NoteWarning
	NoteInfo
	NoteRaw // a line that wasn't an event
)

// Note is something worth a permanent line of output.
type Note struct {
	Kind     NoteKind
	Text     string
	Download *Download
	Build    *Build
	Transfer *Transfer
	Plan     *Plan
}

// Plan is what Nix announced before starting work.
type Plan struct {
	Fetch        []storepath.Name
	FetchPaths   []string
	Build        []storepath.Name
	DownloadSize string // as Nix wrote it, e.g. "14.2 MiB"
	UnpackedSize string
}

type activity struct {
	typ    int
	parent uint64
	dl     *Download // set on substitute, copy and their file transfer
	build  *Build
	xfer   *Transfer
	url    string
}

// Tracker turns events into state and notes. It isn't safe for concurrent
// use; the caller serialises Handle and the snapshot reads.
type Tracker struct {
	acts      map[uint64]*activity
	downloads []*Download // active, in start order
	builds    []*Build
	transfers []*Transfer
	urls      map[string]*Download
	totals    Totals
	doneBytes int64  // bytes of finished downloads
	plan      *Plan  // everything announced so far
	part      *Plan  // the announcement whose path list is arriving
	planKind  string // "fetch" or "build" while a plan's path list is arriving
	now       func() time.Time
}

// NewTracker returns an empty tracker.
func NewTracker() *Tracker {
	return &Tracker{
		acts: map[uint64]*activity{},
		urls: map[string]*Download{},
		now:  time.Now,
	}
}

// Line handles one raw line and returns the notes it produced.
func (t *Tracker) Line(line []byte) []Note {
	ev, ok := Parse(line)
	if !ok {
		s := strings.TrimRight(string(line), "\r\n")
		if strings.TrimSpace(s) == "" {
			return nil
		}
		return t.flushPlan(Note{Kind: NoteRaw, Text: s})
	}
	return t.Handle(ev)
}

// Handle applies one event.
func (t *Tracker) Handle(ev Event) []Note {
	switch ev.Action {
	case "start":
		notes := t.flushPlan()
		return append(notes, t.start(ev)...)
	case "stop":
		return t.stop(ev)
	case "result":
		t.result(ev)
	case "msg":
		return t.msg(ev)
	}
	return nil
}

func (t *Tracker) start(ev Event) []Note {
	a := &activity{typ: ev.Type, parent: ev.Parent}
	t.acts[ev.ID] = a
	switch ev.Type {
	case actSubstitute:
		d := &Download{Path: ev.Str(0), Name: storepath.Parse(ev.Str(0)), Started: t.now()}
		a.dl = d
		t.downloads = append(t.downloads, d)
	case actCopyPath:
		if p := t.acts[ev.Parent]; p != nil && p.dl != nil {
			a.dl = p.dl
		}
	case actFileTransfer:
		url := ev.Str(0)
		a.url = url
		if p := t.acts[ev.Parent]; p != nil && p.dl != nil {
			a.dl = p.dl
			t.urls[url] = p.dl
			return nil
		}
		// Metadata lookups are many and quick; only real downloads show.
		if ev.Level >= lvlTalk && strings.HasSuffix(url, ".narinfo") {
			return nil
		}
		if ev.Level < lvlTalk || isSource(url) {
			x := &Transfer{URL: url, Started: t.now()}
			a.xfer = x
			t.transfers = append(t.transfers, x)
		}
	case actBuild:
		drv := ev.Str(0)
		name := storepath.Parse(drv)
		if hidden(name.Name) {
			return nil
		}
		b := &Build{Drv: drv, Name: name, Started: t.now()}
		a.build = b
		t.builds = append(t.builds, b)
		return []Note{{Kind: NoteBuilding, Build: b}}
	}
	return nil
}

// isSource says whether a transfer is a source download worth showing, such
// as a flake input tarball.
func isSource(url string) bool {
	return strings.Contains(url, "/archive/") || strings.HasSuffix(url, ".tar.gz") ||
		strings.HasSuffix(url, ".tar.xz") || strings.HasSuffix(url, ".tar.zst")
}

// hidden builds are Nix's own bookkeeping, not packages.
func hidden(name string) bool {
	return name == "profile" || name == "user-environment" || name == "buildenv"
}

func (t *Tracker) stop(ev Event) []Note {
	a := t.acts[ev.ID]
	if a == nil {
		return nil
	}
	delete(t.acts, ev.ID)
	switch {
	case a.typ == actSubstitute && a.dl != nil:
		d := a.dl
		t.downloads = remove(t.downloads, d)
		t.doneBytes += max(d.Done, d.Size)
		if d.Done < d.Size {
			d.Done = d.Size
		}
		return []Note{{Kind: NoteFetched, Download: d}}
	case a.typ == actBuild && a.build != nil:
		t.builds = remove(t.builds, a.build)
		return []Note{{Kind: NoteBuilt, Build: a.build}}
	case a.typ == actFileTransfer && a.xfer != nil:
		t.transfers = remove(t.transfers, a.xfer)
		if a.xfer.Size > 0 || a.xfer.Done > 0 {
			return []Note{{Kind: NoteSource, Transfer: a.xfer}}
		}
	case a.typ == actFileTransfer:
		delete(t.urls, a.url)
	}
	return nil
}

func (t *Tracker) result(ev Event) {
	a := t.acts[ev.ID]
	switch ev.Type {
	case resProgress:
		done, expected := ev.Int(0), ev.Int(1)
		if a == nil {
			return
		}
		switch a.typ {
		case actFileTransfer:
			if a.dl != nil {
				a.dl.Done, a.dl.Size = done, max(expected, a.dl.Size)
			} else if a.xfer != nil {
				a.xfer.Done, a.xfer.Size = done, expected
			}
		case actCopyPath:
			if a.dl != nil && expected > 0 {
				a.dl.Unpacked = expected
			}
		case actCopyPaths:
			t.totals.PathsDone, t.totals.Paths = int(done), int(expected)
		case actBuilds:
			t.totals.BuildsDone, t.totals.Builds = int(done), int(expected)
		}
	case resSetExpected:
		if a == nil || a.typ != actRealise {
			return
		}
		switch ev.Int(0) {
		case actFileTransfer:
			t.totals.DownloadSize = ev.Int(1)
		case actCopyPath:
			t.totals.UnpackedSize = ev.Int(1)
		}
	case resSetPhase:
		if a != nil && a.build != nil {
			a.build.Phase = ev.Str(0)
		}
	case resBuildLogLine:
		if a != nil && a.build != nil {
			a.build.Line = StripANSI(ev.Str(0))
		}
	}
}

var (
	planFetch = regexp.MustCompile(`^(?:these (\d+) paths|this path) will be fetched \(([^,]+) download, ([^)]+) unpacked\)`)
	planBuild = regexp.MustCompile(`^(?:these (\d+) derivations|this derivation) will be built`)
	retryURL  = regexp.MustCompile(`'(https?://[^']+)'`)
	retryCurl = regexp.MustCompile(`\(curl error: ([^)]+)\)`)
	retryHTTP = regexp.MustCompile(`HTTP error \d+`)
)

func (t *Tracker) msg(ev Event) []Note {
	text := StripANSI(ev.Msg)
	if ev.Level >= lvlTalk {
		return nil
	}
	// A plan's paths arrive one message each (older Nix: one message with
	// newlines).
	if t.planKind != "" && strings.HasPrefix(text, "  /nix/store/") {
		t.addPlanPaths(text)
		return nil
	}
	notes := t.flushPlan()
	first, rest, _ := strings.Cut(text, "\n")
	if m := planFetch.FindStringSubmatch(first); m != nil {
		t.ensurePlan()
		t.part = &Plan{DownloadSize: m[2], UnpackedSize: m[3]}
		t.plan.DownloadSize, t.plan.UnpackedSize = m[2], m[3]
		t.planKind = "fetch"
		t.addPlanPaths(rest)
		return notes
	}
	if planBuild.MatchString(first) {
		t.ensurePlan()
		t.part = &Plan{}
		t.planKind = "build"
		t.addPlanPaths(rest)
		return notes
	}
	if strings.Contains(text, "retrying in") {
		if n, ok := t.retry(text); ok {
			return append(notes, n)
		}
	}
	switch {
	case strings.HasPrefix(text, "error (ignored):"):
		// Nix recovered by itself (a busy eval cache, say); not the user's problem.
		return notes
	case strings.HasPrefix(text, "warning:"):
		return append(notes, Note{Kind: NoteWarning, Text: strings.TrimSpace(strings.TrimPrefix(text, "warning:"))})
	case ev.Level == lvlError:
		text = strings.TrimPrefix(text, "error:")
		return append(notes, Note{Kind: NoteError, Text: Condense(strings.TrimSpace(text))})
	case ev.Level == lvlWarn:
		return append(notes, Note{Kind: NoteWarning, Text: text})
	}
	return append(notes, Note{Kind: NoteInfo, Text: text})
}

func (t *Tracker) retry(text string) (Note, bool) {
	m := retryURL.FindStringSubmatch(text)
	if m == nil {
		return Note{}, false
	}
	why := "network error"
	if w := retryCurl.FindStringSubmatch(text); w != nil {
		why = w[1]
	} else if w := retryHTTP.FindString(text); w != "" {
		why = w
	}
	if d := t.urls[m[1]]; d != nil {
		d.Retries++
		return Note{Kind: NoteRetry, Download: d, Text: why}, true
	}
	return Note{Kind: NoteRetry, Text: why, Transfer: &Transfer{URL: m[1]}}, true
}

func (t *Tracker) ensurePlan() {
	if t.plan == nil {
		t.plan = &Plan{}
	}
}

func (t *Tracker) addPlanPaths(text string) {
	for _, l := range strings.Split(text, "\n") {
		p := strings.TrimSpace(l)
		if !strings.HasPrefix(p, storepath.Dir) {
			continue
		}
		for _, pl := range []*Plan{t.plan, t.part} {
			if t.planKind == "fetch" {
				pl.FetchPaths = append(pl.FetchPaths, p)
				pl.Fetch = append(pl.Fetch, storepath.Parse(p))
			} else {
				pl.Build = append(pl.Build, storepath.Parse(p))
			}
		}
	}
}

// flushPlan emits the plan once its path list has ended, then any notes
// passed in.
func (t *Tracker) flushPlan(more ...Note) []Note {
	if t.planKind == "" {
		return more
	}
	t.planKind = ""
	return append([]Note{{Kind: NotePlan, Plan: t.part}}, more...)
}

// Finish emits anything still pending at the end of the stream.
func (t *Tracker) Finish() []Note { return t.flushPlan() }

// Downloads are the active downloads, oldest first.
func (t *Tracker) Downloads() []*Download { return t.downloads }

// Builds are the active builds, oldest first.
func (t *Tracker) Builds() []*Build { return t.builds }

// Transfers are active source downloads.
func (t *Tracker) Transfers() []*Transfer { return t.transfers }

// Totals are the operation's numbers so far.
func (t *Tracker) Totals() Totals {
	tot := t.totals
	tot.Downloaded = t.doneBytes
	for _, d := range t.downloads {
		tot.Downloaded += d.Done
	}
	return tot
}

// PlanSoFar is the plan Nix announced, or nil.
func (t *Tracker) PlanSoFar() *Plan { return t.plan }

func remove[T any](s []*T, x *T) []*T {
	for i, v := range s {
		if v == x {
			return append(s[:i:i], s[i+1:]...)
		}
	}
	return s
}

// ParseSize reads a size as Nix prints it ("14.2 MiB") into bytes.
func ParseSize(s string) int64 {
	f := strings.Fields(s)
	if len(f) != 2 {
		return 0
	}
	v, err := strconv.ParseFloat(f[0], 64)
	if err != nil {
		return 0
	}
	mult := map[string]float64{"B": 1, "KiB": 1 << 10, "MiB": 1 << 20, "GiB": 1 << 30, "TiB": 1 << 40}[f[1]]
	return int64(v * mult)
}

var refusing = regexp.MustCompile(`Refusing to evaluate package '([^']+)' in \S+ because it (.+)`)

// Condense cuts a Nix error down to what went wrong: the innermost error of
// an evaluation trace, without the paragraphs on how to configure Nix.
func Condense(msg string) string {
	if i := strings.LastIndex(msg, "error: "); i >= 0 && strings.Contains(msg[:i], "…") {
		msg = msg[i+len("error: "):]
	}
	if i := strings.Index(msg, "\n\n"); i >= 0 {
		msg = msg[:i]
	}
	lines := strings.Split(msg, "\n")
	for i := range lines {
		lines[i] = strings.TrimSpace(lines[i])
	}
	msg = strings.Join(lines, "\n")
	if m := refusing.FindStringSubmatch(lines[0]); m != nil {
		why := strings.NewReplacer("‘", "", "’", "").Replace(m[2])
		return m[1] + " " + strings.TrimSuffix(why, ".")
	}
	return msg
}
