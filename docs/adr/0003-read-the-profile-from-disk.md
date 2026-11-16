# nx reads the profile manifest and registry from disk

What's installed comes from `~/.local/state/nix/profiles/profile/manifest.json`
(version 3), its generations from the `profile-N-link` entries beside it, and
the pin from `~/.config/nix/registry.json`. Reading them takes microseconds;
`nix profile list --json` and `nix flake metadata` take a tenth of a second
or more each, and the bash nx called them several times per command.

## Consequences

- A manifest version nx doesn't know is an error that says to run `nix
  profile list` once (which upgrades it). Writing stays with nix
  ([ADR-0001](0001-nix-does-every-store-write.md)).

**Revisit when:** Nix changes the manifest format again.
