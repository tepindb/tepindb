//! The embedding seam. tepin-core defines the trait and drives the
//! write→embed→search pipeline; implementations live in tepin-embed
//! (ONNX + bge-small) — the application controls which model loads and
//! when, never the core.

use crate::Result;

/// One embedded text. `truncated` is the loud part of loud truncation:
/// callers surface it, they never swallow it.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    pub vector: Vec<f32>,
    pub truncated: bool,
}

pub trait Embedder: Send + Sync {
    /// Model identifier recorded next to the vectors it produces — the db
    /// refuses to mix vectors from different models.
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Result<Embedding>;
}

/// Cosine similarity of two equal-length vectors — the scoring primitive
/// for brute-force search. bge vectors are L2-normalized, so this is a
/// plain dot product for them; the norms keep it correct for any input.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// A deterministic, dependency-free embedder for tests and for exercising
/// the pipeline without a model: same text → same unit vector.
/// No semantic meaning — only determinism, dimensionality, and speed.
pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        "mock"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Result<Embedding> {
        // FNV-ish rolling hash seeds a tiny xorshift PRNG per dimension.
        let mut seed = 0xcbf2_9ce4_8422_2325u64;
        for b in text.bytes() {
            seed = (seed ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        let mut vector = Vec::with_capacity(self.dim);
        let mut x = seed.max(1);
        for _ in 0..self.dim {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            vector.push(((x as f64 / u64::MAX as f64) as f32) * 2.0 - 1.0);
        }
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        for v in &mut vector {
            *v /= norm;
        }
        Ok(Embedding {
            vector,
            truncated: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0, "zero vector is safe");
    }

    #[test]
    fn mock_is_deterministic_unit_vectors() {
        let m = MockEmbedder::new(16);
        let a = m.embed("hello").unwrap();
        let b = m.embed("hello").unwrap();
        let c = m.embed("different").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        let norm: f32 = a.vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }
}
