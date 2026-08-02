package tepin

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"time"
)

// platformKey matches the asset naming shared with the npm platform
// packages: darwin-arm64, linux-x64, win32-x64, …
func platformKey() string {
	osName := runtime.GOOS
	arch := runtime.GOARCH
	if osName == "windows" {
		osName = "win32"
	}
	switch arch {
	case "amd64":
		arch = "x64"
	case "arm64":
		arch = "arm64"
	}
	return osName + "-" + arch
}

func libFileName() string {
	switch runtime.GOOS {
	case "darwin":
		return "libtepin-" + platformKey() + ".dylib"
	case "windows":
		return "libtepin-" + platformKey() + ".dll"
	default:
		return "libtepin-" + platformKey() + ".so"
	}
}

// locateLibrary resolves the engine: TEPIN_LIB always wins; otherwise
// the user cache, filled by a one-time SHA-256-verified download from
// the project's GitHub release.
func locateLibrary() (string, error) {
	if explicit := os.Getenv("TEPIN_LIB"); explicit != "" {
		if _, err := os.Stat(explicit); err != nil {
			return "", &Error{
				Code:    "library_load_failed",
				Message: fmt.Sprintf("TEPIN_LIB points at %s, which does not exist", explicit),
				Hint:    "fix or unset TEPIN_LIB; a dev build is `cargo build -p tepin-ffi` in the tepindb repo",
			}
		}
		return explicit, nil
	}

	if libVersion == "" {
		return "", &Error{
			Code:    "library_load_failed",
			Message: "this development build of the driver has no pinned libtepin release",
			Hint:    "set TEPIN_LIB to a built library (cargo build -p tepin-ffi → target/debug/libtepin_ffi.*), or use a tagged driver release",
		}
	}

	pin, ok := libSHA256[platformKey()]
	if !ok {
		return "", &Error{
			Code:    "library_load_failed",
			Message: fmt.Sprintf("no prebuilt libtepin for %s in driver release %s", platformKey(), libVersion),
			Hint:    "build from source (cargo build -p tepin-ffi) and set TEPIN_LIB",
		}
	}

	cacheRoot, err := os.UserCacheDir()
	if err != nil {
		return "", &Error{
			Code:    "library_load_failed",
			Message: fmt.Sprintf("no usable cache directory: %v", err),
			Hint:    "set TEPIN_LIB to a library path instead",
		}
	}
	dest := filepath.Join(cacheRoot, "tepindb", "lib", libVersion, libFileName())
	if _, err := os.Stat(dest); err == nil {
		return dest, nil
	}
	if err := download(dest, pin); err != nil {
		return "", err
	}
	return dest, nil
}

// download fetches the pinned release asset into dest (atomically, via a
// temp file), verifying its SHA-256 before it is ever loadable. Same
// supply-chain model as the embedding model download: GitHub releases
// only, pinned digest, no fallback hosts.
func download(dest, pinnedSHA string) *Error {
	url := fmt.Sprintf(
		"https://github.com/tepindb/tepindb/releases/download/v%s/%s",
		libVersion, libFileName(),
	)
	netErr := func(detail string) *Error {
		return &Error{
			Code:    "library_download_failed",
			Message: fmt.Sprintf("could not download %s: %s", url, detail),
			Hint:    "check network access to github.com, or set TEPIN_LIB to a locally built library",
		}
	}

	if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
		return netErr(err.Error())
	}
	client := &http.Client{Timeout: 5 * time.Minute}
	resp, err := client.Get(url)
	if err != nil {
		return netErr(err.Error())
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return netErr(fmt.Sprintf("HTTP %d", resp.StatusCode))
	}

	tmp, err := os.CreateTemp(filepath.Dir(dest), ".libtepin-*")
	if err != nil {
		return netErr(err.Error())
	}
	defer os.Remove(tmp.Name())

	hasher := sha256.New()
	if _, err := io.Copy(io.MultiWriter(tmp, hasher), resp.Body); err != nil {
		tmp.Close()
		return netErr(err.Error())
	}
	if err := tmp.Close(); err != nil {
		return netErr(err.Error())
	}

	got := hex.EncodeToString(hasher.Sum(nil))
	if got != pinnedSHA {
		return &Error{
			Code:    "checksum_mismatch",
			Message: fmt.Sprintf("downloaded libtepin digest %s does not match the pinned %s", got, pinnedSHA),
			Hint:    "the download was corrupted or tampered with; retry, and report it if this persists",
		}
	}
	if err := os.Rename(tmp.Name(), dest); err != nil {
		return netErr(err.Error())
	}
	return nil
}
