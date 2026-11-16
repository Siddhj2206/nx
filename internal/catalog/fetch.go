package catalog

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"github.com/andybalholm/brotli"
)

// Channel is where nixpkgs releases come from. A channel only advances once
// Hydra has built it, so its revision is one the binary cache has.
const Channel = "https://channels.nixos.org/nixpkgs-unstable"

// Release is one channel release.
type Release struct {
	Rev  string // nixpkgs git revision
	Name string // e.g. nixpkgs-26.11pre1120695.5d2b81a7a3be
	URL  string // the release directory
}

// Short is the first seven characters of the revision.
func (r Release) Short() string { return short(r.Rev) }

func short(rev string) string {
	if len(rev) > 7 {
		return rev[:7]
	}
	return rev
}

// Latest asks the channel for its current release.
func Latest(ctx context.Context, client *http.Client) (Release, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, Channel+"/git-revision", nil)
	if err != nil {
		return Release{}, err
	}
	resp, err := client.Do(req)
	if err != nil {
		return Release{}, fmt.Errorf("asking the nixpkgs channel for its release: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return Release{}, fmt.Errorf("nixpkgs channel: %s", resp.Status)
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, 128))
	if err != nil {
		return Release{}, err
	}
	dir := strings.TrimSuffix(resp.Request.URL.String(), "/git-revision")
	return Release{
		Rev:  strings.TrimSpace(string(body)),
		Name: dir[strings.LastIndexByte(dir, '/')+1:],
		URL:  dir,
	}, nil
}

// Dir is where catalog files live, one per nixpkgs revision.
func Dir() (string, error) {
	dir, err := os.UserCacheDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "nx"), nil
}

// Path is the catalog file for a revision.
func Path(rev string) (string, error) {
	dir, err := Dir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "catalog-"+rev), nil
}

// Open loads the catalog for a revision, or returns an error wrapping
// os.ErrNotExist when there isn't one yet (or it's from an older nx).
func Open(rev string) (*Catalog, error) {
	p, err := Path(rev)
	if err != nil {
		return nil, err
	}
	c, err := Load(p)
	if errors.Is(err, errOldFormat) {
		return nil, fmt.Errorf("%w: %w", os.ErrNotExist, err)
	}
	return c, err
}

// Prune deletes catalog files for revisions other than keep.
func Prune(keep ...string) {
	dir, err := Dir()
	if err != nil {
		return
	}
	files, _ := filepath.Glob(filepath.Join(dir, "catalog-*"))
	for _, f := range files {
		if !slices.Contains(keep, strings.TrimPrefix(filepath.Base(f), "catalog-")) {
			os.Remove(f)
		}
	}
}

// Progress reports bytes downloaded of the compressed package list.
type Progress func(done, total int64)

// Download fetches a release's package list and writes the catalog file in
// place of the old one.
func Download(ctx context.Context, client *http.Client, rel Release, progress Progress) (int, error) {
	path, err := Path(rel.Rev)
	if err != nil {
		return 0, err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return 0, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rel.URL+"/packages.json.br", nil)
	if err != nil {
		return 0, err
	}
	resp, err := client.Do(req)
	if err != nil {
		return 0, fmt.Errorf("downloading the package list: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return 0, fmt.Errorf("package list: %s", resp.Status)
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), ".catalog-*")
	if err != nil {
		return 0, err
	}
	defer os.Remove(tmp.Name())
	body := io.Reader(resp.Body)
	if progress != nil {
		body = &counter{r: body, total: resp.ContentLength, fn: progress}
	}
	n, err := Build(brotli.NewReader(body), tmp, Header{Rev: rel.Rev, Release: rel.Name, Built: time.Now().Unix()})
	if cerr := tmp.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		return 0, err
	}
	return n, os.Rename(tmp.Name(), path)
}

type counter struct {
	r     io.Reader
	done  int64
	total int64
	fn    Progress
}

func (c *counter) Read(p []byte) (int, error) {
	n, err := c.r.Read(p)
	c.done += int64(n)
	c.fn(c.done, c.total)
	return n, err
}
