# Changelog

What changed in each release of nx. Written by `dev/release` from the
commit history (git-cliff, `cliff.toml`); edit the commits, not this file.

## [1.0.1] - 2026-11-20

### Fixes

- Resolve other images against their own rpmdb
- **nx-solv:** Load one rpmdb at a time

## [1.0.0] - 2026-11-20

### Features

- Start nx 1.0 in Rust with nx status
- **cli:** Bring back nx 0.x's output
- **host:** Resolve host-tier packages against the image with libsolv
- **librpm:** Verify and unpack package files with the image's librpm
- **host:** Build generations from the store; add list, why and repo
- **activate:** nxd, polkit and the sysext and prefix backends
- **activate:** Several administrators, and invariant 1 again at activation
- Translate scriptlets and set up services (M4)
- Image upgrade lifecycle (M5)
- Partial re-resolve, recorded sync failures and a testable boot digest
- Switch to the prefix backend when boot's merge fails, and keep skipped packages out of nx add
- The tool tier (M6)
- nx apply, a weekly tool upgrade and desktop notifications
- nx doctor, nx migrate and the image hint for kernel modules
- Shell completions with live values, and man pages
- nx add --without, so --override has one meaning
- nx 1.0, the Rust rebuild (#5)

### Fixes

- **host:** Retry transient download failures and blame a skipped repo
- **host:** Read metadata from the cache and keep search in step with add
- **host:** Close the M2 review findings
- **activate:** Close the M3 review findings
- **activate:** Treat a symlink to /dev/null as no unit, and document M4's limits
- Close the M5 review findings
- Close the review findings on partial re-resolve
- Settle leaves placeholders and forced sysext alone, and records before unlinking
- Close the M6 review findings
- Close the remaining M6 review findings
- Close the review findings on the one-source refactor
- Refuse a tool package whose every command already exists
- Close the first four findings of the 1.0 audit
- Merge root's copy of a generation, not the builder's files
- Pin repo keys by content, named from the lockfile
- Stream downloads, read mirrorlists, quieter notify, more dry runs

## [0.2.0] - 2026-11-17

### Features

- **add:** Warn when an earlier PATH entry shadows a new command

## [0.1.0] - 2026-11-16

### Features

- nx in Go, a Homebrew-style front end to a Nix profile
