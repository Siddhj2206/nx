// Package ui is nx's output: colour that turns itself off for pipes, the
// symbols every command shares, sizes, and a live region for progress.
//
// Results go to stdout and progress to stderr, so `nx list | grep` sees only
// results. Every line has a two-space indent and one symbol for its kind:
//
//	› step   ↑ upgraded   + added   - removed   ↓ fetched
//	⚙ building   ✓ done   ! warning   ✗ error
package ui

import (
	"fmt"
	"io"
	"os"
	"strings"
	"unicode/utf8"

	"golang.org/x/term"
)

// Symbols, one per kind of line.
const (
	SymStep    = "›"
	SymUp      = "↑"
	SymAdd     = "+"
	SymRemove  = "-"
	SymFetch   = "↓"
	SymBuild   = "⚙"
	SymDone    = "✓"
	SymWarn    = "!"
	SymError   = "✗"
	SymArrow   = "→"
	SymPending = "·"
)

// Style colours text for one stream, or leaves it plain.
type Style struct{ on bool }

func (s Style) wrap(code, x string) string {
	if !s.on || x == "" {
		return x
	}
	return "\x1b[" + code + "m" + x + "\x1b[0m"
}

func (s Style) Bold(x string) string  { return s.wrap("1", x) }
func (s Style) Dim(x string) string   { return s.wrap("2", x) }
func (s Style) Green(x string) string { return s.wrap("32", x) }
func (s Style) BoldGreen(x string) string {
	return s.wrap("1;32", x)
}
func (s Style) Cyan(x string) string   { return s.wrap("36", x) }
func (s Style) Red(x string) string    { return s.wrap("31", x) }
func (s Style) Yellow(x string) string { return s.wrap("33", x) }
func (s Style) On() bool               { return s.on }

// Stream is stdout or stderr with its own colour decision.
type Stream struct {
	W io.Writer
	Style
	TTY bool
	fd  int
}

func newStream(f *os.File) *Stream {
	fd := int(f.Fd())
	tty := term.IsTerminal(fd)
	colour := tty && os.Getenv("NO_COLOR") == "" && os.Getenv("TERM") != "dumb"
	return &Stream{W: f, Style: Style{on: colour}, TTY: tty, fd: fd}
}

// Width is the terminal's width, or 100 when it isn't a terminal.
func (s *Stream) Width() int {
	if s.TTY {
		if w, _, err := term.GetSize(s.fd); err == nil && w > 0 {
			return w
		}
	}
	return 100
}

// Line writes one indented line.
func (s *Stream) Line(format string, a ...any) {
	fmt.Fprintf(s.W, "  "+format+"\n", a...)
}

// Blank writes an empty line.
func (s *Stream) Blank() { fmt.Fprintln(s.W) }

// UI is both streams.
type UI struct {
	Out *Stream // results
	Err *Stream // progress, steps, errors
}

// New sets up the process's own stdout and stderr.
func New() *UI {
	return &UI{Out: newStream(os.Stdout), Err: newStream(os.Stderr)}
}

// Plain is a UI that writes uncoloured text, for tests.
func Plain(out, err io.Writer) *UI {
	return &UI{Out: &Stream{W: out}, Err: &Stream{W: err}}
}

// Step announces something about to take a while.
func (u *UI) Step(format string, a ...any) {
	u.Err.Line("%s %s", u.Err.Cyan(SymStep), fmt.Sprintf(format, a...))
}

// Done reports success, on stdout.
func (u *UI) Done(format string, a ...any) {
	u.Out.Line("%s %s", u.Out.Green(SymDone), fmt.Sprintf(format, a...))
}

// Warn reports something odd that didn't stop the work.
func (u *UI) Warn(format string, a ...any) {
	u.Err.Line("%s %s", u.Err.Yellow(SymWarn), fmt.Sprintf(format, a...))
}

// Error reports a failure. Continuation lines get the same indent.
func (u *UI) Error(format string, a ...any) {
	msg := strings.ReplaceAll(fmt.Sprintf(format, a...), "\n", "\n    ")
	u.Err.Line("%s %s", u.Err.Red(SymError), msg)
}

// Size formats bytes the way Nix and dnf do: 1024-based, one decimal from MiB.
func Size(n int64) string {
	switch {
	case n >= 1<<30:
		return fmt.Sprintf("%.1f GiB", float64(n)/(1<<30))
	case n >= 1<<20:
		return fmt.Sprintf("%.1f MiB", float64(n)/(1<<20))
	case n >= 1<<10:
		return fmt.Sprintf("%d KiB", n>>10)
	case n > 0:
		return fmt.Sprintf("%d B", n)
	}
	return "0 B"
}

// Plural picks the word for a count: Plural(1, "package") is "1 package".
func Plural(n int, word string) string {
	if n == 1 {
		return fmt.Sprintf("%d %s", n, word)
	}
	return fmt.Sprintf("%d %ss", n, word)
}

// VisibleLen is a string's width on screen, ignoring colour codes. Every
// rune counts as one column.
func VisibleLen(s string) int {
	n, esc := 0, false
	for _, r := range s {
		switch {
		case esc:
			if r >= '@' && r <= '~' && r != '[' {
				esc = false
			}
		case r == '\x1b':
			esc = true
		default:
			n++
		}
	}
	return n
}

// Truncate cuts s to w visible columns, ending in "…" when it cut, and keeps
// colour codes intact.
func Truncate(s string, w int) string {
	if w <= 0 || VisibleLen(s) <= w {
		return s
	}
	var b strings.Builder
	n, esc := 0, false
	for i := 0; i < len(s); {
		r, size := utf8.DecodeRuneInString(s[i:])
		switch {
		case esc:
			b.WriteRune(r)
			if r >= '@' && r <= '~' && r != '[' {
				esc = false
			}
		case r == '\x1b':
			esc = true
			b.WriteRune(r)
		default:
			if n == w-1 {
				b.WriteString("…")
				if strings.Contains(s, "\x1b[") {
					b.WriteString("\x1b[0m")
				}
				return b.String()
			}
			b.WriteRune(r)
			n++
		}
		i += size
	}
	return b.String()
}

// Pad right-pads s with spaces to w visible columns.
func Pad(s string, w int) string {
	if n := VisibleLen(s); n < w {
		return s + strings.Repeat(" ", w-n)
	}
	return s
}

// Bar draws a progress bar w columns wide.
func (s Style) Bar(done, total int64, w int) string {
	if total <= 0 {
		return s.Dim(strings.Repeat("─", w))
	}
	fill := int(float64(w) * float64(min(done, total)) / float64(total))
	return s.Cyan(strings.Repeat("━", fill)) + s.Dim(strings.Repeat("─", w-fill))
}
