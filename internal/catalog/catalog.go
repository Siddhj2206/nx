// Package catalog is nx's local copy of what nixpkgs offers: every package's
// attribute, name, version and description for one nixpkgs revision. It comes
// from the channel's packages.json, so search and info never evaluate Nix.
package catalog

import (
	"bytes"
	"fmt"
	"os"
	"slices"
	"strconv"
	"strings"
)

// Package is one catalog entry.
type Package struct {
	Attr        string // attribute path in nixpkgs, what `nx add` installs
	Name        string // pname
	Version     string
	Description string
	Homepage    string
	License     string
	MainProgram string
	Outputs     string // outputs installed, comma separated; "" means out
	Flags       string // u unfree, b broken, i insecure, x unavailable
}

func (p *Package) Unfree() bool   { return strings.IndexByte(p.Flags, 'u') >= 0 }
func (p *Package) Broken() bool   { return strings.IndexByte(p.Flags, 'b') >= 0 }
func (p *Package) Insecure() bool { return strings.IndexByte(p.Flags, 'i') >= 0 }

// Catalog is a loaded catalog file.
type Catalog struct {
	Header
	Packages []Package // sorted by attribute
}

// Load reads a catalog file.
func Load(path string) (*Catalog, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return parse(data)
}

func parse(data []byte) (*Catalog, error) {
	head, rest, ok := bytes.Cut(data, []byte{eol})
	if !ok {
		return nil, fmt.Errorf("catalog: empty file")
	}
	hf := strings.Split(string(head), string(sep))
	if len(hf) != 4 || hf[0] != fileVersion {
		return nil, errOldFormat
	}
	built, _ := strconv.ParseInt(hf[3], 10, 64)
	c := &Catalog{Header: Header{Rev: hf[1], Release: hf[2], Built: built}}
	// One string for the whole body; every field is a slice of it.
	body := string(rest)
	c.Packages = make([]Package, 0, bytes.Count(rest, []byte{eol}))
	for len(body) > 0 {
		line := body
		if i := strings.IndexByte(body, eol); i >= 0 {
			line, body = body[:i], body[i+1:]
		} else {
			body = ""
		}
		var f [9]string
		for i := range 8 {
			j := strings.IndexByte(line, sep)
			if j < 0 {
				return nil, fmt.Errorf("catalog: short record %q", line)
			}
			f[i], line = line[:j], line[j+1:]
		}
		f[8] = line
		c.Packages = append(c.Packages, Package{
			Attr: f[0], Name: f[1], Version: f[2], Description: f[3], Homepage: f[4],
			License: f[5], MainProgram: f[6], Outputs: f[7], Flags: f[8],
		})
	}
	byAttr := func(a, b Package) int { return strings.Compare(a.Attr, b.Attr) }
	if !slices.IsSortedFunc(c.Packages, byAttr) {
		slices.SortFunc(c.Packages, byAttr)
	}
	return c, nil
}

var errOldFormat = fmt.Errorf("catalog: written by another nx version")

// Get finds a package by attribute.
func (c *Catalog) Get(attr string) (*Package, bool) {
	i, ok := slices.BinarySearchFunc(c.Packages, attr, func(p Package, a string) int {
		return strings.Compare(p.Attr, a)
	})
	if !ok {
		return nil, false
	}
	return &c.Packages[i], true
}

// Match is a search result.
type Match struct {
	*Package
	rank int
}

// Search finds packages whose name or description holds every term. Name
// matches come first: exact, then prefix, then anywhere; top-level
// attributes before ones inside package sets.
func (c *Catalog) Search(terms []string) []Match {
	if len(terms) == 0 {
		return nil
	}
	lower := make([]string, len(terms))
	for i, t := range terms {
		lower[i] = strings.ToLower(t)
	}
	whole := strings.Join(lower, " ")
	var out []Match
	for i := range c.Packages {
		p := &c.Packages[i]
		if r, ok := rank(p, lower, whole); ok {
			out = append(out, Match{Package: p, rank: r})
		}
	}
	slices.SortStableFunc(out, func(a, b Match) int {
		if a.rank != b.rank {
			return a.rank - b.rank
		}
		if d := len(a.Attr) - len(b.Attr); d != 0 && a.rank <= 2 {
			return d
		}
		return strings.Compare(a.Attr, b.Attr)
	})
	return out
}

func rank(p *Package, terms []string, whole string) (int, bool) {
	attr := strings.ToLower(p.Attr)
	name := strings.ToLower(p.Name)
	leaf := attr
	if i := strings.LastIndexByte(attr, '.'); i >= 0 {
		leaf = attr[i+1:]
	}
	nested := 0
	if leaf != attr {
		nested = 10
	}
	switch {
	case attr == whole || name == whole && nested == 0:
		return 0, true
	case leaf == whole || name == whole:
		return 1 + nested, true
	}
	inName, inDesc := true, true
	var desc string
	for _, t := range terms {
		if strings.Contains(attr, t) || strings.Contains(name, t) {
			continue
		}
		inName = false
		if desc == "" {
			desc = strings.ToLower(p.Description)
		}
		if !strings.Contains(desc, t) {
			inDesc = false
			break
		}
	}
	switch {
	case inName && (strings.HasPrefix(leaf, whole) || strings.HasPrefix(name, whole) ||
		len(terms) > 1 && (slices.Contains(terms, leaf) || slices.Contains(terms, name))):
		return 2 + nested, true
	case inName:
		return 3 + nested, true
	case inDesc:
		return 4 + nested, true
	}
	return 0, false
}

// Similar suggests top-level attributes within two edits of name, closest
// first, for "did you mean".
func (c *Catalog) Similar(name string, n int) []string {
	name = strings.ToLower(name)
	type cand struct {
		attr string
		d    int
	}
	var cands []cand
	for i := range c.Packages {
		attr := c.Packages[i].Attr
		if strings.IndexByte(attr, '.') >= 0 || abs(len(attr)-len(name)) > 2 {
			continue
		}
		if d := distance(name, strings.ToLower(attr), 2); d <= 2 {
			cands = append(cands, cand{attr, d})
		}
	}
	slices.SortStableFunc(cands, func(a, b cand) int { return a.d - b.d })
	var out []string
	for i := 0; i < len(cands) && i < n; i++ {
		out = append(out, cands[i].attr)
	}
	return out
}

func abs(x int) int {
	if x < 0 {
		return -x
	}
	return x
}

// distance is the optimal string alignment distance (Levenshtein plus
// transpositions), giving up once every path exceeds limit.
func distance(a, b string, limit int) int {
	prev2 := make([]int, len(b)+1)
	prev := make([]int, len(b)+1)
	cur := make([]int, len(b)+1)
	for j := range prev {
		prev[j] = j
	}
	for i := 1; i <= len(a); i++ {
		cur[0] = i
		best := cur[0]
		for j := 1; j <= len(b); j++ {
			cost := 1
			if a[i-1] == b[j-1] {
				cost = 0
			}
			cur[j] = min(prev[j]+1, cur[j-1]+1, prev[j-1]+cost)
			if i > 1 && j > 1 && a[i-1] == b[j-2] && a[i-2] == b[j-1] {
				cur[j] = min(cur[j], prev2[j-2]+1)
			}
			best = min(best, cur[j])
		}
		if best > limit {
			return limit + 1
		}
		prev2, prev, cur = prev, cur, prev2
	}
	return prev[len(b)]
}
