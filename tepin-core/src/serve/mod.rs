//! Multi-process read access via in-driver serving (docs/serving.md).
//!
//! The process that wins the exclusive file lock can host an IPC listener
//! (`ServeMode::Host`); a process that loses the lock can discover that
//! host through a sidecar file and read through it (`ServeMode::Discover`).
//! Served reads run inside the writer's process as ordinary redb read
//! transactions, so every answer is a consistent MVCC snapshot — only
//! queries and results ever cross the process boundary, never raw pages.

pub(crate) mod client;
pub(crate) mod host;
pub(crate) mod sidecar;
mod wire;

/// Bumped whenever the wire protocol changes shape. A host and client
/// with different versions never talk — the client falls back to
/// `database_locked` instead of guessing.
pub(crate) const PROTOCOL_VERSION: u32 = 1;
