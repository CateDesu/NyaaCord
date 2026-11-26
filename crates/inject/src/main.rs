//! `NyaaCord-x86_64.exe`, the self-contained Windows launcher, also usable as
//! `phoenix-inject [phoenix.dll] [-- discord args]`.
//!
//! `Discord.exe` resolves its imports from System32 only (`DependentLoadFlags`
//! 0x800), so a DLL planted beside it is never loaded. This starts it
//! suspended and queues `LoadLibraryW(phoenix.dll)` as an APC on its main
//! thread, which runs once the OS loader finishes and before Discord's own
//! code.
//!
//! Without a DLL argument it installs itself, `phoenix.dll`,
//! `libplm.plugins.so`, and any bundled Discord Phoenix client into
//! `%LOCALAPPDATA%\NyaaCord` with Start Menu and Desktop shortcuts. User data
//! and settings in `%LOCALAPPDATA%\DiscordPhoenix` stay untouched across
//! updates and uninstalls. `--uninstall` removes the install and shortcuts.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
   ffi::{OsStr, OsString},
   path::{Path, PathBuf},
   sync::atomic::{AtomicU64, Ordering},
   time::{SystemTime, UNIX_EPOCH},
};

use misstep::{OptionExt, ResultExt, bail, ensure};

#[cfg_attr(
   not(windows),
   allow(dead_code, reason = "embedded payloads are extracted on Windows")
)]
mod embedded {
   include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));
}

const VERSION: &str = env!("NYAACORD_VERSION");

const USAGE: &str = "\
Usage: NyaaCord-x86_64.exe [options] [-- Discord arguments]

Options:
  --install-only        Install to %LOCALAPPDATA%\\NyaaCord and create shortcuts without launching
  --check-update        Check GitHub Releases for a newer NyaaCord release
  --update              Download, verify, and replace NyaaCord with the latest release
  --rollback            Restore the previous NyaaCord backup
  --uninstall           Remove shortcuts and %LOCALAPPDATA%\\NyaaCord
  --no-runtime          Launch Discord with only the access gate opened
  --gate-only           Alias for --no-runtime
  --target-exe <path>   Override the Discord.exe path (or set NYAACORD_DISCORD_EXE)
  --quiet, -q           Suppress status messages and modal dialogs
  --version             Print NyaaCord version
  --help, -h            Show this help message";

#[derive(Debug, PartialEq, Eq)]
enum Mode {
   Version,
   Help,
   InstallOnly,
   CheckUpdate {
      repo: Option<String>,
      api: Option<String>,
      timeout: Option<i64>,
      json: bool,
   },
   Update {
      target: Option<PathBuf>,
      repo: Option<String>,
      api: Option<String>,
      timeout: Option<i64>,
      force: bool,
   },
   Rollback {
      target: Option<PathBuf>,
   },
   Uninstall,
   Launch {
      explicit_dll: Option<PathBuf>,
      target_exe: Option<PathBuf>,
      no_runtime: bool,
      discord_args: Vec<OsString>,
   },
}

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
   mode: Mode,
   quiet: bool,
}

fn next_str(args: &mut impl Iterator<Item = OsString>, flag: &str) -> misstep::Result<String> {
   let raw = args
      .next()
      .with_context(|| format!("missing value for {flag}\n{USAGE}"))?;
   raw.into_string()
      .map_err(|_| misstep::Report::msg(format!("{flag} value is not valid UTF-8")))
}

fn next_path(args: &mut impl Iterator<Item = OsString>, flag: &str) -> misstep::Result<PathBuf> {
   args
      .next()
      .map(PathBuf::from)
      .with_context(|| format!("missing value for {flag}\n{USAGE}"))
}

fn parse_timeout(raw: &str) -> misstep::Result<i64> {
   let secs = raw
      .parse::<i64>()
      .with_context(|| format!("invalid --timeout {raw:?}"))?;
   ensure!(secs > 0, "--timeout must be a positive number of seconds");
   Ok(secs)
}

fn split_flag_value<'a>(arg: &'a str, flag: &str) -> Option<&'a str> {
   arg.strip_prefix(flag)?.strip_prefix('=')
}

fn parse_check_update(
   args: &mut impl Iterator<Item = OsString>,
   quiet: &mut bool,
) -> misstep::Result<Mode> {
   let mut repo = None;
   let mut api = None;
   let mut timeout = None;
   let mut json = false;

   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--repo") => repo = Some(next_str(args, "--repo")?),
         Some("--api") => api = Some(next_str(args, "--api")?),
         Some("--timeout") => timeout = Some(parse_timeout(&next_str(args, "--timeout")?)?),
         Some("--json") => json = true,
         Some("--quiet" | "-q") => *quiet = true,
         Some("-h" | "--help") => return Ok(Mode::Help),
         Some(other) => {
            if let Some(val) = split_flag_value(other, "--repo") {
               repo = Some(val.to_owned());
            } else if let Some(val) = split_flag_value(other, "--api") {
               api = Some(val.to_owned());
            } else if let Some(val) = split_flag_value(other, "--timeout") {
               timeout = Some(parse_timeout(val)?);
            } else {
               bail!("unexpected argument {other:?} for --check-update\n{USAGE}");
            }
         },
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   Ok(Mode::CheckUpdate {
      repo,
      api,
      timeout,
      json,
   })
}

fn parse_update(
   args: &mut impl Iterator<Item = OsString>,
   quiet: &mut bool,
) -> misstep::Result<Mode> {
   let mut target = None;
   let mut repo = None;
   let mut api = None;
   let mut timeout = None;
   let mut force = false;

   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--target") => target = Some(next_path(args, "--target")?),
         Some("--repo") => repo = Some(next_str(args, "--repo")?),
         Some("--api") => api = Some(next_str(args, "--api")?),
         Some("--timeout") => timeout = Some(parse_timeout(&next_str(args, "--timeout")?)?),
         Some("--force") => force = true,
         Some("--quiet" | "-q") => *quiet = true,
         Some("-h" | "--help") => return Ok(Mode::Help),
         Some(other) => {
            if let Some(val) = split_flag_value(other, "--target") {
               target = Some(PathBuf::from(val));
            } else if let Some(val) = split_flag_value(other, "--repo") {
               repo = Some(val.to_owned());
            } else if let Some(val) = split_flag_value(other, "--api") {
               api = Some(val.to_owned());
            } else if let Some(val) = split_flag_value(other, "--timeout") {
               timeout = Some(parse_timeout(val)?);
            } else {
               bail!("unexpected argument {other:?} for --update\n{USAGE}");
            }
         },
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   Ok(Mode::Update {
      target,
      repo,
      api,
      timeout,
      force,
   })
}

fn parse_rollback(
   args: &mut impl Iterator<Item = OsString>,
   quiet: &mut bool,
) -> misstep::Result<Mode> {
   let mut target = None;

   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--target") => target = Some(next_path(args, "--target")?),
         Some("--quiet" | "-q") => *quiet = true,
         Some("-h" | "--help") => return Ok(Mode::Help),
         Some(other) => {
            if let Some(val) = split_flag_value(other, "--target") {
               target = Some(PathBuf::from(val));
            } else {
               bail!("unexpected argument {other:?} for --rollback\n{USAGE}");
            }
         },
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   Ok(Mode::Rollback { target })
}

fn parse_quiet_only(
   args: impl Iterator<Item = OsString>,
   flag: &str,
   mode: Mode,
   quiet: &mut bool,
) -> misstep::Result<Mode> {
   for rest in args {
      match rest.to_str() {
         Some("--quiet" | "-q") => *quiet = true,
         Some("-h" | "--help") => return Ok(Mode::Help),
         Some(other) => bail!("unexpected argument {other:?} for {flag}\n{USAGE}"),
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }
   Ok(mode)
}

fn parse_args<I>(iter: I) -> misstep::Result<CliOptions>
where
   I: IntoIterator<Item = OsString>,
{
   let mut args = iter.into_iter().peekable();
   let mut quiet = false;
   let mut no_runtime = false;
   let mut target_exe: Option<PathBuf> = None;
   let mut explicit_dll: Option<PathBuf> = None;
   let mut discord_args: Vec<OsString> = Vec::new();

   while let Some(arg) = args.next() {
      if arg == "--" {
         discord_args.extend(args);
         break;
      }
      if arg == "--quiet" || arg == "-q" {
         quiet = true;
         continue;
      }
      if arg == "--version" || arg == "-V" {
         return Ok(CliOptions {
            mode: Mode::Version,
            quiet,
         });
      }
      if arg == "--help" || arg == "-h" {
         return Ok(CliOptions {
            mode: Mode::Help,
            quiet,
         });
      }
      if arg == "--install-only" {
         let mode = parse_quiet_only(args, "--install-only", Mode::InstallOnly, &mut quiet)?;
         return Ok(CliOptions { mode, quiet });
      }
      if arg == "--uninstall" {
         let mode = parse_quiet_only(args, "--uninstall", Mode::Uninstall, &mut quiet)?;
         return Ok(CliOptions { mode, quiet });
      }
      if arg == "--check-update" {
         let mode = parse_check_update(&mut args, &mut quiet)?;
         return Ok(CliOptions { mode, quiet });
      }
      if arg == "--update" {
         let mode = parse_update(&mut args, &mut quiet)?;
         return Ok(CliOptions { mode, quiet });
      }
      if arg == "--rollback" {
         let mode = parse_rollback(&mut args, &mut quiet)?;
         return Ok(CliOptions { mode, quiet });
      }
      if arg == "--no-runtime" || arg == "--gate-only" {
         no_runtime = true;
         continue;
      }
      if arg == "--target-exe" {
         target_exe = Some(next_path(&mut args, "--target-exe")?);
         continue;
      }
      if let Some(text) = arg.to_str()
         && let Some(val) = split_flag_value(text, "--target-exe")
      {
         ensure!(!val.is_empty(), "--target-exe requires a non-empty path");
         target_exe = Some(PathBuf::from(val));
         continue;
      }
      if explicit_dll.is_none()
         && Path::new(&arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
      {
         explicit_dll = Some(PathBuf::from(arg));
         continue;
      }
      discord_args.push(arg);
   }

   Ok(CliOptions {
      mode: Mode::Launch {
         explicit_dll,
         target_exe,
         no_runtime,
         discord_args,
      },
      quiet,
   })
}

/// Quotes one argument according to Windows `CommandLineToArgvW` / MSVCRT rules.
#[cfg_attr(
   not(any(windows, test)),
   allow(
      dead_code,
      reason = "used by Windows CreateProcessW and cross-platform tests"
   )
)]
fn quote_windows_arg(arg: &OsStr, always_quote: bool) -> String {
   let text = arg.to_string_lossy();
   let needs_quotes =
      always_quote || text.is_empty() || text.bytes().any(|b| matches!(b, b' ' | b'\t' | b'"'));
   if !needs_quotes {
      return text.into_owned();
   }

   let mut out = String::with_capacity(text.len() + 2);
   out.push('"');
   let mut backslashes: usize = 0;
   for ch in text.chars() {
      match ch {
         '\\' => backslashes += 1,
         '"' => {
            for _ in 0..=(backslashes * 2) {
               out.push('\\');
            }
            out.push('"');
            backslashes = 0;
         },
         _ => {
            for _ in 0..backslashes {
               out.push('\\');
            }
            out.push(ch);
            backslashes = 0;
         },
      }
   }
   for _ in 0..backslashes * 2 {
      out.push('\\');
   }
   out.push('"');
   out
}

/// Builds a properly quoted Windows command line for `CreateProcessW`.
#[cfg_attr(
   not(any(windows, test)),
   allow(
      dead_code,
      reason = "used by Windows CreateProcessW and cross-platform tests"
   )
)]
fn build_windows_cmdline(exe: &Path, args: &[OsString]) -> String {
   let mut line = quote_windows_arg(exe.as_os_str(), true);
   for arg in args {
      line.push(' ');
      line.push_str(&quote_windows_arg(arg, false));
   }
   line
}

#[cfg(windows)]
fn main() {
   misstep::install_panic_hook();
   let has_console = windows::init_console();
   let parsed = match parse_args(std::env::args_os().skip(1)) {
      Ok(opts) => opts,
      Err(err) => {
         let ui = windows::Ui::new(false, has_console);
         ui.error(&format!("{err}"));
         std::process::exit(1);
      },
   };
   let ui = windows::Ui::new(parsed.quiet, has_console);
   if let Err(err) = windows::run(parsed.mode, &ui) {
      ui.error(&format!("{err:?}"));
      std::process::exit(1);
   }
}

