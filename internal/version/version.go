// Package version is the version nx was built as.
package version

import "runtime/debug"

// Version is set at release time with -ldflags "-X ...version.Version=v1.2.3".
var Version = ""

// String is the release version, or the module version or VCS revision of a
// development build.
func String() string {
	if Version != "" {
		return Version
	}
	info, ok := debug.ReadBuildInfo()
	if !ok {
		return "dev"
	}
	if v := info.Main.Version; v != "" && v != "(devel)" {
		return v
	}
	for _, s := range info.Settings {
		if s.Key == "vcs.revision" && len(s.Value) >= 7 {
			return "dev-" + s.Value[:7]
		}
	}
	return "dev"
}
