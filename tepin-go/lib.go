package tepin

import (
	"encoding/json"
	"fmt"
	"sync"
	"unsafe"

	"github.com/ebitengine/purego"
)

// library holds the registered libtepin entry points. One per process,
// loaded lazily on first use.
type library struct {
	version func() uintptr
	open    func(string) uintptr
	call    func(uintptr, string, string) uintptr
	close   func(uintptr) uintptr
	free    func(uintptr)
}

var (
	libOnce sync.Once
	lib     *libraryFacade
	libErr  error
)

// libraryFacade wraps the raw entry points with string marshaling and
// envelope parsing, so the rest of the driver never touches pointers.
type libraryFacade struct {
	raw library
}

func load() (*libraryFacade, error) {
	libOnce.Do(func() {
		path, err := locateLibrary()
		if err != nil {
			libErr = err
			return
		}
		handle, err := openLibrary(path)
		if err != nil {
			libErr = &Error{
				Code:    "library_load_failed",
				Message: fmt.Sprintf("could not load libtepin from %s: %v", path, err),
				Hint:    "the file may be for another platform or truncated; delete it and retry, or point TEPIN_LIB at a good build",
			}
			return
		}
		var raw library
		purego.RegisterLibFunc(&raw.version, handle, "tepin_version")
		purego.RegisterLibFunc(&raw.open, handle, "tepin_open")
		purego.RegisterLibFunc(&raw.call, handle, "tepin_call")
		purego.RegisterLibFunc(&raw.close, handle, "tepin_close")
		purego.RegisterLibFunc(&raw.free, handle, "tepin_free")
		lib = &libraryFacade{raw: raw}
	})
	return lib, libErr
}

// take copies the NUL-terminated UTF-8 answer and frees the C buffer.
func (l *libraryFacade) take(ptr uintptr) string {
	if ptr == 0 {
		return `{"error":{"code":"library_load_failed","message":"libtepin returned NULL","hint":"this is a tepindb bug; please report it"}}`
	}
	defer l.raw.free(ptr)
	// The address came from Rust's allocator, never the Go heap, so the
	// uintptr→Pointer conversion vet warns about cannot trip the GC; the
	// &ptr reinterpret states that explicitly.
	base := *(*unsafe.Pointer)(unsafe.Pointer(&ptr))
	n := 0
	for *(*byte)(unsafe.Add(base, n)) != 0 {
		n++
	}
	return string(unsafe.Slice((*byte)(base), n))
}

// unwrap splits the {"ok": …} / {"error": …} envelope.
func unwrap(raw string) (json.RawMessage, *Error) {
	var env struct {
		Ok  json.RawMessage `json:"ok"`
		Err *Error          `json:"error"`
	}
	if err := json.Unmarshal([]byte(raw), &env); err != nil {
		return nil, &Error{
			Code:    "invalid_json",
			Message: fmt.Sprintf("libtepin answered non-JSON: %v", err),
			Hint:    "the library and driver may be incompatible versions; upgrade both",
		}
	}
	if env.Err != nil {
		return nil, env.Err
	}
	return env.Ok, nil
}

func (l *libraryFacade) version() (json.RawMessage, *Error) {
	return unwrap(l.take(l.raw.version()))
}

func (l *libraryFacade) open(optionsJSON string) (json.RawMessage, *Error) {
	return unwrap(l.take(l.raw.open(optionsJSON)))
}

func (l *libraryFacade) call(handle uintptr, op, argsJSON string) (json.RawMessage, *Error) {
	return unwrap(l.take(l.raw.call(handle, op, argsJSON)))
}

func (l *libraryFacade) close(handle uintptr) (json.RawMessage, *Error) {
	return unwrap(l.take(l.raw.close(handle)))
}
