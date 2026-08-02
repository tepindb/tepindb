//! # libtepin — the C ABI the Go and TypeScript drivers stand on
//!
//! Five functions, JSON in, JSON out. Every returned string is a
//! NUL-terminated UTF-8 buffer the caller must release with
//! [`tepin_free`]; every response is either `{"ok": …}` or the standard
//! TepinDB `{"error": {code, message, hint}}` — the same shapes the CLI
//! and MCP server answer with, because ops route through the one shared
//! dispatch surface (`tepin_core::ops`).
//!
//! ```c
//! char* tepin_version(void);
//! char* tepin_open(const char* options_json);   // {"ok":{"handle":1,...}}
//! char* tepin_call(uint64_t handle, const char* op, const char* args_json);
//! char* tepin_close(uint64_t handle);
//! void  tepin_free(char* ptr);
//! ```
//!
//! Handles are process-local u64s, safe to share across threads — the
//! engine is thread-safe and every call runs on the caller's thread.
//! Nothing here ever unwinds across the ABI: panics come back as
//! `{"error": {"code": "panic", …}}`.

pub mod driver;

use std::ffi::{c_char, CStr, CString};

use serde_json::{json, Value};
use tepin_core::TepinError;

fn to_c(v: Value) -> *mut c_char {
    let s = serde_json::to_string(&v).unwrap_or_else(|_| {
        r#"{"error":{"code":"io_error","message":"response serialization failed","hint":"this is a tepindb bug; please report it"}}"#.into()
    });
    // JSON strings never contain NUL; the fallback covers hostile input.
    CString::new(s)
        .unwrap_or_else(|_| CString::new(r#"{"error":{"code":"io_error","message":"response contained NUL","hint":"this is a tepindb bug; please report it"}}"#).unwrap())
        .into_raw()
}

fn envelope(r: Result<Value, TepinError>) -> Value {
    match r {
        Ok(v) => json!({"ok": v}),
        Err(e) => e.to_json(),
    }
}

/// Run `f` with panics converted to a JSON error — nothing unwinds
/// across the ABI.
fn guarded(f: impl FnOnce() -> Value + std::panic::UnwindSafe) -> *mut c_char {
    match std::panic::catch_unwind(f) {
        Ok(v) => to_c(v),
        Err(_) => to_c(json!({"error": {
            "code": "panic",
            "message": "an internal panic was caught at the FFI boundary",
            "hint": "this is a tepindb bug; please report it with TEPIN_TRACE=1 output",
        }})),
    }
}

/// `None` on NULL or non-UTF-8 input.
unsafe fn from_c<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn bad_input(what: &str) -> Value {
    json!({"error": {
        "code": "invalid_json",
        "message": format!("{what} must be a NUL-terminated UTF-8 string"),
        "hint": "pass valid UTF-8; JSON arguments are objects like {\"collection\": \"notes\"}",
    }})
}

/// Library, format, and op-surface introspection — call this first when
/// debugging a driver: `{"ok": {"version", "format_version", "embedding",
/// "ops"}}`.
#[no_mangle]
pub extern "C" fn tepin_version() -> *mut c_char {
    guarded(|| json!({"ok": driver::version_info()}))
}

/// Open a database; the one JSON argument is documented on
/// [`driver::open`]. Answers `{"ok": {"handle": N, "served": bool}}`.
///
/// # Safety
/// `options_json` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tepin_open(options_json: *const c_char) -> *mut c_char {
    let opts = unsafe { from_c(options_json) }.map(str::to_string);
    guarded(move || {
        let Some(opts) = opts else {
            return bad_input("options_json");
        };
        envelope(driver::open(&opts))
    })
}

/// Run one op against an open handle — op names, argument names, and
/// result shapes are exactly the CLI/MCP surface (`tepin_core::ops`).
///
/// # Safety
/// `op` and `args_json` must each be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tepin_call(
    handle: u64,
    op: *const c_char,
    args_json: *const c_char,
) -> *mut c_char {
    let op = unsafe { from_c(op) }.map(str::to_string);
    let args = unsafe { from_c(args_json) }.map(str::to_string);
    guarded(move || {
        let Some(op) = op else { return bad_input("op") };
        let args: Value = match args {
            None => json!({}),
            Some(s) if s.trim().is_empty() => json!({}),
            Some(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(e) => return TepinError::from(e).to_json(),
            },
        };
        envelope(driver::call(handle, &op, &args))
    })
}

