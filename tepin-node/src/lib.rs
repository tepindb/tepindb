//! # tepin.node — the Node addon behind the tepindb TypeScript driver
//!
//! Four functions, JSON strings in, JSON envelope strings out —
//! `{"ok": …}` or the standard `{"error": {code, message, hint}}`. The
//! addon never throws for database errors and never blocks the event
//! loop for db work: `call` runs on the libuv worker pool and returns a
//! Promise. All logic lives in `tepin_ffi::driver` / `tepin_core::ops`;
//! this file only marshals.

use napi::bindgen_prelude::*;
use napi::Task;
use napi_derive::napi;
use serde_json::{json, Value};
use tepin_ffi::driver;

fn envelope(r: std::result::Result<Value, tepin_core::TepinError>) -> String {
    let v = match r {
        Ok(v) => json!({"ok": v}),
        Err(e) => e.to_json(),
    };
    v.to_string()
}

fn parse_args(args_json: &str) -> std::result::Result<Value, tepin_core::TepinError> {
    if args_json.trim().is_empty() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(args_json)?)
}

fn run_call(handle: u64, op: &str, args_json: &str) -> String {
    let run = std::panic::catch_unwind(|| {
        envelope(parse_args(args_json).and_then(|args| driver::call(handle, op, &args)))
    });
    run.unwrap_or_else(|_| {
        json!({"error": {
            "code": "panic",
            "message": "an internal panic was caught at the addon boundary",
            "hint": "this is a tepindb bug; please report it with TEPIN_TRACE=1 output",
        }})
        .to_string()
    })
}

/// `{"ok": {"version", "format_version", "embedding", "ops"}}`.
#[napi]
pub fn version() -> String {
    json!({"ok": driver::version_info()}).to_string()
}

/// Open a database from JSON options (see `tepin_ffi::driver::open`).
/// Sync because opening is fast — except with `retry_ms`, where the
/// caller opted into waiting.
#[napi]
pub fn open_sync(options_json: String) -> String {
    std::panic::catch_unwind(|| envelope(driver::open(&options_json))).unwrap_or_else(|_| {
        json!({"error": {"code": "panic", "message": "an internal panic was caught at the addon boundary", "hint": "this is a tepindb bug; please report it"}}).to_string()
    })
}

pub struct CallTask {
    handle: u64,
    op: String,
    args_json: String,
}

impl Task for CallTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        Ok(run_call(self.handle, &self.op, &self.args_json))
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Run one op off the event loop; resolves to a JSON envelope string.
/// Op names, argument names, and result shapes are exactly the CLI/MCP
/// surface.
#[napi(ts_return_type = "Promise<string>")]
pub fn call(handle: i64, op: String, args_json: String) -> AsyncTask<CallTask> {
    AsyncTask::new(CallTask {
        handle: handle as u64,
        op,
        args_json,
    })
}

/// The synchronous variant, for scripts and REPLs where blocking is fine.
#[napi]
pub fn call_sync(handle: i64, op: String, args_json: String) -> String {
    run_call(handle as u64, &op, &args_json)
}

/// Close a handle, releasing the file lock.
#[napi]
pub fn close_sync(handle: i64) -> String {
    envelope(driver::close(handle as u64))
}
