#!/usr/bin/env bash
# M0 spike: does a hand-built directory sysext merge on this bootc image, how
# long does a refresh take, and do the -devel files work once merged?
#
# Run as root inside a bootc VM built from your own image, not on the daily
# driver. Writes /var/lib/extensions/nx-spike and nothing else; --remove
# takes it out again.
#
#   sudo spike/m0-sysext.sh                       # libxkbcommon-devel
#   sudo spike/m0-sysext.sh wayland-devel libxkbcommon-devel
#   sudo spike/m0-sysext.sh --remove
#
# Record what it prints in docs/design.md (M0) and answer the open questions
# there: refresh latency, SELinux labels, overlay headroom.
set -euo pipefail

name=nx-spike
ext=/var/lib/extensions/$name

die() { echo "m0: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

[[ $EUID -eq 0 ]] || die "run as root"
command -v systemd-sysext >/dev/null || die "systemd-sysext not found"

if [[ ${1:-} == --remove ]]; then
	rm -rf "$ext"
	t=$(date +%s%N); systemd-sysext refresh; echo "refresh after removal: $(ms "$t") ms"
	systemd-sysext status
	exit 0
fi

virt=$(systemd-detect-virt || true)
if [[ $virt == none && ${M0_ON_METAL:-} != 1 ]]; then
	die "not in a VM (systemd-detect-virt: none); set M0_ON_METAL=1 to run anyway"
fi

packages=("$@")
[[ ${#packages[@]} -gt 0 ]] || packages=(libxkbcommon-devel)

# shellcheck source=/dev/null
. /usr/lib/os-release
work=$(mktemp -d /var/tmp/nx-m0.XXXXXX)
trap 'rm -rf "$work"' EXIT

step "host"
echo "release   $ID $VERSION_ID ($PRETTY_NAME)"
echo "systemd   $(systemctl --version | head -1)"
echo "kernel    $(uname -r)"
echo "virt      $virt"
echo "selinux   $(getenforce 2>/dev/null || echo unknown)"
echo "/usr      $(findmnt -no FSTYPE,SOURCE /usr || echo 'not a separate mount')"
echo "/         $(findmnt -no FSTYPE,SOURCE /)"

step "already in the image"
for p in "${packages[@]}"; do
	if rpm -q "$p" >/dev/null 2>&1; then
		die "$p is already in the image; pick packages the image lacks"
	fi
	echo "$p: not installed (good)"
done

step "download (only the named packages, no dependency resolution)"
t=$(date +%s%N)
dnf download --destdir "$work/rpms" "${packages[@]}"
echo "download: $(ms "$t") ms"

step "unpack into $ext"
rm -rf "$ext"
mkdir -p "$ext"
t=$(date +%s%N)
for rpm in "$work"/rpms/*.rpm; do
	(cd "$ext" && rpm2cpio "$rpm" | cpio -idm --quiet)
done
# A sysext only merges /usr and /opt; anything else in the payload is lost.
for top in "$ext"/*; do
	case ${top##*/} in
	usr | opt) ;;
	*) echo "dropped (outside /usr and /opt): ${top##*/}"; rm -rf "$top" ;;
	esac
done
mkdir -p "$ext/usr/lib/extension-release.d"
printf 'ID=%s\nVERSION_ID=%s\n' "$ID" "$VERSION_ID" \
	>"$ext/usr/lib/extension-release.d/extension-release.$name"
echo "unpack: $(ms "$t") ms, $(find "$ext" -type f | wc -l) files, $(du -sh "$ext" | cut -f1)"

step "SELinux labels"
if command -v restorecon >/dev/null; then
	t=$(date +%s%N); restorecon -RF "$ext"; echo "restorecon: $(ms "$t") ms"
	sample=$(find "$ext/usr" -type f -name '*.pc' -print -quit)
	[[ -n $sample ]] && ls -Z "$sample"
fi

step "merge"
t=$(date +%s%N)
if ! systemd-sysext refresh; then
	journalctl -b -u systemd-sysext --no-pager | tail -20
	die "refresh failed: fall back to the prefix backend (record this in the design doc)"
fi
echo "refresh: $(ms "$t") ms"
systemd-sysext status
findmnt -no FSTYPE,OPTIONS /usr | tr ',' '\n' | grep -E '^(lowerdir|overlay)' || true

step "second refresh with no change (should be cheap; nx skips it anyway)"
t=$(date +%s%N); systemd-sysext refresh; echo "refresh: $(ms "$t") ms"

step "do the files work?"
for pc in "$ext"/usr/lib64/pkgconfig/*.pc "$ext"/usr/share/pkgconfig/*.pc; do
	[[ -e $pc ]] || continue
	mod=$(basename "$pc" .pc)
	printf '%s: ' "$mod"; pkg-config --cflags --libs "$mod" || echo "pkg-config FAILED"
done
sample=$(find "$ext/usr/include" -type f -name '*.h' -print -quit 2>/dev/null || true)
if [[ -n $sample ]]; then
	merged=/${sample#"$ext"/}
	[[ -r $merged ]] && echo "header visible at $merged" || echo "header NOT visible at $merged"
fi

cat <<EOF

Next, as your normal user in this VM: build a Bevy hello-world (cargo new,
cargo add bevy, cargo build) and record whether it links and runs. Then
reboot and check that systemd-sysext.service merges $name at boot.
Remove with: $0 --remove
EOF
