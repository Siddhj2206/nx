package storepath

import "testing"

func TestParse(t *testing.T) {
	cases := []struct{ in, name, version, output string }{
		{"/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0", "ripgrep", "15.2.0", ""},
		{"/nix/store/mkzp61qar527mf0fxhhykj76zlrvn5vk-pcsclite-2.5.2-lib", "pcsclite", "2.5.2", "lib"},
		{"/nix/store/9jvm8v958xbdskkb5ymd4kxxi30i0d8g-tk-8.6.16-man", "tk", "8.6.16", "man"},
		{"/nix/store/4dyfcf97ajakj4v9kkbg551025d88z5f-libedit-20260508-3.1", "libedit", "20260508-3.1", ""},
		{"/nix/store/myhwfdgqm39yms5clinafii9i1nmq43h-gcc-16.2.0-lib", "gcc", "16.2.0", "lib"},
		{"/nix/store/6jwxyrfwyiz9lqhq6qxkc1i33zk3gzph-ripgrep-15.2.0.drv", "ripgrep", "15.2.0", ""},
		{"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source", "source", "", ""},
		{"python3.13-requests-2.34.2", "python3.13-requests", "2.34.2", ""},
		{"mold-wrapper-2.40.1", "mold-wrapper", "2.40.1", ""},
	}
	for _, c := range cases {
		got := Parse(c.in)
		if got.Name != c.name || got.Version != c.version || got.Output != c.output {
			t.Errorf("Parse(%q) = %+v, want %s %s %s", c.in, got, c.name, c.version, c.output)
		}
	}
}

func TestHash(t *testing.T) {
	if h := Hash("/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0"); h != "whbrk657qmih065xal60m4khwf6cb9pj" {
		t.Fatal(h)
	}
	if h := Hash("whbrk657qmih065xal60m4khwf6cb9pj"); h != "whbrk657qmih065xal60m4khwf6cb9pj" {
		t.Fatal(h)
	}
}
