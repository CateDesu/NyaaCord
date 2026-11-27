//! `AppImage` verification, atomic replacement, rollback, and relaunch watchdog.

use std::{
   env,
   ffi::OsStr,
   fs::{self, File, OpenOptions, Permissions},
   io::{self, Read, Seek, SeekFrom, Write},
   os::unix::{
      ffi::OsStrExt,
      fs::{OpenOptionsExt, PermissionsExt},
      process::CommandExt,
   },
   path::{Component, Path, PathBuf},
   process::{Command, Stdio},
   sync::atomic::{AtomicU64, Ordering},
   thread,
   time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
   ApplyOutcome, Error,
   github::{Config, ReleaseInfo},
   http, sha256,
};

pub const MAX_APPIMAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const OFFSET_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CAPTURE_BYTES: u64 = 4096;

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

fn has_mount_component(path: &Path) -> bool {
   path.components().any(|comp| {
      matches!(
         comp,
         Component::Normal(name) if name.as_encoded_bytes().starts_with(b".mount_")
      )
   })
}

fn has_normal_component(path: &Path) -> bool {
   path
      .components()
      .any(|comp| matches!(comp, Component::Normal(_)))
}

fn path_inside_appdirs(path: &Path, appdirs: &[PathBuf]) -> bool {
   appdirs
      .iter()
      .any(|dir| has_normal_component(dir) && path.starts_with(dir))
}

fn normalize_appdirs(raw_dirs: &[PathBuf]) -> Vec<PathBuf> {
   let mut dirs = Vec::new();
   for raw in raw_dirs {
      if raw.as_os_str().is_empty() {
         continue;
      }
      if has_normal_component(raw) {
         dirs.push(raw.clone());
      }
      if let Ok(canon) = fs::canonicalize(raw)
         && has_normal_component(&canon)
         && !dirs.contains(&canon)
      {
         dirs.push(canon);
      }
   }
   dirs
}

fn env_appdirs() -> Vec<PathBuf> {
   let mut raw = Vec::new();
   for var in ["APPDIR", "NYAACORD_APPDIR"] {
      if let Some(val) = env::var_os(var)
         && !val.is_empty()
      {
         raw.push(PathBuf::from(val));
      }
   }
   raw
}

fn is_mount_path_with_appdirs(path: &Path, raw_appdirs: &[PathBuf]) -> bool {
   let appdirs = normalize_appdirs(raw_appdirs);
   let check =
      |candidate: &Path| has_mount_component(candidate) || path_inside_appdirs(candidate, &appdirs);

   if check(path) {
      return true;
   }

   let abs = if path.is_absolute() {
      path.to_path_buf()
   } else if let Ok(cwd) = env::current_dir() {
      cwd.join(path)
   } else {
      path.to_path_buf()
   };
   if check(&abs) {
      return true;
   }

   if let Ok(canon) = fs::canonicalize(&abs)
      && check(&canon)
   {
      return true;
   }

   for ancestor in abs.ancestors().skip(1) {
      if ancestor.as_os_str().is_empty() {
         continue;
      }
      if let Ok(canon) = fs::canonicalize(ancestor) {
         return check(&canon);
      }
   }
   false
}

/// Returns true if `path` resides inside an `AppImage` mount directory or active `APPDIR`.
#[must_use]
pub fn is_mount_path(path: &Path) -> bool {
   is_mount_path_with_appdirs(path, &env_appdirs())
}

fn ensure_not_mount_path(target: &Path) -> Result<(), Error> {
   if is_mount_path(target) {
      return Err(Error::MountPath {
         path: target.to_path_buf(),
      });
   }
   Ok(())
}

