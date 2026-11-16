package ui

import "testing"

func TestTruncateKeepsColour(t *testing.T) {
	s := Style{on: true}
	in := s.Bold("ripgrep") + " " + s.Dim("15.2.0")
	if VisibleLen(in) != 14 {
		t.Fatal(VisibleLen(in))
	}
	got := Truncate(in, 10)
	if VisibleLen(got) != 10 || got[len(got)-4:] != "\x1b[0m" {
		t.Fatalf("%q", got)
	}
	if Truncate("short", 10) != "short" {
		t.Fatal("cut a short string")
	}
}

func TestSize(t *testing.T) {
	for n, want := range map[int64]string{0: "0 B", 512: "512 B", 2048: "2 KiB", 3 << 20: "3.0 MiB", 5 << 30: "5.0 GiB"} {
		if got := Size(n); got != want {
			t.Errorf("Size(%d) = %s, want %s", n, got, want)
		}
	}
}
