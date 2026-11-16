// Package cache asks a binary cache about store paths without Nix: sizes and
// references from .narinfo files, many at once. It only reads; downloads
// into the store go through nix.
package cache

import (
	"bufio"
	"context"
	"fmt"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"

	"github.com/Siddhj2206/nx/internal/storepath"
)

// Default is the binary cache nixpkgs builds go to.
const Default = "https://cache.nixos.org"

// Info is one store path as the cache describes it.
type Info struct {
	Path       string
	FileSize   int64 // compressed download
	NarSize    int64 // unpacked
	References []string
	Local      bool // already in the local store
	Missing    bool // the cache doesn't have it, so Nix would build it
}

// Closure walks the references from roots and returns every path in the
// closure. It asks up to 32 paths at a time.
func Closure(ctx context.Context, client *http.Client, base string, roots []string) (map[string]*Info, error) {
	var (
		mu    sync.Mutex
		seen  = map[string]*Info{}
		wg    sync.WaitGroup
		sem   = make(chan struct{}, 32)
		first error
	)
	var visit func(p string)
	visit = func(p string) {
		mu.Lock()
		if _, ok := seen[p]; ok || first != nil {
			mu.Unlock()
			return
		}
		info := &Info{Path: p}
		seen[p] = info
		mu.Unlock()
		wg.Add(1)
		go func() {
			defer wg.Done()
			sem <- struct{}{}
			err := fetch(ctx, client, base, info)
			<-sem
			if err != nil {
				mu.Lock()
				if first == nil {
					first = err
				}
				mu.Unlock()
				return
			}
			for _, r := range info.References {
				if r != p {
					visit(r)
				}
			}
		}()
	}
	for _, r := range roots {
		visit(r)
	}
	wg.Wait()
	return seen, first
}

func fetch(ctx context.Context, client *http.Client, base string, info *Info) error {
	if _, err := os.Lstat(info.Path); err == nil {
		info.Local = true
	}
	hash := storepath.Hash(info.Path)
	if hash == "" {
		return fmt.Errorf("not a store path: %s", info.Path)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, base+"/"+hash+".narinfo", nil)
	if err != nil {
		return err
	}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("asking the binary cache: %w", err)
	}
	defer resp.Body.Close()
	switch resp.StatusCode {
	case http.StatusOK:
	case http.StatusNotFound, http.StatusForbidden:
		info.Missing = true
		return nil
	default:
		return fmt.Errorf("binary cache: %s for %s", resp.Status, storepath.Base(info.Path))
	}
	sc := bufio.NewScanner(resp.Body)
	for sc.Scan() {
		k, v, ok := strings.Cut(sc.Text(), ": ")
		if !ok {
			continue
		}
		switch k {
		case "FileSize":
			info.FileSize, _ = strconv.ParseInt(v, 10, 64)
		case "NarSize":
			info.NarSize, _ = strconv.ParseInt(v, 10, 64)
		case "References":
			for _, r := range strings.Fields(v) {
				info.References = append(info.References, storepath.Dir+r)
			}
		}
	}
	return sc.Err()
}

// Sum totals a closure: everything, and what isn't in the local store yet.
type Sum struct {
	Paths, MissingPaths int
	NarSize             int64 // whole closure, unpacked
	Download, Unpacked  int64 // what isn't local yet
	Unbuilt             int   // paths the cache doesn't have
}

// Total adds up a closure.
func Total(c map[string]*Info) Sum {
	var s Sum
	for _, i := range c {
		s.Paths++
		s.NarSize += i.NarSize
		if i.Missing {
			s.Unbuilt++
		}
		if !i.Local {
			s.MissingPaths++
			s.Download += i.FileSize
			s.Unpacked += i.NarSize
		}
	}
	return s
}
