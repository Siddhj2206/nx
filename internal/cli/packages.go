package cli

import (
	"encoding/json/v2"
	"fmt"
	"os"
	"os/exec"
	"slices"
	"strings"
	"syscall"

	"github.com/Siddhj2206/nx/internal/cache"
	"github.com/Siddhj2206/nx/internal/catalog"
	"github.com/Siddhj2206/nx/internal/nix"
	"github.com/Siddhj2206/nx/internal/profile"
	"github.com/Siddhj2206/nx/internal/storepath"
	"github.com/Siddhj2206/nx/internal/ui"
)

const searchLimit = 25

func (a *app) add(args []string) error {
	args, set, err := flags(args, "unfree")
	if err != nil {
		return err
	}
	if len(args) == 0 {
		return misuse("add: name at least one package")
	}
	pin, err := a.pin()
	if err != nil {
		return err
	}
	cat, stale, err := a.catalogFor(pin)
	if err != nil {
		a.u.Warn("no package catalog (%s); installing by name", err)
	}
	before, err := profile.Read()
	if err != nil {
		return err
	}
	var targets, labels []string
	failed := false
	for _, name := range args {
		attr, pkg, suggest := resolve(cat, name)
		if p, ok := profile.Find(before, attr); ok {
			a.u.Done("%s is already installed %s", p.Name, a.u.Out.Dim(p.Version))
			continue
		}
		if cat != nil && pkg == nil && !stale {
			a.notFound(name, suggest)
			failed = true
			continue
		}
		label := attr
		if pkg != nil {
			if pkg.Unfree() && !set["unfree"] {
				a.u.Error("%s is unfree %s %s", attr, a.u.Err.Dim("("+pkg.License+")"),
					a.u.Err.Dim("· nx add --unfree "+attr+" installs it anyway"))
				failed = true
				continue
			}
			label += " " + a.u.Err.Dim(pkg.Version)
			if pkg.Broken() {
				a.u.Warn("%s is marked broken in nixpkgs; it may fail to build", attr)
			}
		}
		targets = append(targets, "nixpkgs#"+attr)
		labels = append(labels, label)
	}
	if len(targets) == 0 {
		if failed {
			return errQuiet
		}
		return nil
	}
	a.u.Step("Installing %s", strings.Join(labels, ", "))
	res, runErr := nix.RunWith(a.ctx, a.u, "Resolving", nix.Unfree(set["unfree"]),
		append([]string{"profile", "add"}, targets...)...)
	after, err := profile.Read()
	if err != nil {
		return err
	}
	c := diff(before, after)
	if runErr == nil || !c.empty() {
		a.report(c, &res, "Nothing added")
	}
	a.warnShadowed(c.added, after)
	if runErr != nil {
		return runErr
	}
	if failed {
		return errQuiet
	}
	return nil
}

func (a *app) notFound(name string, suggest []string) {
	if len(suggest) == 0 {
		a.u.Error("no package called %s %s", name, a.u.Err.Dim("(try nx search "+name+")"))
		return
	}
	a.u.Error("no package called %s %s", name, a.u.Err.Dim("· did you mean "+strings.Join(suggest, ", ")+"?"))
}

func (a *app) remove(args []string) error {
	if len(args) == 0 {
		return misuse("remove: name at least one package")
	}
	before, err := profile.Read()
	if err != nil {
		return err
	}
	var names []string
	failed := false
	for _, n := range args {
		p, ok := profile.Find(before, brewName(n))
		if !ok {
			a.u.Error("%s is not installed", n)
			failed = true
			continue
		}
		if !slices.Contains(names, p.Name) {
			names = append(names, p.Name)
		}
	}
	if len(names) == 0 {
		return errQuiet
	}
	res, runErr := nix.Run(a.ctx, a.u, "Removing", append([]string{"profile", "remove"}, names...)...)
	after, err := profile.Read()
	if err != nil {
		return err
	}
	c := diff(before, after)
	if runErr == nil || !c.empty() {
		a.report(c, &res, "Nothing removed")
	}
	if len(c.removed) > 0 {
		a.u.Out.Line("%s", a.u.Out.Dim("nx clean frees the disk space"))
	}
	if runErr != nil {
		return runErr
	}
	if failed {
		return errQuiet
	}
	return nil
}

