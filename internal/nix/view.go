package nix

import (
	"fmt"
	"strings"
	"time"

	"github.com/Siddhj2206/nx/internal/nixlog"
	"github.com/Siddhj2206/nx/internal/storepath"
	"github.com/Siddhj2206/nx/internal/ui"
)

// view draws a run: one permanent line per package as it lands, and below
// them a live block with the downloads in flight and a total bar.
type view struct {
	u       *ui.UI
	s       *ui.Stream
	live    *ui.Live
	tr      *nixlog.Tracker
	label   string
	nameW   int
	verW    int
	frame   int
	samples []sample // recent byte counts, for the speed
}

type sample struct {
	at    time.Time
	bytes int64
}

const maxActive = 5

func newView(u *ui.UI, label string) *view {
	return &view{
		u: u, s: u.Err, live: ui.NewLive(u.Err), tr: nixlog.NewTracker(),
		label: label, nameW: 22, verW: 14,
	}
}

func (v *view) notes(notes []nixlog.Note, res *Result) {
	for _, n := range notes {
		switch n.Kind {
		case nixlog.NotePlan:
			v.plan(n.Plan)
		case nixlog.NoteFetched:
			res.Fetched = append(res.Fetched, n.Download)
			v.live.Print(v.packageLine(v.s.Green(ui.SymFetch), n.Download.Name, v.s.Dim(ui.Size(n.Download.Size))))
		case nixlog.NoteBuilt:
			res.Built = append(res.Built, n.Build)
			took := time.Since(n.Build.Started).Round(time.Second)
			v.live.Print(v.packageLine(v.s.Cyan(ui.SymBuild), n.Build.Name, v.s.Dim("built in "+took.String())))
		case nixlog.NoteSource:
			v.live.Print(fmt.Sprintf("    %s %s %s", v.s.Green(ui.SymFetch),
				ui.Pad(sourceLabel(n.Transfer.URL), v.nameW+v.verW+1), v.s.Dim(ui.Size(max(n.Transfer.Size, n.Transfer.Done)))))
		case nixlog.NoteRetry:
			what := "a download"
			if n.Download != nil {
				what = n.Download.Name.Label()
			}
			v.live.Print(fmt.Sprintf("    %s %s", v.s.Yellow(ui.SymWarn),
				v.s.Dim("retrying "+what+" · "+n.Text)))
		case nixlog.NoteError:
			v.live.Print(fmt.Sprintf("  %s %s", v.s.Red(ui.SymError), indent(n.Text)))
		case nixlog.NoteWarning:
			v.live.Print(fmt.Sprintf("  %s %s", v.s.Yellow(ui.SymWarn), indent(n.Text)))
		case nixlog.NoteInfo:
			if quiet(n.Text) {
				continue
			}
			v.live.Print("  " + v.s.Dim(indent(n.Text)))
		case nixlog.NoteRaw:
			v.live.Print(n.Text)
		}
	}
}

// quiet drops Nix messages nx says better itself.
func quiet(text string) bool {
	for _, p := range []string{"upgrading '", "copying path", "building '", "evaluating", "removing '", "removed ", "no packages to", "switching profile"} {
		if strings.HasPrefix(text, p) {
			return true
		}
	}
	return false
}

func indent(s string) string { return strings.ReplaceAll(s, "\n", "\n    ") }

func (v *view) plan(p *nixlog.Plan) {
	for _, n := range append(p.Fetch, p.Build...) {
		v.nameW = max(v.nameW, min(len(n.Label()), 30))
		v.verW = max(v.verW, min(len(n.Version), 20))
	}
	if len(p.Fetch) > 0 {
		v.live.Print(fmt.Sprintf("  %s Fetching %s %s", v.s.Cyan(ui.SymStep),
			ui.Plural(len(p.Fetch), "package"),
			v.s.Dim(fmt.Sprintf("· %s download, %s unpacked", p.DownloadSize, p.UnpackedSize))))
	}
	if len(p.Build) > 0 {
		v.live.Print(fmt.Sprintf("  %s Building %s from source", v.s.Cyan(ui.SymStep),
			ui.Plural(len(p.Build), "package")))
	}
}

func (v *view) packageLine(sym string, n storepath.Name, note string) string {
	return fmt.Sprintf("    %s %s %s %s", sym,
		ui.Pad(ui.Truncate(n.Label(), v.nameW), v.nameW),
		ui.Pad(v.s.Dim(ui.Truncate(n.Version, v.verW)), v.verW), note)
}

