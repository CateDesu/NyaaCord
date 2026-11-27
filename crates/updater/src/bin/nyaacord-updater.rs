//! `nyaacord-updater` CLI for checking, downloading, verifying, rolling back,
//! and relaunching `Nyaacord` `AppImage` releases.

use std::{
   env,
   io::{self, Write},
   path::PathBuf,
   time::{Duration, Instant},
};

use misstep::{OptionExt, ResultExt, bail, ensure};
#[cfg(unix)]
use phoenix_updater::spawn_relaunch_watchdog;
use phoenix_updater::{
   CheckResult, Config, ReleaseInfo, apply_update, check_latest, fetch_latest, rollback,
   verify_package, version,
};
use serde::Serialize;

const USAGE: &str = "\
usage:
  nyaacord-updater check --current <ver> [--repo <repo>] [--api <url>] [--timeout <sec>] [--json]
  nyaacord-updater update --current <ver> --target <path> [--repo <repo>] [--api <url>] [--timeout <sec>] [--force] [--quiet]
  nyaacord-updater rollback --target <path>
  nyaacord-updater verify --file <path> [--expected-size <n>] [--expected-sha256 <hex>]
  nyaacord-updater relaunch --target <path> [--previous <path>] --wait-pid <pid>";

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

fn next_str(
   args: &mut impl Iterator<Item = std::ffi::OsString>,
   flag: &str,
) -> misstep::Result<String> {
   let raw = args
      .next()
      .with_context(|| format!("missing value for {flag}\n{USAGE}"))?;
   raw.into_string()
      .map_err(|_| misstep::Report::msg(format!("{flag} value is not valid UTF-8")))
}

fn next_path(
   args: &mut impl Iterator<Item = std::ffi::OsString>,
   flag: &str,
) -> misstep::Result<PathBuf> {
   args
      .next()
      .map(PathBuf::from)
      .with_context(|| format!("missing value for {flag}\n{USAGE}"))
}

fn build_config(
   repo: Option<String>,
   api: Option<String>,
   timeout_sec: Option<i64>,
) -> misstep::Result<Config> {
   let mut config = Config::from_env()?;
   if let Some(repo) = repo {
      phoenix_updater::github::validate_repo(&repo)?;
      config.repo = repo;
   }
   if let Some(api) = api {
      let _ = phoenix_updater::http::validate_url(&api)?;
      config.api_url = Some(api);
   }
   if let Some(secs) = timeout_sec {
      ensure!(secs > 0, "--timeout must be a positive number of seconds");
      config.stall_timeout_secs = secs;
      config.connect_timeout_ms = secs.saturating_mul(1000);
   }
   Ok(config)
}

fn cmd_check(args: &mut impl Iterator<Item = std::ffi::OsString>) -> misstep::Result<()> {
   let mut current: Option<String> = None;
   let mut repo: Option<String> = None;
   let mut api: Option<String> = None;
   let mut timeout: Option<i64> = None;
   let mut json = false;

   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--current") => current = Some(next_str(args, "--current")?),
         Some("--repo") => repo = Some(next_str(args, "--repo")?),
         Some("--api") => api = Some(next_str(args, "--api")?),
         Some("--timeout") => {
            let text = next_str(args, "--timeout")?;
            timeout = Some(
               text
                  .parse::<i64>()
                  .with_context(|| format!("invalid --timeout {text:?}"))?,
            );
         },
         Some("--json") => json = true,
         Some("-h" | "--help") => {
            println!("{USAGE}");
            return Ok(());
         },
         Some(other) => bail!("unexpected argument {other:?}\n{USAGE}"),
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   let current = current.context(format!("--current is required\n{USAGE}"))?;
   let config = build_config(repo, api, timeout)?;
   let result = check_latest(&current, &config)?;

   if json {
      let payload = match &result {
         CheckResult::UpToDate { current, latest } => CheckJson::UpToDate {
            update_available: false,
            current,
            latest,
         },
         CheckResult::Available(release) => CheckJson::Available {
            update_available: true,
            current: version::normalize(&current),
            release,
         },
      };
      println!("{}", serde_json::to_string(&payload)?);
      return Ok(());
   }

   match result {
      CheckResult::UpToDate { current, latest } => {
         println!("Up to date (current: {current}, latest: {latest})");
      },
      CheckResult::Available(release) => {
         println!(
            "Update available: {} -> {} ({}, {} bytes)",
            version::normalize(&current),
            release.version,
            release.asset_name,
            release.size
         );
         println!("Release: {}", release.release_url);
      },
   }
   Ok(())
}

