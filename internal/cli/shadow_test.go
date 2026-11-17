package cli

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Siddhj2206/nx/internal/profile"
)

func touch(t *testing.T, dir, name string, mode os.FileMode) {
	t.Helper()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, name), nil, mode); err != nil {
		t.Fatal(err)
	}
}

func TestShadowed(t *testing.T) {
	root := t.TempDir()
	store := filepath.Join(root, "store", "clang")
	for _, n := range []string{"cc", "clang", "ld"} {
		touch(t, filepath.Join(store, "bin"), n, 0o755)
	}
	// The profile's bin, reached through a symlink as ~/.nix-profile is.
	profileBin := filepath.Join(root, "profile", "bin")
	if err := os.MkdirAll(profileBin, 0o755); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(root, "nix-profile")
	if err := os.Symlink(filepath.Join(root, "profile"), link); err != nil {
		t.Fatal(err)
	}
	usr := filepath.Join(root, "usr", "bin")
	touch(t, usr, "cc", 0o755)
	touch(t, usr, "ld", 0o755)
	touch(t, usr, "clang", 0o644) // not executable: doesn't count
	later := filepath.Join(root, "later")
	touch(t, later, "clang", 0o755) // after the profile: doesn't count

	pkg := profile.Package{Name: "clang", StorePaths: []string{store}}
	path := strings.Join([]string{usr, filepath.Join(link, "bin"), later}, ":")
	got := shadowed(pkg, profileBin, path)
	if len(got) != 1 || got[0].dir != usr || strings.Join(got[0].names, ",") != "cc,ld" {
		t.Fatalf("%+v", got)
	}

	// Profile first: nothing is shadowed.
	if got := shadowed(pkg, profileBin, filepath.Join(link, "bin")+":"+usr); len(got) != 0 {
		t.Fatalf("%+v", got)
	}
	// Profile not on PATH: nothing to say per package.
	if got := shadowed(pkg, profileBin, usr); len(got) != 0 {
		t.Fatalf("%+v", got)
	}
}

func TestListNames(t *testing.T) {
	if got := listNames([]string{"a", "b", "c"}, 2); got != "a, b and 1 more" {
		t.Fatal(got)
	}
}
