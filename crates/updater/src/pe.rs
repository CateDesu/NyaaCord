//! Windows PE32+ executable verification, atomic replacement, and rollback.

use std::{
   env,
   fs::{self, File, OpenOptions},
   io::{self, Read, Seek, SeekFrom},
   path::{Component, Path, PathBuf},
   sync::atomic::{AtomicU64, Ordering},
   time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use crate::{
   ApplyOutcome,
   github::{Config, ReleaseInfo},
   http,
};
use crate::{Error, sha256};

#[cfg(windows)]
pub const MAX_EXE_BYTES: u64 = 512 * 1024 * 1024;
const DOS_HEADER_LEN: usize = 64;
const PE_MIN_HEADER_LEN: usize = 26;
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_FILE_DLL: u16 = 0x2000;
const PE32_PLUS_MAGIC: u16 = 0x020B;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempFileGuard {
   path: PathBuf,
   armed: bool,
}

impl TempFileGuard {
   const fn new(path: PathBuf) -> Self {
      Self { path, armed: true }
   }

   fn path(&self) -> &Path {
      &self.path
   }

   const fn disarm(&mut self) {
      self.armed = false;
   }
}

impl Drop for TempFileGuard {
   fn drop(&mut self) {
      if self.armed {
         let _ = fs::remove_file(&self.path);
      }
   }
}

/// Returns `<target>.previous` alongside `target`.
#[must_use]
pub fn previous_path(target: &Path) -> PathBuf {
   let mut os = target.as_os_str().to_os_string();
   os.push(".previous");
   PathBuf::from(os)
}

fn unique_temp_path(parent: &Path, name: &str, tag: &str) -> PathBuf {
   let pid = std::process::id();
   let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_or(0, |d| d.as_nanos());
   let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
   parent.join(format!(".{name}.{tag}.{pid}.{nanos}.{seq}"))
}

fn create_unique_temp_file(
   parent: &Path,
   name: &str,
   tag: &str,
) -> Result<(File, TempFileGuard), Error> {
   for _ in 0..16 {
      let path = unique_temp_path(parent, name, tag);
      match OpenOptions::new().write(true).create_new(true).open(&path) {
         Ok(file) => return Ok((file, TempFileGuard::new(path))),
         Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {},
         Err(source) => return Err(Error::Io { path, source }),
      }
   }
   Err(Error::Io {
      path: parent.to_path_buf(),
      source: io::Error::new(
         io::ErrorKind::AlreadyExists,
         "could not create unique temporary file",
      ),
   })
}

fn absolutize(path: &Path) -> Result<PathBuf, Error> {
   let joined = if path.is_absolute() {
      path.to_path_buf()
   } else {
      let cwd = env::current_dir().map_err(|source| Error::Io {
         path: path.to_path_buf(),
         source,
      })?;
      cwd.join(path)
   };
   let mut clean = PathBuf::new();
   for comp in joined.components() {
      if comp != Component::CurDir {
         clean.push(comp);
      }
   }
   Ok(clean)
}

fn target_parent_and_name(target: &Path) -> Result<(PathBuf, String), Error> {
   let name = target
      .file_name()
      .and_then(|n| n.to_str())
      .filter(|n| !n.is_empty())
      .ok_or_else(|| Error::InvalidExecutable {
         path: target.to_path_buf(),
         reason: "target path has no valid file name".to_owned(),
      })?
      .to_owned();

   let parent = match target.parent() {
      Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
      _ => PathBuf::from("."),
   };
   Ok((parent, name))
}

#[cfg(windows)]
fn probe_writable(parent: &Path, name: &str, target: &Path) -> Result<(), Error> {
   for _ in 0..16 {
      let probe = unique_temp_path(parent, name, "probe");
      match OpenOptions::new().write(true).create_new(true).open(&probe) {
         Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            return Ok(());
         },
         Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {},
         Err(source) => {
            return Err(Error::NotWritable {
               path: target.to_path_buf(),
               source,
            });
         },
      }
   }
   Err(Error::NotWritable {
      path: target.to_path_buf(),
      source: io::Error::new(
         io::ErrorKind::AlreadyExists,
         "could not create unique writability probe",
      ),
   })
}