struct ProgressBar {
   quiet: bool,
   fallback_total: Option<u64>,
   last_report: Instant,
   last_percent: Option<u64>,
   printed: bool,
}

impl ProgressBar {
   fn new(quiet: bool, release_size: u64) -> Self {
      Self {
         quiet,
         fallback_total: (release_size > 0).then_some(release_size),
         last_report: Instant::now(),
         last_percent: None,
         printed: false,
      }
   }

   fn update(&mut self, downloaded: u64, total: Option<u64>) {
      if self.quiet {
         return;
      }
      let known_total = total.filter(|&bytes| bytes > 0).or(self.fallback_total);
      if let Some(total) = known_total {
         let pct = downloaded
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(0)
            .min(100);
         let should_print = pct == 100
            || self
               .last_percent
               .is_none_or(|prev| pct >= prev.saturating_add(5))
            || self.last_report.elapsed() >= Duration::from_millis(250);
         if should_print {
            let _ = write!(
               io::stderr(),
               "\rDownloading: {pct}% ({downloaded}/{total} bytes)"
            );
            let _ = io::stderr().flush();
            self.last_percent = Some(pct);
            self.printed = true;
            self.last_report = Instant::now();
         }
      } else if self.last_report.elapsed() >= Duration::from_millis(250) {
         let _ = write!(io::stderr(), "\rDownloading: {downloaded} bytes");
         let _ = io::stderr().flush();
         self.printed = true;
         self.last_report = Instant::now();
      }
   }
}

fn cmd_update(args: &mut impl Iterator<Item = std::ffi::OsString>) -> misstep::Result<()> {
   let mut current: Option<String> = None;
   let mut target: Option<PathBuf> = None;
   let mut repo: Option<String> = None;
   let mut api: Option<String> = None;
   let mut timeout: Option<i64> = None;
   let mut force = false;
   let mut quiet = false;

   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--current") => current = Some(next_str(args, "--current")?),
         Some("--target") => target = Some(next_path(args, "--target")?),
         Some("--repo") => repo = Some(next_str(args, "--repo")?),
         Some("--api") => api = Some(next_str(args, "--api")?),
         Some("--timeout") => {
            let text = next_str(args, "--timeout")?;
            timeout = Some(
               text
                  .parse::<i64>()
                  .with_context(|| format!("invalid --timeout {text:?}"))?,
            );
         },
         Some("--force") => force = true,
         Some("--quiet") => quiet = true,
         Some("-h" | "--help") => {
            println!("{USAGE}");
            return Ok(());
         },
         Some(other) => bail!("unexpected argument {other:?}\n{USAGE}"),
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   let current = current.context(format!("--current is required\n{USAGE}"))?;
   let target = target.context(format!("--target is required\n{USAGE}"))?;
   let config = build_config(repo, api, timeout)?;

   let release = if force {
      fetch_latest(&config)?
   } else {
      match check_latest(&current, &config)? {
         CheckResult::UpToDate { current, latest } => {
            if !quiet {
               println!("Up to date (current: {current}, latest: {latest})");
            }
            return Ok(());
         },
         CheckResult::Available(info) => info,
      }
   };

   if !quiet {
      eprintln!(
         "Downloading {} ({})...",
         release.asset_name, release.version
      );
   }

   let mut progress = ProgressBar::new(quiet, release.size);
   let outcome = apply_update(&release, &target, &config, &mut |done, total| {
      progress.update(done, total);
   })?;
   if !quiet {
      if progress.printed {
         eprintln!();
      }
      println!(
         "Updated {} to {}",
         outcome.target.display(),
         outcome.version
      );
      if let Some(prev) = outcome.previous {
         println!("Backup saved at {}", prev.display());
      }
   }
   Ok(())
}