func (a *app) list(args []string) error {
	pkgs, err := profile.Read()
	if err != nil {
		return err
	}
	s := a.u.Out
	if len(pkgs) == 0 {
		s.Line("No packages yet. Add one with %s.", s.Bold("nx add <name>"))
		return nil
	}
	pin, _ := profile.ReadPin()
	var cat *catalog.Catalog
	if s.TTY {
		cat, _ = catalog.Open(pin.Rev) // descriptions if they're at hand; never download
	}
	w, vw := 0, 0
	for _, p := range pkgs {
		w, vw = max(w, len(p.Name)), max(vw, len(p.Version))
	}
	width := s.Width()
	for _, p := range pkgs {
		line := fmt.Sprintf("%s  %s", s.Bold(pad(p.Name, w)), s.Dim(pad(p.Version, vw)))
		if cat != nil {
			if c, ok := cat.Get(p.Attr); ok && c.Description != "" {
				line += "  " + s.Dim(ui.Truncate(c.Description, width-w-vw-8))
			}
		}
		s.Line("%s", line)
	}
	s.Blank()
	s.Line("%s", s.Dim(fmt.Sprintf("%s · nixpkgs %s (%s)", ui.Plural(len(pkgs), "package"), pin.Short(), pin.Modified.Format("Jan 02"))))
	return nil
}

func (a *app) search(args []string) error {
	if len(args) == 0 {
		return misuse("search: give a search term")
	}
	pin, err := a.pin()
	if err != nil {
		return err
	}
	cat, stale, err := a.catalogFor(pin)
	if err != nil {
		return err
	}
	installed, _ := profile.Read()
	have := map[string]bool{}
	for _, p := range installed {
		have[p.Attr] = true
	}
	matches := cat.Search(args)
	s := a.u.Out
	if len(matches) == 0 {
		s.Line("No packages match.")
		return nil
	}
	width := s.Width()
	for i, m := range matches {
		if i == searchLimit {
			break
		}
		mark := ""
		if have[m.Attr] {
			mark = "  " + s.Green(ui.SymDone+" installed")
		}
		var flags []string
		if m.Unfree() {
			flags = append(flags, "unfree")
		}
		if m.Broken() {
			flags = append(flags, "broken")
		}
		if len(flags) > 0 {
			mark += "  " + s.Yellow(strings.Join(flags, ", "))
		}
		s.Line("%s %s%s", s.Bold(m.Attr), s.Dim(m.Version), mark)
		if m.Description != "" {
			s.Line("  %s", ui.Truncate(m.Description, width-6))
		}
	}
	s.Blank()
	foot := fmt.Sprintf("%d %s", len(matches), map[bool]string{true: "match", false: "matches"}[len(matches) == 1])
	if len(matches) > searchLimit {
		foot = fmt.Sprintf("%d of %d matches; add a word to narrow it", searchLimit, len(matches))
	}
	foot += " · install with nx add <name>"
	if stale {
		foot += fmt.Sprintf(" · versions from nixpkgs %s; nx upgrade to get them", cat.Rev[:7])
	}
	s.Line("%s", s.Dim(foot))
	return nil
}

func (a *app) info(args []string) error {
	if len(args) != 1 {
		return misuse("info: name one package")
	}
	pin, err := a.pin()
	if err != nil {
		return err
	}
	cat, _, err := a.catalogFor(pin)
	if err != nil {
		return err
	}
	attr, pkg, suggest := resolve(cat, args[0])
	if pkg == nil {
		a.notFound(args[0], suggest)
		return errQuiet
	}
	installed, _ := profile.Read()
	inst, isInstalled := profile.Find(installed, attr)

	s := a.u.Out
	s.Blank()
	head := s.Bold(pkg.Attr) + " " + s.Dim(pkg.Version)
	if isInstalled {
		head += "  " + s.Green(ui.SymDone+" installed")
	}
	s.Line("%s", head)
	if pkg.Description != "" {
		s.Line("%s", pkg.Description)
	}
	if pkg.Homepage != "" {
		s.Line("%s", s.Cyan(pkg.Homepage))
	}
	s.Blank()
	row := func(k, v string) {
		if v != "" {
			s.Line("%s %s", s.Dim(pad(k, 11)), v)
		}
	}
	row("Command", pkg.MainProgram)
	row("License", pkg.License)
	var notes []string
	if pkg.Unfree() {
		notes = append(notes, "unfree")
	}
	if pkg.Broken() {
		notes = append(notes, "marked broken")
	}
	if pkg.Insecure() {
		notes = append(notes, "marked insecure")
	}
	if len(notes) > 0 {
		row("Notes", s.Yellow(strings.Join(notes, ", ")))
	}
	if isInstalled {
		from := "nixpkgs " + short(inst.Rev)
		row("Installed", inst.Version+" "+s.Dim("from "+from))
	}

	// Sizes need the store path, which needs an evaluation, then the
	// binary cache.
	live := ui.NewLive(a.u.Err)
	live.Draw([]string{"  " + a.u.Err.Cyan(ui.SymStep) + " Working out the size"})
	paths, evalErr := outPaths(a, attr, nix.Unfree(pkg.Unfree()))
	var closure map[string]*cache.Info
	var cacheErr error
	if evalErr == nil {
		closure, cacheErr = cache.Closure(a.ctx, a.http, cache.Default, paths)
	}
	live.Close()
	switch {
	case evalErr != nil:
		row("Size", s.Dim("unknown: "+firstLine(evalErr.Error())))
	case cacheErr != nil:
		row("Size", s.Dim("unknown: "+cacheErr.Error()))
	default:
		a.sizeRows(row, paths, closure)
	}
	s.Blank()
	return nil
}