fn cleanup_stale_files(parent: &Path, name: &str) {
   let prefix = format!(".{name}.stale.");
   let Ok(entries) = fs::read_dir(parent) else {
      return;
   };
   for entry in entries.flatten() {
      if entry
         .file_name()
         .to_str()
         .is_some_and(|file| file.starts_with(&prefix))
      {
         let _ = fs::remove_file(entry.path());
      }
   }
}

fn verify_pe64_headers(path: &Path, size: u64) -> Result<(), Error> {
   let mut file = File::open(path).map_err(|source| Error::Io {
      path: path.to_path_buf(),
      source,
   })?;

   let mut dos = [0_u8; DOS_HEADER_LEN];
   file
      .read_exact(&mut dos)
      .map_err(|_| Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: "file is smaller than DOS header".to_owned(),
      })?;
   if &dos[0..2] != b"MZ" {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: "missing MZ header".to_owned(),
      });
   }

   let pe_offset = u64::from(u32::from_le_bytes([dos[60], dos[61], dos[62], dos[63]]));
   if pe_offset < DOS_HEADER_LEN as u64
      || pe_offset
         .checked_add(PE_MIN_HEADER_LEN as u64)
         .is_none_or(|end| end > size)
   {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: format!("invalid PE header offset {pe_offset}"),
      });
   }

   file
      .seek(SeekFrom::Start(pe_offset))
      .map_err(|source| Error::Io {
         path: path.to_path_buf(),
         source,
      })?;
   let mut pe = [0_u8; PE_MIN_HEADER_LEN];
   file
      .read_exact(&mut pe)
      .map_err(|_| Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: "truncated PE header".to_owned(),
      })?;

   if &pe[0..4] != b"PE\0\0" {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: "missing PE signature".to_owned(),
      });
   }

   let machine = u16::from_le_bytes([pe[4], pe[5]]);
   if machine != IMAGE_FILE_MACHINE_AMD64 {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: format!("unsupported COFF machine {machine:#06X}, expected x86-64 (0x8664)"),
      });
   }

   let opt_size = u16::from_le_bytes([pe[20], pe[21]]);
   if opt_size < 2
      || pe_offset
         .checked_add(24)
         .and_then(|off| off.checked_add(u64::from(opt_size)))
         .is_none_or(|end| end > size)
   {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: format!("invalid PE optional header size {opt_size}"),
      });
   }

   let characteristics = u16::from_le_bytes([pe[22], pe[23]]);
   if (characteristics & IMAGE_FILE_DLL) != 0 {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: "file is a DLL, expected an executable".to_owned(),
      });
   }

   let opt_magic = u16::from_le_bytes([pe[24], pe[25]]);
   if opt_magic != PE32_PLUS_MAGIC {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: format!("unsupported PE optional header magic {opt_magic:#06X}, expected 0x020B"),
      });
   }

   Ok(())
}

/// Verifies a Windows PE32+ x86-64 executable header, plus optional size and SHA-256.
pub fn verify_pe64(
   path: &Path,
   expected_size: Option<u64>,
   expected_sha256: Option<&str>,
) -> Result<(), Error> {
   let meta = fs::metadata(path).map_err(|source| Error::Io {
      path: path.to_path_buf(),
      source,
   })?;
   if !meta.is_file() {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: "not a regular file".to_owned(),
      });
   }

   let size = meta.len();
   if size == 0 {
      return Err(Error::InvalidExecutable {
         path: path.to_path_buf(),
         reason: "file is empty".to_owned(),
      });
   }
   if let Some(expected) = expected_size
      && size != expected
   {
      return Err(Error::SizeMismatch {
         expected,
         actual: size,
      });
   }

   verify_pe64_headers(path, size)?;

   if let Some(expected_raw) = expected_sha256 {
      let expected = sha256::normalize_hex(expected_raw).ok_or_else(|| Error::InvalidChecksum {
         value: expected_raw.to_owned(),
      })?;
      let actual = sha256::sha256_file(path)?;
      if actual != expected {
         return Err(Error::ChecksumMismatch { expected, actual });
      }
   }

   Ok(())
}

