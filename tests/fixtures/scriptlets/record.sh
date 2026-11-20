#!/usr/bin/env bash
# Records a package's real scriptlets, triggers and unit files from its RPM
# into tests/fixtures/scriptlets/<name>/, using the `rpm` command as ground
# truth (nothing here is written by hand). The scriptlet translator's corpus
# tests read these directories; so does nx-librpm's reader test, which must
# agree with what `rpm` prints.
#   tests/fixtures/scriptlets/record.sh path/to/foo-1.0-1.fc45.x86_64.rpm...
#
# Per package, in <name>/ (a file exists only when the tag does):
#   pretrans pre post preun postun posttrans preuntrans postuntrans
#       the script text; <name>.prog holds the interpreter when it isn't
#       /bin/sh, or when the body is empty (rpm keeps an empty `-p /bin/sh`)
#   triggers.tsv   name <TAB> type <TAB> version <TAB> script index
#   trigger.<n>    the script for index n
#   filetriggers.tsv, filetrigger.<n>   the same for file triggers, and
#   transfiletriggers.tsv, transfiletrigger.<n> for transaction file triggers
#   units          unit files, sysusers.d, tmpfiles.d and preset files shipped
#   nevra          the package's name-version-release.arch
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)

sep='----nx-record-separator----'

for rpmfile in "$@"; do
	name=$(rpm -qp --qf '%{NAME}' "$rpmfile")
	out=$here/$name
	rm -rf "$out"
	mkdir -p "$out"
	rpm -qp --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\n' "$rpmfile" >"$out/nevra"

	for pair in PRETRANS:pretrans PREIN:pre POSTIN:post PREUN:preun POSTUN:postun \
		POSTTRANS:posttrans PREUNTRANS:preuntrans POSTUNTRANS:postuntrans; do
		tag=${pair%%:*}
		file=${pair##*:}
		body=$(rpm -qp --qf "%{$tag}" "$rpmfile")
		prog=$(rpm -qp --qf "%{${tag}PROG}" "$rpmfile")
		# rpm prints "(none)" for a missing tag.
		[[ $body == "(none)" ]] && body=""
		[[ $prog == "(none)" ]] && prog=""
		if [[ -n $body ]]; then
			printf '%s\n' "$body" >"$out/$file"
		fi
		if [[ -n $prog && ( $prog != /bin/sh || -z $body ) ]]; then
			printf '%s\n' "$prog" >"$out/$file.prog"
		fi
	done

	for kind in TRIGGER FILETRIGGER TRANSFILETRIGGER; do
		lower=$(tr '[:upper:]' '[:lower:]' <<<"$kind")
		list=$(rpm -qp --qf "[%{${kind}NAME}\t%{${kind}TYPE}\t%{${kind}VERSION}\t%{${kind}INDEX}\n]" "$rpmfile" 2>/dev/null || true)
		[[ $list == "(none)" ]] && list=""
		[[ -z $list ]] && continue
		printf '%s\n' "$list" >"$out/${lower}s.tsv"
		scripts=$(rpm -qp --qf "[%{${kind}SCRIPTS}\n$sep\n]" "$rpmfile")
		# One script per index, separated by the marker line.
		n=0
		buf=""
		while IFS= read -r line; do
			if [[ $line == "$sep" ]]; then
				printf '%s' "$buf" >"$out/$lower.$n"
				n=$((n + 1))
				buf=""
			else
				buf+="$line"$'\n'
			fi
		done <<<"$scripts"
	done

	rpm -qp --list "$rpmfile" |
		grep -E '^/(usr/)?lib/(systemd/(system|user|system-preset|user-preset)|sysusers\.d|tmpfiles\.d)/|^/etc/(sysusers|tmpfiles)\.d/' \
			>"$out/units" || true
	[[ -s $out/units ]] || rm -f "$out/units"
	echo "recorded $name ($(find "$out" -type f | wc -l) files)"
done
