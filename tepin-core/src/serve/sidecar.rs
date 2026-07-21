//! The discovery sidecar: a small JSON file in the OS runtime dir that
//! advertises the process currently serving a .tepin file. It lives OUT of
//! the data directory (no sockets or metadata next to the database, nothing
//! to leak into VCS or file sync), keyed by a hash of the canonical db path
//! so a reader can compute its location from the db path alone.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Sidecar {
    pub pid: u32,
    /// "unix" | "windows-pipe"
    pub transport: String,
    /// Socket path (unix) or namespaced pipe name (windows).
    pub endpoint: String,
    /// Random per-host token, echoed in the hello response — proves the
    /// listener is the process this sidecar describes (guards pid reuse
    /// and unrelated listeners on a recycled endpoint).
    pub nonce: String,
    pub protocol_version: u32,
    pub format_version: u32,
    /// The canonical db path. The filename hash is only collision-
    /// resistant enough for accidents; discovery verifies this field.
    pub path: String,
    pub started_at_unix: u64,
}

pub(crate) fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("tepindb")
}

/// FNV-1a 64 with a caller-chosen basis; two bases give a 128-bit key.
fn fnv(s: &str, mut h: u64) -> u64 {
    for b in s.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Where the sidecar for `db_path` lives, plus the canonical path both
/// sides agree on. Errors if the db file does not exist (it always does
/// by the time either side gets here).
pub(crate) fn location(db_path: &Path) -> Result<(PathBuf, String)> {
    let canonical = std::fs::canonicalize(db_path)?
        .to_string_lossy()
        .into_owned();
    let key = format!(
        "{:016x}{:016x}",
        fnv(&canonical, 0xcbf2_9ce4_8422_2325),
        fnv(&canonical, 0x6c62_272e_07bb_0142)
    );
    Ok((runtime_dir().join(format!("{key}.json")), canonical))
}

pub(crate) fn write(file: &Path, sidecar: &Sidecar) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    std::fs::write(file, serde_json::to_vec_pretty(sidecar)?)?;
    Ok(())
}

pub(crate) fn read(file: &Path) -> Option<Sidecar> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice(&bytes).ok()
}