/// Close a handle, releasing the file lock (and stopping its serve host,
/// when it had one). Closing an unknown handle is an error, not a crash.
#[no_mangle]
pub extern "C" fn tepin_close(handle: u64) -> *mut c_char {
    guarded(move || envelope(driver::close(handle)))
}

/// Release a string returned by any other libtepin function. NULL is a
/// no-op.
///
/// # Safety
/// `ptr` must be NULL or a pointer previously returned by a libtepin
/// function, released at most once.
#[no_mangle]
pub unsafe extern "C" fn tepin_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Call an extern fn and parse its JSON answer, freeing the buffer.
    fn read(ptr: *mut c_char) -> Value {
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        unsafe { tepin_free(ptr) };
        serde_json::from_str(&s).unwrap()
    }

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn open(options: &str) -> u64 {
        let v = read(unsafe { tepin_open(c(options).as_ptr()) });
        v["ok"]["handle"].as_u64().expect("open ok")
    }

    fn call(h: u64, op: &str, args: &str) -> Value {
        read(unsafe { tepin_call(h, c(op).as_ptr(), c(args).as_ptr()) })
    }

    #[test]
    fn version_reports_surface() {
        let v = read(tepin_version());
        assert!(v["ok"]["format_version"].as_u64().is_some());
        assert!(v["ok"]["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "query"));
    }

    #[test]
    fn full_crud_round_trip_through_the_abi() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ffi.tepin");
        let h = open(&json!({"path": path}).to_string());

        let ins = call(
            h,
            "insert",
            r#"{"collection": "notes", "doc": {"title": "hello", "stars": 4}}"#,
        );
        let id = ins["ok"]["inserted"].as_str().unwrap().to_string();

        let q = call(
            h,
            "query",
            r#"{"collection": "notes", "filter": {"stars": {"$gte": 3}}}"#,
        );
        assert_eq!(q["ok"]["count"], 1);

        let got = call(
            h,
            "get",
            &json!({"collection": "notes", "id": id}).to_string(),
        );
        assert_eq!(got["ok"]["doc"]["title"], "hello");

        let md = call(h, "inspect", &json!({"path": path}).to_string());
        assert!(md["ok"]["markdown"].as_str().unwrap().contains("notes"));

        read(tepin_close(h));
        // The lock is released: a fresh open works.
        let h2 = open(&json!({"path": path}).to_string());
        read(tepin_close(h2));
    }

    #[test]
    fn errors_keep_the_standard_shape() {
        let h = open(r#"{"in_memory": true}"#);
        let e = call(h, "get", r#"{"collection": "nope", "id": "x"}"#);
        assert_eq!(e["error"]["code"], "collection_not_found");
        assert!(!e["error"]["hint"].as_str().unwrap().is_empty());

        let e = call(h, "quyre", "{}");
        assert_eq!(e["error"]["code"], "not_implemented");
        assert!(e["error"]["hint"].as_str().unwrap().contains("query"));
        read(tepin_close(h));
    }

    #[test]
    fn bad_handles_and_bad_input_answer_cleanly() {
        let e = read(unsafe { tepin_call(999_999, c("query").as_ptr(), c("{}").as_ptr()) });
        assert_eq!(e["error"]["code"], "invalid_handle");

        let e = read(unsafe { tepin_call(1, std::ptr::null(), std::ptr::null()) });
        assert_eq!(e["error"]["code"], "invalid_json");

        let e = read(tepin_close(999_999));
        assert_eq!(e["error"]["code"], "invalid_handle");

        let e = read(unsafe {
            tepin_open(c(r#"{"existing": true, "path": "/definitely/not/here.tepin"}"#).as_ptr())
        });
        assert_eq!(e["error"]["code"], "file_not_found");
        unsafe { tepin_free(std::ptr::null_mut()) }; // NULL free is a no-op
    }

    #[test]
    fn open_existing_and_in_memory_modes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.tepin");
        let h = open(&json!({"path": path}).to_string());
        call(h, "insert", r#"{"collection": "kv", "doc": {"k": "v"}}"#);
        read(tepin_close(h));

        let h = open(&json!({"path": path, "existing": true}).to_string());
        let q = call(h, "query", r#"{"collection": "kv"}"#);
        assert_eq!(q["ok"]["count"], 1);
        read(tepin_close(h));
    }
}