var spinner = []string{"⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"}

func (v *view) draw() {
	if !v.s.TTY {
		return
	}
	v.frame++
	spin := v.s.Cyan(spinner[v.frame%len(spinner)])
	var lines []string
	downloads := v.tr.Downloads()
	for i, d := range downloads {
		if i == maxActive {
			lines = append(lines, v.s.Dim(fmt.Sprintf("      and %d more", len(downloads)-maxActive)))
			break
		}
		progress := v.s.Dim("waiting")
		if d.Size > 0 {
			progress = v.s.Bar(d.Done, d.Size, 16) + " " + v.s.Dim(fmt.Sprintf("%s / %s", ui.Size(d.Done), ui.Size(d.Size)))
		}
		lines = append(lines, v.packageLine(spin, d.Name, progress))
	}
	for _, b := range v.tr.Builds() {
		what := b.Phase
		if what == "" {
			what = "building"
		}
		if b.Line != "" {
			what += " · " + strings.TrimSpace(b.Line)
		}
		took := time.Since(b.Started).Round(time.Second)
		lines = append(lines, v.packageLine(spin, b.Name, v.s.Dim(took.String()+" "+what)))
	}
	for _, x := range v.tr.Transfers() {
		progress := v.s.Dim(ui.Size(x.Done))
		if x.Size > 0 {
			progress = v.s.Bar(x.Done, x.Size, 16) + " " + v.s.Dim(fmt.Sprintf("%s / %s", ui.Size(x.Done), ui.Size(x.Size)))
		}
		lines = append(lines, fmt.Sprintf("    %s %s %s", spin, ui.Pad(sourceLabel(x.URL), v.nameW+v.verW+1), progress))
	}
	if t := v.tr.Totals(); t.Paths > 0 || t.DownloadSize > 0 || t.Builds > 0 {
		lines = append(lines, v.totalLine(t))
	} else if len(lines) == 0 && v.label != "" {
		lines = append(lines, fmt.Sprintf("  %s %s", spin, v.label))
	}
	v.live.Draw(lines)
}

func (v *view) totalLine(t nixlog.Totals) string {
	now := time.Now()
	v.samples = append(v.samples, sample{now, t.Downloaded})
	for len(v.samples) > 2 && now.Sub(v.samples[0].at) > 3*time.Second {
		v.samples = v.samples[1:]
	}
	parts := []string{}
	if t.Paths > 0 {
		parts = append(parts, fmt.Sprintf("%d/%d fetched", t.PathsDone, t.Paths))
	}
	if t.Builds > 0 {
		parts = append(parts, fmt.Sprintf("%d/%d built", t.BuildsDone, t.Builds))
	}
	if t.DownloadSize > 0 {
		parts = append(parts, fmt.Sprintf("%s / %s", ui.Size(t.Downloaded), ui.Size(t.DownloadSize)))
	}
	if first := v.samples[0]; now.Sub(first.at) > 500*time.Millisecond {
		rate := float64(t.Downloaded-first.bytes) / now.Sub(first.at).Seconds()
		if rate > 0 {
			parts = append(parts, ui.Size(int64(rate))+"/s")
		}
	}
	// The bar counts finished work: bytes while downloading, then builds.
	done, total := t.Downloaded, t.DownloadSize
	if total == 0 || (done >= total && t.Builds > t.BuildsDone) {
		done, total = int64(t.PathsDone+t.BuildsDone), int64(t.Paths+t.Builds)
	}
	return fmt.Sprintf("  %s %s", v.s.Bar(done, total, 30), v.s.Dim(strings.Join(parts, " · ")))
}

// sourceLabel names a source download: "nixpkgs 5d2b81a" for a GitHub
// tarball of nixpkgs, else the URL's last part.
func sourceLabel(url string) string {
	if i := strings.Index(url, "github.com/"); i >= 0 {
		parts := strings.Split(url[i+len("github.com/"):], "/")
		if len(parts) >= 4 && parts[2] == "archive" {
			rev := strings.TrimSuffix(parts[3], ".tar.gz")
			if len(rev) > 7 {
				rev = rev[:7]
			}
			return parts[1] + " " + rev
		}
	}
	if i := strings.LastIndexByte(url, '/'); i >= 0 {
		return url[i+1:]
	}
	return url
}
