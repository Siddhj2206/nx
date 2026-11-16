// Package storepath splits Nix store path names into the parts people read:
// package name, version and output.
package storepath

import (
	"strings"
)

// Dir is the Nix store every path lives in.
const Dir = "/nix/store/"

// outputs are the output suffixes nixpkgs uses often enough to strip. Others
// stay part of the version, which is harmless.
var outputs = map[string]bool{
	"bin": true, "dev": true, "lib": true, "man": true, "doc": true,
	"info": true, "static": true, "out": true, "debug": true, "dist": true,
	"devdoc": true, "py": true, "lib32": true, "data": true, "share": true,
}

// Name is a store path's name read the way nixpkgs builds it:
// <name>-<version>[-<output>].
type Name struct {
	Name    string
	Version string
	Output  string // "" for the default output
}

// Base returns the name part of a store path: no directory, no hash.
// "/nix/store/<hash>-ripgrep-15.2.0" gives "ripgrep-15.2.0". A ".drv" suffix
// is dropped too.
func Base(p string) string {
	p = strings.TrimPrefix(p, Dir)
	if i := strings.IndexByte(p, '/'); i >= 0 {
		p = p[:i]
	}
	if len(p) > 33 && p[32] == '-' {
		p = p[33:]
	}
	return strings.TrimSuffix(p, ".drv")
}

// Hash returns the 32-character hash part of a store path, or "".
func Hash(p string) string {
	p = strings.TrimPrefix(p, Dir)
	if len(p) >= 32 && (len(p) == 32 || p[32] == '-') {
		return p[:32]
	}
	return ""
}

// Parse splits a store path (or a bare name) like Nix's DrvName does: the
// version starts at the first dash followed by a digit.
func Parse(p string) Name {
	base := Base(p)
	var n Name
	if i := strings.LastIndexByte(base, '-'); i > 0 && outputs[base[i+1:]] {
		n.Output = base[i+1:]
		base = base[:i]
	}
	for i := 0; i < len(base)-1; i++ {
		if base[i] == '-' && base[i+1] >= '0' && base[i+1] <= '9' {
			n.Name, n.Version = base[:i], base[i+1:]
			return n
		}
	}
	n.Name = base
	return n
}

// Label is the name with its output, as a person would say it: "pcre2 (dev)".
func (n Name) Label() string {
	if n.Output == "" || n.Output == "out" {
		return n.Name
	}
	return n.Name + " (" + n.Output + ")"
}
