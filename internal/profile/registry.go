package profile

import (
	"encoding/json/v2"
	"os"
	"path/filepath"
	"time"
)

// Pin is the nixpkgs revision the user registry pins. Every nx package
// comes from it, and only `nx upgrade` moves it.
type Pin struct {
	Rev      string
	Modified time.Time // the commit's time
}

// Short is the first seven characters of the revision.
func (p Pin) Short() string {
	if len(p.Rev) > 7 {
		return p.Rev[:7]
	}
	return p.Rev
}

type registry struct {
	Flakes []struct {
		From struct {
			ID   string `json:"id"`
			Type string `json:"type"`
		} `json:"from"`
		To struct {
			Type         string `json:"type"`
			Owner        string `json:"owner"`
			Repo         string `json:"repo"`
			Rev          string `json:"rev"`
			LastModified int64  `json:"lastModified"`
		} `json:"to"`
	} `json:"flakes"`
}

// RegistryPath is the user's flake registry.
func RegistryPath() string {
	dir, err := os.UserConfigDir()
	if err != nil {
		home, _ := os.UserHomeDir()
		dir = filepath.Join(home, ".config")
	}
	return filepath.Join(dir, "nix", "registry.json")
}

// ReadPin returns the user registry's nixpkgs pin. ok is false when nixpkgs
// isn't pinned to a revision.
func ReadPin() (Pin, bool) {
	data, err := os.ReadFile(RegistryPath())
	if err != nil {
		return Pin{}, false
	}
	var r registry
	if json.Unmarshal(data, &r) != nil {
		return Pin{}, false
	}
	for _, f := range r.Flakes {
		if f.From.ID == "nixpkgs" && f.From.Type == "indirect" && f.To.Rev != "" {
			return Pin{Rev: f.To.Rev, Modified: time.Unix(f.To.LastModified, 0)}, true
		}
	}
	return Pin{}, false
}
