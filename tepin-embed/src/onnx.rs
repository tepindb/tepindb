//! The real embedder: bge-small via onnxruntime.
//!
//! bge sentence embeddings are the CLS token of the last hidden state,
//! L2-normalized (per the model card). Input beyond the 512-token window
//! is truncated *loudly*: the returned [`Embedding`] carries
//! `truncated: true` and callers surface it.

use std::path::Path;
use std::sync::Mutex;

use tepin_core::embed::{Embedder, Embedding};
use tepin_core::{Result, TepinError};
use tokenizers::Tokenizer;

use crate::fetch::{self, ModelSpec};
use crate::lazy::LazyEmbedder;

/// bge's sequence limit; the tokenizer truncates here.
const MAX_TOKENS: usize = 512;

pub struct OnnxEmbedder {
    model_id: String,
    dim: usize,
    tokenizer: Tokenizer,
    // ort sessions want &mut for run(); Embedder::embed takes &self.
    session: Mutex<ort::session::Session>,
}

impl OnnxEmbedder {
    /// The production entry point: returns instantly; downloading (if
    /// needed), verifying, and loading all happen on a background thread.
    /// Only embed()/search block on readiness.
    pub fn spawn_lazy(spec: &'static ModelSpec, cache_dir: std::path::PathBuf) -> LazyEmbedder {
        LazyEmbedder::spawn(spec.id, spec.dim, move || {
            let paths = fetch::ensure_model(spec, &cache_dir)?;
            let embedder = OnnxEmbedder::load(&paths.model, &paths.tokenizer, spec.id, spec.dim)?;
            Ok(Box::new(embedder) as Box<dyn Embedder>)
        })
    }

    /// Load synchronously from local files (the lazy path calls this).
    pub fn load(model: &Path, tokenizer: &Path, model_id: &str, dim: usize) -> Result<Self> {
        let mut tokenizer = Tokenizer::from_file(tokenizer).map_err(|e| load_err(e.to_string()))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| load_err(e.to_string()))?;

        let session = (|| -> std::result::Result<_, String> {
            ort::session::Session::builder()
                .map_err(|e| e.to_string())?
                .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
                .map_err(|e| e.to_string())?
                .commit_from_file(model)
                .map_err(|e| e.to_string())
        })()
        .map_err(load_err)?;

        Ok(Self {
            model_id: model_id.to_string(),
            dim,
            tokenizer,
            session: Mutex::new(session),
        })
    }
}

fn load_err(detail: String) -> TepinError {
    TepinError::new(
        "model_load_failed",
        format!("could not load the embedding model: {detail}"),
        "delete the cached model directory (~/.cache/tepindb/models); it re-downloads on next use",
    )
}

fn run_err(detail: String) -> TepinError {
    TepinError::new(
        "embedding_failed",
        format!("model inference failed: {detail}"),
        "this text could not be embedded; if it repeats, report it with the input",
    )
}

impl Embedder for OnnxEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Result<Embedding> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| run_err(e.to_string()))?;
        let truncated = !encoding.get_overflowing().is_empty();

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&v| i64::from(v)).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&v| i64::from(v))
            .collect();
        let type_ids: Vec<i64> = encoding
            .get_type_ids()
            .iter()
            .map(|&v| i64::from(v))
            .collect();
        let seq = ids.len();

        let mut session = self.session.lock().unwrap();
        let outputs = session
            .run(ort::inputs![
                "input_ids" => ort::value::Tensor::from_array(([1usize, seq], ids)).map_err(|e| run_err(e.to_string()))?,
                "attention_mask" => ort::value::Tensor::from_array(([1usize, seq], mask)).map_err(|e| run_err(e.to_string()))?,
                "token_type_ids" => ort::value::Tensor::from_array(([1usize, seq], type_ids)).map_err(|e| run_err(e.to_string()))?,
            ])
            .map_err(|e| run_err(e.to_string()))?;

        let (_, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(|e| run_err(e.to_string()))?;

        // CLS pooling: the first token's hidden state, L2-normalized.
        if data.len() < self.dim {
            return Err(run_err(format!(
                "output too small: {} floats for dim {}",
                data.len(),
                self.dim
            )));
        }
        let mut vector: Vec<f32> = data[..self.dim].to_vec();
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vector {
                *v /= norm;
            }
        }

        Ok(Embedding { vector, truncated })
    }
}
