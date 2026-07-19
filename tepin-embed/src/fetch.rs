//! Lazy model download with pinned SHA-256 verification.
//!
//! Security posture (see SECURITY.md): the runtime downloads models only
//! from URLs pinned in this source, verifies every file against a pinned
//! SHA-256 — on first download AND on every cache read — and writes
//! atomically (.part + rename), so a torn download can never be loaded.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tepin_core::{Result, TepinError};

/// Where the model comes from and what it must hash to.
pub struct ModelSpec {
    pub id: &'static str,
    pub dim: usize,
    pub model_url: &'static str,
    pub model_sha256: &'static str,
    pub tokenizer_url: &'static str,
    pub tokenizer_sha256: &'static str,
}

/// bge-small-en-v1.5, int8-quantized ONNX.
///
/// Served exclusively from this project's GitHub releases (a dedicated
/// `model-*` release, so the URL is stable across software versions).
/// Origin: Xenova/bge-small-en-v1.5 (Apache-2.0); the SHA-256 pins
/// enforce byte-exact content on top of the trusted host.
pub const BGE_SMALL: ModelSpec = ModelSpec {
    id: crate::DEFAULT_MODEL,
    dim: crate::DEFAULT_DIM,
    model_url:
        "https://github.com/tepindb/tepindb/releases/download/model-bge-small-en-v1.5/model_quantized.onnx",
    model_sha256: "6c9c6101a956d62dfb5e7190c538226c0c5bb9cb27b651234b6df063ee7dbfe4",
    tokenizer_url: "https://github.com/tepindb/tepindb/releases/download/model-bge-small-en-v1.5/tokenizer.json",
    tokenizer_sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
};

pub struct ModelPaths {
    pub model: PathBuf,
    pub tokenizer: PathBuf,
}

/// The shared model cache: `~/.cache/tepindb/models` (or XDG_CACHE_HOME)
/// on unix, `%LOCALAPPDATA%\tepindb\models` on Windows. Shared across
/// every db and tool on the machine — the model downloads once.
pub fn default_cache_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

    let base = base.ok_or_else(|| {
        TepinError::new(
            "model_download_failed",
            "could not resolve a cache directory (no HOME/LOCALAPPDATA)",
            "set XDG_CACHE_HOME (unix) or LOCALAPPDATA (windows) and retry",
        )
    })?;
    Ok(base.join("tepindb").join("models"))
}

/// Ensure both model files exist in `dir` and match their pinned hashes.
/// Existing files are re-verified (a poisoned cache must not load);
/// a hash mismatch on a cached file triggers one re-download.
pub fn ensure_model(spec: &ModelSpec, dir: &Path) -> Result<ModelPaths> {
    let model_dir = dir.join(spec.id);
    std::fs::create_dir_all(&model_dir)?;
    let model = model_dir.join("model.onnx");
    let tokenizer = model_dir.join("tokenizer.json");
    ensure_file(spec.model_url, spec.model_sha256, &model)?;
    ensure_file(spec.tokenizer_url, spec.tokenizer_sha256, &tokenizer)?;
    Ok(ModelPaths { model, tokenizer })
}

fn ensure_file(url: &str, sha256: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        match verify(dest, sha256) {
            Ok(()) => return Ok(()),
            Err(_) => {
                // Cached file no longer matches the pin — re-download once.
                std::fs::remove_file(dest)?;
            }
        }
    }
    download(url, dest)?;
    verify(dest, sha256).inspect_err(|_| {
        let _ = std::fs::remove_file(dest);
    })
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let part = dest.with_extension("part");
    let mut response = ureq::get(url).call().map_err(|e| {
        TepinError::new(
            "model_download_failed",
            format!("could not download {url}: {e}"),
            "check network access; the model is fetched once and cached",
        )
    })?;
    let mut reader = response.body_mut().as_reader();
    let mut file = std::fs::File::create(&part)?;
    std::io::copy(&mut reader, &mut file).inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&part, dest)?;
    Ok(())
}

fn verify(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(TepinError::new(
            "checksum_mismatch",
            format!(
                "{} hashes to {actual}, expected {expected}",
                path.display()
            ),
            "the file is corrupt or tampered with; it will be re-downloaded — if this repeats, report it",
        ));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_rejects_wrong_hash_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"evil bytes").unwrap();
        let err = verify(&p, &"0".repeat(64)).unwrap_err();
        assert_eq!(err.code, "checksum_mismatch");
        assert!(!err.hint.is_empty());
    }

    #[test]
    fn tampered_cached_file_is_detected() {
        // ensure_file's cache path must never accept a file that fails the pin
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("model.onnx");
        std::fs::write(&p, b"tampered").unwrap();
        // re-download will fail (bogus url), so the whole ensure fails —
        // the point is that the tampered bytes were NOT accepted
        let err = ensure_file("http://127.0.0.1:1/nope", &"a".repeat(64), &p).unwrap_err();
        assert_eq!(err.code, "model_download_failed");
        assert!(!p.exists(), "tampered file must be removed, not kept");
    }
}
