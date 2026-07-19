//! # tepin-embed
//!
//! Embedder *implementations* for TepinDB. The trait itself lives in
//! `tepin_core::embed` (the core drives the write→embed→search pipeline);
//! this crate supplies:
//!
//! - [`LazyEmbedder`] — the async-init wrapper: construction returns
//!   instantly, loading happens on a background thread, and only `embed()`
//!   awaits readiness. This is how `db.open()` stays instant while the
//!   model warms up behind it.
//! - `onnx` feature — the real thing: bge-small via onnxruntime, with
//!   lazy SHA-256-verified model download and loud truncation at the
//!   model's 512-token window.

mod lazy;

#[cfg(feature = "onnx")]
pub mod fetch;
#[cfg(feature = "onnx")]
mod onnx;

pub use lazy::{EmbedderStatus, LazyEmbedder};
#[cfg(feature = "onnx")]
pub use onnx::OnnxEmbedder;
// Re-exported so implementors and tests need only this crate.
pub use tepin_core::embed::{cosine, Embedder, Embedding, MockEmbedder};

/// Pinned identity of the default model. The db file records which model
/// produced its vectors, so dimension mismatches are detectable.
pub const DEFAULT_MODEL: &str = "bge-small-en-v1.5-int8";
pub const DEFAULT_DIM: usize = 384;