#[cfg(not(windows))]
fn main() {
   match parse_args(std::env::args_os().skip(1)) {
      Ok(CliOptions {
         mode: Mode::Version,
         ..
      }) => println!("NyaaCord {VERSION}"),
      Ok(CliOptions {
         mode: Mode::Help, ..
      }) => println!("{USAGE}"),
      Ok(_) => {
         eprintln!("phoenix-inject only runs on Windows");
         std::process::exit(1);
      },
      Err(err) => {
         eprintln!("{err}");
         std::process::exit(1);
      },
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client helpers run on Windows and in tests")
)]
const CLIENT_STAMP_ENTRY: &str = "nyaacord-client.stamp";
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client helpers run on Windows and in tests")
)]
const CLIENT_STAMP_FILE: &str = "client-stamp.txt";
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client helpers run on Windows and in tests")
)]
const CLIENT_DIR_NAME: &str = "app";
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client helpers run on Windows and in tests")
)]
const CLIENT_EXE_NAME: &str = "Discord.exe";
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client helpers run on Windows and in tests")
)]
const MAX_STAMP_BYTES: usize = 128;

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "temp naming helper used on Windows and in tests")
)]
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "install home helper used on Windows and in tests")
)]
fn install_home_in(local: &Path) -> PathBuf {
   local.join("NyaaCord")
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "legacy client path helper used on Windows and in tests")
)]
fn legacy_discord_exe_in(local: &Path) -> PathBuf {
   local
      .join("DiscordPhoenix")
      .join(CLIENT_DIR_NAME)
      .join(CLIENT_EXE_NAME)
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client path helper used on Windows and in tests")
)]
fn bundled_discord_exe_in(local: &Path) -> PathBuf {
   install_home_in(local)
      .join(CLIENT_DIR_NAME)
      .join(CLIENT_EXE_NAME)
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "path matching helper used on Windows and in tests")
)]
fn paths_equal(a: &Path, b: &Path) -> bool {
   if a == b {
      return true;
   }
   #[cfg(windows)]
   {
      let norm = |p: &Path| {
         let s = p.as_os_str().to_string_lossy().replace('/', "\\");
         s.strip_prefix(r"\\?\").unwrap_or(&s).to_lowercase()
      };
      if norm(a) == norm(b) {
         return true;
      }
   }
   a.canonicalize()
      .ok()
      .zip(b.canonicalize().ok())
      .is_some_and(|(ca, cb)| ca == cb)
}

/// Returns the target Discord executable path first, followed by the alternate
/// known NyaaCord/DiscordPhoenix install path when `exe` is one of the two and
/// was not explicitly overridden.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "used on Windows process matching and in tests")
)]
fn discord_match_paths(exe: &Path, local: &Path, explicit_target: bool) -> Vec<PathBuf> {
   if explicit_target {
      return vec![exe.to_path_buf()];
   }
   let bundled = bundled_discord_exe_in(local);
   let legacy = legacy_discord_exe_in(local);
   if paths_equal(exe, &bundled) {
      vec![exe.to_path_buf(), legacy]
   } else if paths_equal(exe, &legacy) {
      vec![exe.to_path_buf(), bundled]
   } else {
      vec![exe.to_path_buf()]
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "temp naming helper used on Windows and in tests")
)]
fn unique_sibling(dir: &Path, base: &str, tag: &str) -> PathBuf {
   let pid = std::process::id();
   let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_or(0, |d| d.as_nanos());
   let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
   dir.join(format!("{base}.{pid}.{nanos}.{seq}.{tag}"))
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client cleanup runs on Windows and in tests")
)]
fn is_old_client_dir(name: &str, prefix: &str) -> bool {
   name.starts_with(prefix)
      && Path::new(name)
         .extension()
         .is_some_and(|ext| ext.eq_ignore_ascii_case("old"))
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client cleanup runs on Windows and in tests")
)]
fn cleanup_old_dirs(dir: &Path, base: &str) {
   let prefix = format!("{base}.");
   let Ok(entries) = std::fs::read_dir(dir) else {
      return;
   };
   for entry in entries.flatten() {
      if entry
         .file_name()
         .to_str()
         .is_some_and(|name| is_old_client_dir(name, &prefix))
      {
         let _ = std::fs::remove_dir_all(entry.path());
      }
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "install file cleanup runs on Windows and in tests")
)]
fn cleanup_old_files(dir: &Path, base: &str) {
   let prefix = format!("{base}.");
   let Ok(entries) = std::fs::read_dir(dir) else {
      return;
   };
   for entry in entries.flatten() {
      if let Some(name) = entry.file_name().to_str()
         && name.starts_with(&prefix)
         && Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("old"))
      {
         let _ = std::fs::remove_file(entry.path());
      }
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "install lock helper runs on Windows and in tests")
)]
fn acquire_install_lock(dir: &Path) -> misstep::Result<phoenix_updater::pe::UpdateLock> {
   Ok(phoenix_updater::pe::UpdateLock::acquire(dir).with_context(|| {
      format!(
         "locking {}",
         dir.join(phoenix_updater::pe::INSTALL_LOCK_FILE).display()
      )
   })?)
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "backup recovery runs on Windows and in tests")
)]
fn recover_client_backup(home: &Path, app_dir: &Path) {
   if app_dir.exists() {
      return;
   }
   let prefix = format!("{CLIENT_DIR_NAME}.");
   let Ok(entries) = std::fs::read_dir(home) else {
      return;
   };
   let mut candidates: Vec<(Option<SystemTime>, String, PathBuf)> = Vec::new();
   for entry in entries.flatten() {
      let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
         continue;
      };
      if !is_old_client_dir(&name, &prefix) {
         continue;
      }
      let path = entry.path();
      if path.join(CLIENT_EXE_NAME).is_file() {
         let mtime = entry.metadata().and_then(|meta| meta.modified()).ok();
         candidates.push((mtime, name, path));
      }
   }
   candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
   if let Some((_, _, best)) = candidates.pop() {
      let _ = std::fs::rename(best, app_dir);
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "staging guard used on Windows and in tests")
)]
struct StagingDirGuard {
   path: PathBuf,
   armed: bool,
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "staging guard used on Windows and in tests")
)]
impl StagingDirGuard {
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

impl Drop for StagingDirGuard {
   fn drop(&mut self) {
      if self.armed {
         let _ = std::fs::remove_dir_all(&self.path);
      }
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "staging file guard used on Windows and in tests")
)]
struct StagingFileGuard {
   path: PathBuf,
   armed: bool,
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "staging file guard used on Windows and in tests")
)]
impl StagingFileGuard {
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

impl Drop for StagingFileGuard {
   fn drop(&mut self) {
      if self.armed {
         let _ = std::fs::remove_file(&self.path);
      }
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "stream copy helper used on Windows and in tests")
)]
fn stream_copy(
   reader: &mut impl std::io::Read,
   writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
   let mut buf = vec![0_u8; 256 * 1024];
   loop {
      match reader.read(&mut buf) {
         Ok(0) => return Ok(()),
         Ok(n) => writer.write_all(&buf[..n])?,
         Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {},
         Err(err) => return Err(err),
      }
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "streaming archive hasher runs on Windows and in tests")
)]
fn stream_hash_and_copy(
   reader: &mut impl std::io::Read,
   mut writer: Option<&mut std::fs::File>,
) -> std::io::Result<(u64, String)> {
   use std::io::Write as _;
   let mut buf = vec![0_u8; 256 * 1024];
   let mut hasher = phoenix_updater::sha256::Hasher::new();
   let mut total: u64 = 0;
   loop {
      match reader.read(&mut buf) {
         Ok(0) => return Ok((total, hasher.finalize_hex())),
         Ok(n) => {
            let chunk = &buf[..n];
            hasher.update(chunk);
            if let Some(out) = writer.as_deref_mut() {
               out.write_all(chunk)?;
            }
            total += n as u64;
         },
         Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {},
         Err(err) => return Err(err),
      }
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "manifest path helper runs on Windows and in tests")
)]
fn rel_path_posix(rel: &Path) -> misstep::Result<String> {
   let mut parts = Vec::new();
   for comp in rel.components() {
      match comp {
         std::path::Component::Normal(part) => {
            let text = part.to_str().with_context(|| {
               format!(
                  "non-UTF-8 path {} inside bundled client archive",
                  rel.display()
               )
            })?;
            parts.push(text);
         },
         _ => bail!("unsafe path {} inside bundled client archive", rel.display()),
      }
   }
   Ok(parts.join("/"))
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "manifest stamp helper runs on Windows and in tests")
)]
fn compute_manifest_stamp(entries: &mut [(String, u64, String)]) -> misstep::Result<String> {
   entries.sort_by(|a, b| a.0.cmp(&b.0));
   for pair in entries.windows(2) {
      ensure!(
         pair[0].0 != pair[1].0,
         "duplicate path {:?} inside bundled client archive",
         pair[0].0
      );
   }
   let mut hasher = phoenix_updater::sha256::Hasher::new();
   for (rel, size, digest) in entries.iter() {
      hasher.update(format!("{rel}\0{size}\0{digest}\n").as_bytes());
   }
   Ok(format!("v1:{}", hasher.finalize_hex()))
}

/// Swaps `staging` into `app_dir`, returning any displaced backup path (still on disk)
/// so the caller can roll back if a later step fails.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client swap runs on Windows and in tests")
)]
fn swap_client_dir_keep_backup(
   home: &Path,
   mut staging: StagingDirGuard,
   app_dir: &Path,
) -> misstep::Result<(Option<PathBuf>, bool)> {
   if app_dir.exists() {
      let backup = unique_sibling(home, CLIENT_DIR_NAME, "old");
      std::fs::rename(app_dir, &backup).with_context(|| {
         format!(
            "Close Discord before updating the bundled Discord Phoenix client at {}",
            app_dir.display()
         )
      })?;
      if let Err(err) = std::fs::rename(staging.path(), app_dir) {
         let _ = std::fs::rename(&backup, app_dir);
         return Err(err).with_context(|| format!("activating {}", app_dir.display()))?;
      }
      staging.disarm();
      Ok((Some(backup), false))
   } else {
      std::fs::rename(staging.path(), app_dir)
         .with_context(|| format!("activating {}", app_dir.display()))?;
      staging.disarm();
      Ok((None, true))
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "installed version policy runs on Windows and in tests")
)]
#[derive(Debug, PartialEq, Eq)]
enum InstalledAction {
   HandOffToInstalled,
   CheckWith(String),
   InstalledVersionUnknown,
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "installed version policy runs on Windows and in tests")
)]
fn decide_installed_action(
   running_ver: &str,
   installed_ver: Option<&str>,
   same_binary: bool,
) -> InstalledAction {
   use phoenix_updater::version::{self, Version};

   let running_norm = version::normalize(running_ver);
   if same_binary {
      return InstalledAction::CheckWith(running_norm);
   }
   let Some(inst_raw) = installed_ver.filter(|val| !val.trim().is_empty()) else {
      return InstalledAction::InstalledVersionUnknown;
   };
   let inst_norm = version::normalize(inst_raw);
   let Some(installed) = Version::parse(&inst_norm) else {
      return InstalledAction::InstalledVersionUnknown;
   };

   match Version::parse(&running_norm) {
      Some(running) if installed > running => InstalledAction::HandOffToInstalled,
      _ => InstalledAction::CheckWith(running_norm),
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "installed version error helper runs on Windows and in tests")
)]
fn installed_version_unknown_error(existing_launcher: &Path) -> misstep::Report {
   misstep::Report::msg(format!(
      "could not verify version of installed NyaaCord at {}; refusing to overwrite. Run --install-only to reinstall.",
      existing_launcher.display()
   ))
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "installed launcher dispatch runs on Windows and in tests")
)]
fn run_installed_action<FHandOff, FUpdate>(
   running_ver: &str,
   installed_ver: Option<&str>,
   same_binary: bool,
   existing_launcher: &Path,
   hand_off: FHandOff,
   auto_update: FUpdate,
) -> misstep::Result<bool>
where
   FHandOff: FnOnce() -> misstep::Result<()>,
   FUpdate: FnOnce(&str) -> bool,
{
   match decide_installed_action(running_ver, installed_ver, same_binary) {
      InstalledAction::HandOffToInstalled => {
         hand_off()?;
         Ok(true)
      },
      InstalledAction::CheckWith(check_ver) => Ok(auto_update(&check_ver)),
      InstalledAction::InstalledVersionUnknown => {
         Err(installed_version_unknown_error(existing_launcher))
      },
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "CLI update precheck runs on Windows and in tests")
)]
#[derive(Debug, PartialEq, Eq)]
enum ManualUpdatePrecheck {
   ForceFetch,
   CheckVersion(String),
   UnknownTarget,
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "CLI update precheck runs on Windows and in tests")
)]
fn plan_manual_update(
   running_ver: &str,
   target_exists: bool,
   same_binary: bool,
   target_ver: Option<&str>,
   force: bool,
) -> ManualUpdatePrecheck {
   use phoenix_updater::version::{self, Version};

   if force {
      return ManualUpdatePrecheck::ForceFetch;
   }
   if !target_exists || same_binary {
      return ManualUpdatePrecheck::CheckVersion(version::normalize(running_ver));
   }
   let Some(inst_raw) = target_ver.filter(|val| !val.trim().is_empty()) else {
      return ManualUpdatePrecheck::UnknownTarget;
   };
   let inst_norm = version::normalize(inst_raw);
   if Version::parse(&inst_norm).is_none() {
      return ManualUpdatePrecheck::UnknownTarget;
   }
   ManualUpdatePrecheck::CheckVersion(inst_norm)
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "CLI update locked guard runs on Windows and in tests")
)]
#[derive(Debug, PartialEq, Eq)]
enum LockedUpdateAction {
   Apply,
   UpToDate(String),
   UnknownTarget,
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "CLI update locked guard runs on Windows and in tests")
)]
fn decide_locked_manual_update(
   target_exists: bool,
   live_ver: Option<&str>,
   release_ver: &str,
   force: bool,
) -> LockedUpdateAction {
   use phoenix_updater::pe::{InstallUpdateDecision, check_installed_for_release};

   if force || !target_exists {
      return LockedUpdateAction::Apply;
   }
   match check_installed_for_release(live_ver, release_ver) {
      InstallUpdateDecision::Apply => LockedUpdateAction::Apply,
      InstallUpdateDecision::AlreadyCurrent => LockedUpdateAction::UpToDate(
         live_ver.map_or_else(
            || phoenix_updater::version::normalize(release_ver),
            phoenix_updater::version::normalize,
         ),
      ),
      InstallUpdateDecision::UnknownInstalled => LockedUpdateAction::UnknownTarget,
   }
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "client stamp parser runs on Windows and in tests")
)]
fn parse_client_stamp(raw: &str) -> Option<String> {
   let hex = raw.trim().strip_prefix("v1:")?;
   if hex.len() != 64 {
      return None;
   }
   let norm = phoenix_updater::sha256::normalize_hex(hex)?;
   Some(format!("v1:{norm}"))
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bounded stamp reader runs on Windows and in tests")
)]
fn read_bounded_stamp(reader: impl std::io::Read, label: &str) -> misstep::Result<String> {
   use std::io::Read as _;
   let limit = (MAX_STAMP_BYTES + 1) as u64;
   let mut buf = Vec::new();
   reader
      .take(limit)
      .read_to_end(&mut buf)
      .with_context(|| format!("reading {label}"))?;
   ensure!(
      buf.len() <= MAX_STAMP_BYTES,
      "{label} exceeds {MAX_STAMP_BYTES} bytes"
   );
   let text = String::from_utf8(buf).with_context(|| format!("{label} is not valid UTF-8"))?;
   parse_client_stamp(&text).with_context(|| format!("invalid client stamp in {label}"))
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client stamp reader runs on Windows and in tests")
)]
fn read_archive_stamp<R: std::io::Read + std::io::Seek>(
   archive: &mut zip::ZipArchive<R>,
   exe_path: &Path,
) -> misstep::Result<String> {
   let entry = archive
      .by_name(CLIENT_STAMP_ENTRY)
      .with_context(|| format!("reading {CLIENT_STAMP_ENTRY} from {}", exe_path.display()))?;
   read_bounded_stamp(
      entry,
      &format!("{CLIENT_STAMP_ENTRY} in {}", exe_path.display()),
   )
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "installed stamp reader runs on Windows and in tests")
)]
fn read_installed_stamp(path: &Path) -> Option<String> {
   let file = std::fs::File::open(path).ok()?;
   read_bounded_stamp(file, &path.display().to_string()).ok()
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "PE overlay detection runs on Windows and in tests")
)]
fn pe_cert_directory(opt: &[u8]) -> (u64, u64) {
   if opt.len() < 152 {
      return (0, 0);
   }
   let num_dirs = u32::from_le_bytes([opt[108], opt[109], opt[110], opt[111]]);
   if num_dirs <= 4 {
      return (0, 0);
   }
   let off = u64::from(u32::from_le_bytes([opt[144], opt[145], opt[146], opt[147]]));
   let sz = u64::from(u32::from_le_bytes([opt[148], opt[149], opt[150], opt[151]]));
   (off, sz)
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "PE overlay detection runs on Windows and in tests")
)]
fn pe_sections_end(
   file: &mut std::fs::File,
   num_sections: u16,
   headers_end: u64,
   file_len: u64,
) -> misstep::Result<u64> {
   use std::io::Read as _;
   let mut pe_end = headers_end;
   let mut sec = [0_u8; 40];
   for _ in 0..num_sections {
      file
         .read_exact(&mut sec)
         .context("reading PE section header")?;
      let raw_size = u64::from(u32::from_le_bytes([sec[16], sec[17], sec[18], sec[19]]));
      let raw_ptr = u64::from(u32::from_le_bytes([sec[20], sec[21], sec[22], sec[23]]));
      if raw_size > 0 {
         let sec_end = raw_ptr
            .checked_add(raw_size)
            .context("PE section end overflow")?;
         ensure!(sec_end <= file_len, "truncated PE section data");
         pe_end = pe_end.max(sec_end);
      }
   }
   Ok(pe_end)
}

