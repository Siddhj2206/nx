#!/usr/bin/env bash
# Records the rpmdbs the tests load as an image's installed set: `empty`
# (an image with nothing) and `xkbcommon` (testdata/rpm's libxkbcommon-devel,
# installed with --justdb). rpm 6 won't initialise a database unprivileged,
# so they are made as root in the image's Fedora. Each is one file,
# <name>/rpmdb.sqlite; a test puts it at the dbpath of the rpm it runs with.
#   tests/fixtures/rpmdb/record.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
rm -rf "$here/empty" "$here/xkbcommon"
podman run --rm --security-opt label=disable -v "$repo:/repo" quay.io/fedora/fedora:45 bash -euc '
cd /repo/tests/fixtures/rpmdb
for name in empty xkbcommon; do rpm --dbpath "$PWD/$name" --initdb; done
rpm --dbpath "$PWD/xkbcommon" -i --justdb --nodeps --nosignature --ignorearch \
	/repo/testdata/rpm/libxkbcommon-devel-1.13.1-4.fc45.x86_64.rpm
rm -f empty/.*.lock xkbcommon/.*.lock empty/rpmdb.sqlite-* xkbcommon/rpmdb.sqlite-*
'