/// Moves an existing `target` to `<target>.previous` and moves `staged` into `target`.
///
/// Renaming a running `.exe` within its directory succeeds on Windows even when overwriting it
/// would fail. If `<target>.previous` is itself locked, it is moved aside to a stale temp name
/// before placing the new backup.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "used by Windows apply_update and host tests")
)]
pub(crate) fn replace_exe_with_backup(
   staged: &Path,
   target: &Path,
) -> Result<Option<PathBuf>, Error> {
   let (parent, name) = target_parent_and_name(target)?;
   cleanup_stale_files(&parent, &name);

   let target_exists = fs::symlink_metadata(target).is_ok();
   if !target_exists {
      fs::rename(staged, target).map_err(|source| Error::Io {
         path: target.to_path_buf(),
         source,
      })?;
      return Ok(None);
   }

   let prev = previous_path(target);
   let mut displaced_prev: Option<PathBuf> = None;
   if let Err(first_err) = fs::rename(target, &prev) {
      if fs::symlink_metadata(&prev).is_ok() {
         let stale_prev = unique_temp_path(&parent, &name, "stale");
         fs::rename(&prev, &stale_prev).map_err(|_| Error::Io {
            path: prev.clone(),
            source: first_err,
         })?;
         displaced_prev = Some(stale_prev);
         if let Err(source) = fs::rename(target, &prev) {
            if let Some(ref stale) = displaced_prev {
               let _ = fs::rename(stale, &prev);
            }
            return Err(Error::Io { path: prev, source });
         }
      } else {
         return Err(Error::Io {
            path: prev,
            source: first_err,
         });
      }
   }

   if let Err(source) = fs::rename(staged, target) {
      let _ = fs::rename(&prev, target);
      if let Some(stale) = displaced_prev {
         let _ = fs::rename(&stale, &prev);
      }
      return Err(Error::Io {
         path: target.to_path_buf(),
         source,
      });
   }

   if let Some(stale) = displaced_prev {
      let _ = fs::remove_file(stale);
   }
   Ok(Some(prev))
}

/// Restores `target` from `<target>.previous` while keeping `.previous` intact.
pub fn rollback_exe(target: &Path) -> Result<PathBuf, Error> {
   let target = absolutize(target)?;
   let prev = previous_path(&target);
   if !prev.exists() {
      return Err(Error::NoPrevious { path: prev });
   }

   verify_pe64(&prev, None, None)?;

   let (parent, name) = target_parent_and_name(&target)?;
   cleanup_stale_files(&parent, &name);

   let (mut output, mut rb_guard) = create_unique_temp_file(&parent, &name, "rb")?;
   let mut input = File::open(&prev).map_err(|source| Error::Io {
      path: prev.clone(),
      source,
   })?;
   io::copy(&mut input, &mut output).map_err(|source| Error::Io {
      path: rb_guard.path().to_path_buf(),
      source,
   })?;
   output.sync_all().map_err(|source| Error::Io {
      path: rb_guard.path().to_path_buf(),
      source,
   })?;
   drop(output);
   drop(input);

   if fs::symlink_metadata(&target).is_ok() {
      let stale = unique_temp_path(&parent, &name, "stale");
      fs::rename(&target, &stale).map_err(|source| Error::Io {
         path: target.clone(),
         source,
      })?;
      if let Err(source) = fs::rename(rb_guard.path(), &target) {
         let _ = fs::rename(&stale, &target);
         return Err(Error::Io {
            path: target,
            source,
         });
      }
      rb_guard.disarm();
      let _ = fs::remove_file(&stale);
   } else {
      fs::rename(rb_guard.path(), &target).map_err(|source| Error::Io {
         path: target,
         source,
      })?;
      rb_guard.disarm();
   }

   Ok(prev)
}

