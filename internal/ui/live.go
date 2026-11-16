package ui

import (
	"fmt"
	"strings"
	"sync"
)

// Live is a block of lines at the bottom of a terminal that redraws in
// place, with permanent lines printed above it, the way dnf and brew show
// downloads. On anything but a terminal only the permanent lines appear.
type Live struct {
	s     *Stream
	mu    sync.Mutex
	drawn int // lines of the region on screen
	shown bool
}

// NewLive starts a live region on a stream.
func NewLive(s *Stream) *Live {
	return &Live{s: s}
}

// Print writes permanent lines above the region.
func (l *Live) Print(lines ...string) {
	l.mu.Lock()
	defer l.mu.Unlock()
	var b strings.Builder
	l.clear(&b)
	for _, line := range lines {
		b.WriteString(line)
		b.WriteByte('\n')
	}
	fmt.Fprint(l.s.W, b.String())
}

// Draw replaces the region's lines. Lines are cut to the terminal's width,
// since a wrapped line would throw off the redraw.
func (l *Live) Draw(lines []string) {
	if !l.s.TTY {
		return
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	var b strings.Builder
	if !l.shown {
		b.WriteString("\x1b[?25l") // hide the cursor while redrawing
		l.shown = true
	}
	l.clear(&b)
	w := l.s.Width() - 1
	for _, line := range lines {
		b.WriteString(Truncate(line, w))
		b.WriteByte('\n')
	}
	l.drawn = len(lines)
	fmt.Fprint(l.s.W, b.String())
}

func (l *Live) clear(b *strings.Builder) {
	if l.drawn > 0 {
		fmt.Fprintf(b, "\x1b[%dA\x1b[J", l.drawn)
		l.drawn = 0
	}
}

// Close removes the region and gives the cursor back.
func (l *Live) Close() {
	l.mu.Lock()
	defer l.mu.Unlock()
	var b strings.Builder
	l.clear(&b)
	if l.shown {
		b.WriteString("\x1b[?25h")
		l.shown = false
	}
	fmt.Fprint(l.s.W, b.String())
}
