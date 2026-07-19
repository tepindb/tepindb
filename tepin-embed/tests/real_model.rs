//! End-to-end test against the REAL bge-small model: downloads (once, into
//! the shared cache) with pinned-hash verification, loads via the lazy
//! machinery, and checks actual semantics.
//!
//! Ignored by default — run with:
//!   cargo test -p tepin-embed --features onnx -- --ignored
#![cfg(feature = "onnx")]

use std::time::{Duration, Instant};

use tepin_embed::fetch::{default_cache_dir, BGE_SMALL};
use tepin_embed::{cosine, Embedder, EmbedderStatus, OnnxEmbedder};

#[test]
#[ignore = "downloads and runs the real 34MB model"]
fn real_bge_small_end_to_end() {
    let cache = default_cache_dir().unwrap();

    // Instant construction even when the model must be fetched
    let t0 = Instant::now();
    let lazy = OnnxEmbedder::spawn_lazy(&BGE_SMALL, cache);
    assert!(
        t0.elapsed() < Duration::from_millis(100),
        "spawn_lazy must not block, took {:?}",
        t0.elapsed()
    );
    assert_eq!(lazy.dim(), 384);
    assert_eq!(lazy.model_id(), "bge-small-en-v1.5-int8");

    // First embed blocks until (download +) load completes
    let hello = lazy.embed("hello world").unwrap();
    println!("time to first embedding: {:?}", t0.elapsed());
    assert_eq!(lazy.status(), EmbedderStatus::Ready);
    assert_eq!(hello.vector.len(), 384);
    assert!(!hello.truncated);

    // Unit norm (bge is L2-normalized)
    let norm: f32 = hello.vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "norm was {norm}");

    // Determinism
    let hello2 = lazy.embed("hello world").unwrap();
    assert_eq!(hello.vector, hello2.vector);

    // Semantics: a paraphrase must beat an unrelated text
    let near = lazy.embed("hi there, world!").unwrap();
    let far = lazy
        .embed("quarterly tax depreciation schedules for industrial equipment")
        .unwrap();
    let sim_near = cosine(&hello.vector, &near.vector);
    let sim_far = cosine(&hello.vector, &far.vector);
    println!("sim(hello, paraphrase) = {sim_near:.3}, sim(hello, unrelated) = {sim_far:.3}");
    assert!(
        sim_near > sim_far + 0.05,
        "expected clear semantic separation, got near={sim_near} far={sim_far}"
    );

    // Retrieval-shaped check: the right document wins for a query
    let query = lazy.embed("how do I reset my password").unwrap();
    let doc_good = lazy
        .embed("To reset your password, open account settings and click 'forgot password'.")
        .unwrap();
    let doc_bad = lazy
        .embed("The chef reduced the sauce over low heat.")
        .unwrap();
    assert!(cosine(&query.vector, &doc_good.vector) > cosine(&query.vector, &doc_bad.vector));

    // Loud truncation past the 512-token window
    let long_text = "embedding ".repeat(2000);
    let long = lazy.embed(&long_text).unwrap();
    assert!(long.truncated, "2000 words must report truncation");
    assert_eq!(long.vector.len(), 384);
    let short = lazy.embed("short text").unwrap();
    assert!(!short.truncated);

    // Empty input is safe
    let empty = lazy.embed("").unwrap();
    assert_eq!(empty.vector.len(), 384);
}
