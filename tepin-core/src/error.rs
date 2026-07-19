//! The TepinDB error contract: every error carries a stable machine `code`,
//! a human `message`, and a `hint` telling the caller what to do next.
//! The registry of codes lives in `docs/errors.md`.

use std::fmt;

pub type Result<T> = std::result::Result<T, TepinError>;

#[derive(Debug)]
pub struct TepinError {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl TepinError {
    pub fn new(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: hint.into(),
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The JSON shape shared by the CLI and MCP surfaces.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "hint": self.hint,
            }
        })
    }
}

impl fmt::Display for TepinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (hint: {})", self.code, self.message, self.hint)
    }
}

impl std::error::Error for TepinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for TepinError {
    fn from(e: std::io::Error) -> Self {
        TepinError::new(
            "io_error",
            format!("i/o failure: {e}"),
            "check the file path exists and is readable/writable",
        )
        .with_source(e)
    }
}

impl From<serde_json::Error> for TepinError {
    fn from(e: serde_json::Error) -> Self {
        TepinError::new(
            "invalid_json",
            format!("could not parse JSON: {e}"),
            "check the JSON syntax; documents and filters must be valid JSON objects",
        )
        .with_source(e)
    }
}

macro_rules! from_redb {
    ($($ty:ty),+) => {$(
        impl From<$ty> for TepinError {
            fn from(e: $ty) -> Self {
                TepinError::new(
                    "storage_error",
                    format!("storage engine failure: {e}"),
                    "the database file may be corrupt or locked by another process; run `tepin inspect` to check it",
                )
                .with_source(e)
            }
        }
    )+};
}

from_redb!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError
);