/// Checks whether a PE32+ executable has trailing non-certificate overlay bytes.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "PE overlay detection runs on Windows and in tests")
)]
fn detect_pe_overlay(file: &mut std::fs::File) -> misstep::Result<bool> {
   use std::io::{Read as _, Seek as _, SeekFrom};

   const DOS_LEN: usize = 64;
   const COFF_LEN: usize = 24;
   const SEC_LEN: u64 = 40;
   const AMD64_MACHINE: u16 = 0x8664;
   const PE32_PLUS_MAGIC: u16 = 0x020B;

   let file_len = file
      .metadata()
      .context("reading executable metadata")?
      .len();
   file.seek(SeekFrom::Start(0)).context("seeking PE start")?;

   let mut dos = [0_u8; DOS_LEN];
   file
      .read_exact(&mut dos)
      .context("reading PE DOS header")?;
   ensure!(&dos[0..2] == b"MZ", "missing MZ header in launcher");

   let e_lfanew = u64::from(u32::from_le_bytes([dos[60], dos[61], dos[62], dos[63]]));
   ensure!(
      e_lfanew >= DOS_LEN as u64
         && e_lfanew
            .checked_add(COFF_LEN as u64)
            .is_some_and(|end| end <= file_len),
      "invalid PE header offset {e_lfanew}"
   );

   file
      .seek(SeekFrom::Start(e_lfanew))
      .context("seeking COFF header")?;
   let mut coff = [0_u8; COFF_LEN];
   file
      .read_exact(&mut coff)
      .context("reading PE COFF header")?;
   ensure!(&coff[0..4] == b"PE\0\0", "missing PE signature in launcher");

   let machine = u16::from_le_bytes([coff[4], coff[5]]);
   ensure!(
      machine == AMD64_MACHINE,
      "unsupported PE machine {machine:#06X}"
   );
   let num_sections = u16::from_le_bytes([coff[6], coff[7]]);
   let opt_size = u16::from_le_bytes([coff[20], coff[21]]);
   ensure!(opt_size >= 2, "invalid PE optional header size {opt_size}");

   let sec_off = (e_lfanew + COFF_LEN as u64)
      .checked_add(u64::from(opt_size))
      .context("PE section table offset overflow")?;
   let headers_end = sec_off
      .checked_add(u64::from(num_sections) * SEC_LEN)
      .context("PE section table size overflow")?;
   ensure!(headers_end <= file_len, "truncated PE headers");

   let mut opt = vec![0_u8; usize::from(opt_size)];
   file
      .read_exact(&mut opt)
      .context("reading PE optional header")?;
   let opt_magic = u16::from_le_bytes([opt[0], opt[1]]);
   ensure!(
      opt_magic == PE32_PLUS_MAGIC,
      "unsupported PE optional header magic {opt_magic:#06X}"
   );

   let (cert_off, cert_size) = pe_cert_directory(&opt);
   let pe_end = pe_sections_end(file, num_sections, headers_end, file_len)?;

   let has_overlay = if file_len == pe_end {
      false
   } else if cert_size > 0
      && cert_off >= pe_end
      && cert_off - pe_end <= 7
      && cert_off.checked_add(cert_size) == Some(file_len)
   {
      let pad_len = usize::try_from(cert_off - pe_end).unwrap_or(0);
      let mut pad = [0_u8; 7];
      if pad_len > 0 {
         file
            .seek(SeekFrom::Start(pe_end))
            .context("seeking PE cert padding")?;
         file
            .read_exact(&mut pad[..pad_len])
            .context("reading PE cert padding")?;
      }
      pad[..pad_len].iter().any(|&b| b != 0)
   } else {
      true
   };

   file.seek(SeekFrom::Start(0)).context("rewinding PE file")?;
   Ok(has_overlay)
}

/// Decompresses every payload entry in streaming chunks, checks CRC-32, and verifies
/// the deterministic manifest SHA-256 matches `want_stamp`.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "archive payload verifier runs on Windows and in tests")
)]
fn verify_archive_payload<R: std::io::Read + std::io::Seek>(
   archive: &mut zip::ZipArchive<R>,
   exe_path: &Path,
   want_stamp: &str,
) -> misstep::Result<()> {
   let mut manifest: Vec<(String, u64, String)> = Vec::new();
   let mut has_discord = false;
   for idx in 0..archive.len() {
      let mut entry = archive
         .by_index(idx)
         .with_context(|| format!("reading archive entry {idx} in {}", exe_path.display()))?;
      ensure!(
         !entry.is_symlink(),
         "unexpected symlink {:?} inside bundled client archive",
         entry.name()
      );
      let Some(rel) = entry.enclosed_name() else {
         bail!(
            "unsafe path {:?} inside bundled client archive",
            entry.name()
         );
      };
      if rel.as_os_str().is_empty() || rel == Path::new(CLIENT_STAMP_ENTRY) || entry.is_dir() {
         continue;
      }
      let rel_posix = rel_path_posix(&rel)?;
      if rel_posix == CLIENT_EXE_NAME {
         has_discord = true;
      }
      let (size, digest) = stream_hash_and_copy(&mut entry, None)
         .with_context(|| format!("verifying {rel_posix} in {}", exe_path.display()))?;
      manifest.push((rel_posix, size, digest));
   }

   ensure!(
      has_discord,
      "bundled client archive in {} does not contain {CLIENT_EXE_NAME}",
      exe_path.display()
   );
   let computed = compute_manifest_stamp(&mut manifest)?;
   ensure!(
      computed == want_stamp,
      "bundled client manifest mismatch in {}: expected {want_stamp}, got {computed}",
      exe_path.display()
   );
   Ok(())
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "extracted on Windows and verified in tests")
)]
fn extract_archive_to_staging<R: std::io::Read + std::io::Seek>(
   archive: &mut zip::ZipArchive<R>,
   exe_path: &Path,
   staging: &Path,
   want_stamp: &str,
) -> misstep::Result<()> {
   let mut manifest: Vec<(String, u64, String)> = Vec::new();
   for idx in 0..archive.len() {
      let mut entry = archive
         .by_index(idx)
         .with_context(|| format!("reading archive entry {idx} in {}", exe_path.display()))?;
      ensure!(
         !entry.is_symlink(),
         "unexpected symlink {:?} inside bundled client archive",
         entry.name()
      );
      let Some(rel) = entry.enclosed_name() else {
         bail!(
            "unsafe path {:?} inside bundled client archive",
            entry.name()
         );
      };
      if rel.as_os_str().is_empty() || rel == Path::new(CLIENT_STAMP_ENTRY) {
         continue;
      }
      let dest = staging.join(&rel);
      if entry.is_dir() {
         std::fs::create_dir_all(&dest)
            .with_context(|| format!("creating {}", dest.display()))?;
         continue;
      }
      let rel_posix = rel_path_posix(&rel)?;
      if let Some(parent) = dest.parent() {
         std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
      }
      let mut out = std::fs::File::create(&dest)
         .with_context(|| format!("creating {}", dest.display()))?;
      let (size, digest) = stream_hash_and_copy(&mut entry, Some(&mut out))
         .with_context(|| format!("extracting {rel_posix}"))?;
      manifest.push((rel_posix, size, digest));
   }

   ensure!(
      staging.join(CLIENT_EXE_NAME).is_file(),
      "bundled client archive in {} does not contain {CLIENT_EXE_NAME}",
      exe_path.display()
   );
   let computed = compute_manifest_stamp(&mut manifest)?;
   ensure!(
      computed == want_stamp,
      "bundled client manifest mismatch in {}: expected {want_stamp}, got {computed}",
      exe_path.display()
   );
   std::fs::write(staging.join(CLIENT_STAMP_FILE), format!("{want_stamp}\n"))
      .with_context(|| format!("writing {}", staging.join(CLIENT_STAMP_FILE).display()))?;
   Ok(())
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "archive metadata check runs on Windows and in tests")
)]
fn validate_archive_entries<R: std::io::Read + std::io::Seek>(
   archive: &mut zip::ZipArchive<R>,
   exe_path: &Path,
) -> misstep::Result<()> {
   let mut has_discord = false;
   for idx in 0..archive.len() {
      let entry = archive
         .by_index_raw(idx)
         .with_context(|| format!("reading archive entry {idx} in {}", exe_path.display()))?;
      ensure!(
         !entry.is_symlink(),
         "unexpected symlink {:?} inside bundled client archive",
         entry.name()
      );
      let Some(rel) = entry.enclosed_name() else {
         bail!(
            "unsafe path {:?} inside bundled client archive",
            entry.name()
         );
      };
      if !entry.is_dir() && rel == Path::new(CLIENT_EXE_NAME) {
         has_discord = true;
      }
   }
   ensure!(
      has_discord,
      "bundled client archive in {} does not contain {CLIENT_EXE_NAME}",
      exe_path.display()
   );
   Ok(())
}

/// Validates a PE-overlay client archive without modifying `<home>` or
/// decompressing the client files.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "bundled client validator runs on Windows and in tests")
)]
fn validate_bundled_client_with(
   exe_path: &Path,
   expect_bundle: bool,
) -> misstep::Result<Option<String>> {
   let mut file =
      std::fs::File::open(exe_path).with_context(|| format!("opening {}", exe_path.display()))?;
   if !detect_pe_overlay(&mut file)? {
      ensure!(
         !expect_bundle,
         "expected bundled Discord Phoenix client payload is missing from {}",
         exe_path.display()
      );
      return Ok(None);
   }

   let reader = std::io::BufReader::with_capacity(64 * 1024, file);
   let mut archive = zip::ZipArchive::new(reader)
      .with_context(|| format!("reading bundled client archive in {}", exe_path.display()))?;
   let stamp = read_archive_stamp(&mut archive, exe_path)?;
   validate_archive_entries(&mut archive, exe_path)?;
   Ok(Some(stamp))
}

#[cfg_attr(
   not(windows),
   allow(dead_code, reason = "entry point wrapper used on Windows")
)]
fn validate_bundled_client(exe_path: &Path) -> misstep::Result<Option<String>> {
   validate_bundled_client_with(
      exe_path,
      option_env!("NYAACORD_EXPECT_CLIENT") == Some("1"),
   )
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "client sync options used on Windows and in tests")
)]
#[derive(Clone, Copy)]
struct ClientSyncOptions {
   expect_bundle: bool,
   extract: bool,
   verify_payload_if_skipped: bool,
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "tracked file replacement runs on Windows and in tests")
)]
struct ReplacedFile {
   dest: PathBuf,
   backup: Option<PathBuf>,
}

/// Holds `UpdateLock` and rolls back all replaced files and any swapped or fresh
/// `app` directory if dropped before `commit()`.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "client sync transaction runs on Windows and in tests")
)]
struct ClientTransaction {
   _lock: phoenix_updater::pe::UpdateLock,
   home: PathBuf,
   app_dir: PathBuf,
   swapped_backup: Option<PathBuf>,
   created_fresh: bool,
   replaced_files: Vec<ReplacedFile>,
   committed: bool,
   discord_exe: Option<PathBuf>,
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "client sync transaction runs on Windows and in tests")
)]
impl ClientTransaction {
   fn unchanged(
      lock: phoenix_updater::pe::UpdateLock,
      home: &Path,
      app_dir: PathBuf,
      discord_exe: Option<PathBuf>,
   ) -> Self {
      Self {
         _lock: lock,
         home: home.to_path_buf(),
         app_dir,
         swapped_backup: None,
         created_fresh: false,
         replaced_files: Vec::new(),
         committed: false,
         discord_exe,
      }
   }

   fn replace_file_with(
      &mut self,
      dest: &Path,
      write_tmp: impl FnOnce(&mut std::fs::File) -> misstep::Result<()>,
   ) -> misstep::Result<()> {
      let dir = dest
         .parent()
         .context("destination has no parent directory")?;
      let name = dest
         .file_name()
         .and_then(OsStr::to_str)
         .context("destination has no valid file name")?;
      std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

      let mut tmp = StagingFileGuard::new(unique_sibling(dir, name, "new"));
      {
         let mut file = std::fs::File::create(tmp.path())
            .with_context(|| format!("creating {}", tmp.path().display()))?;
         write_tmp(&mut file)?;
         file
            .sync_all()
            .with_context(|| format!("syncing {}", tmp.path().display()))?;
      }

      let backup = if dest.exists() {
         let old = unique_sibling(dir, name, "old");
         std::fs::rename(dest, &old)
            .with_context(|| format!("moving existing {}", dest.display()))?;
         if let Err(err) = std::fs::rename(tmp.path(), dest) {
            let _ = std::fs::rename(&old, dest);
            return Err(err).with_context(|| format!("replacing {}", dest.display()))?;
         }
         Some(old)
      } else {
         std::fs::rename(tmp.path(), dest)
            .with_context(|| format!("installing {}", dest.display()))?;
         None
      };
      tmp.disarm();
      self.replaced_files.push(ReplacedFile {
         dest: dest.to_path_buf(),
         backup,
      });
      Ok(())
   }

   fn write_file(&mut self, dest: &Path, bytes: &[u8]) -> misstep::Result<()> {
      use std::io::Write as _;
      self.replace_file_with(dest, |file| {
         file
            .write_all(bytes)
            .with_context(|| format!("writing {}", dest.display()))?;
         Ok(())
      })
   }

   fn copy_file(&mut self, src: &Path, dest: &Path) -> misstep::Result<()> {
      self.replace_file_with(dest, |file| {
         let mut input =
            std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
         stream_copy(&mut input, file)
            .with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
         Ok(())
      })
   }

   fn commit(mut self) -> Option<PathBuf> {
      self.committed = true;
      for replaced in self.replaced_files.drain(..) {
         if let Some(backup) = replaced.backup {
            let _ = std::fs::remove_file(backup);
         }
         if let (Some(dir), Some(name)) = (
            replaced.dest.parent(),
            replaced.dest.file_name().and_then(OsStr::to_str),
         ) {
            cleanup_old_files(dir, name);
         }
      }
      for name in ["NyaaCord.exe", "phoenix.dll", "libplm.plugins.so", "version.txt"] {
         cleanup_old_files(&self.home, name);
      }
      if let Some(backup) = self.swapped_backup.take() {
         let _ = std::fs::remove_dir_all(backup);
      }
      if self.app_dir.join(CLIENT_EXE_NAME).is_file() {
         cleanup_old_dirs(&self.home, CLIENT_DIR_NAME);
      }
      self.discord_exe.take()
   }
}

impl Drop for ClientTransaction {
   fn drop(&mut self) {
      if self.committed {
         return;
      }
      for replaced in self.replaced_files.iter().rev() {
         if let Some(ref backup) = replaced.backup {
            let failed = if replaced.dest.exists() {
               let dir = replaced.dest.parent().unwrap_or(&self.home);
               let name = replaced
                  .dest
                  .file_name()
                  .and_then(OsStr::to_str)
                  .unwrap_or("file");
               let moved = unique_sibling(dir, name, "failed");
               std::fs::rename(&replaced.dest, &moved)
                  .ok()
                  .map(|()| moved)
            } else {
               None
            };
            if std::fs::rename(backup, &replaced.dest).is_ok()
               && let Some(failed_file) = failed
            {
               let _ = std::fs::remove_file(failed_file);
            }
         } else {
            let _ = std::fs::remove_file(&replaced.dest);
         }
      }
      if let Some(ref backup) = self.swapped_backup {
         let failed = if self.app_dir.exists() {
            let moved = unique_sibling(&self.home, CLIENT_DIR_NAME, "failed");
            std::fs::rename(&self.app_dir, &moved).ok().map(|()| moved)
         } else {
            None
         };
         if std::fs::rename(backup, &self.app_dir).is_ok()
            && let Some(failed_dir) = failed
         {
            let _ = std::fs::remove_dir_all(failed_dir);
         }
      } else if self.created_fresh {
         let _ = std::fs::remove_dir_all(&self.app_dir);
      }
   }
}

/// Starts a client-sync transaction using an already-acquired `UpdateLock`,
/// preserving replaced file and `app` backups until `ClientTransaction::commit()`.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "extracted on Windows and verified in tests")
)]
fn begin_client_sync_locked(
   lock: phoenix_updater::pe::UpdateLock,
   exe_path: &Path,
   home: &Path,
   opts: ClientSyncOptions,
) -> misstep::Result<ClientTransaction> {
   let mut file =
      std::fs::File::open(exe_path).with_context(|| format!("opening {}", exe_path.display()))?;
   let has_overlay = detect_pe_overlay(&mut file)?;
   if !has_overlay {
      ensure!(
         !opts.expect_bundle,
         "expected bundled Discord Phoenix client payload is missing from {}",
         exe_path.display()
      );
   }

   let mut archive_and_stamp = if has_overlay {
      let reader = std::io::BufReader::with_capacity(64 * 1024, file);
      let mut archive = zip::ZipArchive::new(reader)
         .with_context(|| format!("reading bundled client archive in {}", exe_path.display()))?;
      let want_stamp = read_archive_stamp(&mut archive, exe_path)?;
      validate_archive_entries(&mut archive, exe_path)?;
      Some((archive, want_stamp))
   } else {
      None
   };

   let app_dir = home.join(CLIENT_DIR_NAME);
   let discord_exe = app_dir.join(CLIENT_EXE_NAME);
   recover_client_backup(home, &app_dir);

   let Some((ref mut archive, ref want_stamp)) = archive_and_stamp else {
      return Ok(ClientTransaction::unchanged(lock, home, app_dir, None));
   };

   if !opts.extract {
      if opts.verify_payload_if_skipped {
         verify_archive_payload(archive, exe_path, want_stamp)?;
      }
      return Ok(ClientTransaction::unchanged(lock, home, app_dir, None));
   }

   let stamp_file = app_dir.join(CLIENT_STAMP_FILE);
   if discord_exe.is_file()
      && read_installed_stamp(&stamp_file).is_some_and(|have| have == *want_stamp)
   {
      if opts.verify_payload_if_skipped {
         verify_archive_payload(archive, exe_path, want_stamp)?;
      }
      return Ok(ClientTransaction::unchanged(
         lock,
         home,
         app_dir,
         Some(discord_exe),
      ));
   }

   let staging = StagingDirGuard::new(unique_sibling(home, CLIENT_DIR_NAME, "staging"));
   std::fs::create_dir(staging.path())
      .with_context(|| format!("creating {}", staging.path().display()))?;
   extract_archive_to_staging(archive, exe_path, staging.path(), want_stamp)?;
   let (swapped_backup, created_fresh) = swap_client_dir_keep_backup(home, staging, &app_dir)?;
   Ok(ClientTransaction {
      _lock: lock,
      home: home.to_path_buf(),
      app_dir,
      swapped_backup,
      created_fresh,
      replaced_files: Vec::new(),
      committed: false,
      discord_exe: Some(discord_exe),
   })
}

