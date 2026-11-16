// Package nixlog reads Nix's --log-format internal-json stream and keeps the
// state a progress display needs: what is being fetched or built, how far each
// download is, and the totals.
//
// The format is Nix's own and not versioned. Lines that don't parse come out
// as raw text, so a newer Nix degrades to its own words instead of silence.
package nixlog

import (
	"encoding/json/v2"
	"regexp"
	"strings"
)

// Activity types (Nix's ActivityType).
const (
	actCopyPath      = 100
	actFileTransfer  = 101
	actRealise       = 102
	actCopyPaths     = 103
	actBuilds        = 104
	actBuild         = 105
	actSubstitute    = 108
	actQueryPathInfo = 109
)

// Result types (Nix's ResultType).
const (
	resBuildLogLine = 101
	resSetPhase     = 104
	resProgress     = 105
	resSetExpected  = 106
)

// Verbosity levels (Nix's Verbosity).
const (
	lvlError  = 0
	lvlWarn   = 1
	lvlNotice = 2
	lvlInfo   = 3
	lvlTalk   = 4
)

// Event is one line of the stream.
type Event struct {
	Action string `json:"action"`
	ID     uint64 `json:"id"`
	Parent uint64 `json:"parent"`
	Type   int    `json:"type"`
	Level  int    `json:"level"`
	Text   string `json:"text"`
	Msg    string `json:"msg"`
	Fields []any  `json:"fields"`
}

const prefix = "@nix "

// Parse reads one line. ok is false for a line that isn't an event.
func Parse(line []byte) (ev Event, ok bool) {
	s := strings.TrimSpace(string(line))
	if !strings.HasPrefix(s, prefix) {
		return ev, false
	}
	if err := json.Unmarshal([]byte(s[len(prefix):]), &ev); err != nil {
		return Event{}, false
	}
	return ev, true
}

// Int returns field i as an integer, or 0.
func (e *Event) Int(i int) int64 {
	if i >= len(e.Fields) {
		return 0
	}
	switch v := e.Fields[i].(type) {
	case float64:
		return int64(v)
	case int64:
		return v
	}
	return 0
}

// Str returns field i as a string, or "".
func (e *Event) Str(i int) string {
	if i >= len(e.Fields) {
		return ""
	}
	s, _ := e.Fields[i].(string)
	return s
}

var ansi = regexp.MustCompile("\x1b\\[[0-9;]*[A-Za-z]")

// StripANSI removes colour codes Nix puts in messages.
func StripANSI(s string) string {
	if !strings.Contains(s, "\x1b") {
		return s
	}
	return ansi.ReplaceAllString(s, "")
}