fn cmd_rollback(args: &mut impl Iterator<Item = std::ffi::OsString>) -> misstep::Result<()> {
   let mut target: Option<PathBuf> = None;
   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--target") => target = Some(next_path(args, "--target")?),
         Some("-h" | "--help") => {
            println!("{USAGE}");
            return Ok(());
         },
         Some(other) => bail!("unexpected argument {other:?}\n{USAGE}"),
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   let target = target.context(format!("--target is required\n{USAGE}"))?;
   let prev = rollback(&target)?;
   println!("Rolled back {} from {}", target.display(), prev.display());
   Ok(())
}

fn cmd_verify(args: &mut impl Iterator<Item = std::ffi::OsString>) -> misstep::Result<()> {
   let mut file: Option<PathBuf> = None;
   let mut expected_size: Option<u64> = None;
   let mut expected_sha256: Option<String> = None;

   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--file") => file = Some(next_path(args, "--file")?),
         Some("--expected-size") => {
            let text = next_str(args, "--expected-size")?;
            expected_size = Some(
               text
                  .parse::<u64>()
                  .with_context(|| format!("invalid --expected-size {text:?}"))?,
            );
         },
         Some("--expected-sha256") => {
            expected_sha256 = Some(next_str(args, "--expected-sha256")?);
         },
         Some("-h" | "--help") => {
            println!("{USAGE}");
            return Ok(());
         },
         Some(other) => bail!("unexpected argument {other:?}\n{USAGE}"),
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   let file = file.context(format!("--file is required\n{USAGE}"))?;
   verify_package(&file, expected_size, expected_sha256.as_deref())?;
   println!("Verified release package: {}", file.display());
   Ok(())
}

#[cfg(unix)]
fn cmd_relaunch(args: &mut impl Iterator<Item = std::ffi::OsString>) -> misstep::Result<()> {
   let mut target: Option<PathBuf> = None;
   let mut previous: Option<PathBuf> = None;
   let mut wait_pid: Option<u32> = None;

   while let Some(arg) = args.next() {
      match arg.to_str() {
         Some("--target") => target = Some(next_path(args, "--target")?),
         Some("--previous") => previous = Some(next_path(args, "--previous")?),
         Some("--wait-pid") => {
            let text = next_str(args, "--wait-pid")?;
            wait_pid = Some(
               text
                  .parse::<u32>()
                  .with_context(|| format!("invalid --wait-pid {text:?}"))?,
            );
         },
         Some("-h" | "--help") => {
            println!("{USAGE}");
            return Ok(());
         },
         Some(other) => bail!("unexpected argument {other:?}\n{USAGE}"),
         None => bail!("argument is not valid UTF-8\n{USAGE}"),
      }
   }

   let target = target.context(format!("--target is required\n{USAGE}"))?;
   let wait_pid = wait_pid.context(format!("--wait-pid is required\n{USAGE}"))?;
   ensure!(wait_pid > 0, "--wait-pid must be greater than 0");
   spawn_relaunch_watchdog(&target, previous.as_deref(), wait_pid)?;
   println!(
      "Spawned relaunch watchdog for {} waiting on pid {wait_pid}",
      target.display()
   );
   Ok(())
}

#[cfg(not(unix))]
fn cmd_relaunch(_args: &mut impl Iterator<Item = std::ffi::OsString>) -> misstep::Result<()> {
   bail!("relaunch watchdog is only supported on Unix");
}

fn main() -> misstep::Result<()> {
   misstep::install_panic_hook();
   let mut args = env::args_os().skip(1);
   let Some(cmd) = args.next() else {
      bail!("{USAGE}");
   };
   match cmd.to_str() {
      Some("check") => cmd_check(&mut args),
      Some("update") => cmd_update(&mut args),
      Some("rollback") => cmd_rollback(&mut args),
      Some("verify") => cmd_verify(&mut args),
      Some("relaunch") => cmd_relaunch(&mut args),
      Some("-h" | "--help") => {
         println!("{USAGE}");
         Ok(())
      },
      Some(other) => bail!("unknown subcommand {other:?}\n{USAGE}"),
      None => bail!("subcommand is not valid UTF-8\n{USAGE}"),
   }
}