#[cfg(windows)]
fn resolve_target_path(target: &Path) -> Result<PathBuf, Error> {
   let abs = absolutize(target)?;
   match fs::symlink_metadata(&abs) {
      Ok(meta) if meta.file_type().is_symlink() => {
         let real = fs::canonicalize(&abs).map_err(|source| Error::InvalidExecutable {
            path: target.to_path_buf(),
            reason: format!("target symlink cannot be resolved: {source}"),
         })?;
         let real_meta = fs::metadata(&real).map_err(|source| Error::Io {
            path: real.clone(),
            source,
         })?;
         if !real_meta.is_file() {
            return Err(Error::InvalidExecutable {
               path: target.to_path_buf(),
               reason: "target symlink does not point to a regular file".to_owned(),
            });
         }
         Ok(real)
      },
      Ok(meta) if !meta.is_file() => Err(Error::InvalidExecutable {
         path: target.to_path_buf(),
         reason: "target exists and is not a regular file".to_owned(),
      }),
      Ok(_) => Ok(abs),
      Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(abs),
      Err(source) => Err(Error::Io {
         path: target.to_path_buf(),
         source,
      }),
   }
}

/// Downloads, verifies, backs up, and atomically replaces a Windows `.exe`.
#[cfg(windows)]
pub fn apply_update(
   release: &ReleaseInfo,
   target: &Path,
   config: &Config,
   on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<ApplyOutcome, Error> {
   let target = resolve_target_path(target)?;
   let expected_sha =
      sha256::normalize_hex(&release.sha256).ok_or_else(|| Error::InvalidChecksum {
         value: release.sha256.clone(),
      })?;
   let _ = http::validate_url(&release.download_url)?;

   let (parent, name) = target_parent_and_name(&target)?;
   if !parent.exists() {
      fs::create_dir_all(&parent).map_err(|source| Error::NotWritable {
         path: target.clone(),
         source,
      })?;
   }
   probe_writable(&parent, &name, &target)?;

   let (mut file, mut new_guard) = create_unique_temp_file(&parent, &name, "new")?;
   let download = {
      let opts = http::DownloadOptions {
         url: &release.download_url,
         headers: &[],
         max_bytes: MAX_EXE_BYTES,
         connect_timeout_ms: config.connect_timeout_ms,
         stall_timeout_secs: config.stall_timeout_secs,
      };
      let res = http::download_into_file(&opts, &mut file, new_guard.path(), on_progress);
      drop(file);
      res?
   };

   if release.size > 0 && download.bytes != release.size {
      return Err(Error::SizeMismatch {
         expected: release.size,
         actual: download.bytes,
      });
   }
   if download.sha256 != expected_sha {
      return Err(Error::ChecksumMismatch {
         expected: expected_sha,
         actual: download.sha256,
      });
   }

   let expected_size = (release.size > 0).then_some(release.size);
   verify_pe64(new_guard.path(), expected_size, None)?;

   let previous = replace_exe_with_backup(new_guard.path(), &target)?;
   new_guard.disarm();

   Ok(ApplyOutcome {
      target,
      previous,
      version: release.version.clone(),
   })
}

#[cfg(test)]
mod tests {
   use super::*;

   fn build_fake_pe64(payload: &[u8]) -> Vec<u8> {
      const PE_OFFSET: u32 = 0x80;
      const OPT_SIZE: u16 = 112;
      let pe_offset = PE_OFFSET as usize;
      let total_header = pe_offset + 24 + usize::from(OPT_SIZE);
      let mut buf = vec![0_u8; total_header];
      buf[0..2].copy_from_slice(b"MZ");
      buf[60..64].copy_from_slice(&PE_OFFSET.to_le_bytes());
      buf[pe_offset..pe_offset + 4].copy_from_slice(b"PE\0\0");
      buf[pe_offset + 4..pe_offset + 6].copy_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
      buf[pe_offset + 20..pe_offset + 22].copy_from_slice(&OPT_SIZE.to_le_bytes());
      buf[pe_offset + 22..pe_offset + 24].copy_from_slice(&0x0022_u16.to_le_bytes());
      buf[pe_offset + 24..pe_offset + 26].copy_from_slice(&PE32_PLUS_MAGIC.to_le_bytes());
      buf.extend_from_slice(payload);
      buf
   }

   #[test]
   fn verifies_pe64_and_rejects_bad_headers() {
      let dir = env::temp_dir().join(format!(
         "phoenix-pe-test-{}-{}",
         std::process::id(),
         TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&dir).expect("create temp dir");

      let good = build_fake_pe64(b"nyaacord-v1");
      let mut hasher = sha256::Hasher::new();
      hasher.update(&good);
      let good_sha = hasher.finalize_hex();
      let good_path = dir.join("good.exe");
      fs::write(&good_path, &good).expect("write good pe");

      verify_pe64(&good_path, Some(good.len() as u64), Some(&good_sha))
         .expect("valid PE64 should pass");

      assert!(matches!(
         verify_pe64(&good_path, Some(123), None),
         Err(Error::SizeMismatch { .. })
      ));
      assert!(matches!(
         verify_pe64(&good_path, None, Some(&"0".repeat(64))),
         Err(Error::ChecksumMismatch { .. })
      ));

      let mut bad_mz = good.clone();
      bad_mz[0] = b'X';
      let bad_mz_path = dir.join("bad_mz.exe");
      fs::write(&bad_mz_path, &bad_mz).expect("write bad mz");
      assert!(matches!(
         verify_pe64(&bad_mz_path, None, None),
         Err(Error::InvalidExecutable { .. })
      ));

      let mut bad_machine = good.clone();
      bad_machine[0x80 + 4..0x80 + 6].copy_from_slice(&0x014C_u16.to_le_bytes());
      let bad_machine_path = dir.join("bad_machine.exe");
      fs::write(&bad_machine_path, &bad_machine).expect("write bad machine");
      assert!(matches!(
         verify_pe64(&bad_machine_path, None, None),
         Err(Error::InvalidExecutable { .. })
      ));

      let mut bad_opt = good.clone();
      bad_opt[0x80 + 24..0x80 + 26].copy_from_slice(&0x010B_u16.to_le_bytes());
      let bad_opt_path = dir.join("bad_opt.exe");
      fs::write(&bad_opt_path, &bad_opt).expect("write bad opt");
      assert!(matches!(
         verify_pe64(&bad_opt_path, None, None),
         Err(Error::InvalidExecutable { .. })
      ));

      let mut dll = good;
      dll[0x80 + 22..0x80 + 24].copy_from_slice(&0x2022_u16.to_le_bytes());
      let dll_path = dir.join("lib.dll");
      fs::write(&dll_path, &dll).expect("write dll");
      assert!(matches!(
         verify_pe64(&dll_path, None, None),
         Err(Error::InvalidExecutable { .. })
      ));

      let _ = fs::remove_dir_all(&dir);
   }

   #[test]
   fn replaces_and_rolls_back_pe64_with_backup() {
      let dir = env::temp_dir().join(format!(
         "phoenix-pe-swap-{}-{}",
         std::process::id(),
         TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&dir).expect("create temp dir");

      let target = dir.join("NyaaCord.exe");
      let staged = dir.join(".NyaaCord.exe.new");
      let v1 = build_fake_pe64(b"exe-v1");
      let v2 = build_fake_pe64(b"exe-v2-updated");
      fs::write(&target, &v1).expect("write v1");
      fs::write(&staged, &v2).expect("write v2");

      let prev = replace_exe_with_backup(&staged, &target).expect("replace");
      assert_eq!(prev, Some(previous_path(&target)));
      assert_eq!(fs::read(&target).expect("read target"), v2);
      assert_eq!(fs::read(previous_path(&target)).expect("read prev"), v1);

      let restored_from = rollback_exe(&target).expect("rollback");
      assert_eq!(restored_from, previous_path(&target));
      assert_eq!(fs::read(&target).expect("read rolled back"), v1);
      assert!(previous_path(&target).exists());

      let _ = fs::remove_dir_all(&dir);
   }
}