fn resolve_target_path(target: &Path) -> Result<PathBuf, Error> {
   ensure_not_mount_path(target)?;
   let abs = absolutize(target)?;
   ensure_not_mount_path(&abs)?;

   match fs::symlink_metadata(&abs) {
      Ok(meta) if meta.file_type().is_symlink() => {
         let real = fs::canonicalize(&abs).map_err(|source| Error::InvalidAppImage {
            path: target.to_path_buf(),
            reason: format!("target symlink cannot be resolved: {source}"),
         })?;
         ensure_not_mount_path(&real)?;
         let real_meta = fs::metadata(&real).map_err(|source| Error::Io {
            path: real.clone(),
            source,
         })?;
         if !real_meta.is_file() {
            return Err(Error::InvalidAppImage {
               path: target.to_path_buf(),
               reason: "target symlink does not point to a regular file".to_owned(),
            });
         }
         Ok(real)
      },
      Ok(meta) if !meta.is_file() => Err(Error::InvalidAppImage {
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

fn target_parent_and_name(target: &Path) -> Result<(PathBuf, String), Error> {
   let name = target
      .file_name()
      .and_then(|n| n.to_str())
      .filter(|n| !n.is_empty())
      .ok_or_else(|| Error::InvalidAppImage {
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

fn sync_dir(dir: &Path) {
   if let Ok(handle) = File::open(dir) {
      let _ = handle.sync_all();
   }
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
   mode: u32,
) -> Result<(File, TempFileGuard), Error> {
   for _ in 0..16 {
      let path = unique_temp_path(parent, name, tag);
      match OpenOptions::new()
         .write(true)
         .create_new(true)
         .mode(mode)
         .open(&path)
      {
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

fn hardlink_or_copy_to_temp(
   src: &Path,
   parent: &Path,
   name: &str,
   tag: &str,
) -> Result<TempFileGuard, Error> {
   for _ in 0..16 {
      let dst = unique_temp_path(parent, name, tag);
      match fs::hard_link(src, &dst) {
         Ok(()) => return Ok(TempFileGuard::new(dst)),
         Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {},
         Err(_) => {
            let mut input = File::open(src).map_err(|source| Error::Io {
               path: src.to_path_buf(),
               source,
            })?;
            let (mut output, guard) = create_unique_temp_file(parent, name, tag, 0o755)?;
            io::copy(&mut input, &mut output).map_err(|source| Error::Io {
               path: guard.path().to_path_buf(),
               source,
            })?;
            output.sync_all().map_err(|source| Error::Io {
               path: guard.path().to_path_buf(),
               source,
            })?;
            return Ok(guard);
         },
      }
   }
   Err(Error::Io {
      path: parent.to_path_buf(),
      source: io::Error::new(
         io::ErrorKind::AlreadyExists,
         "could not create unique temporary backup file",
      ),
   })
}

fn kill_child_group(child: &mut std::process::Child) {
   if let Ok(pgid) = libc::pid_t::try_from(child.id())
      && pgid > 0
   {
      // SAFETY: `child` called `setpgid(0, 0)` in `pre_exec`, so `-pgid` targets only its group.
      unsafe {
         libc::kill(-pgid, libc::SIGKILL);
      }
   }
   let _ = child.kill();
   let _ = child.wait();
}

fn join_pipe(handle: Option<thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
   handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

fn parse_offset_output(
   path: &Path,
   status: std::process::ExitStatus,
   stdout: Vec<u8>,
   stderr: &[u8],
) -> Result<u64, Error> {
   if !status.success() {
      let detail = String::from_utf8_lossy(stderr).trim().to_owned();
      let reason = if detail.is_empty() {
         format!("--appimage-offset exited with {status}")
      } else {
         format!("--appimage-offset exited with {status}: {detail}")
      };
      return Err(Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason,
      });
   }

   let text = String::from_utf8(stdout).map_err(|_| Error::InvalidAppImage {
      path: path.to_path_buf(),
      reason: "--appimage-offset output is not UTF-8".to_owned(),
   })?;
   let offset = text
      .trim()
      .parse::<u64>()
      .map_err(|_| Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason: format!("invalid --appimage-offset output {:?}", text.trim()),
      })?;
   if offset == 0 {
      return Err(Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason: "--appimage-offset returned zero".to_owned(),
      });
   }
   Ok(offset)
}

fn query_appimage_offset(path: &Path) -> Result<u64, Error> {
   let exec_path = absolutize(path)?;
   let mut cmd = Command::new(&exec_path);
   cmd.arg("--appimage-offset")
      .env_remove("LD_PRELOAD")
      .env_remove("APPIMAGE")
      .env_remove("APPDIR")
      .env_remove("OWD")
      .env_remove("ARGV0")
      .env_remove("NYAACORD_APPDIR")
      .env_remove("APPIMAGE_EXTRACT_AND_RUN")
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());

   // SAFETY: `setpgid` is async signal safe per POSIX.
   unsafe {
      cmd.pre_exec(|| {
         if libc::setpgid(0, 0) == -1 {
            return Err(io::Error::last_os_error());
         }
         Ok(())
      });
   }

   let mut child = cmd.spawn().map_err(|source| Error::InvalidAppImage {
      path: path.to_path_buf(),
      reason: format!("failed to run --appimage-offset: {source}"),
   })?;

   let stdout_handle = child.stdout.take().map(|mut pipe| {
      thread::spawn(move || {
         let mut buf = Vec::new();
         let _ = (&mut pipe).take(MAX_CAPTURE_BYTES).read_to_end(&mut buf);
         buf
      })
   });
   let stderr_handle = child.stderr.take().map(|mut pipe| {
      thread::spawn(move || {
         let mut buf = Vec::new();
         let _ = (&mut pipe).take(MAX_CAPTURE_BYTES).read_to_end(&mut buf);
         buf
      })
   });

   let start = Instant::now();
   let status = loop {
      match child.try_wait() {
         Ok(Some(status)) => break status,
         Ok(None) => {
            if start.elapsed() >= OFFSET_TIMEOUT {
               kill_child_group(&mut child);
               let _ = join_pipe(stdout_handle);
               let _ = join_pipe(stderr_handle);
               return Err(Error::InvalidAppImage {
                  path: path.to_path_buf(),
                  reason: "--appimage-offset timed out".to_owned(),
               });
            }
            thread::sleep(Duration::from_millis(20));
         },
         Err(source) => {
            kill_child_group(&mut child);
            let _ = join_pipe(stdout_handle);
            let _ = join_pipe(stderr_handle);
            return Err(Error::InvalidAppImage {
               path: path.to_path_buf(),
               reason: format!("failed waiting for --appimage-offset: {source}"),
            });
         },
      }
   };

   let stdout = join_pipe(stdout_handle);
   let stderr = join_pipe(stderr_handle);
   parse_offset_output(path, status, stdout, &stderr)
}

/// Verifies an `AppImage` file header, optional size and SHA 256, runtime offset,
/// and embedded `SquashFS` superblock magic.
pub fn verify_appimage(
   path: &Path,
   expected_size: Option<u64>,
   expected_sha256: Option<&str>,
) -> Result<(), Error> {
   let meta = fs::metadata(path).map_err(|source| Error::Io {
      path: path.to_path_buf(),
      source,
   })?;
   if !meta.is_file() {
      return Err(Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason: "not a regular file".to_owned(),
      });
   }

   let size = meta.len();
   if size == 0 {
      return Err(Error::InvalidAppImage {
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

   let mut header = [0_u8; 11];
   {
      let mut file = File::open(path).map_err(|source| Error::Io {
         path: path.to_path_buf(),
         source,
      })?;
      file
         .read_exact(&mut header)
         .map_err(|_| Error::InvalidAppImage {
            path: path.to_path_buf(),
            reason: "file is smaller than AppImage header".to_owned(),
         })?;
   }

   if &header[0..4] != b"\x7fELF" {
      return Err(Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason: "missing ELF magic".to_owned(),
      });
   }
   if &header[8..11] != b"AI\x02" {
      return Err(Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason: "missing AppImage type 2 magic".to_owned(),
      });
   }

   if let Some(expected_raw) = expected_sha256 {
      let expected = sha256::normalize_hex(expected_raw).ok_or_else(|| Error::InvalidChecksum {
         value: expected_raw.to_owned(),
      })?;
      let actual = sha256::sha256_file(path)?;
      if actual != expected {
         return Err(Error::ChecksumMismatch { expected, actual });
      }
   }

   if (meta.permissions().mode() & 0o100) == 0 {
      fs::set_permissions(path, Permissions::from_mode(0o755)).map_err(|source| Error::Io {
         path: path.to_path_buf(),
         source,
      })?;
   }

   let offset = query_appimage_offset(path)?;
   if offset.checked_add(4).is_none_or(|end| end > size) {
      return Err(Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason: format!("SquashFS offset {offset} exceeds file size {size}"),
      });
   }

   let mut magic = [0_u8; 4];
   let mut file = File::open(path).map_err(|source| Error::Io {
      path: path.to_path_buf(),
      source,
   })?;
   file
      .seek(SeekFrom::Start(offset))
      .map_err(|source| Error::Io {
         path: path.to_path_buf(),
         source,
      })?;
   file.read_exact(&mut magic).map_err(|source| Error::Io {
      path: path.to_path_buf(),
      source,
   })?;
   if &magic != b"hsqs" {
      return Err(Error::InvalidAppImage {
         path: path.to_path_buf(),
         reason: format!("missing SquashFS hsqs magic at offset {offset}"),
      });
   }

   Ok(())
}

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

/// Downloads, verifies, backs up, and atomically replaces `target`.
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
   ensure_not_mount_path(&target)?;
   probe_writable(&parent, &name, &target)?;

   let (mut file, mut new_guard) = create_unique_temp_file(&parent, &name, "new", 0o755)?;
   let download = {
      let opts = http::DownloadOptions {
         url: &release.download_url,
         headers: &[],
         max_bytes: MAX_APPIMAGE_BYTES,
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

   fs::set_permissions(new_guard.path(), Permissions::from_mode(0o755)).map_err(|source| {
      Error::Io {
         path: new_guard.path().to_path_buf(),
         source,
      }
   })?;
   let expected_size = (release.size > 0).then_some(release.size);
   verify_appimage(new_guard.path(), expected_size, None)?;

   let previous = if fs::symlink_metadata(&target).is_ok() {
      let prev_path = previous_path(&target);
      let mut prev_guard = hardlink_or_copy_to_temp(&target, &parent, &name, "prev")?;
      fs::rename(prev_guard.path(), &prev_path).map_err(|source| Error::Io {
         path: prev_path.clone(),
         source,
      })?;
      prev_guard.disarm();
      Some(prev_path)
   } else {
      None
   };

   fs::rename(new_guard.path(), &target).map_err(|source| Error::Io {
      path: target.clone(),
      source,
   })?;
   new_guard.disarm();
   sync_dir(&parent);

   Ok(ApplyOutcome {
      target,
      previous,
      version: release.version.clone(),
   })
}

/// Restores `target` from its `.previous` backup after verifying the backup.
pub fn rollback(target: &Path) -> Result<PathBuf, Error> {
   let target = resolve_target_path(target)?;
   let prev = previous_path(&target);
   if !prev.exists() {
      return Err(Error::NoPrevious { path: prev });
   }

   verify_appimage(&prev, None, None)?;

   let (parent, name) = target_parent_and_name(&target)?;
   let mut rb_guard = hardlink_or_copy_to_temp(&prev, &parent, &name, "rb")?;
   fs::rename(rb_guard.path(), &target).map_err(|source| Error::Io {
      path: target,
      source,
   })?;
   rb_guard.disarm();
   sync_dir(&parent);

   Ok(prev)
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

fn sh_quote(path: &Path) -> Result<String, Error> {
   let bytes = path.as_os_str().as_bytes();
   let text = std::str::from_utf8(bytes)
      .map_err(|_| Error::Watchdog("path is not valid UTF-8".to_owned()))?;
   if text.contains('\0') {
      return Err(Error::Watchdog("path contains NUL byte".to_owned()));
   }
   let mut out = String::with_capacity(text.len() + 2);
   out.push('\'');
   for ch in text.chars() {
      if ch == '\'' {
         out.push_str("'\\''");
      } else {
         out.push(ch);
      }
   }
   out.push('\'');
   Ok(out)
}

fn is_executable_file(path: &Path) -> bool {
   fs::metadata(path).is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
}

fn xdg_sunbird_dir() -> Option<PathBuf> {
   let data_home = env::var_os("XDG_DATA_HOME")
      .filter(|val| !val.is_empty())
      .map(PathBuf::from)
      .or_else(|| {
         env::var_os("HOME")
            .filter(|val| !val.is_empty())
            .map(|home| PathBuf::from(home).join(".local/share"))
      })?;
   Some(data_home.join("sunbird"))
}

fn find_updater_exe(target_abs: &Path) -> Option<PathBuf> {
   let mut persistent = Vec::new();
   let mut ephemeral = Vec::new();

   if let Ok(exe) = env::current_exe()
      && exe.file_name() == Some(OsStr::new("nyaacord-updater"))
   {
      if is_mount_path(&exe) {
         ephemeral.push(exe);
      } else {
         persistent.push(exe);
      }
   }

   if let Some(launcher) = env::var_os("NYAACORD_LAUNCHER").filter(|val| !val.is_empty())
      && let Some(dir) = PathBuf::from(launcher).parent()
   {
      persistent.push(dir.join("nyaacord-updater"));
   }

   if let Some(dir) = target_abs.parent() {
      persistent.push(dir.join("nyaacord-updater"));
   }

   if let Some(dir) = xdg_sunbird_dir() {
      persistent.push(dir.join("nyaacord-updater"));
   }

   for var in ["NYAACORD_APPDIR", "APPDIR"] {
      if let Some(appdir) = env::var_os(var).filter(|val| !val.is_empty()) {
         let candidate = PathBuf::from(appdir).join("usr/bin/nyaacord-updater");
         if is_mount_path(&candidate) {
            ephemeral.push(candidate);
         } else {
            persistent.push(candidate);
         }
      }
   }

   persistent
      .into_iter()
      .find(|path| !is_mount_path(path) && is_executable_file(path))
      .or_else(|| ephemeral.into_iter().find(|path| is_executable_file(path)))
}

fn create_watchdog_script(content: &str) -> Result<PathBuf, Error> {
   let pid = std::process::id();
   let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_or(0, |d| d.as_nanos());
   let tmp_dir = {
      let candidate = env::temp_dir();
      if candidate.as_os_str().is_empty() || is_mount_path(&candidate) {
         PathBuf::from("/tmp")
      } else {
         candidate
      }
   };

   for attempt in 0_u32..16 {
      let suffix = if attempt == 0 {
         format!("{pid}-{nanos}")
      } else {
         format!("{pid}-{nanos}-{attempt}")
      };
      let script_path = tmp_dir.join(format!("nyaacord-relaunch-{suffix}.sh"));
      match OpenOptions::new()
         .write(true)
         .create_new(true)
         .mode(0o700)
         .open(&script_path)
      {
         Ok(mut file) => {
            if let Err(source) = file
               .write_all(content.as_bytes())
               .and_then(|()| file.sync_all())
            {
               let _ = fs::remove_file(&script_path);
               return Err(Error::Io {
                  path: script_path,
                  source,
               });
            }
            return Ok(script_path);
         },
         Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {},
         Err(source) => {
            return Err(Error::Io {
               path: script_path,
               source,
            });
         },
      }
   }

   Err(Error::Watchdog(
      "could not create unique watchdog script".to_owned(),
   ))
}

fn build_watchdog_script(target_q: &str, prev_q: &str, updater_q: &str, wait_pid: u32) -> String {
   // `/proc/<pid>/stat` is checked alongside `kill -0` because a background child
   // that exited stays a zombie until `wait`, and `kill -0` succeeds on zombies.
   format!(
      r#"#!/bin/sh
SCRIPT="$0"
TARGET={target_q}
PREV={prev_q}
UPDATER={updater_q}
WAIT_PID={wait_pid}

cleanup() {{
   rm -f "$SCRIPT"
}}
trap cleanup EXIT INT TERM HUP

unset LD_PRELOAD APPIMAGE APPDIR OWD ARGV0 NYAACORD_APPDIR NYAACORD_VERSION NYAACORD_ICON NYAACORD_NO_RELAUNCH

is_alive() {{
   if ! kill -0 "$1" 2>/dev/null; then
      return 1
   fi
   if [ -r "/proc/$1/stat" ]; then
      STAT=$(cat "/proc/$1/stat" 2>/dev/null || true)
      STATE=${{STAT##*\) }}
      case "$STATE" in
         Z*|X*) return 1 ;;
      esac
   fi
   return 0
}}

verify_prev() {{
   if [ -n "$UPDATER" ] && [ -x "$UPDATER" ]; then
      "$UPDATER" verify --file "$1" >/dev/null 2>&1
      return $?
   fi
   HEX=$(od -An -tx1 -N11 "$1" 2>/dev/null | tr -d ' \t\r\n')
   case "$HEX" in
      7f454c46????????414902) return 0 ;;
      *) return 1 ;;
   esac
}}

if [ "$WAIT_PID" -gt 1 ]; then
   i=0
   while is_alive "$WAIT_PID"; do
      if [ "$i" -eq 10 ]; then
         kill -TERM "$WAIT_PID" 2>/dev/null || true
      elif [ "$i" -eq 50 ]; then
         kill -KILL "$WAIT_PID" 2>/dev/null || true
      elif [ "$i" -ge 75 ]; then
         break
      fi
      sleep 0.2
      i=$((i + 1))
   done
fi

sleep 0.2
NYAACORD_AUTO_UPDATE=0 "$TARGET" </dev/null >/dev/null 2>&1 &
NEW_PID=$!

j=0
while [ "$j" -lt 8 ]; do
   sleep 0.5
   if ! is_alive "$NEW_PID"; then
      wait "$NEW_PID"
      STATUS=$?
      if [ "$STATUS" -ne 0 ] && [ -n "$PREV" ] && [ -f "$PREV" ]; then
         if verify_prev "$PREV"; then
            DIR=$(dirname "$TARGET")
            NAME=$(basename "$TARGET")
            STAMP=$(date +%s 2>/dev/null || echo 0)
            TMP="$DIR/.${{NAME}}.rb.$$.$STAMP"
            rm -f "$TMP"
            if (ln "$PREV" "$TMP" 2>/dev/null || cp -f "$PREV" "$TMP") && chmod 0755 "$TMP" && mv -f "$TMP" "$TARGET"; then
               if [ -f "$DIR/version" ]; then
                  OLD_VER=$(NYAACORD_AUTO_UPDATE=0 "$TARGET" --version 2>/dev/null | sed -n 's/^Nyaa[Cc]ord //p' | head -n 1 | tr -d '\r' | sed 's/^[vV]//' || true)
                  if [ -n "$OLD_VER" ]; then
                     printf '%s\n' "$OLD_VER" > "$DIR/version" || true
                  fi
               fi
               if command -v notify-send >/dev/null 2>&1; then
                  if [ -f "$DIR/nyaacord.png" ]; then
                     notify-send --app-name=Nyaacord --hint=string:desktop-entry:com.discord.libdiscord -i "$DIR/nyaacord.png" -- "Nyaacord update rolled back" "The updated AppImage exited early and the previous version was restored." >/dev/null 2>&1 || true
                  else
                     notify-send --app-name=Nyaacord --hint=string:desktop-entry:com.discord.libdiscord -- "Nyaacord update rolled back" "The updated AppImage exited early and the previous version was restored." >/dev/null 2>&1 || true
                  fi
               fi
               NYAACORD_AUTO_UPDATE=0 "$TARGET" </dev/null >/dev/null 2>&1 &
            else
               rm -f "$TMP"
            fi
         fi
      fi
      exit 0
   fi
   j=$((j + 1))
done

exit 0
"#
   )
}

/// Spawns a detached POSIX watchdog that waits for `wait_pid`, relaunches
/// `target`, and rolls back to `previous` if the new `AppImage` crashes on start.
pub fn spawn_relaunch_watchdog(
   target: &Path,
   previous: Option<&Path>,
   wait_pid: u32,
) -> Result<(), Error> {
   ensure_not_mount_path(target)?;
   let target_abs = absolutize(target)?;
   ensure_not_mount_path(&target_abs)?;
   let target_q = sh_quote(&target_abs)?;
   let prev_q = match previous {
      Some(p) => sh_quote(&absolutize(p)?)?,
      None => "''".to_owned(),
   };
   let updater_q = match find_updater_exe(&target_abs) {
      Some(exe) => sh_quote(&exe)?,
      None => "''".to_owned(),
   };

   let script = build_watchdog_script(&target_q, &prev_q, &updater_q, wait_pid);
   let script_path = create_watchdog_script(&script)?;

   let mut cmd = Command::new("/bin/sh");
   cmd.arg(&script_path)
      .env_remove("LD_PRELOAD")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null());

   // SAFETY: `setsid` is async signal safe per POSIX.
   unsafe {
      cmd.pre_exec(|| {
         if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
         }
         Ok(())
      });
   }

   if let Err(err) = cmd.spawn() {
      let _ = fs::remove_file(&script_path);
      return Err(Error::Watchdog(format!("failed to spawn /bin/sh: {err}")));
   }

   Ok(())
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn previous_path_appends_suffix() {
      let p = Path::new("/opt/Nyaacord-x86_64.AppImage");
      assert_eq!(
         previous_path(p),
         PathBuf::from("/opt/Nyaacord-x86_64.AppImage.previous")
      );
   }

   #[test]
   fn detects_mount_paths() {
      let appdirs = vec![PathBuf::from("/custom/extracted_appdir")];
      assert!(is_mount_path_with_appdirs(
         Path::new("/tmp/.mount_NyaaCo12345/usr/bin/app"),
         &appdirs
      ));
      assert!(is_mount_path_with_appdirs(
         Path::new("/tmp/.mount_abc"),
         &appdirs
      ));
      assert!(is_mount_path_with_appdirs(
         Path::new("/home/cate/.t3/tmp/.mount_NyaaCo9999/Nyaacord.AppImage"),
         &appdirs
      ));
      assert!(is_mount_path_with_appdirs(
         Path::new("/custom/extracted_appdir/Nyaacord.AppImage"),
         &appdirs
      ));
      assert!(!is_mount_path_with_appdirs(
         Path::new("/tmp/nyaacord-test/app.AppImage"),
         &appdirs
      ));
      assert!(!is_mount_path_with_appdirs(
         Path::new("/home/cate/Apps/Nyaacord.AppImage"),
         &appdirs
      ));
   }

   #[test]
   fn quotes_shell_paths_safely() {
      let path = Path::new("/tmp/it's a \"test\"/$HOME/`id`/app.AppImage");
      assert_eq!(
         sh_quote(path).expect("quoted"),
         "'/tmp/it'\\''s a \"test\"/$HOME/`id`/app.AppImage'"
      );
   }

   #[test]
   fn watchdog_script_uses_discord_desktop_entry_hint() {
      let script = build_watchdog_script(
         "'/tmp/Nyaacord.AppImage'",
         "'/tmp/Nyaacord.AppImage.previous'",
         "'/tmp/nyaacord-updater'",
         42,
      );
      assert!(script.contains("--hint=string:desktop-entry:com.discord.libdiscord"));
      assert!(!script.contains("--hint=string:desktop-entry:sunbird"));
   }
}
