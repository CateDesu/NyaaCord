//! Release updater backed by GitHub Releases.

#[cfg(unix)]
pub mod appimage;
pub mod github;
pub mod http;
pub mod pe;
pub mod sha256;
pub mod version;

use std::path::{Path, PathBuf};

#[cfg(unix)]
pub use appimage::{
   apply_update, is_mount_path, previous_path, rollback, spawn_relaunch_watchdog, verify_appimage,
};
pub use github::{CheckResult, Config, DEFAULT_REPO, ReleaseInfo, check_latest, fetch_latest};
pub use pe::verify_pe64;
#[cfg(windows)]
pub use pe::{apply_update, previous_path, rollback_exe as rollback};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
   pub target: PathBuf,
   pub previous: Option<PathBuf>,
   pub version: String,
}

/// Verifies a platform release package (`AppImage` on Unix, PE32+ `.exe` on Windows).
pub fn verify_package(
   path: &Path,
   expected_size: Option<u64>,
   expected_sha256: Option<&str>,
) -> Result<(), Error> {
   #[cfg(unix)]
   {
      verify_appimage(path, expected_size, expected_sha256)
   }
   #[cfg(windows)]
   {
      verify_pe64(path, expected_size, expected_sha256)
   }
}

#[derive(Debug, flub::Error)]
pub enum Error {
   #[error("invalid repository {repo:?}: expected owner/name")]
   InvalidRepo { repo: String },
   #[error("invalid URL {url:?}: {reason}")]
   InvalidUrl { url: String, reason: String },
   #[error("{url}: HTTP {status}")]
   HttpStatus { url: String, status: i32 },
   #[error("{url}: response exceeds limit of {limit} bytes")]
   TooLarge { url: String, limit: u64 },
   #[error("{url}: {message}")]
   Http { url: String, message: String },
   #[error("invalid release JSON: {0}")]
   Json(String),
   #[error("release {tag} is a draft")]
   DraftRelease { tag: String },
   #[error("release {tag} has no matching AppImage asset")]
   NoAppImageAsset { tag: String },
   #[error("release {tag} has no matching Windows executable asset")]
   NoWindowsAsset { tag: String },
   #[error("release asset {asset} has no valid SHA256 checksum")]
   MissingChecksum { asset: String },
   #[error("invalid SHA256 hex digest {value:?}")]
   InvalidChecksum { value: String },
   #[error("size mismatch: expected {expected} bytes, got {actual} bytes")]
   SizeMismatch { expected: u64, actual: u64 },
   #[error("checksum mismatch: expected {expected}, got {actual}")]
   ChecksumMismatch { expected: String, actual: String },
   #[error("{path}: invalid AppImage: {reason}")]
   InvalidAppImage { path: PathBuf, reason: String },
   #[error("{path}: invalid Windows executable: {reason}")]
   InvalidExecutable { path: PathBuf, reason: String },
   #[error("{path}: refused to modify mount path")]
   MountPath { path: PathBuf },
   #[error("{path}: target directory is not writable: {source}")]
   NotWritable {
      path: PathBuf,
      source: std::io::Error,
   },
   #[error("{path}: no previous backup found")]
   NoPrevious { path: PathBuf },
   #[error("{path}: {source}")]
   Io {
      path: PathBuf,
      source: std::io::Error,
   },
   #[error("relaunch watchdog failed: {0}")]
   Watchdog(String),
}
