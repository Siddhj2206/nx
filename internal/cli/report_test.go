package cli

import (
	"bytes"
	"strings"
	"testing"

	"github.com/Siddhj2206/nx/internal/profile"
	"github.com/Siddhj2206/nx/internal/ui"
)

func TestDiffAndReport(t *testing.T) {
	before := []profile.Package{
		{Name: "fd", Version: "10.2.0", StorePaths: []string{"a"}},
		{Name: "git", Version: "2.56.0", StorePaths: []string{"b"}},
		{Name: "jq", Version: "1.8", StorePaths: []string{"c"}},
		{Name: "sl", Version: "5.05", StorePaths: []string{"d"}},
	}
	after := []profile.Package{
		{Name: "fd", Version: "10.3.0", StorePaths: []string{"a2"}},
		{Name: "git", Version: "2.56.0", StorePaths: []string{"b2"}},
		{Name: "jq", Version: "1.8", StorePaths: []string{"c"}},
		{Name: "ripgrep", Version: "15.2.0", StorePaths: []string{"e"}},
	}
	c := diff(before, after)
	if len(c.upgraded) != 1 || len(c.added) != 1 || len(c.removed) != 1 || c.rebuilt != 1 || c.same != 1 {
		t.Fatalf("%+v", c)
	}
	var out bytes.Buffer
	a := &app{u: ui.Plain(&out, &out)}
	a.report(c, nil, "Nothing")
	want := `
  ↑ fd       10.2.0  →  10.3.0
  + ripgrep  15.2.0
  - sl       5.05

  ✓ 1 upgraded, 1 added, 1 removed · 1 rebuilt · 1 unchanged
`
	if out.String() != want {
		t.Fatalf("got:\n%s\nwant:\n%s", out.String(), want)
	}
}

func TestFlags(t *testing.T) {
	rest, set, err := flags([]string{"--unfree", "x", "--", "-v"}, "unfree")
	if err != nil || !set["unfree"] || strings.Join(rest, " ") != "x -- -v" {
		t.Fatal(rest, set, err)
	}
	if _, _, err := flags([]string{"-x"}, "unfree"); err == nil {
		t.Fatal("unknown flag accepted")
	}
}
