package cli

import (
	"fmt"
	"os/exec"
	"regexp"
	"strings"
	"time"

	"github.com/Siddhj2206/nx/internal/catalog"
	"github.com/Siddhj2206/nx/internal/nix"
	"github.com/Siddhj2206/nx/internal/nixlog"
	"github.com/Siddhj2206/nx/internal/profile"
	"github.com/Siddhj2206/nx/internal/ui"
)

func (a *app) upgrade(args []string) error {
	if len(args) > 0 {
		return misuse("upgrade takes no names; it moves every package to the newest nixpkgs")
	}
	before, err := profile.Read()
	if err != nil {
		return err
	}
	old, hadPin := profile.ReadPin()
	a.u.Step("Checking for a newer nixpkgs")
	rel, err := catalog.Latest(a.ctx, a.http)
	if err != nil {
		return err
	}
	s := a.u.Out
	moved := !hadPin || old.Rev != rel.Rev
	// The catalog downloads while nix fetches the new nixpkgs.
	catDone := make(chan error, 1)
	if _, err := catalog.Open(rel.Rev); err == nil {
		catDone <- nil
	} else {
		go func() {
			_, err := catalog.Download(a.ctx, a.http, rel, nil)
			catDone <- err
		}()
	}
	if moved {
		if err := a.setPin(rel.Rev); err != nil {
			return err
		}
	}
	pin, _ := profile.ReadPin()
	if !moved {
		s.Line("%s  %s %s", s.Bold("nixpkgs"), pin.Short(), s.Dim(fmt.Sprintf("(%s, already the newest)", pin.Modified.Format("Jan 02"))))
	} else if hadPin {
		s.Line("%s  %s %s %s", s.Bold("nixpkgs"),
			s.Dim(fmt.Sprintf("%s (%s)", old.Short(), old.Modified.Format("Jan 02"))), s.Cyan(ui.SymArrow),
			s.BoldGreen(pin.Short())+" "+s.Dim("("+pin.Modified.Format("Jan 02")+")"))
	}
	if err := <-catDone; err != nil {
		a.u.Warn("package catalog: %s", err)
	} else {
		catalog.Prune(rel.Rev)
	}
	// Every element already built from the pin means there is nothing to do;
	// otherwise finish the upgrade, even one a previous run left half done.
	if upToDate(before, pin.Rev) {
		s.Blank()
		a.u.Done("Everything is up to date %s", s.Dim(fmt.Sprintf("(%s)", ui.Plural(len(before), "package"))))
		return nil
	}
	// Unfree packages already installed must upgrade too; evaluation goes
	// impure (and uncached) only when there are some.
	var unfree nix.Unfree
	if cat, err := catalog.Open(rel.Rev); err == nil {
		a.previewUpgrade(before, cat)
		for _, p := range before {
			if c, ok := cat.Get(p.Attr); ok && c.Unfree() {
				unfree = true
			}
		}
	}
	res, runErr := nix.RunWith(a.ctx, a.u, "Evaluating "+ui.Plural(len(before), "package"), unfree, "profile", "upgrade", "--all")
	after, err := profile.Read()
	if err != nil {
		return err
	}
	if c := diff(before, after); runErr == nil || !c.empty() {
		a.report(c, &res, "Everything is up to date")
	}
	return runErr
}

// upToDate says whether every flake:nixpkgs element was built from rev.
func upToDate(pkgs []profile.Package, rev string) bool {
	for _, p := range pkgs {
		if p.Source == "flake:nixpkgs" && p.Rev != rev {
			return false
		}
	}
	return true
}

// previewUpgrade says which versions will move before any downloading
// starts, from the catalog alone.
func (a *app) previewUpgrade(installed []profile.Package, cat *catalog.Catalog) {
	var c changes
	for _, p := range installed {
		if n, ok := cat.Get(p.Attr); ok && n.Version != p.Version {
			c.upgraded = append(c.upgraded, change{name: p.Name, old: p.Version, new: n.Version})
		}
	}
	if len(c.upgraded) == 0 {
		return
	}
	a.u.Step("Upgrading %s", ui.Plural(len(c.upgraded), "package"))
}

