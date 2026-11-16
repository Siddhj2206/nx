// Package profile reads the user's Nix profile and registry straight from
// disk, which takes microseconds where `nix profile list` takes a tenth of
// a second. Changes still go through nix.
package profile

import (
	"encoding/json/v2"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/Siddhj2206/nx/internal/storepath"
)

// Package is one installed element of the profile.
type Package struct {
	Name       string // the element's name, what `nix profile remove` takes
	Attr       string // nixpkgs attribute, without legacyPackages.<system>.
	Version    string
	Source     string // originalUrl, e.g. flake:nixpkgs
	Rev        string // nixpkgs revision it was built from, if known
	StorePaths []string
}

type manifest struct {
	Version  int                `json:"version"`
	Elements map[string]element `json:"elements"`
}

type element struct {
	Active      bool     `json:"active"`
	AttrPath    string   `json:"attrPath"`
	OriginalURL string   `json:"originalUrl"`
	URL         string   `json:"url"`
	StorePaths  []string `json:"storePaths"`
}

// Dir is where the profile's generations live.
func Dir() string {
	if d := os.Getenv("XDG_STATE_HOME"); d != "" {
		return filepath.Join(d, "nix", "profiles")
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".local", "state", "nix", "profiles")
}

// Link is the profile symlink itself.
func Link() string { return filepath.Join(Dir(), "profile") }

var revInURL = regexp.MustCompile(`github:NixOS/nixpkgs/([0-9a-f]{40})`)

// Read lists the installed packages, sorted by name. A profile that doesn't
// exist yet is empty.
func Read() ([]Package, error) {
	return ReadAt(Link())
}

// ReadAt lists the packages of one profile generation (a profile link or a
// profile-N-link).
func ReadAt(link string) ([]Package, error) {
	data, err := os.ReadFile(filepath.Join(link, "manifest.json"))
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var m manifest
	if err := json.Unmarshal(data, &m); err != nil {
		return nil, fmt.Errorf("reading the profile: %w", err)
	}
	if m.Version < 3 {
		return nil, fmt.Errorf("profile manifest version %d is older than nx understands; run `nix profile list` once to upgrade it", m.Version)
	}
	pkgs := make([]Package, 0, len(m.Elements))
	for name, e := range m.Elements {
		if !e.Active {
			continue
		}
		p := Package{Name: name, Source: e.OriginalURL, StorePaths: e.StorePaths}
		p.Attr = e.AttrPath
		if parts := strings.SplitN(e.AttrPath, ".", 3); len(parts) == 3 && parts[0] == "legacyPackages" {
			p.Attr = parts[2]
		}
		if m := revInURL.FindStringSubmatch(e.URL); m != nil {
			p.Rev = m[1]
		}
		p.Version = version(name, e.StorePaths)
		pkgs = append(pkgs, p)
	}
	slices.SortFunc(pkgs, func(a, b Package) int { return strings.Compare(a.Name, b.Name) })
	return pkgs, nil
}

// version reads the version from the store path, preferring the default
// output's path.
func version(name string, paths []string) string {
	if len(paths) == 0 {
		return ""
	}
	best := storepath.Parse(paths[0])
	for _, p := range paths {
		if n := storepath.Parse(p); n.Output == "" {
			best = n
			break
		}
	}
	return best.Version
}

// Find returns the installed package with the name or attribute.
func Find(pkgs []Package, name string) (*Package, bool) {
	for i := range pkgs {
		if pkgs[i].Name == name || pkgs[i].Attr == name {
			return &pkgs[i], true
		}
	}
	return nil, false
}

// Generation is one saved version of the profile.
type Generation struct {
	Number  int
	Link    string
	Time    time.Time
	Current bool
}

var genLink = regexp.MustCompile(`^profile-(\d+)-link$`)

// Generations lists the profile's saved versions, oldest first.
func Generations() ([]Generation, error) {
	dir := Dir()
	entries, err := os.ReadDir(dir)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	current, _ := os.Readlink(Link())
	var gens []Generation
	for _, e := range entries {
		m := genLink.FindStringSubmatch(e.Name())
		if m == nil {
			continue
		}
		n, _ := strconv.Atoi(m[1])
		info, err := os.Lstat(filepath.Join(dir, e.Name()))
		if err != nil {
			continue
		}
		gens = append(gens, Generation{
			Number: n, Link: filepath.Join(dir, e.Name()), Time: info.ModTime(),
			Current: current == e.Name(),
		})
	}
	slices.SortFunc(gens, func(a, b Generation) int { return a.Number - b.Number })
	return gens, nil
}
