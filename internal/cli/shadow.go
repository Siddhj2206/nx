package cli

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/Siddhj2206/nx/internal/profile"
)

// shadow is a set of a package's commands that an earlier PATH directory
// also provides, so typing them runs that directory's copy instead.
type shadow struct {
	dir   string
	names []string
}

// shadowed lists, per PATH directory, the commands in pkg's bin directories
// that resolve somewhere other than the profile's bin. Nothing is shadowed
// when the profile's bin isn't on PATH at all: then no command of pkg runs,
// and that's a setup problem, not a per-package one.
func shadowed(pkg profile.Package, profileBin, path string) []shadow {
	own, err := filepath.EvalSymlinks(profileBin)
	if err != nil {
		return nil
	}
	dirs := filepath.SplitList(path)
	ownAt := slices.IndexFunc(dirs, func(d string) bool { return sameDir(d, own) })
	if ownAt < 0 {
		return nil
	}
	var out []shadow
	for _, name := range binNames(pkg) {
		for _, d := range dirs[:ownAt] {
			if d == "" || !executable(filepath.Join(d, name)) {
				continue
			}
			i := slices.IndexFunc(out, func(s shadow) bool { return s.dir == d })
			if i < 0 {
				out = append(out, shadow{dir: d})
				i = len(out) - 1
			}
			out[i].names = append(out[i].names, name)
			break
		}
	}
	return out
}

// binNames is every name in the package's bin directories, sorted.
func binNames(pkg profile.Package) []string {
	var names []string
	for _, sp := range pkg.StorePaths {
		entries, _ := os.ReadDir(filepath.Join(sp, "bin"))
		for _, e := range entries {
			if !slices.Contains(names, e.Name()) {
				names = append(names, e.Name())
			}
		}
	}
	slices.Sort(names)
	return names
}

func sameDir(d, resolved string) bool {
	r, err := filepath.EvalSymlinks(d)
	return err == nil && r == resolved
}

func executable(p string) bool {
	fi, err := os.Stat(p)
	return err == nil && !fi.IsDir() && fi.Mode()&0o111 != 0
}

// warnShadowed says which of the added packages' commands won't run because
// something earlier on PATH has the same name.
func (a *app) warnShadowed(added []change, after []profile.Package) {
	bin := filepath.Join(profile.Link(), "bin")
	path := os.Getenv("PATH")
	for _, c := range added {
		p, ok := profile.Find(after, c.name)
		if !ok {
			continue
		}
		for _, s := range shadowed(*p, bin, path) {
			a.u.Warn("%s: %s %s", p.Name, listNames(s.names, 5),
				a.u.Err.Dim("· "+s.dir+" comes first on PATH, so its copy runs"))
		}
	}
}

func listNames(names []string, limit int) string {
	if len(names) <= limit {
		return strings.Join(names, ", ")
	}
	return strings.Join(names[:limit], ", ") + fmt.Sprintf(" and %d more", len(names)-limit)
}
