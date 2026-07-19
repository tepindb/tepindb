//! Alias for [`tepindb`] — the AI-first single-file database for CLI tools
//! and agents.
//!
//! Depend on [`tepindb`](https://crates.io/crates/tepindb) directly for the
//! library (this crate just re-exports it), or install
//! [`tepin-cli`](https://crates.io/crates/tepin-cli) for the `tepin` binary.
//! Enable this crate's `embedding` feature for built-in vector search.

pub use tepindb::*;
