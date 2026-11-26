//! `NyaaCord-x86_64.exe`, the self-contained Windows launcher, also usable as
//! `phoenix-inject [phoenix.dll] [-- discord args]`.
//!
//! `Discord.exe` resolves its imports from System32 only (`DependentLoadFlags`
//! 0x800), so a DLL planted beside it is never loaded. This starts it
//! suspended and queues `LoadLibraryW(phoenix.dll)` as an APC on its main
//! thread, which runs once the OS loader finishes and before Discord's own
//! code.
//!
//! Without a DLL argument it installs itself and its embedded `phoenix.dll`
//! and `libplm.plugins.so` bundle into `%LOCALAPPDATA%\NyaaCord` with Start
//! Menu and Desktop shortcuts. Discord's own files stay untouched, so its
//! updates never undo `NyaaCord`. `--uninstall` removes the install and
//! shortcuts.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
   ffi::{OsStr, OsString},
   path::{Path, PathBuf},
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

#[cfg(windows)]
mod windows {
   use std::{
      env,
      ffi::{OsStr, OsString, c_void},
      fs::{self, File},
      io::{self, Write},
      os::windows::{ffi::OsStrExt, process::CommandExt},
      path::{Path, PathBuf},
      process::{Command, Stdio},
      ptr,
      sync::atomic::{AtomicU64, Ordering},
      time::{Duration, Instant, SystemTime, UNIX_EPOCH},
   };

   use misstep::{OptionExt, ResultExt, bail};
   use phoenix_updater::{
      CheckResult, Config, ReleaseInfo, apply_update, check_latest, fetch_latest, rollback, sha256,
      version,
   };
   use serde::Serialize;