/// Validates `exe_path`, acquires `UpdateLock`, and begins a client-sync transaction.
#[cfg_attr(
   not(test),
   allow(dead_code, reason = "convenience wrapper used in tests")
)]
fn begin_client_sync(
   exe_path: &Path,
   home: &Path,
   opts: ClientSyncOptions,
) -> misstep::Result<ClientTransaction> {
   validate_bundled_client_with(exe_path, opts.expect_bundle)?;
   std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
   let lock = acquire_install_lock(home)?;
   begin_client_sync_locked(lock, exe_path, home, opts)
}

#[cfg_attr(
   not(windows),
   allow(dead_code, reason = "entry point wrapper used on Windows")
)]
fn begin_bundled_client_sync_locked(
   lock: phoenix_updater::pe::UpdateLock,
   exe_path: &Path,
   home: &Path,
   extract: bool,
   verify_payload_if_skipped: bool,
) -> misstep::Result<ClientTransaction> {
   begin_client_sync_locked(
      lock,
      exe_path,
      home,
      ClientSyncOptions {
         expect_bundle: option_env!("NYAACORD_EXPECT_CLIENT") == Some("1"),
         extract,
         verify_payload_if_skipped,
      },
   )
}

#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "used on Windows and verified in tests")
)]
fn resolve_discord_exe_with(
   override_path: Option<PathBuf>,
   env_path: Option<OsString>,
   bundled_exe: Option<PathBuf>,
   local: &Path,
) -> PathBuf {
   if let Some(path) = override_path {
      return path;
   }
   if let Some(path) = env_path.filter(|val| !val.is_empty()) {
      return PathBuf::from(path);
   }
   if let Some(path) = bundled_exe.filter(|path| path.is_file()) {
      return path;
   }
   let installed = bundled_discord_exe_in(local);
   if installed.is_file() {
      return installed;
   }
   legacy_discord_exe_in(local)
}

#[cfg(windows)]
mod windows {
   use std::{
      env,
      ffi::{OsStr, OsString, c_void},
      fs,
      io::{self, Write},
      os::windows::{ffi::OsStrExt, process::CommandExt},
      path::{Path, PathBuf},
      process::Command,
      ptr,
      time::{Duration, Instant},
   };

   use misstep::{OptionExt, ResultExt, bail};
   use phoenix_updater::{
      CheckResult, Config, ReleaseInfo, check_latest, fetch_latest,
      pe::{
         InstallUpdateDecision, UpdateLock, apply_update_locked, check_installed_for_release,
         query_exe_version, rollback_exe_locked,
      },
      sha256, version,
   };
   use serde::Serialize;

   use super::{
      ClientTransaction, InstalledAction, LockedUpdateAction, ManualUpdatePrecheck, Mode, USAGE,
      VERSION, acquire_install_lock, begin_bundled_client_sync_locked, build_windows_cmdline,
      decide_installed_action, decide_locked_manual_update, discord_match_paths,
      embedded::{EMBEDDED_DLL, EMBEDDED_PLUGINS},
      install_home_in, installed_version_unknown_error, plan_manual_update,
      resolve_discord_exe_with, run_installed_action, validate_bundled_client,
   };

   type Handle = *mut c_void;

   #[repr(C)]
   struct ProcessInformation {
      process: Handle,
      thread: Handle,
      process_id: u32,
      thread_id: u32,
   }

   const CREATE_SUSPENDED: u32 = 0x4;
   const MEM_COMMIT_RESERVE: u32 = 0x3000;
   const PAGE_READWRITE: u32 = 0x4;
   const NO_WINDOW: u32 = 0x0800_0000;
   const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
   const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
   const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
   const FILE_TYPE_UNKNOWN: u32 = 0;
   const SNAPSHOT_PROCESSES: u32 = 0x2;
   const SNAPSHOT_MODULES: u32 = 0x8 | 0x10;
   const PROCESS_ACCESS: u32 = 0x0010_1001;

   #[link(name = "kernel32")]
   unsafe extern "system" {
      fn AttachConsole(process_id: u32) -> i32;
      fn GetStdHandle(which: u32) -> Handle;
      fn GetFileType(handle: Handle) -> u32;
      fn CreateProcessW(
         application: *const u16,
         command_line: *mut u16,
         process_attributes: *const c_void,
         thread_attributes: *const c_void,
         inherit_handles: i32,
         flags: u32,
         environment: *const c_void,
         directory: *const u16,
         startup: *const c_void,
         info: *mut ProcessInformation,
      ) -> i32;
      fn VirtualAllocEx(
         process: Handle,
         address: *const c_void,
         size: usize,
         kind: u32,
         protect: u32,
      ) -> *mut c_void;
      fn WriteProcessMemory(
         process: Handle,
         address: *mut c_void,
         buffer: *const c_void,
         size: usize,
         written: *mut usize,
      ) -> i32;
      fn GetModuleHandleW(name: *const u16) -> Handle;
      fn GetProcAddress(module: Handle, name: *const u8) -> *mut c_void;
      fn QueueUserAPC(function: *mut c_void, thread: Handle, data: usize) -> u32;
      fn ResumeThread(thread: Handle) -> u32;
      fn TerminateProcess(process: Handle, code: u32) -> i32;
      fn CloseHandle(handle: Handle) -> i32;
      fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
      fn Process32FirstW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn Process32NextW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn Module32FirstW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn Module32NextW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
      fn QueryFullProcessImageNameW(
         process: Handle,
         flags: u32,
         name: *mut u16,
         size: *mut u32,
      ) -> i32;
      fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
   }

   #[link(name = "user32")]
   unsafe extern "system" {
      fn MessageBoxW(window: Handle, text: *const u16, caption: *const u16, kind: u32) -> i32;
   }

   fn std_handle_ready(which: u32) -> bool {
      // SAFETY: `GetStdHandle` and `GetFileType` accept standard handle IDs.
      unsafe {
         let handle = GetStdHandle(which);
         if handle.is_null() || handle as isize == -1 {
            return false;
         }
         GetFileType(handle) != FILE_TYPE_UNKNOWN
      }
   }

   /// Attaches to a parent shell console when launched from cmd or PowerShell while
   /// preserving redirected stdout or stderr pipes.
   pub fn init_console() -> bool {
      let redirected = std_handle_ready(STD_OUTPUT_HANDLE) || std_handle_ready(STD_ERROR_HANDLE);
      // SAFETY: attaching to the parent console has no memory preconditions.
      let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
      redirected || attached
   }

   pub struct Ui {
      pub quiet: bool,
      has_console: bool,
   }

   impl Ui {
      pub const fn new(quiet: bool, has_console: bool) -> Self {
         Self { quiet, has_console }
      }

      pub fn info(&self, text: &str) {
         if !self.quiet {
            let _ = writeln!(io::stdout(), "{text}");
         }
      }

      pub fn error(&self, text: &str) {
         let _ = writeln!(io::stderr(), "{text}");
         if !self.quiet && !self.has_console {
            show_message_box(text, 0x10);
         }
      }

      pub fn notice(&self, text: &str) {
         if !self.quiet {
            let _ = writeln!(io::stdout(), "{text}");
         }
         if !self.quiet && !self.has_console {
            show_message_box(text, 0x40);
         }
      }

      fn ask(&self, text: &str) -> bool {
         if self.quiet {
            return false;
         }
         // `MB_YESNO | MB_ICONQUESTION`, returning `IDYES` (6).
         show_message_box(text, 0x24) == 6
      }
   }

   fn show_message_box(text: &str, kind: u32) -> i32 {
      // SAFETY: both strings are NUL terminated and outlive `MessageBoxW`.
      unsafe {
         MessageBoxW(
            ptr::null_mut(),
            wide(text).as_ptr(),
            wide("NyaaCord").as_ptr(),
            kind,
         )
      }
   }

   /// A program in System32, by full path: `Command` looks beside this
   /// launcher first, which may be the Downloads folder on the first run.
   fn system32(program: &str) -> PathBuf {
      PathBuf::from(env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
         .join("System32")
         .join(program)
   }

   fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
      text.as_ref().encode_wide().chain([0]).collect()
   }

   fn check(ok: bool) -> io::Result<()> {
      if ok {
         Ok(())
      } else {
         Err(io::Error::last_os_error())
      }
   }

