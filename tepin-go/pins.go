package tepin

// Stamped by the release workflow (see .github/workflows/release.yml):
// every driver release pins the exact libtepin build it was tested with.
// Empty in the development tree — set TEPIN_LIB to a local build there
// (cargo build -p tepin-ffi).

// libVersion is the GitHub release tag (without the leading v) whose
// libtepin assets this driver downloads.
const libVersion = ""

// libSHA256 pins each platform asset, keyed by "<os>-<arch>" as named on
// the release: libtepin-<key>.<dylib|so|dll>.
var libSHA256 = map[string]string{}
