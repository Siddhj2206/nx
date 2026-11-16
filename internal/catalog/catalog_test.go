package catalog

import (
	"bytes"
	"slices"
	"strings"
	"testing"
)

const sample = `{"version":2,"packages":{
 "fd":{"pname":"fd","version":"10.3.0","meta":{"description":"Simple, fast alternative to find","license":{"spdxId":"MIT"},"mainProgram":"fd","outputsToInstall":["out"]}},
 "python313Packages.requests":{"pname":"requests","version":"2.34.2","meta":{"description":"HTTP library for Python","license":{"spdxId":"Apache-2.0"}}},
 "ripgrep":{"pname":"ripgrep","version":"15.2.0","meta":{"description":"Utility that combines the usability of The Silver Searcher with the raw speed of grep","homepage":["https://github.com/BurntSushi/ripgrep"],"license":{"licenseType":"compound","operator":"OR","licenses":[{"spdxId":"Unlicense"},{"spdxId":"MIT"}]},"mainProgram":"rg"}},
 "ripgrep-all":{"pname":"ripgrep-all","version":"0.10.10","meta":{"description":"Ripgrep, but also search in PDFs"}},
 "unrar":{"pname":"unrar","version":"7.3.2","meta":{"description":"Utility for RAR archives","unfree":true,"license":{"shortName":"unfreeRedistributable"},"outputsToInstall":["out","man"]}}
}}`

func load(t *testing.T) *Catalog {
	t.Helper()
	var buf bytes.Buffer
	n, err := Build(strings.NewReader(sample), &buf, Header{Rev: "abc", Release: "rel", Built: 1})
	if err != nil || n != 5 {
		t.Fatalf("Build = %d, %v", n, err)
	}
	c, err := parse(buf.Bytes())
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func TestBuildAndGet(t *testing.T) {
	c := load(t)
	if c.Rev != "abc" || c.Release != "rel" {
		t.Fatalf("header %+v", c.Header)
	}
	p, ok := c.Get("ripgrep")
	if !ok || p.License != "Unlicense OR MIT" || p.Homepage != "https://github.com/BurntSushi/ripgrep" || p.MainProgram != "rg" {
		t.Fatalf("%+v", p)
	}
	u, _ := c.Get("unrar")
	if !u.Unfree() || u.Outputs != "out,man" || u.License != "unfreeRedistributable" {
		t.Fatalf("%+v", u)
	}
}

func attrs(ms []Match) []string {
	var out []string
	for _, m := range ms {
		out = append(out, m.Attr)
	}
	return out
}

func TestSearchRanking(t *testing.T) {
	c := load(t)
	if got := attrs(c.Search([]string{"ripgrep"})); !slices.Equal(got, []string{"ripgrep", "ripgrep-all"}) {
		t.Fatal(got)
	}
	if got := attrs(c.Search([]string{"python", "requests"})); !slices.Equal(got, []string{"python313Packages.requests"}) {
		t.Fatal(got)
	}
	if got := attrs(c.Search([]string{"PDFs"})); !slices.Equal(got, []string{"ripgrep-all"}) {
		t.Fatal(got)
	}
}

func TestSimilar(t *testing.T) {
	c := load(t)
	if got := c.Similar("ripgrp", 3); !slices.Equal(got, []string{"ripgrep"}) {
		t.Fatal(got)
	}
	if got := c.Similar("fdd", 3); !slices.Equal(got, []string{"fd"}) {
		t.Fatal(got)
	}
}
