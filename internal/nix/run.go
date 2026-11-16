// Package nix runs the nix command. Anything that changes the store goes
// through it (and so through the daemon); nx only reads Nix's progress log
// and draws it.
package nix

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"strings"
	"time"

	"github.com/Siddhj2206/nx/internal/nixlog"
	"github.com/Siddhj2206/nx/internal/ui"
)

// Result is what a run did.
type Result struct {
	Plan    *nixlog.Plan
	Fetched []*nixlog.Download
	Built   []*nixlog.Build
	Totals  nixlog.Totals
	Elapsed time.Duration
}

// Downloaded is the compressed bytes fetched.
func (r Result) Downloaded() int64 {
	var n int64
	for _, d := range r.Fetched {
		n += d.Size
	}
	return n
}

// ErrFailed is returned when nix exits non-zero. Nix's own error has been
// shown already.
var ErrFailed = errors.New("nix failed")

// Unfree lets nixpkgs evaluate unfree packages. Flakes ignore the
// environment unless evaluation is impure, which also skips the eval cache,
// so it's only for the commands that need it.
type Unfree bool

func (u Unfree) apply(cmd *exec.Cmd) {
	if !u {
		return
	}
	cmd.Args = append(cmd.Args, "--impure")
	cmd.Env = append(os.Environ(), "NIXPKGS_ALLOW_UNFREE=1")
}

// Run runs nix with args, drawing its progress on stderr. label names what
// happens while Nix is still evaluating, such as "Resolving ripgrep".
func Run(ctx context.Context, u *ui.UI, label string, args ...string) (Result, error) {
	return RunWith(ctx, u, label, false, args...)
}

// RunWith is Run, allowing unfree packages when unfree is set.
func RunWith(ctx context.Context, u *ui.UI, label string, unfree Unfree, args ...string) (Result, error) {
	start := time.Now()
	cmd := exec.CommandContext(ctx, "nix", append(args, "--log-format", "internal-json")...)
	unfree.apply(cmd)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return Result{}, err
	}
	// Ctrl-C reaches nix too, since it's in our process group; nx waits for
	// it to stop and then tidies the terminal.
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt)
	defer signal.Stop(sig)

	if err := cmd.Start(); err != nil {
		return Result{}, fmt.Errorf("starting nix: %w", err)
	}
	v := newView(u, label)
	lines := make(chan []byte, 256)
	go func() {
		sc := bufio.NewScanner(stderr)
		sc.Buffer(make([]byte, 64<<10), 16<<20)
		for sc.Scan() {
			lines <- bytes.Clone(sc.Bytes())
		}
		close(lines)
	}()
	tick := time.NewTicker(80 * time.Millisecond)
	defer tick.Stop()
	var res Result
loop:
	for {
		select {
		case l, ok := <-lines:
			if !ok {
				break loop
			}
			v.notes(v.tr.Line(l), &res)
		case <-tick.C:
			v.draw()
		case <-sig:
		}
	}
	v.notes(v.tr.Finish(), &res)
	v.live.Close()
	werr := cmd.Wait()
	res.Totals = v.tr.Totals()
	res.Plan = v.tr.PlanSoFar()
	res.Elapsed = time.Since(start)
	if werr != nil {
		var exit *exec.ExitError
		if errors.As(werr, &exit) {
			return res, ErrFailed
		}
		return res, werr
	}
	return res, nil
}

// Output runs nix quietly and returns its stdout. stderr comes back in the
// error when nix fails.
func Output(ctx context.Context, unfree Unfree, args ...string) ([]byte, error) {
	var stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, "nix", args...)
	unfree.apply(cmd)
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		msg := strings.TrimSpace(nixlog.StripANSI(stderr.String()))
		msg = nixlog.Condense(strings.TrimSpace(strings.TrimPrefix(msg, "error:")))
		if msg == "" {
			msg = err.Error()
		}
		return out, errors.New(strings.TrimSpace(msg))
	}
	return out, nil
}