func (a *app) outdated(args []string) error {
	installed, err := profile.Read()
	if err != nil {
		return err
	}
	pin, _ := profile.ReadPin()
	rel, err := catalog.Latest(a.ctx, a.http)
	if err != nil {
		return err
	}
	cat, err := catalog.Open(rel.Rev)
	if err != nil {
		if err := a.downloadCatalog(rel); err != nil {
			return err
		}
		catalog.Prune(pin.Rev, rel.Rev)
		if cat, err = catalog.Open(rel.Rev); err != nil {
			return err
		}
	}
	var c changes
	for _, p := range installed {
		if n, ok := cat.Get(p.Attr); ok && n.Version != p.Version {
			c.upgraded = append(c.upgraded, change{name: p.Name, old: p.Version, new: n.Version})
		}
	}
	s := a.u.Out
	if len(c.upgraded) == 0 {
		if rel.Rev == pin.Rev {
			a.u.Done("Everything is up to date %s", s.Dim("(nixpkgs "+pin.Short()+" is the newest)"))
		} else {
			a.u.Done("No new versions %s", s.Dim(fmt.Sprintf("· nixpkgs %s has rebuilds; nx upgrade picks them up", rel.Short())))
		}
		return nil
	}
	a.printChanges(c)
	s.Blank()
	s.Line("%s", s.Dim(fmt.Sprintf("%s in nixpkgs %s · nx upgrade installs them",
		ui.Plural(len(c.upgraded), "new version"), rel.Short())))
	return nil
}

func (a *app) rollback(args []string) error {
	before, err := profile.Read()
	if err != nil {
		return err
	}
	res, runErr := nix.Run(a.ctx, a.u, "Rolling back", "profile", "rollback")
	after, err := profile.Read()
	if err != nil {
		return err
	}
	if c := diff(before, after); runErr == nil || !c.empty() {
		a.report(c, &res, "Nothing to undo")
	}
	return runErr
}

func (a *app) history(args []string) error {
	gens, err := profile.Generations()
	if err != nil {
		return err
	}
	s := a.u.Out
	if len(gens) == 0 {
		s.Line("No history yet.")
		return nil
	}
	var prev []profile.Package
	start := max(0, len(gens)-15)
	if start > 0 {
		prev, _ = profile.ReadAt(gens[start-1].Link)
	}
	for _, g := range gens[start:] {
		pkgs, err := profile.ReadAt(g.Link)
		if err != nil {
			continue
		}
		c := diff(prev, pkgs)
		prev = pkgs
		mark := "  "
		if g.Current {
			mark = s.Green(ui.SymDone) + " "
		}
		what := summarize(c, start == 0 && g.Number == gens[0].Number)
		s.Line("%s%s  %s  %s", mark, s.Bold(fmt.Sprintf("%4d", g.Number)), s.Dim(g.Time.Format("Jan 02 15:04")), what)
	}
	s.Blank()
	s.Line("%s", s.Dim("nx rollback steps back one; history older than "+keepHistory+" goes with nx clean"))
	return nil
}

// summarize describes one generation's change in a line.
func summarize(c changes, first bool) string {
	if first {
		return fmt.Sprintf("started with %s", ui.Plural(len(c.added), "package"))
	}
	var parts []string
	list := func(sym string, xs []change) {
		if len(xs) == 0 {
			return
		}
		names := make([]string, 0, len(xs))
		for _, x := range xs {
			names = append(names, x.name)
		}
		if len(names) > 4 {
			names = append(names[:3], fmt.Sprintf("%d more", len(xs)-3))
		}
		parts = append(parts, sym+" "+strings.Join(names, ", "))
	}
	list(ui.SymUp, c.upgraded)
	list(ui.SymAdd, c.added)
	list(ui.SymRemove, c.removed)
	if len(parts) == 0 {
		if c.rebuilt > 0 {
			return fmt.Sprintf("%d rebuilt", c.rebuilt)
		}
		return "no change"
	}
	return strings.Join(parts, "  ")
}

var (
	gcFreed   = regexp.MustCompile(`(\d+) store paths deleted, (.+) freed`)
	wipedLine = regexp.MustCompile(`removing profile version`)
)

func (a *app) clean(args []string) error {
	a.u.Step("Dropping profile history older than %s", keepHistory)
	out, err := exec.CommandContext(a.ctx, "nix", "profile", "wipe-history", "--older-than", keepHistory).CombinedOutput()
	if err != nil {
		a.u.Error("%s", strings.TrimSpace(nixlog.StripANSI(string(out))))
		return errQuiet
	}
	removed := len(wipedLine.FindAllIndex(out, -1))
	a.u.Step("Collecting garbage")
	start := time.Now()
	out, err = exec.CommandContext(a.ctx, "nix", "store", "gc").CombinedOutput()
	if err != nil {
		a.u.Error("%s", strings.TrimSpace(nixlog.StripANSI(string(out))))
		return errQuiet
	}
	s := a.u.Out
	s.Blank()
	if removed > 0 {
		a.u.Done("Removed %s of the profile", ui.Plural(removed, "old version"))
	} else {
		a.u.Done("No profile versions older than %s", keepHistory)
	}
	if m := gcFreed.FindStringSubmatch(nixlog.StripANSI(string(out))); m != nil && m[1] != "0" {
		a.u.Done("Freed %s %s", s.Bold(m[2]), s.Dim(fmt.Sprintf("(%s store paths in %s)", m[1], roundDur(time.Since(start)))))
	} else {
		a.u.Done("Nothing to free")
	}
	return nil
}
