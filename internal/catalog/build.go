package catalog

import (
	"bufio"
	"encoding/json/jsontext"
	"encoding/json/v2"
	"fmt"
	"io"
	"slices"
	"strings"
)

// fileVersion changes when the catalog file's layout does; an older file is
// rebuilt.
const fileVersion = "nx-catalog 1"

const (
	sep = '\x1f'
	eol = '\n'
)

// raw is the part of a packages.json entry the catalog keeps.
type raw struct {
	Pname   string `json:"pname"`
	Version string `json:"version"`
	Meta    struct {
		Description      string         `json:"description"`
		Homepage         jsontext.Value `json:"homepage"`
		License          jsontext.Value `json:"license"`
		MainProgram      string         `json:"mainProgram"`
		OutputsToInstall []string       `json:"outputsToInstall"`
		Unfree           bool           `json:"unfree"`
		Broken           bool           `json:"broken"`
		Insecure         bool           `json:"insecure"`
		Available        *bool          `json:"available"`
	} `json:"meta"`
}

type license struct {
	SpdxID    string    `json:"spdxId"`
	ShortName string    `json:"shortName"`
	FullName  string    `json:"fullName"`
	Licenses  []license `json:"licenses"`
	Operator  string    `json:"operator"`
}

func (l license) String() string {
	if len(l.Licenses) > 0 {
		parts := make([]string, 0, len(l.Licenses))
		for _, s := range l.Licenses {
			parts = append(parts, s.String())
		}
		op := " " + strings.ToUpper(l.Operator) + " "
		if l.Operator == "" {
			op = ", "
		}
		return strings.Join(parts, op)
	}
	return cmp(l.SpdxID, l.ShortName, l.FullName)
}

func cmp(s ...string) string {
	for _, v := range s {
		if v != "" {
			return v
		}
	}
	return ""
}

func licenseText(v jsontext.Value) string {
	if len(v) == 0 {
		return ""
	}
	switch v.Kind() {
	case '{':
		var l license
		if json.Unmarshal(v, &l) == nil {
			return l.String()
		}
	case '[':
		var ls []license
		if json.Unmarshal(v, &ls) == nil {
			return license{Licenses: ls}.String()
		}
	case '"':
		var s string
		json.Unmarshal(v, &s)
		return s
	}
	return ""
}

func homepageText(v jsontext.Value) string {
	if len(v) == 0 {
		return ""
	}
	var s string
	if json.Unmarshal(v, &s) == nil {
		return s
	}
	var l []string
	if json.Unmarshal(v, &l) == nil && len(l) > 0 {
		return l[0]
	}
	return ""
}

// Header is the catalog file's first line: what it was built from.
type Header struct {
	Rev     string // nixpkgs git revision
	Release string // channel release name, e.g. nixpkgs-26.11pre1120695.5d2b81a7a3be
	Built   int64  // unix seconds
}

// Build reads a channel's packages.json (already decompressed) and writes the
// catalog file to w. It returns the number of packages written.
func Build(r io.Reader, w io.Writer, h Header) (int, error) {
	bw := bufio.NewWriterSize(w, 1<<20)
	fmt.Fprintf(bw, "%s%c%s%c%s%c%d%c", fileVersion, sep, h.Rev, sep, h.Release, sep, h.Built, eol)

	dec := jsontext.NewDecoder(bufio.NewReaderSize(r, 1<<20))
	if err := expectObject(dec); err != nil {
		return 0, err
	}
	n := 0
	for dec.PeekKind() != '}' {
		key, err := dec.ReadToken()
		if err != nil {
			return n, err
		}
		if key.String() != "packages" {
			if err := dec.SkipValue(); err != nil {
				return n, err
			}
			continue
		}
		if err := expectObject(dec); err != nil {
			return n, err
		}
		for dec.PeekKind() != '}' {
			tok, err := dec.ReadToken()
			if err != nil {
				return n, err
			}
			attr := tok.String() // the token is void after the next read
			var p raw
			if err := json.UnmarshalDecode(dec, &p); err != nil {
				return n, fmt.Errorf("package %s: %w", attr, err)
			}
			writeRecord(bw, attr, &p)
			n++
		}
		if _, err := dec.ReadToken(); err != nil {
			return n, err
		}
	}
	if n == 0 {
		return 0, fmt.Errorf("no packages in the channel's package list")
	}
	return n, bw.Flush()
}

func expectObject(dec *jsontext.Decoder) error {
	tok, err := dec.ReadToken()
	if err != nil {
		return err
	}
	if tok.Kind() != '{' {
		return fmt.Errorf("package list: expected an object, got %v", tok.Kind())
	}
	return nil
}

func clean(s string) string {
	if !strings.ContainsAny(s, "\x1f\n\r\t") {
		return s
	}
	return strings.Join(strings.FieldsFunc(s, func(r rune) bool {
		return r == sep || r == '\n' || r == '\r' || r == '\t'
	}), " ")
}

func writeRecord(w *bufio.Writer, attr string, p *raw) {
	var flags []byte
	if p.Meta.Unfree {
		flags = append(flags, 'u')
	}
	if p.Meta.Broken {
		flags = append(flags, 'b')
	}
	if p.Meta.Insecure {
		flags = append(flags, 'i')
	}
	if p.Meta.Available != nil && !*p.Meta.Available {
		flags = append(flags, 'x')
	}
	outs := p.Meta.OutputsToInstall
	if slices.Equal(outs, []string{"out"}) {
		outs = nil
	}
	fields := [...]string{
		attr, p.Pname, p.Version, strings.TrimSpace(p.Meta.Description),
		homepageText(p.Meta.Homepage), licenseText(p.Meta.License),
		p.Meta.MainProgram, strings.Join(outs, ","), string(flags),
	}
	for i, f := range fields {
		if i > 0 {
			w.WriteByte(sep)
		}
		w.WriteString(clean(f))
	}
	w.WriteByte(eol)
}
