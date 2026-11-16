package profile

import "testing"

func TestReadAt(t *testing.T) {
	pkgs, err := ReadAt("../../testdata/profile/p")
	if err != nil {
		t.Fatal(err)
	}
	if len(pkgs) != 2 || pkgs[0].Name != "neovim" || pkgs[1].Name != "ripgrep" {
		t.Fatalf("%+v", pkgs)
	}
	nv := pkgs[0]
	if nv.Version != "0.12.6" || nv.Attr != "neovim" || nv.Rev != "5d2b81a7a3be3e2f832879028e4d60abbf58e9a0" {
		t.Fatalf("%+v", nv)
	}
	if _, ok := Find(pkgs, "ripgrep"); !ok {
		t.Fatal("Find")
	}
}

func TestMissingProfileIsEmpty(t *testing.T) {
	pkgs, err := ReadAt(t.TempDir())
	if err != nil || len(pkgs) != 0 {
		t.Fatal(pkgs, err)
	}
}
