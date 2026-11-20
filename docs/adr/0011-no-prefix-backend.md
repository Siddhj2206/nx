# One way to activate: the sysext merge

nx 1.0 had two activation backends. `sysext` merges the generation's
extension over `/usr`; `prefix`, the fallback, left it unmerged and `nx
shell` exported the generation's `usr` as a prefix (`PATH`,
`PKG_CONFIG_PATH`, `CPATH`, `LIBRARY_PATH`, `LD_LIBRARY_PATH`). nxd probed
sysext on every activation and fell back when the merge failed;
`nx-settle.service` (`nx select --after-merge`) did the same check after
the boot-time merge; `[host] backend` forced either; `/var/lib/nx/backend`
recorded which was in use.

The design made prefix first-class because sysext on bootc was the
biggest risk (distro-dependent support, overlay stacking depth). M0 and
every Neptuno release since merged without a fallback, and the prefix
backend never set up services, users or files, so a host tier under it
was half there anyway. It cost a boot unit, a hidden command, a state
file nxd wrote as root, a manifest key, a protocol field, and a test path
through every activation.

nx 1.1 has one way: nxd merges, and a merge that fails puts the previous
generation and link back and is an error that carries what
`systemd-sysext` said. `nx doctor` says whether nx's extension is merged,
and points to the journal when it isn't.

## Considered

- **Keep `nx shell` as an error for one release.** One administrator
  reads the changelog; a removed command says enough.
- **Keep the fallback, drop only the boot-time settle.** The fallback is
  most of the cost, and nothing exercised it.

## Consequences

- A kernel or image where the overlay won't stack fails activations
  loudly instead of degrading; the generation stays built, and the
  previous one stays live.
- nxd's "what is merged now" is the extension link, not a state file:
  `/var/lib/nx/backend` is no longer read or written (a 1.0 machine's
  stays behind, harmless).
- An nx 1.0 request naming a `backend` is still accepted; the field is
  ignored. `[host] backend` in a manifest is ignored and dropped the next
  time nx writes it.
