# nx

Homebrew-style packages from nixpkgs, for your user, without root.

```
$ nx add gnuplot
  › Installing gnuplot 6.0.5
  › Fetching 12 packages · 7.8 MiB download, 24.7 MiB unpacked
    ↓ libcerf                3.9            154 KiB
    ↓ libaom                 3.15.2         3.2 MiB
    ↓ gnuplot                6.0.5          1.3 MiB
    …

  + gnuplot  6.0.5

  ✓ 1 added · 28 unchanged · 7.8 MiB in 6.9s
```

Every package comes from one pinned nixpkgs release, and `nx upgrade` moves
them all forward together. Search, info, list and outdated read a local
catalog and your profile directly, so they return in milliseconds.

| | |
|---|---|
| `nx add <name>...` | install (`--unfree` for unfree packages) |
| `nx remove <name>...` | remove |
| `nx list` | what's installed |
| `nx search <term>...` | find packages |
| `nx info <name>` | details, size, dependencies |
| `nx try <name> [-- cmd]` | run without installing |
| `nx upgrade` | newest nixpkgs, everything upgraded |
| `nx outdated` | what upgrade would change |
| `nx history` / `nx rollback` | past versions of your package set |
| `nx clean` | drop old history, free disk space |

Needs Nix with flakes and `nix-command` enabled. Homebrew spellings such as
`node@22` and `python@3.13` work.

## Building

```
make build      # ./dist/nx
make test vet
```

See [docs/product.md](docs/product.md) for the design.