func (a *app) sizeRows(row func(k, v string), roots []string, closure map[string]*cache.Info) {
	s := a.u.Out
	var own int64
	direct := map[string]bool{}
	for _, r := range roots {
		if i := closure[r]; i != nil {
			own += i.NarSize
			for _, ref := range i.References {
				if !slices.Contains(roots, ref) {
					direct[ref] = true
				}
			}
		}
	}
	sum := cache.Total(closure)
	if root := closure[roots[0]]; root != nil && root.Missing && !root.Local {
		row("Size", s.Dim("unknown: the binary cache doesn't have it"))
		row("Download", s.Yellow("none; nx add builds it from source"))
		return
	}
	deps := sum.Paths - len(roots)
	size := ui.Size(own)
	if deps > 0 {
		size += s.Dim(fmt.Sprintf(" · %s with %s", ui.Size(sum.NarSize), ui.Plural(deps, "dependency")))
		size = strings.Replace(size, "dependencys", "dependencies", 1)
	}
	row("Size", size)
	switch {
	case sum.Unbuilt > 0:
		row("Download", s.Yellow(fmt.Sprintf("%s · %s from source", ui.Size(sum.Download), ui.Plural(sum.Unbuilt, "build"))))
	case sum.MissingPaths == 0:
		row("Download", s.Dim("nothing, it's all in the store"))
	default:
		row("Download", fmt.Sprintf("%s %s", ui.Size(sum.Download),
			s.Dim(fmt.Sprintf("· %s new, %s unpacked", ui.Plural(sum.MissingPaths, "path"), ui.Size(sum.Unpacked)))))
	}
	if len(direct) > 0 {
		var names []string
		for p := range direct {
			n := storepath.Parse(p)
			names = append(names, n.Label())
		}
		slices.Sort(names)
		row("Depends on", strings.Join(slices.Compact(names), ", "))
	}
}

// outPaths evaluates the store paths an install of attr would add.
func outPaths(a *app, attr string, unfree nix.Unfree) ([]string, error) {
	out, err := nix.Output(a.ctx, unfree, "eval", "--json", "nixpkgs#"+attr,
		"--apply", `p: map (o: p.${o}.outPath) (p.meta.outputsToInstall or ["out"])`)
	if err != nil {
		return nil, err
	}
	var paths []string
	if err := json.Unmarshal(out, &paths); err != nil {
		return nil, err
	}
	return paths, nil
}

func firstLine(s string) string {
	s, _, _ = strings.Cut(strings.TrimSpace(s), "\n")
	return s
}

func short(rev string) string {
	if len(rev) > 7 {
		return rev[:7]
	}
	if rev == "" {
		return "?"
	}
	return rev
}

func (a *app) try(args []string) error {
	args, set, err := flags(args, "unfree")
	if err != nil {
		return err
	}
	if len(args) == 0 {
		return misuse("try: name a package")
	}
	if _, err := a.pin(); err != nil {
		return err
	}
	attr := brewName(args[0])
	rest := args[1:]
	if len(rest) > 0 && rest[0] == "--" {
		rest = rest[1:]
	}
	ref := "nixpkgs#" + attr
	// Fetch with the package-by-package view; the shell then starts from the
	// local store.
	unfree := nix.Unfree(set["unfree"])
	if _, err := nix.RunWith(a.ctx, a.u, "Resolving "+attr, unfree, "build", "--no-link", ref); err != nil {
		return err
	}
	nixBin, err := exec.LookPath("nix")
	if err != nil {
		return err
	}
	argv := []string{"nix", "shell", "--quiet", ref}
	env := os.Environ()
	if unfree {
		argv = append(argv, "--impure")
		env = append(env, "NIXPKGS_ALLOW_UNFREE=1")
	}
	if len(rest) > 0 {
		argv = append(argv, append([]string{"--command"}, rest...)...)
	} else {
		a.u.Step("A shell with %s; it is not installed, and goes away when you exit", attr)
	}
	return syscall.Exec(nixBin, argv, env)
}