   const fn as_bytes(words: &[u64]) -> &[u8] {
      // SAFETY: any `u64`s are valid bytes and the length covers them.
      unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), size_of_val(words)) }
   }

   fn entry_text(entry: &[u8], at: usize) -> OsString {
      use std::os::windows::ffi::OsStringExt;
      let units: Vec<u16> = entry[at..]
         .as_chunks::<2>()
         .0
         .iter()
         .map(|&pair| u16::from_le_bytes(pair))
         .take_while(|&unit| unit != 0)
         .collect();
      OsString::from_wide(&units)
   }

   fn norm_win_path(path: &Path) -> String {
      let text = path.as_os_str().to_string_lossy().replace('/', "\\");
      text.strip_prefix(r"\\?\").unwrap_or(&text).to_lowercase()
   }

   fn running(exes: &[PathBuf]) -> Vec<(Handle, u32, bool)> {
      if exes.is_empty() {
         return Vec::new();
      }
      let exe_names: Vec<&OsStr> = exes.iter().filter_map(|p| p.file_name()).collect();
      let wants: Vec<String> = exes.iter().map(|p| norm_win_path(p)).collect();
      // SAFETY: process snapshot of the whole system.
      let snapshot = unsafe { CreateToolhelp32Snapshot(SNAPSHOT_PROCESSES, 0) };
      if snapshot as isize == -1 {
         return Vec::new();
      }
      // `PROCESSENTRY32W`: size first, pid at 8, file name at 44.
      let mut entry = [0_u64; 71];
      entry[0] = 568;
      let mut found = Vec::new();
      // SAFETY: `entry` is a 568-byte `PROCESSENTRY32W` with its size set.
      let mut more = unsafe { Process32FirstW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      while more {
         let bytes = as_bytes(&entry);
         let pid = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
         let proc_name = entry_text(bytes, 44);
         if exe_names
            .iter()
            .any(|name| proc_name.eq_ignore_ascii_case(name))
         {
            // SAFETY: opens by pid, checked for null below.
            let process = unsafe { OpenProcess(PROCESS_ACCESS, 0, pid) };
            if !process.is_null() {
               let mut path = [0_u16; 1024];
               let mut len = u32::try_from(path.len()).unwrap_or(1024);
               // SAFETY: `path` holds `len` UTF-16 units.
               let named = unsafe {
                  QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut len)
               } != 0;
               let proc_path = String::from_utf16_lossy(&path[..len as usize]).to_lowercase();
               let matched_idx = named
                  .then(|| wants.iter().position(|want| want == &proc_path))
                  .flatten();
               if let Some(idx) = matched_idx {
                  found.push((process, pid, idx == 0));
               } else {
                  // SAFETY: opened above, closed once.
                  unsafe { CloseHandle(process) };
               }
            }
         }
         // SAFETY: as above.
         more = unsafe { Process32NextW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      }
      // SAFETY: the snapshot is closed once.
      unsafe { CloseHandle(snapshot) };
      found
   }

   fn has_module(pid: u32, dll: &OsStr) -> bool {
      // SAFETY: module snapshot of one process.
      let snapshot = unsafe { CreateToolhelp32Snapshot(SNAPSHOT_MODULES, pid) };
      if snapshot as isize == -1 {
         return false;
      }
      // `MODULEENTRY32W`: size first, module name at 48.
      let mut entry = [0_u64; 135];
      entry[0] = 1080;
      let mut found = false;
      // SAFETY: `entry` is a 1080-byte `MODULEENTRY32W` with its size set.
      let mut more = unsafe { Module32FirstW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      while more && !found {
         found = entry_text(as_bytes(&entry), 48).eq_ignore_ascii_case(dll);
         // SAFETY: as above.
         more = unsafe { Module32NextW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      }
      // SAFETY: the snapshot is closed once.
      unsafe { CloseHandle(snapshot) };
      found
   }

   fn local_app_data() -> misstep::Result<PathBuf> {
      Ok(PathBuf::from(
         env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unset")?,
      ))
   }

   fn same_file(a: &Path, b: &Path) -> bool {
      a.canonicalize()
         .ok()
         .zip(b.canonicalize().ok())
         .is_some_and(|(a, b)| a == b)
   }

   fn sha256_slice(data: &[u8]) -> String {
      let mut hasher = sha256::Hasher::new();
      hasher.update(data);
      hasher.finalize_hex()
   }

   /// Syncs the embedded `phoenix.dll`, `libplm.plugins.so`, and `version.txt` inside `dir`.
   fn sync_embedded_assets(tx: &mut ClientTransaction, dir: &Path) -> misstep::Result<PathBuf> {
      fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
      let dll = dir.join("phoenix.dll");
      let plugins = dir.join("libplm.plugins.so");
      let version_file = dir.join("version.txt");

      if !EMBEDDED_DLL.is_empty() {
         let want_sha = sha256_slice(EMBEDDED_DLL);
         let have_match = dll.is_file() && sha256::sha256_file(&dll).is_ok_and(|s| s == want_sha);
         if !have_match {
            tx.write_file(&dll, EMBEDDED_DLL)?;
         }
      }

      if !EMBEDDED_PLUGINS.is_empty() {
         let want_sha = sha256_slice(EMBEDDED_PLUGINS);
         let have_match =
            plugins.is_file() && sha256::sha256_file(&plugins).is_ok_and(|s| s == want_sha);
         if !have_match {
            tx.write_file(&plugins, EMBEDDED_PLUGINS)?;
         }
      }

      let want_version = format!("{VERSION}\n");
      if fs::read_to_string(&version_file).ok().as_deref() != Some(want_version.as_str()) {
         tx.write_file(&version_file, want_version.as_bytes())?;
      }

      if !dll.is_file() {
         bail!(
            "phoenix.dll is missing at {}. Build with scripts/build-windows.sh or pass a DLL path.",
            dll.display()
         );
      }
      Ok(dll)
   }

   fn launcher_differs(me: &Path, launcher: &Path) -> bool {
      if same_file(me, launcher) {
         return false;
      }
      let same_size = fs::metadata(me)
         .ok()
         .zip(fs::metadata(launcher).ok())
         .is_some_and(|(src, dst)| dst.is_file() && src.len() == dst.len());
      let same_contents = same_size
         && sha256::sha256_file(me)
            .ok()
            .zip(sha256::sha256_file(launcher).ok())
            .is_some_and(|(a, b)| a == b);
      !same_contents
   }

   /// Installs `NyaaCord.exe` and embedded assets inside `tx`; shortcuts refresh after commit.
   fn install_self(
      tx: &mut ClientTransaction,
      home: &Path,
      me: &Path,
   ) -> misstep::Result<(PathBuf, PathBuf, bool)> {
      fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
      let launcher = home.join("NyaaCord.exe");
      let copied = !same_file(me, &launcher);
      if copied && launcher_differs(me, &launcher) {
         tx.copy_file(me, &launcher)
            .context("installing NyaaCord.exe")?;
      }

      let dll = sync_embedded_assets(tx, home)?;
      Ok((launcher, dll, copied))
   }

   /// Manages `NyaaCord.lnk` (and removes legacy `Sunbird.lnk`) on Start Menu and Desktop.
   fn shortcuts(launcher: &Path, remove: bool) -> misstep::Result<()> {
      let script = format!(
         "$w = New-Object -ComObject WScript.Shell; \
          foreach ($dir in @([Environment]::GetFolderPath('Programs'), [Environment]::GetFolderPath('Desktop'))) {{ \
             $legacy = Join-Path $dir 'Sunbird.lnk'; \
             if (Test-Path -LiteralPath $legacy) {{ Remove-Item -LiteralPath $legacy -ErrorAction SilentlyContinue }}; \
             $p = Join-Path $dir 'NyaaCord.lnk'; \
             if (${remove}) {{ Remove-Item -LiteralPath $p -ErrorAction SilentlyContinue; continue }}; \
             $s = $w.CreateShortcut($p); \
             $s.TargetPath = $env:NC_TARGET; \
             $s.IconLocation = $env:NC_TARGET + ',0'; \
             $s.Description = 'NyaaCord for Discord Phoenix'; \
             $s.Save() \
          }}"
      );
      let status = Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"))
         .args(["-NoProfile", "-NonInteractive", "-Command", &script])
         .env("NC_TARGET", launcher)
         .creation_flags(NO_WINDOW)
         .status()
         .context("running powershell.exe")?;
      if !status.success() {
         bail!("updating NyaaCord shortcuts failed");
      }
      Ok(())
   }

   fn build_updater_config(
      repo: Option<String>,
      api: Option<String>,
      timeout: Option<i64>,
   ) -> misstep::Result<Config> {
      let mut cfg = Config::from_env()?;
      if let Some(repo) = repo {
         phoenix_updater::github::validate_repo(&repo)?;
         cfg.repo = repo;
      }
      if let Some(api) = api {
         let _ = phoenix_updater::http::validate_url(&api)?;
         cfg.api_url = Some(api);
      }
      if let Some(secs) = timeout {
         cfg.stall_timeout_secs = secs;
         cfg.connect_timeout_ms = secs.saturating_mul(1000);
      }
      Ok(cfg)
   }

   fn default_update_target() -> misstep::Result<PathBuf> {
      if let Ok(local) = local_app_data() {
         let installed = install_home_in(&local).join("NyaaCord.exe");
         if installed.is_file() {
            return Ok(installed);
         }
      }
      Ok(env::current_exe().context("resolving current executable")?)
   }

   fn write_version_beside(target: &Path, ver: &str) {
      let Some(dir) = target.parent() else {
         return;
      };
      let is_installed = local_app_data().is_ok_and(|local| install_home_in(&local) == dir);
      let stamp = dir.join("version.txt");
      if is_installed || stamp.is_file() {
         let _ = fs::write(stamp, format!("{ver}\n"));
      }
   }

   #[derive(Serialize)]
   #[serde(tag = "status")]
   enum CheckJson<'a> {
      #[serde(rename = "up_to_date")]
      UpToDate {
         update_available: bool,
         current: &'a str,
         latest: &'a str,
      },
      #[serde(rename = "available")]
      Available {
         update_available: bool,
         current: String,
         #[serde(flatten)]
         release: &'a ReleaseInfo,
      },
   }

   fn cmd_check_update(
      repo: Option<String>,
      api: Option<String>,
      timeout: Option<i64>,
      json: bool,
      ui: &Ui,
   ) -> misstep::Result<()> {
      let cfg = build_updater_config(repo, api, timeout)?;
      let result = check_latest(VERSION, &cfg)?;
      if json {
         let payload = match &result {
            CheckResult::UpToDate { current, latest } => CheckJson::UpToDate {
               update_available: false,
               current,
               latest,
            },
            CheckResult::Available(release) => CheckJson::Available {
               update_available: true,
               current: version::normalize(VERSION),
               release,
            },
         };
         let _ = writeln!(io::stdout(), "{}", serde_json::to_string(&payload)?);
         return Ok(());
      }

      match result {
         CheckResult::UpToDate { current, latest } => {
            ui.info(&format!(
               "Up to date (current: {current}, latest: {latest})"
            ));
         },
         CheckResult::Available(release) => {
            ui.info(&format!(
               "Update available: {} -> {} ({}, {} bytes)",
               version::normalize(VERSION),
               release.version,
               release.asset_name,
               release.size
            ));
            ui.info(&format!("Release: {}", release.release_url));
         },
      }
      Ok(())
   }

   fn target_parent_dir(target: &Path) -> PathBuf {
      let resolved = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
      match resolved.parent() {
         Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
         _ => PathBuf::from("."),
      }
   }

   fn resolve_manual_update_release(
      target: &Path,
      cfg: &Config,
      force: bool,
      ui: &Ui,
   ) -> misstep::Result<Option<ReleaseInfo>> {
      let me = env::current_exe().ok();
      let target_exists = target.is_file();
      let same_binary = me.as_deref().is_some_and(|exe| same_file(exe, target));
      let target_ver = (!force && target_exists && !same_binary)
         .then(|| query_exe_version(target))
         .flatten();

      match plan_manual_update(VERSION, target_exists, same_binary, target_ver.as_deref(), force) {
         ManualUpdatePrecheck::UnknownTarget => bail!(
            "could not verify version of installed NyaaCord at {}; pass --force to replace",
            target.display()
         ),
         ManualUpdatePrecheck::ForceFetch => Ok(Some(fetch_latest(cfg)?)),
         ManualUpdatePrecheck::CheckVersion(effective_ver) => match check_latest(&effective_ver, cfg)?
         {
            CheckResult::UpToDate { current, latest } => {
               ui.info(&format!(
                  "Up to date (current: {current}, latest: {latest})"
               ));
               Ok(None)
            },
            CheckResult::Available(info) => Ok(Some(info)),
         },
      }
   }

   fn cmd_update(
      target: Option<PathBuf>,
      repo: Option<String>,
      api: Option<String>,
      timeout: Option<i64>,
      force: bool,
      ui: &Ui,
   ) -> misstep::Result<()> {
      let target = match target {
         Some(path) => path,
         None => default_update_target()?,
      };
      let cfg = build_updater_config(repo, api, timeout)?;
      let Some(release) = resolve_manual_update_release(&target, &cfg, force, ui)? else {
         return Ok(());
      };

      let parent = target_parent_dir(&target);
      fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
      let lock = acquire_install_lock(&parent)?;

      let live_exists = target.is_file();
      let live_ver = (!force && live_exists)
         .then(|| query_exe_version(&target))
         .flatten();
      match decide_locked_manual_update(live_exists, live_ver.as_deref(), &release.version, force) {
         LockedUpdateAction::UpToDate(current) => {
            ui.info(&format!(
               "Up to date (current: {current}, latest: {})",
               release.version
            ));
            return Ok(());
         },
         LockedUpdateAction::UnknownTarget => bail!(
            "could not verify version of installed NyaaCord at {}; pass --force to replace",
            target.display()
         ),
         LockedUpdateAction::Apply => {},
      }

      if !ui.quiet {
         let _ = writeln!(
            io::stderr(),
            "Downloading {} ({})...",
            release.asset_name,
            release.version
         );
      }

      let mut last_report = Instant::now();
      let mut last_pct: Option<u64> = None;
      let mut printed = false;
      let fallback_total = (release.size > 0).then_some(release.size);

      let outcome = apply_update_locked(&lock, &release, &target, &cfg, &mut |done, total| {
         if ui.quiet {
            return;
         }
         let known = total.filter(|&b| b > 0).or(fallback_total);
         if let Some(total) = known {
            let pct = done
               .saturating_mul(100)
               .checked_div(total)
               .unwrap_or(0)
               .min(100);
            if pct == 100
               || last_pct.is_none_or(|p| pct >= p.saturating_add(5))
               || last_report.elapsed() >= Duration::from_millis(250)
            {
               let _ = write!(io::stderr(), "\rDownloading: {pct}% ({done}/{total} bytes)");
               let _ = io::stderr().flush();
               last_pct = Some(pct);
               printed = true;
               last_report = Instant::now();
            }
         }
      })?;

      if !ui.quiet && printed {
         let _ = writeln!(io::stderr());
      }
      write_version_beside(&outcome.target, &outcome.version);
      ui.info(&format!(
         "Updated {} to {}",
         outcome.target.display(),
         outcome.version
      ));
      if let Some(prev) = outcome.previous {
         ui.info(&format!("Backup saved at {}", prev.display()));
      }
      Ok(())
   }

   fn cmd_rollback(target: Option<PathBuf>, ui: &Ui) -> misstep::Result<()> {
      let target = match target {
         Some(path) => path,
         None => default_update_target()?,
      };
      let parent = target_parent_dir(&target);
      let lock = acquire_install_lock(&parent)?;
      let prev = rollback_exe_locked(&lock, &target)?;
      if let Some(restored_ver) = query_exe_version(&target) {
         write_version_beside(&target, &restored_ver);
      }
      ui.info(&format!(
         "Rolled back {} from {}",
         target.display(),
         prev.display()
      ));
      Ok(())
   }

   fn cmd_uninstall(ui: &Ui) -> misstep::Result<()> {
      let local = local_app_data()?;
      let home = install_home_in(&local);
      let _ = shortcuts(&home.join("NyaaCord.exe"), true);
      match fs::remove_dir_all(&home) {
         Err(err) if err.kind() != io::ErrorKind::NotFound => {
            ui.notice(&format!(
               "Shortcuts removed, but {} could not be fully deleted ({err}). Close Discord and delete it.",
               home.display()
            ));
         },
         _ => ui.notice("NyaaCord has been uninstalled. Your Discord data and settings were kept."),
      }
      Ok(())
   }

   fn spawn_launcher_handoff(
      launcher: &Path,
      target_exe: Option<&Path>,
      no_runtime: bool,
      discord_args: &[OsString],
      quiet: bool,
      disable_auto_update: bool,
   ) -> misstep::Result<()> {
      let mut cmd = Command::new(launcher);
      if disable_auto_update {
         cmd.env("NYAACORD_AUTO_UPDATE", "0");
      }
      if quiet {
         cmd.arg("--quiet");
      }
      if no_runtime {
         cmd.arg("--no-runtime");
      }
      if let Some(exe) = target_exe {
         cmd.arg("--target-exe").arg(exe);
      }
      if !discord_args.is_empty() {
         cmd.arg("--").args(discord_args);
      }
      cmd.spawn()
         .with_context(|| format!("launching installed NyaaCord at {}", launcher.display()))?;
      Ok(())
   }

   /// Checks for and applies an update before normal launch under `UpdateLock`,
   /// rolling back if the new binary fails its version smoke check or fails to spawn.
   fn maybe_auto_update(
      home: &Path,
      check_ver: &str,
      launcher: &Path,
      target_exe: Option<&Path>,
      no_runtime: bool,
      discord_args: &[OsString],
      quiet: bool,
   ) -> bool {
      if env::var("NYAACORD_AUTO_UPDATE").is_ok_and(|val| val.trim() == "0") {
         return false;
      }
      let Ok(mut cfg) = Config::from_env() else {
         return false;
      };
      cfg.connect_timeout_ms = 5_000;
      cfg.stall_timeout_secs = 15;

      let Ok(CheckResult::Available(release)) = check_latest(check_ver, &cfg) else {
         return false;
      };
      if fs::create_dir_all(home).is_err() {
         return false;
      }
      let Ok(lock) = UpdateLock::acquire(home) else {
         return false;
      };

      let live_ver = launcher
         .is_file()
         .then(|| query_exe_version(launcher))
         .flatten();
      match check_installed_for_release(live_ver.as_deref(), &release.version) {
         InstallUpdateDecision::AlreadyCurrent => {
            drop(lock);
            return spawn_launcher_handoff(
               launcher,
               target_exe,
               no_runtime,
               discord_args,
               quiet,
               true,
            )
            .is_ok();
         },
         InstallUpdateDecision::UnknownInstalled => {
            drop(lock);
            return false;
         },
         InstallUpdateDecision::Apply => {},
      }

      let Ok(outcome) = apply_update_locked(&lock, &release, launcher, &cfg, &mut |_, _| {}) else {
         return false;
      };

      // Smoke-test the new executable under the lock before handing off.
      let Some(new_ver) = query_exe_version(&outcome.target) else {
         let _ = rollback_exe_locked(&lock, &outcome.target);
         return false;
      };
      write_version_beside(&outcome.target, &new_ver);
      drop(lock);

      if spawn_launcher_handoff(
         &outcome.target,
         target_exe,
         no_runtime,
         discord_args,
         quiet,
         true,
      )
      .is_ok()
      {
         true
      } else {
         if let Ok(rollback_lock) = UpdateLock::acquire(home)
            && query_exe_version(&outcome.target).as_deref() == Some(new_ver.as_str())
            && rollback_exe_locked(&rollback_lock, &outcome.target).is_ok()
            && let Some(restored_ver) = query_exe_version(&outcome.target)
         {
            write_version_beside(&outcome.target, &restored_ver);
         }
         false
      }
   }

   /// Copies `dll` path into the suspended child and queues `LoadLibraryW` on its main thread.
   fn queue_load(info: &ProcessInformation, dll: &[u16]) -> io::Result<()> {
      let size = size_of_val(dll);
      // SAFETY: `info.process` is a live process handle with full access.
      let remote = unsafe {
         VirtualAllocEx(
            info.process,
            ptr::null(),
            size,
            MEM_COMMIT_RESERVE,
            PAGE_READWRITE,
         )
      };
      check(!remote.is_null())?;
      // SAFETY: `remote` holds `size` bytes and `dll` is `size` bytes long.
      check(unsafe {
         WriteProcessMemory(
            info.process,
            remote,
            dll.as_ptr().cast(),
            size,
            ptr::null_mut(),
         ) != 0
      })?;
      // SAFETY: kernel32 is always loaded and names are NUL terminated.
      let load = unsafe {
         GetProcAddress(
            GetModuleHandleW(wide("kernel32.dll").as_ptr()),
            c"LoadLibraryW".as_ptr().cast(),
         )
      };
      check(!load.is_null())?;
      // SAFETY: LoadLibraryW takes one pointer argument, as an APC does.
      check(unsafe { QueueUserAPC(load, info.thread, remote as usize) } != 0)
   }

   struct LaunchTarget<'a> {
      exe: &'a Path,
      app: &'a Path,
      dll: &'a Path,
   }

   fn handle_running_discord(
      target: &LaunchTarget<'_>,
      local: &Path,
      explicit_target: bool,
      no_runtime: bool,
      discord_args: &[OsString],
      ui: &Ui,
   ) -> misstep::Result<bool> {
      let match_paths = discord_match_paths(target.exe, local, explicit_target);
      let open = running(&match_paths);
      if open.is_empty() {
         return Ok(true);
      }

      let dll_name = target.dll.file_name().unwrap_or_default();
      let all_target = open.iter().all(|&(_, _, is_target)| is_target);
      let with_nyaacord = all_target && open.iter().any(|&(_, pid, _)| has_module(pid, dll_name));
      let restart = if no_runtime {
         ui.ask(
            "Discord is already running.\n\nRestart it without the NyaaCord plugin runtime? \
             Any active call will end.",
         )
      } else if !all_target {
         ui.ask(
            "Discord is already running from another installation.\n\nRestart it with NyaaCord? \
             Any active call will end.",
         )
      } else {
         !with_nyaacord
            && ui.ask(
               "Discord is already running without NyaaCord.\n\nRestart it with NyaaCord? \
                Any active call will end.",
            )
      };

      for &(process, _, _) in &open {
         if restart {
            // SAFETY: opened with TERMINATE by `running`.
            unsafe { TerminateProcess(process, 0) };
         }
      }
      for (process, _, _) in open {
         if restart {
            // SAFETY: opened with SYNCHRONIZE by `running`.
            unsafe { WaitForSingleObject(process, 5000) };
         }
         // SAFETY: opened by `running`, closed once.
         unsafe { CloseHandle(process) };
      }

      if !restart {
         if all_target && with_nyaacord {
            Command::new(target.exe)
               .args(discord_args)
               .current_dir(target.app)
               .spawn()?;
         }
         return Ok(false);
      }
      Ok(true)
   }

   fn spawn_injected_discord(
      exe: &Path,
      app: &Path,
      dll: &Path,
      discord_args: &[OsString],
   ) -> io::Result<u32> {
      let mut command = wide(build_windows_cmdline(exe, discord_args));
      // STARTUPINFOW is 104 bytes on x64 and only `cb` must be set.
      let mut startup = [0_u64; 13];
      startup[0] = 104;
      let mut info = ProcessInformation {
         process: ptr::null_mut(),
         thread: ptr::null_mut(),
         process_id: 0,
         thread_id: 0,
      };
      // SAFETY: all pointers are valid for the call and strings are NUL terminated.
      check(unsafe {
         CreateProcessW(
            wide(exe).as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_SUSPENDED,
            ptr::null(),
            wide(app).as_ptr(),
            startup.as_ptr().cast(),
            &raw mut info,
         ) != 0
      })?;

      let queued = queue_load(&info, &wide(dll));
      if queued.is_err() {
         // SAFETY: `info.process` is the suspended child created above.
         unsafe { TerminateProcess(info.process, 1) };
      } else {
         // SAFETY: `info.thread` is the child's suspended main thread.
         unsafe { ResumeThread(info.thread) };
      }
      // SAFETY: both handles came from CreateProcessW and are closed once.
      unsafe {
         CloseHandle(info.thread);
         CloseHandle(info.process);
      }
      queued?;
      Ok(info.process_id)
   }

   fn check_existing_launcher(
      home: &Path,
      me: &Path,
      existing_launcher: &Path,
      target_exe: Option<&Path>,
      no_runtime: bool,
      discord_args: &[OsString],
      quiet: bool,
   ) -> misstep::Result<bool> {
      let same_binary = same_file(me, existing_launcher);
      let installed_ver = (!same_binary)
         .then(|| query_exe_version(existing_launcher))
         .flatten();
      run_installed_action(
         VERSION,
         installed_ver.as_deref(),
         same_binary,
         existing_launcher,
         || {
            spawn_launcher_handoff(
               existing_launcher,
               target_exe,
               no_runtime,
               discord_args,
               quiet,
               false,
            )
         },
         |check_ver| {
            maybe_auto_update(
               home,
               check_ver,
               existing_launcher,
               target_exe,
               no_runtime,
               discord_args,
               quiet,
            )
         },
      )
   }

   /// Rechecks `existing_launcher` under `lock`, failing closed if its version is unreadable
   /// and releasing `lock` before handing off to a newer install.
   fn hand_off_if_newer_locked(
      lock: UpdateLock,
      me: &Path,
      existing_launcher: &Path,
      target_exe: Option<&Path>,
      no_runtime: bool,
      discord_args: &[OsString],
      quiet: bool,
   ) -> misstep::Result<Option<UpdateLock>> {
      if existing_launcher.is_file() && !same_file(me, existing_launcher) {
         let installed_ver = query_exe_version(existing_launcher);
         match decide_installed_action(VERSION, installed_ver.as_deref(), false) {
            InstalledAction::HandOffToInstalled => {
               drop(lock);
               spawn_launcher_handoff(
                  existing_launcher,
                  target_exe,
                  no_runtime,
                  discord_args,
                  quiet,
                  false,
               )?;
               return Ok(None);
            },
            InstalledAction::InstalledVersionUnknown => {
               drop(lock);
               return Err(installed_version_unknown_error(existing_launcher));
            },
            InstalledAction::CheckWith(_) => {},
         }
      }
      Ok(Some(lock))
   }

   fn prepare_managed_launch(
      home: &Path,
      me: &Path,
      explicit_target: bool,
      target_exe: Option<&Path>,
      no_runtime: bool,
      discord_args: &[OsString],
      quiet: bool,
   ) -> misstep::Result<Option<(PathBuf, PathBuf, Option<PathBuf>)>> {
      validate_bundled_client(me)?;
      let existing_launcher = home.join("NyaaCord.exe");
      let had_launcher = existing_launcher.is_file();
      if had_launcher
         && check_existing_launcher(
            home,
            me,
            &existing_launcher,
            target_exe,
            no_runtime,
            discord_args,
            quiet,
         )?
      {
         return Ok(None);
      }

      fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
      let lock = acquire_install_lock(home)?;
      let Some(lock) = hand_off_if_newer_locked(
         lock,
         me,
         &existing_launcher,
         target_exe,
         no_runtime,
         discord_args,
         quiet,
      )?
      else {
         return Ok(None);
      };

      let will_replace = launcher_differs(me, &existing_launcher);
      let mut tx =
         begin_bundled_client_sync_locked(lock, me, home, !explicit_target, will_replace)?;
      let (launcher, dll, copied) = install_self(&mut tx, home, me)?;
      let bundled_exe = tx.commit();
      if copied {
         let _ = shortcuts(&launcher, false);
      }

      if !had_launcher
         && maybe_auto_update(
            home,
            VERSION,
            &launcher,
            target_exe,
            no_runtime,
            discord_args,
            quiet,
         )
      {
         return Ok(None);
      }
      Ok(Some((launcher, dll, bundled_exe)))
   }

   fn cmd_launch(
      explicit_dll: Option<PathBuf>,
      target_exe: Option<PathBuf>,
      no_runtime: bool,
      discord_args: &[OsString],
      ui: &Ui,
   ) -> misstep::Result<()> {
      let local = local_app_data()?;
      let home = install_home_in(&local);
      let me = env::current_exe().context("resolving current executable")?;
      let env_discord = env::var_os("NYAACORD_DISCORD_EXE");
      let explicit_target =
         target_exe.is_some() || env_discord.as_deref().is_some_and(|val| !val.is_empty());

      let (managed_exe, dll, bundled_exe) = if let Some(path) = explicit_dll {
         (None, path, None)
      } else {
         let Some((launcher, dll, bundled_exe)) = prepare_managed_launch(
            &home,
            &me,
            explicit_target,
            target_exe.as_deref(),
            no_runtime,
            discord_args,
            ui.quiet,
         )?
         else {
            return Ok(());
         };
         (Some(launcher), dll, bundled_exe)
      };

      let exe = resolve_discord_exe_with(target_exe, env_discord, bundled_exe, &local);
      if !exe.is_file() {
         bail!(
            "Discord Phoenix is not installed at {}. Download the full NyaaCord executable or pass --target-exe.",
            exe.display()
         );
      }
      let app = exe
         .parent()
         .filter(|p| !p.as_os_str().is_empty())
         .unwrap_or_else(|| Path::new("."))
         .to_path_buf();

      let dll = dll
         .canonicalize()
         .with_context(|| format!("resolving {}", dll.display()))?;

      let launch_target = LaunchTarget {
         exe: &exe,
         app: &app,
         dll: &dll,
      };
      // A second Discord instance hands off to the running one and exits, so NyaaCord
      // would never load into the Discord window the user sees.
      if !handle_running_discord(
         &launch_target,
         &local,
         explicit_target,
         no_runtime,
         discord_args,
         ui,
      )? {
         return Ok(());
      }

      if let Some(launcher) = managed_exe {
         // SAFETY: launcher is single-threaded here; child `Discord.exe` inherits this path.
         unsafe { env::set_var("NYAACORD_MANAGED_EXE", &launcher) };
      }
      // SAFETY: launcher is single-threaded here; child `Discord.exe` inherits this version.
      unsafe { env::set_var("NYAACORD_VERSION", VERSION) };
      if no_runtime {
         // SAFETY: launcher is single-threaded here; child opens only the access gate.
         unsafe { env::set_var("SUNBIRD_GATE_ONLY", "1") };
      }

      let pid = spawn_injected_discord(&exe, &app, &dll, discord_args)?;
      ui.info(&format!(
         "Started Discord (pid {pid}) with {}",
         dll.display()
      ));
      Ok(())
   }

   pub fn run(mode: Mode, ui: &Ui) -> misstep::Result<()> {
      match mode {
         Mode::Version => {
            let _ = writeln!(io::stdout(), "NyaaCord {VERSION}");
            Ok(())
         },
         Mode::Help => {
            let _ = writeln!(io::stdout(), "{USAGE}");
            Ok(())
         },
         Mode::InstallOnly => {
            let local = local_app_data()?;
            let home = install_home_in(&local);
            let me = env::current_exe().context("resolving current executable")?;
            validate_bundled_client(&me)?;
            fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
            let lock = acquire_install_lock(&home)?;
            let will_replace = launcher_differs(&me, &home.join("NyaaCord.exe"));
            let mut tx = begin_bundled_client_sync_locked(lock, &me, &home, true, will_replace)?;
            let (launcher, dll, _) = install_self(&mut tx, &home, &me)?;
            let bundled = tx.commit();
            let _ = shortcuts(&launcher, false);
            ui.info(&format!("Installed NyaaCord {VERSION}"));
            ui.info(&format!("Launcher: {}", launcher.display()));
            ui.info(&format!("Loader: {}", dll.display()));
            if let Some(client_exe) = bundled {
               ui.info(&format!("Client: {}", client_exe.display()));
            }
            Ok(())
         },
         Mode::CheckUpdate {
            repo,
            api,
            timeout,
            json,
         } => cmd_check_update(repo, api, timeout, json, ui),
         Mode::Update {
            target,
            repo,
            api,
            timeout,
            force,
         } => cmd_update(target, repo, api, timeout, force, ui),
         Mode::Rollback { target } => cmd_rollback(target, ui),
         Mode::Uninstall => cmd_uninstall(ui),
         Mode::Launch {
            explicit_dll,
            target_exe,
            no_runtime,
            discord_args,
         } => cmd_launch(explicit_dll, target_exe, no_runtime, &discord_args, ui),
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   fn os_args(items: &[&str]) -> Vec<OsString> {
      items.iter().map(OsString::from).collect()
   }

   #[test]
   fn parses_cli_modes_and_flags() {
      assert_eq!(
         parse_args(os_args(&["--version"])).expect("version"),
         CliOptions {
            mode: Mode::Version,
            quiet: false,
         }
      );
      assert_eq!(
         parse_args(os_args(&["--quiet", "--install-only"])).expect("install-only"),
         CliOptions {
            mode: Mode::InstallOnly,
            quiet: true,
         }
      );
      assert_eq!(
         parse_args(os_args(&[
            "--check-update",
            "--repo",
            "CateDesu/NyaaCord",
            "--json"
         ]))
         .expect("check-update"),
         CliOptions {
            mode: Mode::CheckUpdate {
               repo: Some("CateDesu/NyaaCord".to_owned()),
               api: None,
               timeout: None,
               json: true,
            },
            quiet: false,
         }
      );
      assert_eq!(
         parse_args(os_args(&[
            "--update",
            "--target",
            r"C:\Apps\NyaaCord.exe",
            "--force",
            "--quiet"
         ]))
         .expect("update"),
         CliOptions {
            mode: Mode::Update {
               target: Some(PathBuf::from(r"C:\Apps\NyaaCord.exe")),
               repo: None,
               api: None,
               timeout: None,
               force: true,
            },
            quiet: true,
         }
      );
      assert_eq!(
         parse_args(os_args(&["--rollback", "--target=NyaaCord.exe"])).expect("rollback"),
         CliOptions {
            mode: Mode::Rollback {
               target: Some(PathBuf::from("NyaaCord.exe")),
            },
            quiet: false,
         }
      );
      assert_eq!(
         parse_args(os_args(&["--uninstall", "-q"])).expect("uninstall"),
         CliOptions {
            mode: Mode::Uninstall,
            quiet: true,
         }
      );
   }

   #[test]
   fn parses_launch_options_and_discord_args() {
      let opts = parse_args(os_args(&[
         "--no-runtime",
         "--target-exe",
         r"C:\Program Files\Discord\Discord.exe",
         r"C:\dev\phoenix.dll",
         "--",
         "--start-minimized",
         "--url=discord://app/settings",
      ]))
      .expect("launch");

      assert_eq!(
         opts,
         CliOptions {
            mode: Mode::Launch {
               explicit_dll: Some(PathBuf::from(r"C:\dev\phoenix.dll")),
               target_exe: Some(PathBuf::from(r"C:\Program Files\Discord\Discord.exe")),
               no_runtime: true,
               discord_args: os_args(&["--start-minimized", "--url=discord://app/settings"]),
            },
            quiet: false,
         }
      );
   }

   #[test]
   fn quotes_windows_command_lines_safely() {
      let exe = Path::new(r"C:\Program Files\Discord Phoenix\Discord.exe");
      let args = os_args(&[
         "--flag",
         "arg with spaces",
         r#"say "hello""#,
         r"C:\trailing slash\",
         "",
      ]);
      let cmdline = build_windows_cmdline(exe, &args);
      assert_eq!(
         cmdline,
         r#""C:\Program Files\Discord Phoenix\Discord.exe" --flag "arg with spaces" "say \"hello\"" "C:\trailing slash\\" """#
      );
   }

   fn build_fake_pe64(cert_bytes: Option<&[u8]>, overlay: &[u8]) -> Vec<u8> {
      const PE_OFF: usize = 0x80;
      const OPT_SIZE: u16 = 240;
      const SEC_RAW_PTR: u32 = 0x200;
      const SEC_RAW_SIZE: u32 = 0x200;
      const PE_END: usize = (SEC_RAW_PTR + SEC_RAW_SIZE) as usize;

      let mut buf = vec![0_u8; PE_END];
      buf[0..2].copy_from_slice(b"MZ");
      buf[60..64].copy_from_slice(&u32::try_from(PE_OFF).expect("pe_off").to_le_bytes());

      buf[PE_OFF..PE_OFF + 4].copy_from_slice(b"PE\0\0");
      buf[PE_OFF + 4..PE_OFF + 6].copy_from_slice(&0x8664_u16.to_le_bytes());
      buf[PE_OFF + 6..PE_OFF + 8].copy_from_slice(&1_u16.to_le_bytes());
      buf[PE_OFF + 20..PE_OFF + 22].copy_from_slice(&OPT_SIZE.to_le_bytes());
      buf[PE_OFF + 22..PE_OFF + 24].copy_from_slice(&0x0022_u16.to_le_bytes());

      let opt_off = PE_OFF + 24;
      buf[opt_off..opt_off + 2].copy_from_slice(&0x020B_u16.to_le_bytes());
      buf[opt_off + 108..opt_off + 112].copy_from_slice(&16_u32.to_le_bytes());

      if let Some(cert) = cert_bytes {
         let cert_off = u32::try_from(PE_END).expect("cert_off");
         let cert_sz = u32::try_from(cert.len()).expect("cert_sz");
         buf[opt_off + 144..opt_off + 148].copy_from_slice(&cert_off.to_le_bytes());
         buf[opt_off + 148..opt_off + 152].copy_from_slice(&cert_sz.to_le_bytes());
      }

      let sec_off = opt_off + usize::from(OPT_SIZE);
      buf[sec_off..sec_off + 5].copy_from_slice(b".text");
      buf[sec_off + 16..sec_off + 20].copy_from_slice(&SEC_RAW_SIZE.to_le_bytes());
      buf[sec_off + 20..sec_off + 24].copy_from_slice(&SEC_RAW_PTR.to_le_bytes());

      if let Some(cert) = cert_bytes {
         buf.extend_from_slice(cert);
      }
      buf.extend_from_slice(overlay);
      buf
   }

   fn build_pe_with_raw_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
      use std::io::Write as _;
      let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
      let opts = zip::write::SimpleFileOptions::default()
         .compression_method(zip::CompressionMethod::Deflated);
      for &(name, data) in entries {
         writer.start_file(name, opts).expect("start zip file");
         writer.write_all(data).expect("write zip data");
      }
      let archive = writer.finish().expect("finish zip").into_inner();
      build_fake_pe64(None, &archive)
   }

   fn manifest_stamp_for(entries: &[(&str, &[u8])]) -> String {
      let mut items: Vec<(String, u64, String)> = entries
         .iter()
         .map(|&(name, data)| {
            let mut hasher = phoenix_updater::sha256::Hasher::new();
            hasher.update(data);
            (name.to_owned(), data.len() as u64, hasher.finalize_hex())
         })
         .collect();
      compute_manifest_stamp(&mut items).expect("manifest stamp")
   }

   fn build_pe_with_zip(entries: &[(&str, &[u8])]) -> (Vec<u8>, String) {
      let stamp = manifest_stamp_for(entries);
      let stamp_line = format!("{stamp}\n");
      let mut all = Vec::with_capacity(entries.len() + 1);
      all.extend_from_slice(entries);
      all.push((CLIENT_STAMP_ENTRY, stamp_line.as_bytes()));
      (build_pe_with_raw_zip(&all), stamp)
   }

   fn sync_commit(
      exe_path: &Path,
      home: &Path,
      expect_bundle: bool,
      verify_payload_if_skipped: bool,
   ) -> misstep::Result<Option<PathBuf>> {
      let tx = begin_client_sync(
         exe_path,
         home,
         ClientSyncOptions {
            expect_bundle,
            extract: true,
            verify_payload_if_skipped,
         },
      )?;
      Ok(tx.commit())
   }

   #[test]
   fn parses_and_validates_client_stamps() {
      let hex_a = "A".repeat(64);
      let expected = format!("v1:{}", "a".repeat(64));
      assert_eq!(
         parse_client_stamp(&format!("  v1:{hex_a}\r\n")),
         Some(expected)
      );
      assert_eq!(parse_client_stamp("v1:bad"), None);
      assert_eq!(parse_client_stamp(&format!("v1: {hex_a}")), None);
      assert_eq!(parse_client_stamp(&hex_a), None);
      assert_eq!(parse_client_stamp(&format!("v1:{}", "g".repeat(64))), None);
   }

   #[test]
   fn rejects_missing_corrupt_and_invalid_overlay_bundles() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-overlay-err-test", "dir");
      std::fs::create_dir_all(&root).expect("create root");
      let home = root.join("home");

      // Valid thin PE32+ returns Ok(None) when not expecting a bundle, Err when expecting one.
      let thin_exe = root.join("thin.exe");
      std::fs::write(&thin_exe, build_fake_pe64(None, &[])).expect("write thin exe");
      assert_eq!(sync_commit(&thin_exe, &home, false, false).expect("thin"), None);
      assert!(sync_commit(&thin_exe, &home, true, false).is_err());

      // Valid PE32+ with Authenticode certificate table only is treated as thin.
      let signed_thin = root.join("signed-thin.exe");
      std::fs::write(&signed_thin, build_fake_pe64(Some(&[0xAA_u8; 32]), &[]))
         .expect("write signed thin exe");
      assert_eq!(
         sync_commit(&signed_thin, &home, false, false).expect("signed thin"),
         None
      );

      // Corrupt trailing overlay bytes must fail even when expect_bundle is false.
      let corrupt_exe = root.join("corrupt-overlay.exe");
      std::fs::write(&corrupt_exe, build_fake_pe64(None, b"not-a-valid-zip"))
         .expect("write corrupt overlay");
      assert!(sync_commit(&corrupt_exe, &home, false, false).is_err());
      assert!(sync_commit(&corrupt_exe, &home, true, false).is_err());

      // ZIP overlay without nyaacord-client.stamp must fail.
      let missing_stamp = root.join("missing-stamp.exe");
      std::fs::write(
         &missing_stamp,
         build_pe_with_raw_zip(&[(CLIENT_EXE_NAME, b"discord")]),
      )
      .expect("write missing stamp bundle");
      assert!(sync_commit(&missing_stamp, &home, false, false).is_err());

      // ZIP overlay with oversized (>128 B) or non-hex stamp must fail.
      let big_stamp = format!("v1:{}", "a".repeat(200));
      let oversized_exe = root.join("oversized-stamp.exe");
      std::fs::write(
         &oversized_exe,
         build_pe_with_raw_zip(&[
            (CLIENT_STAMP_ENTRY, big_stamp.as_bytes()),
            (CLIENT_EXE_NAME, b"discord"),
         ]),
      )
      .expect("write oversized stamp bundle");
      assert!(sync_commit(&oversized_exe, &home, false, false).is_err());

      let bad_stamp_exe = root.join("bad-stamp.exe");
      std::fs::write(
         &bad_stamp_exe,
         build_pe_with_raw_zip(&[
            (CLIENT_STAMP_ENTRY, b"v1:bad\n"),
            (CLIENT_EXE_NAME, b"discord"),
         ]),
      )
      .expect("write bad stamp bundle");
      assert!(sync_commit(&bad_stamp_exe, &home, false, false).is_err());

      // Mismatched manifest stamp (valid hex format, wrong digest) must fail.
      let wrong_stamp = format!("v1:{}\n", "f".repeat(64));
      let wrong_stamp_exe = root.join("wrong-stamp.exe");
      std::fs::write(
         &wrong_stamp_exe,
         build_pe_with_raw_zip(&[
            (CLIENT_STAMP_ENTRY, wrong_stamp.as_bytes()),
            (CLIENT_EXE_NAME, b"discord"),
         ]),
      )
      .expect("write wrong stamp bundle");
      assert!(sync_commit(&wrong_stamp_exe, &home, true, false).is_err());

      let _ = std::fs::remove_dir_all(&root);
   }

   #[test]
   fn extracts_and_syncs_bundled_pe_overlay_client() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-inject-test", "dir");
      std::fs::create_dir_all(&root).expect("create root");
      let home = root.join("home");

      // Bundled PE with Deflate overlay extracts nested files and writes stamp.
      let exe_v1 = root.join("bundle-v1.exe");
      let (payload_v1, stamp_one) = build_pe_with_zip(&[
         (CLIENT_EXE_NAME, b"discord-bin-v1"),
         (
            "modules/discord_clips-0/discord_clips/clip.json",
            b"{\"ok\":true}",
         ),
      ]);
      std::fs::write(&exe_v1, payload_v1).expect("write v1 bundle");

      let extracted = sync_commit(&exe_v1, &home, true, true)
         .expect("extract v1")
         .expect("discord path");
      assert_eq!(extracted, home.join("app").join("Discord.exe"));
      assert_eq!(
         std::fs::read(&extracted).expect("read extracted exe"),
         b"discord-bin-v1"
      );
      assert_eq!(
         std::fs::read_to_string(
            home
               .join("app")
               .join("modules/discord_clips-0/discord_clips/clip.json")
         )
         .expect("read nested file"),
         "{\"ok\":true}"
      );
      assert_eq!(
         std::fs::read_to_string(home.join("app").join(CLIENT_STAMP_FILE))
            .expect("read stamp")
            .trim(),
         stamp_one
      );

      // Matching stamp skips re-extraction, leaving marker file untouched.
      let marker = home.join("app").join("keep.marker");
      std::fs::write(&marker, b"kept").expect("write marker");
      let second = sync_commit(&exe_v1, &home, true, true)
         .expect("second sync")
         .expect("discord path");
      assert_eq!(second, extracted);
      assert!(marker.is_file());

      // Changed stamp replaces the app directory cleanly.
      let exe_v2 = root.join("bundle-v2.exe");
      let (payload_v2, stamp_two) = build_pe_with_zip(&[(CLIENT_EXE_NAME, b"discord-bin-v2")]);
      std::fs::write(&exe_v2, payload_v2).expect("write v2 bundle");

      sync_commit(&exe_v2, &home, true, true)
         .expect("extract v2")
         .expect("discord v2 path");
      assert_eq!(
         std::fs::read(&extracted).expect("read v2 exe"),
         b"discord-bin-v2"
      );
      assert!(!marker.exists());
      assert_eq!(
         std::fs::read_to_string(home.join("app").join(CLIENT_STAMP_FILE))
            .expect("read v2 stamp")
            .trim(),
         stamp_two
      );

      // Zip-slip path is rejected before replacing anything.
      let evil_exe = root.join("evil.exe");
      let evil_stamp = format!("v1:{}\n", "e".repeat(64));
      let evil_payload = build_pe_with_raw_zip(&[
         (CLIENT_STAMP_ENTRY, evil_stamp.as_bytes()),
         (CLIENT_EXE_NAME, b"discord-evil"),
         ("../escape.txt", b"nope"),
      ]);
      std::fs::write(&evil_exe, evil_payload).expect("write evil bundle");
      assert!(sync_commit(&evil_exe, &home, true, true).is_err());
      assert!(!home.join("escape.txt").exists());
      assert!(!root.join("escape.txt").exists());
      assert_eq!(
         std::fs::read(&extracted).expect("v2 intact after rejected zip"),
         b"discord-bin-v2"
      );

      let _ = std::fs::remove_dir_all(&root);
   }

   #[test]
   fn rejects_corrupt_deflate_payload_with_matching_stamp() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-deflate-err-test", "dir");
      std::fs::create_dir_all(&root).expect("create root");
      let home = root.join("home");

      let notice = b"ThirdPartyNotices repeated payload text for deflate compression! ".repeat(16);
      let (good_bytes, _) = build_pe_with_zip(&[
         (CLIENT_EXE_NAME, b"discord-good"),
         ("ThirdPartyNotices.html", &notice),
      ]);
      let good_exe = root.join("good.exe");
      std::fs::write(&good_exe, &good_bytes).expect("write good exe");
      sync_commit(&good_exe, &home, true, true).expect("initial sync");

      // Corrupt one compressed byte inside ThirdPartyNotices.html while keeping headers and stamp valid.
      let mut tampered = good_bytes;
      let flip_offset = {
         let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(&tampered)).expect("open good zip");
         let entry = archive
            .by_name("ThirdPartyNotices.html")
            .expect("find notice entry");
         usize::try_from(entry.data_start().expect("data_start")).expect("data_start fits usize")
            + 4
      };
      tampered[flip_offset] ^= 0x55;

      let tampered_exe = root.join("tampered.exe");
      std::fs::write(&tampered_exe, &tampered).expect("write tampered exe");

      // Even though app/client-stamp.txt matches, verify_payload_if_skipped rejects the corrupt stream.
      assert!(sync_commit(&tampered_exe, &home, true, true).is_err());
      // Explicit-target skip path with verify_payload_if_skipped also rejects it.
      let skip_res = begin_client_sync(
         &tampered_exe,
         &home,
         ClientSyncOptions {
            expect_bundle: true,
            extract: false,
            verify_payload_if_skipped: true,
         },
      );
      assert!(skip_res.is_err());
      assert_eq!(
         std::fs::read(home.join(CLIENT_DIR_NAME).join(CLIENT_EXE_NAME)).expect("intact exe"),
         b"discord-good"
      );

      let _ = std::fs::remove_dir_all(&root);
   }

   #[test]
   fn rolls_back_client_transaction_on_uncommitted_drop() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-tx-rollback-test", "dir");
      std::fs::create_dir_all(&root).expect("create root");
      let home = root.join("home");

      let exe_v1 = root.join("v1.exe");
      let (bytes_v1, stamp_v1) = build_pe_with_zip(&[(CLIENT_EXE_NAME, b"discord-v1")]);
      std::fs::write(&exe_v1, bytes_v1).expect("write v1");

      // Uncommitted fresh install removes the newly created app dir on drop.
      {
         let tx = begin_client_sync(
            &exe_v1,
            &home,
            ClientSyncOptions {
               expect_bundle: true,
               extract: true,
               verify_payload_if_skipped: true,
            },
         )
         .expect("begin fresh v1");
         assert!(home.join(CLIENT_DIR_NAME).join(CLIENT_EXE_NAME).is_file());
         drop(tx);
      }
      assert!(!home.join(CLIENT_DIR_NAME).exists());

      // Commit v1 cleanly.
      sync_commit(&exe_v1, &home, true, true).expect("commit v1");
      assert_eq!(
         std::fs::read(home.join(CLIENT_DIR_NAME).join(CLIENT_EXE_NAME)).expect("read v1"),
         b"discord-v1"
      );

      // Begin v2, then simulate install_self failing by dropping tx without commit().
      let exe_v2 = root.join("v2.exe");
      let (bytes_v2, _) = build_pe_with_zip(&[(CLIENT_EXE_NAME, b"discord-v2")]);
      std::fs::write(&exe_v2, bytes_v2).expect("write v2");
      {
         let tx = begin_client_sync(
            &exe_v2,
            &home,
            ClientSyncOptions {
               expect_bundle: true,
               extract: true,
               verify_payload_if_skipped: true,
            },
         )
         .expect("begin v2");
         assert_eq!(
            std::fs::read(home.join(CLIENT_DIR_NAME).join(CLIENT_EXE_NAME)).expect("temp v2"),
            b"discord-v2"
         );
         drop(tx);
      }

      // app/Discord.exe and app/client-stamp.txt must be restored back to v1.
      assert_eq!(
         std::fs::read(home.join(CLIENT_DIR_NAME).join(CLIENT_EXE_NAME)).expect("restored v1"),
         b"discord-v1"
      );
      assert_eq!(
         std::fs::read_to_string(home.join(CLIENT_DIR_NAME).join(CLIENT_STAMP_FILE))
            .expect("restored v1 stamp")
            .trim(),
         stamp_v1
      );

      let _ = std::fs::remove_dir_all(&root);
   }

   fn begin_full_sync(exe_path: &Path, home: &Path) -> misstep::Result<ClientTransaction> {
      begin_client_sync(
         exe_path,
         home,
         ClientSyncOptions {
            expect_bundle: true,
            extract: true,
            verify_payload_if_skipped: true,
         },
      )
   }

   fn assert_no_tx_leftovers(dir: &Path) {
      for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
         let name = entry.file_name().to_string_lossy().into_owned();
         let ext = Path::new(&name)
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");
         assert!(
            !ext.eq_ignore_ascii_case("new")
               && !ext.eq_ignore_ascii_case("old")
               && !ext.eq_ignore_ascii_case("failed"),
            "unexpected leftover transaction artifact {name:?}"
         );
      }
   }

   #[test]
   fn rolls_back_launcher_runtime_and_client_on_dll_failure() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-full-rollback-test", "dir");
      std::fs::create_dir_all(&root).expect("create root");
      let home = root.join("home");

      let exe_v1 = root.join("v1.exe");
      let (bytes_v1, stamp_v1) = build_pe_with_zip(&[(CLIENT_EXE_NAME, b"discord-v1")]);
      std::fs::write(&exe_v1, &bytes_v1).expect("write v1 exe");
      let exe_v2 = root.join("v2.exe");
      let (bytes_v2, _) = build_pe_with_zip(&[(CLIENT_EXE_NAME, b"discord-v2")]);
      std::fs::write(&exe_v2, &bytes_v2).expect("write v2 exe");

      let launcher = home.join("NyaaCord.exe");
      let dll = home.join("phoenix.dll");
      let plugins = home.join("libplm.plugins.so");
      let version_file = home.join("version.txt");
      let discord = home.join(CLIENT_DIR_NAME).join(CLIENT_EXE_NAME);
      let stamp_file = home.join(CLIENT_DIR_NAME).join(CLIENT_STAMP_FILE);

      // Fresh install failure removes newly created launcher, plugins, and app dir.
      {
         let mut tx = begin_full_sync(&exe_v1, &home).expect("begin fresh v1");
         tx.copy_file(&exe_v1, &launcher).expect("copy fresh launcher");
         tx.write_file(&plugins, b"plugins-v1").expect("write fresh plugins");
         assert!(tx.replace_file_with(&dll, |_| bail!("simulated fresh DLL failure")).is_err());
      }
      assert!(!launcher.exists() && !plugins.exists() && !dll.exists());
      assert!(!home.join(CLIENT_DIR_NAME).exists());

      // Commit complete v1 installation and snapshot all v1 digests.
      {
         let mut tx = begin_full_sync(&exe_v1, &home).expect("begin v1");
         tx.copy_file(&exe_v1, &launcher).expect("install v1 launcher");
         tx.write_file(&dll, b"phoenix-dll-v1").expect("install v1 dll");
         tx.write_file(&plugins, b"plugins-v1").expect("install v1 plugins");
         tx.write_file(&version_file, b"0.2.1\n").expect("install v1 version");
         tx.commit();
      }
      let tracked = [&launcher, &dll, &plugins, &version_file, &discord];
      let v1_hashes: Vec<String> = tracked
         .iter()
         .map(|path| phoenix_updater::sha256::sha256_file(path).expect("v1 sha"))
         .collect();

      // Upgrade app, launcher, plugins, and version to v2, then fail on phoenix.dll.
      {
         let mut tx = begin_full_sync(&exe_v2, &home).expect("begin v2");
         tx.copy_file(&exe_v2, &launcher).expect("stage v2 launcher");
         tx.write_file(&plugins, b"plugins-v2").expect("stage v2 plugins");
         tx.write_file(&version_file, b"0.2.2\n").expect("stage v2 version");
         assert_eq!(std::fs::read(&plugins).expect("temp v2 plugins"), b"plugins-v2");
         assert_eq!(std::fs::read(&discord).expect("temp v2 discord"), b"discord-v2");
         assert!(tx.replace_file_with(&dll, |_| bail!("simulated locked DLL")).is_err());
      }

      // Every v1 file and the bundled client must be restored identically with no leftovers.
      for (path, expected) in tracked.iter().zip(&v1_hashes) {
         let actual = phoenix_updater::sha256::sha256_file(path).expect("restored sha");
         assert_eq!(&actual, expected, "mismatch for {}", path.display());
      }
      assert_eq!(std::fs::read_to_string(&stamp_file).expect("stamp").trim(), stamp_v1);
      assert_no_tx_leftovers(&home);

      let _ = std::fs::remove_dir_all(&root);
   }

   #[test]
   fn recovers_and_preserves_client_backups_safely() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-backup-test", "dir");
      let home = root.join("home");
      std::fs::create_dir_all(&home).expect("create home");

      let thin_exe = root.join("thin.exe");
      std::fs::write(&thin_exe, build_fake_pe64(None, &[])).expect("write thin exe");

      // Missing app dir recovers the sorted-last usable app.*.old backup.
      let old_a = home.join("app.1.1.0.old");
      let old_b = home.join("app.1.2.0.old");
      std::fs::create_dir_all(&old_a).expect("mkdir old_a");
      std::fs::create_dir_all(&old_b).expect("mkdir old_b");
      std::fs::write(old_a.join(CLIENT_EXE_NAME), b"older-bin").expect("write old_a");
      std::fs::write(old_b.join(CLIENT_EXE_NAME), b"newer-bin").expect("write old_b");

      assert_eq!(
         sync_commit(&thin_exe, &home, false, false).expect("thin sync recovers backup"),
         None
      );
      let recovered_exe = home.join(CLIENT_DIR_NAME).join(CLIENT_EXE_NAME);
      assert_eq!(
         std::fs::read(&recovered_exe).expect("read recovered exe"),
         b"newer-bin"
      );
      assert!(!old_a.exists());
      assert!(!old_b.exists());

      // When app is unusable (exists without Discord.exe), a failed sync preserves app.*.old.
      let broken_app = home.join(CLIENT_DIR_NAME);
      std::fs::remove_dir_all(&broken_app).expect("remove app");
      std::fs::create_dir_all(&broken_app).expect("create empty broken app");
      let preserved_old = home.join("app.9.9.0.old");
      std::fs::create_dir_all(&preserved_old).expect("mkdir preserved_old");
      std::fs::write(preserved_old.join(CLIENT_EXE_NAME), b"last-good").expect("write last-good");

      let corrupt_exe = root.join("corrupt.exe");
      std::fs::write(&corrupt_exe, build_fake_pe64(None, b"bad-overlay")).expect("write corrupt");
      assert!(sync_commit(&corrupt_exe, &home, true, false).is_err());
      assert!(preserved_old.join(CLIENT_EXE_NAME).is_file());
      assert_eq!(
         sync_commit(&thin_exe, &home, false, false).expect("thin with broken app"),
         None
      );
      assert!(preserved_old.join(CLIENT_EXE_NAME).is_file());

      let _ = std::fs::remove_dir_all(&root);
   }

   #[test]
   fn validates_bundled_client_without_extracting() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-validate-test", "dir");
      std::fs::create_dir_all(&root).expect("create root");
      let stamp = format!("v1:{}", "a".repeat(64));
      let stamp_line = format!("{stamp}\n");

      // Thin PE32+ passes when not expecting a bundle and fails when expecting one.
      let thin = root.join("thin.exe");
      std::fs::write(&thin, build_fake_pe64(None, &[])).expect("write thin");
      assert_eq!(
         validate_bundled_client_with(&thin, false).expect("thin ok"),
         None
      );
      assert!(validate_bundled_client_with(&thin, true).is_err());

      // Corrupt trailing overlay fails.
      let corrupt = root.join("corrupt.exe");
      std::fs::write(&corrupt, build_fake_pe64(None, b"garbage")).expect("write corrupt");
      assert!(validate_bundled_client_with(&corrupt, false).is_err());

      // Missing Discord.exe fails.
      let no_discord = root.join("no-discord.exe");
      std::fs::write(
         &no_discord,
         build_pe_with_raw_zip(&[
            (CLIENT_STAMP_ENTRY, stamp_line.as_bytes()),
            ("other.dll", b"dll"),
         ]),
      )
      .expect("write no-discord");
      assert!(validate_bundled_client_with(&no_discord, true).is_err());

      // Zip-slip path fails.
      let slip = root.join("slip.exe");
      std::fs::write(
         &slip,
         build_pe_with_raw_zip(&[
            (CLIENT_STAMP_ENTRY, stamp_line.as_bytes()),
            (CLIENT_EXE_NAME, b"discord"),
            ("../escape.txt", b"nope"),
         ]),
      )
      .expect("write slip");
      assert!(validate_bundled_client_with(&slip, true).is_err());

      // Valid bundle returns the normalized stamp.
      let good = root.join("good.exe");
      let (good_bytes, good_stamp) = build_pe_with_zip(&[(CLIENT_EXE_NAME, b"discord")]);
      std::fs::write(&good, good_bytes).expect("write good");
      assert_eq!(
         validate_bundled_client_with(&good, true).expect("valid bundle"),
         Some(good_stamp)
      );

      let _ = std::fs::remove_dir_all(&root);
   }

   #[test]
   fn decides_installed_launcher_action() {
      assert_eq!(
         decide_installed_action("0.2.1", Some("0.2.2"), false),
         InstalledAction::HandOffToInstalled
      );
      assert_eq!(
         decide_installed_action("0.2.2", Some("0.2.1"), false),
         InstalledAction::CheckWith("0.2.2".to_owned())
      );
      assert_eq!(
         decide_installed_action("0.2.1", Some("0.2.1"), false),
         InstalledAction::CheckWith("0.2.1".to_owned())
      );
      assert_eq!(
         decide_installed_action("0.2.1", Some("0.2.2"), true),
         InstalledAction::CheckWith("0.2.1".to_owned())
      );
      assert_eq!(
         decide_installed_action("0.2.1", None, true),
         InstalledAction::CheckWith("0.2.1".to_owned())
      );
      assert_eq!(
         decide_installed_action("0.2.1", Some("0.2.1"), true),
         InstalledAction::CheckWith("0.2.1".to_owned())
      );
      assert_eq!(
         decide_installed_action("v0.2.1", None, false),
         InstalledAction::InstalledVersionUnknown
      );
      assert_eq!(
         decide_installed_action("0.2.1", Some("   "), false),
         InstalledAction::InstalledVersionUnknown
      );
      assert_eq!(
         decide_installed_action("0.2.1", Some("bad-ver"), false),
         InstalledAction::InstalledVersionUnknown
      );
   }

   #[test]
   fn propagates_handoff_spawn_error() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-handoff-err-test", "dir");
      let missing_exe = root.join("missing-nyaacord-launcher");

      let mut ran_update = false;
      let err = run_installed_action(
         "0.2.1",
         Some("0.2.2"),
         false,
         &missing_exe,
         || {
            std::process::Command::new(&missing_exe)
               .spawn()
               .with_context(|| {
                  format!("launching installed NyaaCord at {}", missing_exe.display())
               })?;
            Ok(())
         },
         |_| {
            ran_update = true;
            false
         },
      )
      .expect_err("spawn failure during HandOffToInstalled must propagate Err");
      assert!(!ran_update);
      assert!(
         err.to_string().contains("launching installed NyaaCord"),
         "unexpected error: {err}"
      );

      // Unknown or unparseable installed version on a different binary fails closed.
      for bad_ver in [None, Some(""), Some("bad-ver")] {
         let mut called_handoff = false;
         let mut called_update = false;
         let err = run_installed_action(
            "0.2.1",
            bad_ver,
            false,
            &missing_exe,
            || {
               called_handoff = true;
               Ok(())
            },
            |_| {
               called_update = true;
               false
            },
         )
         .expect_err("unknown installed version must fail closed");
         assert!(!called_handoff);
         assert!(!called_update);
         assert!(
            err.to_string().contains("--install-only"),
            "unexpected error: {err}"
         );
      }

      // Successful handoff returns Ok(true) without calling auto_update.
      let handed = run_installed_action(
         "0.2.1",
         Some("0.2.2"),
         false,
         &missing_exe,
         || Ok(()),
         |_| panic!("auto_update must not run when handing off"),
      )
      .expect("successful handoff");
      assert!(handed);

      // Newer or equal running binary runs auto_update with the higher version.
      let mut checked_ver = String::new();
      let updated = run_installed_action(
         "0.2.2",
         Some("0.2.1"),
         false,
         &missing_exe,
         || panic!("hand_off must not run when running binary is newer"),
         |ver| {
            checked_ver = ver.to_owned();
            false
         },
      )
      .expect("check update");
      assert!(!updated);
      assert_eq!(checked_ver, "0.2.2");
   }

   #[test]
   fn plans_manual_update_and_force_decisions() {
      // Non-force --update against a different existing target always compares the target's version.
      assert_eq!(
         plan_manual_update("0.2.1", true, false, Some("0.2.2"), false),
         ManualUpdatePrecheck::CheckVersion("0.2.2".to_owned())
      );
      assert_eq!(
         plan_manual_update("0.2.1", true, false, Some("0.2.0"), false),
         ManualUpdatePrecheck::CheckVersion("0.2.0".to_owned())
      );
      assert_eq!(
         plan_manual_update("0.2.2", true, false, Some("0.2.1"), false),
         ManualUpdatePrecheck::CheckVersion("0.2.1".to_owned())
      );
      assert_eq!(
         plan_manual_update("0.2.1", true, false, Some("0.2.1"), false),
         ManualUpdatePrecheck::CheckVersion("0.2.1".to_owned())
      );
      // Missing or unparseable target version fails closed unless --force is set.
      assert_eq!(
         plan_manual_update("0.2.1", true, false, None, false),
         ManualUpdatePrecheck::UnknownTarget
      );
      assert_eq!(
         plan_manual_update("0.2.1", true, false, Some("   "), false),
         ManualUpdatePrecheck::UnknownTarget
      );
      assert_eq!(
         plan_manual_update("0.2.1", true, false, Some("not-semver"), false),
         ManualUpdatePrecheck::UnknownTarget
      );
      assert_eq!(
         plan_manual_update("0.2.1", true, false, None, true),
         ManualUpdatePrecheck::ForceFetch
      );
      assert_eq!(
         plan_manual_update("0.2.1", false, false, None, false),
         ManualUpdatePrecheck::CheckVersion("0.2.1".to_owned())
      );
      assert_eq!(
         plan_manual_update("0.2.1", true, true, None, false),
         ManualUpdatePrecheck::CheckVersion("0.2.1".to_owned())
      );

      // Under UpdateLock, an already-current target skips replacement (preserving .previous) unless --force.
      assert_eq!(
         decide_locked_manual_update(true, Some("0.2.2"), "0.2.2", false),
         LockedUpdateAction::UpToDate("0.2.2".to_owned())
      );
      assert_eq!(
         decide_locked_manual_update(true, Some("0.2.3"), "0.2.2", false),
         LockedUpdateAction::UpToDate("0.2.3".to_owned())
      );
      assert_eq!(
         decide_locked_manual_update(true, Some("0.2.1"), "0.2.2", false),
         LockedUpdateAction::Apply
      );
      assert_eq!(
         decide_locked_manual_update(true, None, "0.2.2", false),
         LockedUpdateAction::UnknownTarget
      );
      assert_eq!(
         decide_locked_manual_update(true, Some("bad"), "0.2.2", false),
         LockedUpdateAction::UnknownTarget
      );
      assert_eq!(
         decide_locked_manual_update(true, Some("0.2.2"), "0.2.2", true),
         LockedUpdateAction::Apply
      );
      assert_eq!(
         decide_locked_manual_update(false, None, "0.2.2", false),
         LockedUpdateAction::Apply
      );
   }

   #[test]
   fn matches_both_install_paths_for_known_discord_locations() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-match-test", "dir");
      let local = root.join("local");
      let bundled = bundled_discord_exe_in(&local);
      let legacy = legacy_discord_exe_in(&local);
      let custom = root.join("custom").join("Discord.exe");

      // Default resolution matches both standard install locations.
      assert_eq!(
         discord_match_paths(&bundled, &local, false),
         vec![bundled.clone(), legacy.clone()]
      );
      assert_eq!(
         discord_match_paths(&legacy, &local, false),
         vec![legacy.clone(), bundled.clone()]
      );
      assert_eq!(
         discord_match_paths(&custom, &local, false),
         vec![custom.clone()]
      );

      // Explicit target overrides restrict matching to that exact executable only.
      assert_eq!(
         discord_match_paths(&bundled, &local, true),
         vec![bundled]
      );
      assert_eq!(discord_match_paths(&legacy, &local, true), vec![legacy]);
      assert_eq!(discord_match_paths(&custom, &local, true), vec![custom]);
   }

   #[test]
   fn resolves_discord_exe_priority() {
      let root = unique_sibling(&std::env::temp_dir(), "nyaacord-resolve-test", "dir");
      let local = root.join("local");
      let installed = install_home_in(&local).join("app").join("Discord.exe");
      std::fs::create_dir_all(installed.parent().expect("parent")).expect("mkdir installed");
      std::fs::write(&installed, b"installed").expect("write installed");

      let bundled = root.join("bundled-Discord.exe");
      std::fs::write(&bundled, b"bundled").expect("write bundled");

      // Override flag wins first.
      assert_eq!(
         resolve_discord_exe_with(
            Some(PathBuf::from("override.exe")),
            Some(OsString::from("env.exe")),
            Some(bundled.clone()),
            &local
         ),
         PathBuf::from("override.exe")
      );
      // Env var beats bundled and installed.
      assert_eq!(
         resolve_discord_exe_with(
            None,
            Some(OsString::from("env.exe")),
            Some(bundled.clone()),
            &local
         ),
         PathBuf::from("env.exe")
      );
      // Bundled exe wins when present and no overrides exist.
      assert_eq!(
         resolve_discord_exe_with(None, None, Some(bundled), &local),
         root.join("bundled-Discord.exe")
      );
      // Empty env string falls back to installed bundled client.
      assert_eq!(
         resolve_discord_exe_with(None, Some(OsString::new()), None, &local),
         installed
      );

      let _ = std::fs::remove_file(&installed);
      assert_eq!(
         resolve_discord_exe_with(None, None, None, &local),
         local
            .join("DiscordPhoenix")
            .join("app")
            .join("Discord.exe")
      );

      let _ = std::fs::remove_dir_all(&root);
   }
}
