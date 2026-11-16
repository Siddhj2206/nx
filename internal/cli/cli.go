// Package cli is nx's commands.
package cli

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"time"

	"github.com/Siddhj2206/nx/internal/catalog"
	"github.com/Siddhj2206/nx/internal/nix"
	"github.com/Siddhj2206/nx/internal/profile"
	"github.com/Siddhj2206/nx/internal/ui"
	"github.com/Siddhj2206/nx/internal/version"
)

// Exit codes.
const (
	exitOK      = 0
	exitFailed  = 1
	exitMisuse  = 2
	keepHistory = "14d"
)

type command struct {
	names []string
	args  string
	help  string
	group string
	run   func(a *app, args []string) error
}

var commands []command

func init() {
	commands = []command{
		{[]string{"add", "install"}, "<name>...", "install packages (--unfree to allow unfree ones)", "Packages", (*app).add},
		{[]string{"remove", "rm", "uninstall"}, "<name>...", "remove packages", "Packages", (*app).remove},
		{[]string{"list", "ls"}, "", "what is installed, with versions", "Packages", (*app).list},
		{[]string{"search"}, "<term>...", "find packages in nixpkgs", "Packages", (*app).search},
		{[]string{"info", "show"}, "<name>", "details, size and dependencies of a package", "Packages", (*app).info},
		{[]string{"try", "shell"}, "<name> [-- cmd]", "run a package without installing it", "Packages", (*app).try},
		{[]string{"upgrade", "update"}, "", "move to the newest nixpkgs and upgrade everything", "Maintenance", (*app).upgrade},
		{[]string{"outdated"}, "", "what the newest nixpkgs would upgrade", "Maintenance", (*app).outdated},
		{[]string{"history"}, "", "past versions of your package set", "Maintenance", (*app).history},
		{[]string{"rollback"}, "", "undo the last change", "Maintenance", (*app).rollback},
		{[]string{"clean", "gc", "cleanup"}, "", "drop history older than " + keepHistory + ", then free disk space", "Maintenance", (*app).clean},
	}
}

// errMisuse marks an error in how nx was called.
type errMisuse struct{ msg string }

func (e errMisuse) Error() string { return e.msg }

func misuse(format string, a ...any) error { return errMisuse{fmt.Sprintf(format, a...)} }

// errQuiet is a failure already reported.
var errQuiet = errors.New("failed")

type app struct {
	ctx  context.Context
	u    *ui.UI
	http *http.Client
}

// Main runs nx and returns its exit code.
func Main(args []string) int {
	u := ui.New()
	if len(args) == 0 {
		usage(u)
		return exitOK
	}
	name, rest := args[0], args[1:]
	switch name {
	case "-h", "--help", "help":
		usage(u)
		return exitOK
	case "-V", "--version", "version":
		fmt.Fprintln(u.Out.W, "nx", version.String())
		return exitOK
	}
	cmd := find(name)
	if cmd == nil {
		u.Error("unknown command: %s %s", name, u.Err.Dim("(see nx help)"))
		return exitMisuse
	}
	if os.Geteuid() == 0 {
		u.Error("run as your user, not root; Nix profiles are per user")
		return exitMisuse
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	a := &app{ctx: ctx, u: u, http: &http.Client{Timeout: 60 * time.Second}}
	err := cmd.run(a, rest)
	var m errMisuse
	switch {
	case err == nil:
		return exitOK
	case errors.As(err, &m):
		u.Error("%s", m.msg)
		return exitMisuse
	case errors.Is(err, errQuiet), errors.Is(err, nix.ErrFailed):
		return exitFailed
	case errors.Is(err, context.Canceled):
		return 130
	default:
		u.Error("%s", err)
		return exitFailed
	}
}

func find(name string) *command {
	for i := range commands {
		for _, n := range commands[i].names {
			if n == name {
				return &commands[i]
			}
		}
	}
	return nil
}

func usage(u *ui.UI) {
	s := u.Out
	fmt.Fprintf(s.W, "\n  %s %s\n\n", s.Bold("nx"), s.Dim("· packages from nixpkgs, one pinned revision for all of them"))
	fmt.Fprintf(s.W, "  %s nx <command> [args]\n", s.Bold("Usage:"))
	group := ""
	for _, c := range commands {
		if c.group != group {
			group = c.group
			fmt.Fprintf(s.W, "\n  %s\n", s.Dim(group))
		}
		left := c.names[0]
		if c.args != "" {
			left += " " + c.args
		}
		pad := strings.Repeat(" ", max(1, 22-len(left)))
		fmt.Fprintf(s.W, "    %s%s%s%s\n", s.Cyan(c.names[0]), strings.TrimPrefix(left, c.names[0]), pad, c.help)
	}
	fmt.Fprintf(s.W, "\n  %s\n\n", s.Dim("Homebrew spellings work too: yq, ykman, openjdk@17, node@22, python@3.13."))
}

// pin returns the nixpkgs pin, pinning the channel's current release on
// first use.
func (a *app) pin() (profile.Pin, error) {
	if p, ok := profile.ReadPin(); ok {
		return p, nil
	}
	rel, err := catalog.Latest(a.ctx, a.http)
	if err != nil {
		return profile.Pin{}, err
	}
	a.u.Step("Pinning nixpkgs %s for this user %s", rel.Short(), a.u.Err.Dim("(first use)"))
	if err := a.setPin(rel.Rev); err != nil {
		return profile.Pin{}, err
	}
	p, ok := profile.ReadPin()
	if !ok {
		return profile.Pin{}, errors.New("pinning nixpkgs did not take; check ~/.config/nix/registry.json")
	}
	return p, nil
}

func (a *app) setPin(rev string) error {
	_, err := nix.Run(a.ctx, a.u, "Fetching nixpkgs "+rev[:7],
		"registry", "pin", "nixpkgs", "github:NixOS/nixpkgs/"+rev)
	return err
}

// catalogFor returns the catalog matching the pin, downloading it when
// missing. When the channel has moved on, the catalog for the newest
// release stands in, and stale says so.
func (a *app) catalogFor(pin profile.Pin) (c *catalog.Catalog, stale bool, err error) {
	if c, err := catalog.Open(pin.Rev); err == nil {
		return c, false, nil
	}
	rel, err := catalog.Latest(a.ctx, a.http)
	if err != nil {
		return nil, false, err
	}
	if c, err := catalog.Open(rel.Rev); err == nil {
		return c, rel.Rev != pin.Rev, nil
	}
	if err := a.downloadCatalog(rel); err != nil {
		return nil, false, err
	}
	catalog.Prune(pin.Rev, rel.Rev)
	c, err = catalog.Open(rel.Rev)
	return c, rel.Rev != pin.Rev, err
}

func (a *app) downloadCatalog(rel catalog.Release) error {
	live := ui.NewLive(a.u.Err)
	defer live.Close()
	s := a.u.Err
	last := time.Time{}
	n, err := catalog.Download(a.ctx, a.http, rel, func(done, total int64) {
		if time.Since(last) < 80*time.Millisecond {
			return
		}
		last = time.Now()
		live.Draw([]string{fmt.Sprintf("  %s %s %s", s.Cyan(ui.SymStep), "Package catalog for nixpkgs "+rel.Short(),
			s.Bar(done, total, 20)+" "+s.Dim(ui.Size(done)+" / "+ui.Size(total)))})
	})
	if err != nil {
		return err
	}
	live.Print(fmt.Sprintf("  %s Package catalog for nixpkgs %s %s", s.Green(ui.SymFetch), rel.Short(),
		s.Dim(fmt.Sprintf("· %d packages", n))))
	return nil
}
