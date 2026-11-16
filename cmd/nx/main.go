// Command nx installs and upgrades packages from nixpkgs into the user's Nix
// profile, with output modelled on Homebrew and dnf.
package main

import (
	"os"

	"github.com/Siddhj2206/nx/internal/cli"
)

func main() {
	os.Exit(cli.Main(os.Args[1:]))
}
