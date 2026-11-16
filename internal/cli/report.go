package cli

import (
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/Siddhj2206/nx/internal/nix"
	"github.com/Siddhj2206/nx/internal/profile"
	"github.com/Siddhj2206/nx/internal/ui"
)

type change struct {
	name, old, new string
}

// changes is what moved between two versions of the profile.
type changes struct {
	upgraded, added, removed []change
	rebuilt                  int // same version, new store path (a dependency moved)
	same                     int
}

func (c changes) empty() bool {
	return len(c.upgraded)+len(c.added)+len(c.removed) == 0
}

func diff(before, after []profile.Package) changes {
	var c changes
	old := map[string]profile.Package{}
	for _, p := range before {
		old[p.Name] = p
	}
	for _, p := range after {
		o, ok := old[p.Name]
		switch {
		case !ok:
			c.added = append(c.added, change{name: p.Name, new: p.Version})
		case o.Version != p.Version:
			c.upgraded = append(c.upgraded, change{name: p.Name, old: o.Version, new: p.Version})
		case !slices.Equal(o.StorePaths, p.StorePaths):
			c.rebuilt++
		default:
			c.same++
		}
		delete(old, p.Name)
	}
	for _, p := range before {
		if _, gone := old[p.Name]; gone {
			c.removed = append(c.removed, change{name: p.Name, old: p.Version})
		}
	}
	return c
}

// printChanges writes the ↑ + - lines, aligned.
func (a *app) printChanges(c changes) {
	s := a.u.Out
	w, ow := 0, 0
	for _, l := range [][]change{c.upgraded, c.added, c.removed} {
		for _, x := range l {
			w = max(w, len(x.name))
		}
	}
	for _, x := range c.upgraded {
		ow = max(ow, len(x.old))
	}
	for _, x := range c.upgraded {
		s.Line("%s %s  %s  %s  %s", s.Cyan(ui.SymUp), pad(x.name, w), s.Dim(pad(x.old, ow)),
			s.Cyan(ui.SymArrow), s.BoldGreen(x.new))
	}
	for _, x := range c.added {
		s.Line("%s %s  %s", s.Green(ui.SymAdd), pad(x.name, w), s.Green(x.new))
	}
	for _, x := range c.removed {
		s.Line("%s %s  %s", s.Red(ui.SymRemove), pad(x.name, w), s.Dim(x.old))
	}
}

func pad(s string, w int) string {
	if len(s) >= w {
		return s
	}
	return s + strings.Repeat(" ", w-len(s))
}

// report prints what a command changed, then one summary line.
func (a *app) report(c changes, res *nix.Result, none string) {
	s := a.u.Out
	if c.empty() {
		s.Blank()
		a.u.Done("%s %s", none, s.Dim(fmt.Sprintf("(%s)", ui.Plural(c.same+c.rebuilt, "package"))))
		return
	}
	s.Blank()
	a.printChanges(c)
	s.Blank()
	var parts []string
	if n := len(c.upgraded); n > 0 {
		parts = append(parts, fmt.Sprintf("%d upgraded", n))
	}
	if n := len(c.added); n > 0 {
		parts = append(parts, fmt.Sprintf("%d added", n))
	}
	if n := len(c.removed); n > 0 {
		parts = append(parts, fmt.Sprintf("%d removed", n))
	}
	tail := []string{}
	if c.rebuilt > 0 {
		tail = append(tail, fmt.Sprintf("%d rebuilt", c.rebuilt))
	}
	tail = append(tail, fmt.Sprintf("%d unchanged", c.same))
	if res != nil && (len(res.Fetched) > 0 || len(res.Built) > 0) {
		tail = append(tail, fmt.Sprintf("%s in %s", ui.Size(res.Downloaded()), roundDur(res.Elapsed)))
	}
	a.u.Done("%s %s", strings.Join(parts, ", "), s.Dim("· "+strings.Join(tail, " · ")))
}

func roundDur(d time.Duration) string {
	if d < 10*time.Second {
		return fmt.Sprintf("%.1fs", d.Seconds())
	}
	return d.Round(time.Second).String()
}
