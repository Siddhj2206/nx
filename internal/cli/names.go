package cli

import (
	"slices"
	"strings"

	"github.com/Siddhj2206/nx/internal/catalog"
)

// brewName maps Homebrew spellings to nixpkgs attributes.
func brewName(name string) string {
	switch name {
	case "yq":
		return "yq-go"
	case "ykman":
		return "yubikey-manager"
	case "bats-core":
		return "bats"
	}
	if v, ok := strings.CutPrefix(name, "openjdk@"); ok {
		return "openjdk" + v
	}
	if v, ok := strings.CutPrefix(name, "node@"); ok {
		return "nodejs_" + v
	}
	if v, ok := strings.CutPrefix(name, "python@"); ok {
		return "python" + strings.ReplaceAll(v, ".", "")
	}
	return name
}

// resolve turns what the user typed into a nixpkgs attribute. With a
// catalog it also accepts a package's pname when that's unambiguous, and
// suggests near names when nothing matches.
func resolve(c *catalog.Catalog, name string) (attr string, pkg *catalog.Package, suggestions []string) {
	attr = brewName(strings.TrimPrefix(name, "nixpkgs#"))
	if c == nil {
		return attr, nil, nil
	}
	if p, ok := c.Get(attr); ok {
		return attr, p, nil
	}
	matches := c.Search([]string{attr})
	var exact []*catalog.Package
	for _, m := range matches {
		if strings.EqualFold(m.Name, attr) && !strings.Contains(m.Attr, ".") {
			exact = append(exact, m.Package)
		}
	}
	if len(exact) == 1 {
		return exact[0].Attr, exact[0], nil
	}
	for i := 0; i < len(matches) && len(suggestions) < 3; i++ {
		suggestions = append(suggestions, matches[i].Attr)
	}
	if len(suggestions) == 0 {
		suggestions = c.Similar(attr, 3)
	}
	return attr, nil, suggestions
}

// flags takes the named --flags out of args, up to a "--". Anything else
// that looks like a flag is a misuse.
func flags(args []string, known ...string) (rest []string, set map[string]bool, err error) {
	set = map[string]bool{}
	for i, a := range args {
		if a == "--" {
			return append(rest, args[i:]...), set, nil
		}
		if strings.HasPrefix(a, "-") && len(a) > 1 {
			name := strings.TrimLeft(a, "-")
			if !slices.Contains(known, name) {
				return nil, nil, misuse("unknown flag %s", a)
			}
			set[name] = true
			continue
		}
		rest = append(rest, a)
	}
	return rest, set, nil
}
