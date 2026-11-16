# Search and info read a catalog built from the channel's packages.json

Every nixpkgs channel release publishes `packages.json.br`: each package's
attribute, pname, version and meta (description, homepage, license,
mainProgram, unfree, broken, outputs to install). nx downloads it (10 MB),
stream-decodes the 418 MB of JSON with `encoding/json/v2`, and keeps a 20 MB
file of the fields it uses at `~/.cache/nx/catalog-<rev>`, one per nixpkgs
revision. Building it takes about 1.5 s and happens once per release. Loading
and searching all 150,000 packages takes about 80 ms, where `nix search`
takes seconds or more.

The catalog is keyed by revision so that search shows what `nx add` will
install. `nx upgrade` fetches the new release's catalog while Nix fetches the
new nixpkgs.

## Considered options

- **`nix search`.** Evaluates nixpkgs: 1 s cached, 20 s or more cold.
- **search.nixos.org's Elasticsearch.** A private API with embedded
  credentials, and its results don't match the pinned revision.
- **SQLite for the catalog.** Faster lookups we don't need, and a large
  dependency; a sorted flat file loads in 50 ms.

## Consequences

- Brotli needs `github.com/andybalholm/brotli` (pure Go).
- A pin that isn't a channel release, or one older than the newest, has no
  catalog of its own; the newest stands in until `nx upgrade`, and search
  says so.

**Revisit when:** the channel stops publishing packages.json, or the file
grows enough that loading it is noticeable.