   use super::{
      Mode, USAGE, VERSION, build_windows_cmdline,
      embedded::{EMBEDDED_DLL, EMBEDDED_PLUGINS},
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

   static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

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

   fn running(exe: &Path) -> Vec<(Handle, u32)> {
      let name = exe.file_name().unwrap_or_default();
      let want = exe.as_os_str().to_string_lossy().to_lowercase();
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
         if entry_text(bytes, 44).eq_ignore_ascii_case(name) {
            // SAFETY: opens by pid, checked for null below.
            let process = unsafe { OpenProcess(PROCESS_ACCESS, 0, pid) };
            if !process.is_null() {
               let mut path = [0_u16; 1024];
               let mut len = u32::try_from(path.len()).unwrap_or(1024);
               // SAFETY: `path` holds `len` UTF-16 units.
               let named = unsafe {
                  QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut len)
               } != 0;
               let path = String::from_utf16_lossy(&path[..len as usize]).to_lowercase();
               if named && path == want {
                  found.push((process, pid));
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

   fn install_home(local: &Path) -> PathBuf {
      local.join("NyaaCord")
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

   fn unique_sibling(dir: &Path, base: &str, tag: &str) -> PathBuf {
      let pid = std::process::id();
      let nanos = SystemTime::now()
         .duration_since(UNIX_EPOCH)
         .map_or(0, |d| d.as_nanos());
      let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
      dir.join(format!("{base}.{pid}.{nanos}.{seq}.{tag}"))
   }

   fn cleanup_old_files(dir: &Path, base: &str) {
      let prefix = format!("{base}.");
      let Ok(entries) = fs::read_dir(dir) else {
         return;
      };
      for entry in entries.flatten() {
         if let Some(name) = entry.file_name().to_str()
            && name.starts_with(&prefix)
            && Path::new(name)
               .extension()
               .is_some_and(|ext| ext.eq_ignore_ascii_case("old"))
         {
            let _ = fs::remove_file(entry.path());
         }
      }
   }

   /// Replaces `dest` with `bytes` atomically, renaming any locked existing file aside.
   fn write_replaceable_file(dest: &Path, bytes: &[u8]) -> misstep::Result<()> {
      let dir = dest
         .parent()
         .context("destination has no parent directory")?;
      let name = dest
         .file_name()
         .and_then(OsStr::to_str)
         .context("destination has no valid file name")?;
      cleanup_old_files(dir, name);

      let tmp = unique_sibling(dir, name, "new");
      {
         let mut file =
            File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
         file
            .write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
         file
            .sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
      }

      if dest.exists() {
         let old = dir.join(format!("{name}.old"));
         let _ = fs::remove_file(&old);
         let moved_old = if fs::rename(dest, &old).is_ok() {
            old
         } else {
            let unique_old = unique_sibling(dir, name, "old");
            fs::rename(dest, &unique_old)
               .with_context(|| format!("moving locked {}", dest.display()))?;
            unique_old
         };
         if let Err(err) = fs::rename(&tmp, dest) {
            let _ = fs::rename(&moved_old, dest);
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("replacing {}", dest.display()))?;
         }
         let _ = fs::remove_file(&moved_old);
      } else if let Err(err) = fs::rename(&tmp, dest) {
         let _ = fs::remove_file(&tmp);
         return Err(err).with_context(|| format!("installing {}", dest.display()))?;
      }
      Ok(())
   }

   /// Syncs the embedded `phoenix.dll`, `libplm.plugins.so`, and `version.txt` inside `dir`.
   fn sync_embedded_assets(dir: &Path) -> misstep::Result<PathBuf> {
      fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
      let dll = dir.join("phoenix.dll");
      let plugins = dir.join("libplm.plugins.so");
      let version_file = dir.join("version.txt");

      if !EMBEDDED_DLL.is_empty() {
         let want_sha = sha256_slice(EMBEDDED_DLL);
         let have_match = dll.is_file() && sha256::sha256_file(&dll).is_ok_and(|s| s == want_sha);
         if !have_match {
            write_replaceable_file(&dll, EMBEDDED_DLL)?;
         }
      }

      if !EMBEDDED_PLUGINS.is_empty() {
         let want_sha = sha256_slice(EMBEDDED_PLUGINS);
         let have_match =
            plugins.is_file() && sha256::sha256_file(&plugins).is_ok_and(|s| s == want_sha);
         if !have_match {
            write_replaceable_file(&plugins, EMBEDDED_PLUGINS)?;
         }
      }

      let want_version = format!("{VERSION}\n");
      if fs::read_to_string(&version_file).ok().as_deref() != Some(want_version.as_str()) {
         let _ = fs::write(&version_file, want_version);
      }

      if !dll.is_file() {
         bail!(
            "phoenix.dll is missing at {}. Build with scripts/build-windows.sh or pass a DLL path.",
            dll.display()
         );
      }
      Ok(dll)
   }

   /// Installs `NyaaCord.exe`, extracts embedded assets, and creates Start Menu and Desktop shortcuts.
   fn install_self(
      home: &Path,
      always_refresh_shortcuts: bool,
   ) -> misstep::Result<(PathBuf, PathBuf)> {
      fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
      let launcher = home.join("NyaaCord.exe");
      let me = env::current_exe().context("resolving current executable")?;
      let copied = !same_file(&me, &launcher);
      if copied {
         let bytes = fs::read(&me).with_context(|| format!("reading {}", me.display()))?;
         let want_sha = sha256_slice(&bytes);
         let same_contents =
            launcher.is_file() && sha256::sha256_file(&launcher).is_ok_and(|s| s == want_sha);
         if !same_contents {
            write_replaceable_file(&launcher, &bytes).context("installing NyaaCord.exe")?;
         }
      }

      let dll = sync_embedded_assets(home)?;
      if copied || always_refresh_shortcuts {
         let _ = shortcuts(&launcher, false);
      }
      Ok((launcher, dll))
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
         let installed = install_home(&local).join("NyaaCord.exe");
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
      let is_installed = local_app_data().is_ok_and(|local| install_home(&local) == dir);
      let stamp = dir.join("version.txt");
      if is_installed || stamp.is_file() {
         let _ = fs::write(stamp, format!("{ver}\n"));
      }
   }

   fn query_exe_version(exe: &Path) -> Option<String> {
      let out = Command::new(exe)
         .arg("--version")
         .env("NYAACORD_AUTO_UPDATE", "0")
         .stdin(Stdio::null())
         .stdout(Stdio::piped())
         .stderr(Stdio::null())
         .creation_flags(NO_WINDOW)
         .output()
         .ok()?;
      if !out.status.success() {
         return None;
      }
      let text = String::from_utf8(out.stdout).ok()?;
      let line = text.lines().next()?.trim();
      let ver = line.strip_prefix("NyaaCord ")?.trim();
      (!ver.is_empty()).then(|| version::normalize(ver))
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
      let release = if force {
         fetch_latest(&cfg)?
      } else {
         match check_latest(VERSION, &cfg)? {
            CheckResult::UpToDate { current, latest } => {
               ui.info(&format!(
                  "Up to date (current: {current}, latest: {latest})"
               ));
               return Ok(());
            },
            CheckResult::Available(info) => info,
         }
      };

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

      let outcome = apply_update(&release, &target, &cfg, &mut |done, total| {
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
      let prev = rollback(&target)?;
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
      let home = install_home(&local);
      let _ = shortcuts(&home.join("NyaaCord.exe"), true);
      match fs::remove_dir_all(&home) {
         Err(err) if err.kind() != io::ErrorKind::NotFound => {
            ui.notice(&format!(
               "Shortcuts removed, but {} could not be fully deleted ({err}). Close Discord and delete it.",
               home.display()
            ));
         },
         _ => ui.notice("NyaaCord has been uninstalled. Discord itself was not modified."),
      }
      Ok(())
   }

   /// Checks for and applies an update before normal launch, rolling back if the new
   /// binary fails its version smoke check or fails to spawn.
   fn maybe_auto_update(
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

      let Ok(CheckResult::Available(release)) = check_latest(VERSION, &cfg) else {
         return false;
      };
      let Ok(outcome) = apply_update(&release, launcher, &cfg, &mut |_, _| {}) else {
         return false;
      };

      // Smoke-test the new executable before handing off.
      let Some(new_ver) = query_exe_version(&outcome.target) else {
         let _ = rollback(&outcome.target);
         return false;
      };
      write_version_beside(&outcome.target, &new_ver);

      let mut cmd = Command::new(&outcome.target);
      cmd.env("NYAACORD_AUTO_UPDATE", "0");
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

      if cmd.spawn().is_ok() {
         true
      } else {
         let _ = rollback(&outcome.target);
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

   fn resolve_discord_exe(override_path: Option<PathBuf>, local: &Path) -> PathBuf {
      if let Some(path) = override_path {
         return path;
      }
      if let Some(env_path) = env::var_os("NYAACORD_DISCORD_EXE").filter(|v| !v.is_empty()) {
         return PathBuf::from(env_path);
      }
      local.join("DiscordPhoenix").join("app").join("Discord.exe")
   }

   fn handle_running_discord(
      exe: &Path,
      app: &Path,
      dll: &Path,
      no_runtime: bool,
      discord_args: &[OsString],
      ui: &Ui,
   ) -> misstep::Result<bool> {
      let open = running(exe);
      if open.is_empty() {
         return Ok(true);
      }

      let dll_name = dll.file_name().unwrap_or_default();
      let with_nyaacord = open.iter().any(|&(_, pid)| has_module(pid, dll_name));
      let restart = if no_runtime {
         ui.ask(
            "Discord is already running.\n\nRestart it without the NyaaCord plugin runtime? \
             Any active call will end.",
         )
      } else {
         !with_nyaacord
            && ui.ask(
               "Discord is already running without NyaaCord.\n\nRestart it with NyaaCord? \
                Any active call will end.",
            )
      };

      for &(process, _) in &open {
         if restart {
            // SAFETY: opened with TERMINATE by `running`.
            unsafe { TerminateProcess(process, 0) };
         }
      }
      for (process, _) in open {
         if restart {
            // SAFETY: opened with SYNCHRONIZE by `running`.
            unsafe { WaitForSingleObject(process, 5000) };
         }
         // SAFETY: opened by `running`, closed once.
         unsafe { CloseHandle(process) };
      }

      if !restart {
         if with_nyaacord {
            Command::new(exe)
               .args(discord_args)
               .current_dir(app)
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

   fn cmd_launch(
      explicit_dll: Option<PathBuf>,
      target_exe: Option<PathBuf>,
      no_runtime: bool,
      discord_args: &[OsString],
      ui: &Ui,
   ) -> misstep::Result<()> {
      let local = local_app_data()?;
      let home = install_home(&local);

      let (managed_exe, dll) = if let Some(path) = explicit_dll {
         (None, path)
      } else {
         let (launcher, dll) = install_self(&home, false)?;
         if maybe_auto_update(
            &launcher,
            target_exe.as_deref(),
            no_runtime,
            discord_args,
            ui.quiet,
         ) {
            return Ok(());
         }
         (Some(launcher), dll)
      };

      let exe = resolve_discord_exe(target_exe, &local);
      if !exe.is_file() {
         bail!(
            "Discord Phoenix is not installed at {}. Install it first, then open NyaaCord again.",
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

      // A second Discord instance hands off to the running one and exits, so NyaaCord
      // would never load into the Discord window the user sees.
      if !handle_running_discord(&exe, &app, &dll, no_runtime, discord_args, ui)? {
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
            let home = install_home(&local);
            let (launcher, dll) = install_self(&home, true)?;
            ui.info(&format!("Installed NyaaCord {VERSION}"));
            ui.info(&format!("Launcher: {}", launcher.display()));
            ui.info(&format!("Loader: {}", dll.display()));
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
}
