//! Async model init: [`LazyEmbedder::spawn`] returns instantly and loads
//! the real embedder on a background thread. Non-vector work never waits;
//! `embed()` blocks until the model is ready (or surfaces the load failure
//! on every call thereafter). This is the mechanism behind "open() never
//! blocks on the model".

use std::sync::{Arc, Condvar, Mutex};

use tepin_core::embed::{Embedder, Embedding};
use tepin_core::{Result, TepinError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderStatus {
    Loading,
    Ready,
    Failed,
}

enum LoadState {
    Loading,
    Ready(Box<dyn Embedder>),
    Failed(TepinError),
}

struct Shared {
    state: Mutex<LoadState>,
    cond: Condvar,
}

pub struct LazyEmbedder {
    model_id: String,
    dim: usize,
    shared: Arc<Shared>,
}

impl LazyEmbedder {
    /// Start loading in the background; returns immediately. `model_id` and
    /// `dim` are known from the model spec before any loading happens.
    pub fn spawn(
        model_id: impl Into<String>,
        dim: usize,
        loader: impl FnOnce() -> Result<Box<dyn Embedder>> + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(LoadState::Loading),
            cond: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("tepin-embed-load".into())
            .spawn(move || {
                let outcome = match loader() {
                    Ok(embedder) => LoadState::Ready(embedder),
                    Err(e) => LoadState::Failed(e),
                };
                *thread_shared.state.lock().unwrap() = outcome;
                thread_shared.cond.notify_all();
            })
            .expect("spawn embed loader thread");
        Self {
            model_id: model_id.into(),
            dim,
            shared,
        }
    }

    /// Non-blocking: where is the load right now?
    pub fn status(&self) -> EmbedderStatus {
        match *self.shared.state.lock().unwrap() {
            LoadState::Loading => EmbedderStatus::Loading,
            LoadState::Ready(_) => EmbedderStatus::Ready,
            LoadState::Failed(_) => EmbedderStatus::Failed,
        }
    }

    /// Block until loading finished (ready or failed). Cheap once loaded.
    pub fn wait_ready(&self) -> Result<()> {
        let guard = self.shared.state.lock().unwrap();
        let guard = self
            .shared
            .cond
            .wait_while(guard, |s| matches!(s, LoadState::Loading))
            .unwrap();
        match &*guard {
            LoadState::Loading => unreachable!("wait_while guarantees progress"),
            LoadState::Ready(_) => Ok(()),
            LoadState::Failed(e) => Err(clone_error(e)),
        }
    }
}

impl Embedder for LazyEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Result<Embedding> {
        let guard = self.shared.state.lock().unwrap();
        let guard = self
            .shared
            .cond
            .wait_while(guard, |s| matches!(s, LoadState::Loading))
            .unwrap();
        match &*guard {
            LoadState::Loading => unreachable!("wait_while guarantees progress"),
            LoadState::Ready(embedder) => embedder.embed(text),
            LoadState::Failed(e) => Err(clone_error(e)),
        }
    }
}

/// TepinError carries a boxed source and so isn't Clone; the load failure
/// must survive being reported on every subsequent embed call.
fn clone_error(e: &TepinError) -> TepinError {
    TepinError::new(e.code, e.message.clone(), e.hint.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Test double whose load takes a controlled amount of time.
    fn slow_loader(
        delay: Duration,
        outcome: Result<()>,
    ) -> impl FnOnce() -> Result<Box<dyn Embedder>> + Send + 'static {
        move || {
            std::thread::sleep(delay);
            outcome.map(|()| Box::new(crate::MockEmbedder::new(8)) as Box<dyn Embedder>)
        }
    }

    #[test]
    fn construction_is_instant_while_loading_runs_behind() {
        let t0 = Instant::now();
        let lazy = LazyEmbedder::spawn("mock", 8, slow_loader(Duration::from_millis(300), Ok(())));
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "spawn must not block"
        );
        assert_eq!(lazy.status(), EmbedderStatus::Loading);

        // embed() blocks until the loader finishes, then works
        let embedding = lazy.embed("hello").unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(300));
        assert_eq!(embedding.vector.len(), 8);
        assert_eq!(lazy.status(), EmbedderStatus::Ready);
    }

    #[test]
    fn load_failure_surfaces_on_every_embed_call() {
        let lazy = LazyEmbedder::spawn(
            "mock",
            8,
            slow_loader(
                Duration::from_millis(20),
                Err(TepinError::new(
                    "model_load_failed",
                    "corrupt model file",
                    "delete the cached model; it re-downloads on next use",
                )),
            ),
        );
        for _ in 0..2 {
            let err = lazy.embed("hello").unwrap_err();
            assert_eq!(err.code, "model_load_failed");
            assert!(!err.hint.is_empty());
        }
        assert_eq!(lazy.status(), EmbedderStatus::Failed);
        assert_eq!(lazy.wait_ready().unwrap_err().code, "model_load_failed");
    }

    #[test]
    fn concurrent_embeds_all_wait_and_succeed() {
        let lazy = std::sync::Arc::new(LazyEmbedder::spawn(
            "mock",
            8,
            slow_loader(Duration::from_millis(100), Ok(())),
        ));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let lazy = std::sync::Arc::clone(&lazy);
                std::thread::spawn(move || lazy.embed(&format!("text {i}")).unwrap())
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().vector.len(), 8);
        }
    }

    #[test]
    fn metadata_is_available_before_load_completes() {
        let lazy = LazyEmbedder::spawn(
            "bge-small-en-v1.5-int8",
            384,
            slow_loader(Duration::from_millis(200), Ok(())),
        );
        // dim/model_id must not block: the db needs them to record vector
        // provenance without waiting for the model
        assert_eq!(lazy.dim(), 384);
        assert_eq!(lazy.model_id(), "bge-small-en-v1.5-int8");
        assert_eq!(lazy.status(), EmbedderStatus::Loading);
        lazy.wait_ready().unwrap();
        assert_eq!(lazy.status(), EmbedderStatus::Ready);
    }
}
