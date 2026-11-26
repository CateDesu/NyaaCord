//! GitHub Releases metadata lookup and asset selection.

use std::env;

use serde::{Deserialize, Serialize};

use crate::{Error, http, sha256, version};

pub const DEFAULT_REPO: &str = "CateDesu/NyaaCord";
pub const DEFAULT_CONNECT_TIMEOUT_MS: i64 = 15_000;
pub const DEFAULT_STALL_TIMEOUT_SECS: i64 = 30;
const MAX_API_BYTES: u64 = 4 * 1024 * 1024;
const PREFERRED_APPIMAGE_ASSET: &str = "NyaaCord-x86_64.AppImage";
const PREFERRED_WINDOWS_ASSET: &str = "NyaaCord-x86_64.exe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
   dead_code,
   reason = "other platform variant is used on its target and in tests"
)]
enum TargetPlatform {
   LinuxAppImage,
   WindowsExe,
}

impl TargetPlatform {
   const fn current() -> Self {
      #[cfg(windows)]
      {
         Self::WindowsExe
      }
      #[cfg(not(windows))]
      {
         Self::LinuxAppImage
      }
   }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
   pub repo: String,
   pub api_url: Option<String>,
   pub connect_timeout_ms: i64,
   pub stall_timeout_secs: i64,
}

impl Default for Config {
   fn default() -> Self {
      Self {
         repo: DEFAULT_REPO.to_owned(),
         api_url: None,
         connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
         stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
      }
   }
}

fn resolve_api_base(base: &str, repo: &str) -> Result<String, Error> {
   let trimmed = base.trim();
   let _ = http::validate_url(trimmed)?;
   let without_slash = trimmed.trim_end_matches('/');
   if without_slash.ends_with("/releases/latest")
      || without_slash.ends_with("/latest")
      || std::path::Path::new(without_slash)
         .extension()
         .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
   {
      return Ok(trimmed.to_owned());
   }
   let resolved = format!("{without_slash}/repos/{repo}/releases/latest");
   let _ = http::validate_url(&resolved)?;
   Ok(resolved)
}

fn config_from_values(
   raw_repo: Option<&str>,
   raw_update_api: Option<&str>,
   raw_api_base: Option<&str>,
) -> Result<Config, Error> {
   let repo = match raw_repo {
      Some(val) if !val.trim().is_empty() => {
         let trimmed = val.trim().to_owned();
         validate_repo(&trimmed)?;
         trimmed
      },
      _ => DEFAULT_REPO.to_owned(),
   };

   let api_url = match raw_update_api {
      Some(val) if !val.trim().is_empty() => {
         let trimmed = val.trim().to_owned();
         let _ = http::validate_url(&trimmed)?;
         Some(trimmed)
      },
      _ => match raw_api_base {
         Some(val) if !val.trim().is_empty() => Some(resolve_api_base(val, &repo)?),
         _ => None,
      },
   };

   Ok(Config {
      repo,
      api_url,
      connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
      stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
   })
}

impl Config {
   /// Reads `NYAACORD_REPO`, `NYAACORD_UPDATE_API`, and `NYAACORD_API_BASE` from the environment.
   pub fn from_env() -> Result<Self, Error> {
      let raw_repo = env::var("NYAACORD_REPO").ok();
      let raw_update_api = env::var("NYAACORD_UPDATE_API").ok();
      let raw_api_base = env::var("NYAACORD_API_BASE").ok();
      config_from_values(
         raw_repo.as_deref(),
         raw_update_api.as_deref(),
         raw_api_base.as_deref(),
      )
   }
}

/// Validates a `owner/name` repository identifier.
pub fn validate_repo(repo: &str) -> Result<(), Error> {
   let Some((owner, name)) = repo.split_once('/') else {
      return Err(Error::InvalidRepo {
         repo: repo.to_owned(),
      });
   };
   let valid_part = |part: &str| {
      !part.is_empty()
         && part != "."
         && part != ".."
         && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
   };
   if !valid_part(owner) || !valid_part(name) {
      return Err(Error::InvalidRepo {
         repo: repo.to_owned(),
      });
   }
   Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
   pub version: String,
   pub tag_name: String,
   pub asset_name: String,
   pub download_url: String,
   pub size: u64,
   pub sha256: String,
   pub release_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
   UpToDate { current: String, latest: String },
   Available(ReleaseInfo),
}

#[derive(Deserialize)]
struct GitHubRelease {
   tag_name: String,
   #[serde(default)]
   draft: bool,
   #[serde(default)]
   html_url: String,
   #[serde(default)]
   assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
   name: String,
   browser_download_url: String,
   #[serde(default)]
   size: u64,
   #[serde(default)]
   digest: Option<String>,
}

fn fetch_release_json(config: &Config) -> Result<(GitHubRelease, http::UrlScheme), Error> {
   let (url, scheme) = if let Some(override_url) = &config.api_url {
      let scheme = http::validate_url(override_url)?;
      (override_url.clone(), scheme)
   } else {
      validate_repo(&config.repo)?;
      (
         format!(
            "https://api.github.com/repos/{}/releases/latest",
            config.repo
         ),
         http::UrlScheme::Https,
      )
   };

   let headers = [
      ("Accept", "application/vnd.github+json"),
      ("X-GitHub-Api-Version", "2022-11-28"),
   ];
   let body = http::get_bytes(
      &url,
      &headers,
      config.connect_timeout_ms,
      config.stall_timeout_secs,
      MAX_API_BYTES,
   )?;
   let release: GitHubRelease =
      serde_json::from_slice(&body).map_err(|err| Error::Json(err.to_string()))?;
   if release.draft {
      return Err(Error::DraftRelease {
         tag: release.tag_name,
      });
   }
   Ok((release, scheme))
}

fn select_asset_for(
   release: &GitHubRelease,
   platform: TargetPlatform,
) -> Result<&GitHubAsset, Error> {
   match platform {
      TargetPlatform::LinuxAppImage => select_appimage_asset(release),
      TargetPlatform::WindowsExe => select_windows_asset(release),
   }
}

fn select_appimage_asset(release: &GitHubRelease) -> Result<&GitHubAsset, Error> {
   if let Some(exact) = release
      .assets
      .iter()
      .find(|a| a.name == PREFERRED_APPIMAGE_ASSET)
   {
      return Ok(exact);
   }

   let appimages: Vec<&GitHubAsset> = release
      .assets
      .iter()
      .filter(|a| a.name.ends_with(".AppImage"))
      .collect();

   if let Some(arch) = appimages.iter().copied().find(|a| {
      let lower = a.name.to_ascii_lowercase();
      lower.contains("x86_64") || lower.contains("amd64")
   }) {
      return Ok(arch);
   }

   if appimages.len() == 1 {
      return Ok(appimages[0]);
   }

   Err(Error::NoAppImageAsset {
      tag: release.tag_name.clone(),
   })
}

fn select_windows_asset(release: &GitHubRelease) -> Result<&GitHubAsset, Error> {
   if let Some(exact) = release
      .assets
      .iter()
      .find(|a| a.name.eq_ignore_ascii_case(PREFERRED_WINDOWS_ASSET))
   {
      return Ok(exact);
   }

   let exes: Vec<&GitHubAsset> = release
      .assets
      .iter()
      .filter(|a| a.name.len() > 4 && a.name[a.name.len() - 4..].eq_ignore_ascii_case(".exe"))
      .collect();

   if let Some(arch) = exes.iter().copied().find(|a| {
      let lower = a.name.to_ascii_lowercase();
      lower.contains("x86_64") || lower.contains("amd64") || lower.contains("x64")
   }) {
      return Ok(arch);
   }

   if exes.len() == 1 {
      return Ok(exes[0]);
   }

   Err(Error::NoWindowsAsset {
      tag: release.tag_name.clone(),
   })
}

fn parse_asset_digest(digest: &str) -> Option<String> {
   let trimmed = digest.trim();
   if trimmed.len() <= 7 || !trimmed[..7].eq_ignore_ascii_case("sha256:") {
      return None;
   }
   sha256::normalize_hex(&trimmed[7..])
}

fn parse_sha256sums(contents: &str, asset_name: &str) -> Option<String> {
   for raw_line in contents.lines() {
      let line = raw_line.trim();
      if line.is_empty() || line.starts_with('#') {
         continue;
      }
      let Some(sep) = line.find(|c: char| c.is_ascii_whitespace()) else {
         continue;
      };
      let hash_part = &line[..sep];
      let Some(hash) = sha256::normalize_hex(hash_part) else {
         continue;
      };
      let rest = line[sep..].trim_start();
      let filename = rest.strip_prefix('*').unwrap_or(rest);
      let filename = filename.strip_prefix("./").unwrap_or(filename);
      if filename == asset_name {
         return Some(hash);
      }
   }
   None
}

fn parse_single_sha256_file(contents: &str, asset_name: &str) -> Option<String> {
   if let Some(hash) = parse_sha256sums(contents, asset_name) {
      return Some(hash);
   }
   let mut lines = contents
      .lines()
      .map(str::trim)
      .filter(|line| !line.is_empty() && !line.starts_with('#'));
   let first = lines.next()?;
   if lines.next().is_some() {
      return None;
   }
   sha256::normalize_hex(first)
}

fn validate_release_url(url: &str, api_scheme: http::UrlScheme) -> Result<(), Error> {
   let scheme = http::validate_url(url)?;
   if api_scheme == http::UrlScheme::Https && scheme != http::UrlScheme::Https {
      return Err(Error::InvalidUrl {
         url: url.to_owned(),
         reason: "release URL must use https".to_owned(),
      });
   }
   Ok(())
}

fn resolve_sha256(
   release: &GitHubRelease,
   asset: &GitHubAsset,
   api_scheme: http::UrlScheme,
   config: &Config,
) -> Result<String, Error> {
   if let Some(digest) = asset.digest.as_deref().and_then(parse_asset_digest) {
      return Ok(digest);
   }

   let sums_asset = release
      .assets
      .iter()
      .find(|a| a.name == "SHA256SUMS.txt")
      .or_else(|| release.assets.iter().find(|a| a.name == "SHA256SUMS"));

   if let Some(sums) = sums_asset {
      validate_release_url(&sums.browser_download_url, api_scheme)?;
      let bytes = http::get_bytes(
         &sums.browser_download_url,
         &[],
         config.connect_timeout_ms,
         config.stall_timeout_secs,
         MAX_API_BYTES,
      )?;
      if let Ok(text) = std::str::from_utf8(&bytes)
         && let Some(hash) = parse_sha256sums(text, &asset.name)
      {
         return Ok(hash);
      }
   }

   let sidecar_name = format!("{}.sha256", asset.name);
   if let Some(sidecar) = release
      .assets
      .iter()
      .find(|a| a.name.eq_ignore_ascii_case(&sidecar_name))
   {
      validate_release_url(&sidecar.browser_download_url, api_scheme)?;
      let bytes = http::get_bytes(
         &sidecar.browser_download_url,
         &[],
         config.connect_timeout_ms,
         config.stall_timeout_secs,
         MAX_API_BYTES,
      )?;
      if let Ok(text) = std::str::from_utf8(&bytes)
         && let Some(hash) = parse_single_sha256_file(text, &asset.name)
      {
         return Ok(hash);
      }
   }

   Err(Error::MissingChecksum {
      asset: asset.name.clone(),
   })
}

fn build_release_info(
   release: &GitHubRelease,
   api_scheme: http::UrlScheme,
   config: &Config,
) -> Result<ReleaseInfo, Error> {
   if release.tag_name.trim().is_empty() {
      return Err(Error::Json("release tag_name is empty".to_owned()));
   }
   let asset = select_asset_for(release, TargetPlatform::current())?;
   validate_release_url(&asset.browser_download_url, api_scheme)?;
   validate_release_url(&release.html_url, api_scheme)?;
   let sha256 = resolve_sha256(release, asset, api_scheme, config)?;
   let version = version::normalize(&release.tag_name);

   Ok(ReleaseInfo {
      version,
      tag_name: release.tag_name.clone(),
      asset_name: asset.name.clone(),
      download_url: asset.browser_download_url.clone(),
      size: asset.size,
      sha256,
      release_url: release.html_url.clone(),
   })
}

/// Checks the latest release against `current_version`.
pub fn check_latest(current_version: &str, config: &Config) -> Result<CheckResult, Error> {
   let (release, api_scheme) = fetch_release_json(config)?;
   if !version::is_newer(&release.tag_name, current_version) {
      return Ok(CheckResult::UpToDate {
         current: version::normalize(current_version),
         latest: version::normalize(&release.tag_name),
      });
   }
   let info = build_release_info(&release, api_scheme, config)?;
   Ok(CheckResult::Available(info))
}

/// Fetches the latest release regardless of current version, for forced updates.
pub fn fetch_latest(config: &Config) -> Result<ReleaseInfo, Error> {
   let (release, api_scheme) = fetch_release_json(config)?;
   build_release_info(&release, api_scheme, config)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn validates_repo_names() {
      assert!(validate_repo("CateDesu/NyaaCord").is_ok());
      assert!(validate_repo("a-b_c.1/d-e_f.2").is_ok());
      for bad in [
         "",
         "CateDesu",
         "a/b/c",
         "/name",
         "owner/",
         "../evil",
         "owner/..",
         "owner/name?foo=1",
         "owner/name with space",
      ] {
         assert!(
            validate_repo(bad).is_err(),
            "expected {bad:?} to be invalid"
         );
      }
   }

   #[test]
   fn parses_sha256sums_formats() {
      let hash = "a".repeat(64);
      let upper = "B".repeat(64);
      let text =
         format!("# comment\n{hash}  NyaaCord-x86_64.AppImage\n{upper} *./other.AppImage\n");
      assert_eq!(
         parse_sha256sums(&text, "NyaaCord-x86_64.AppImage"),
         Some(hash)
      );
      assert_eq!(
         parse_sha256sums(&text, "other.AppImage"),
         Some("b".repeat(64))
      );
      assert_eq!(parse_sha256sums(&text, "missing.AppImage"), None);
   }

   #[test]
   fn selects_preferred_and_fallback_assets() {
      let mk = |name: &str| GitHubAsset {
         name: name.to_owned(),
         browser_download_url: "https://example.com/a".to_owned(),
         size: 1,
         digest: None,
      };

      let release = GitHubRelease {
         tag_name: "v0.2.0".to_owned(),
         draft: false,
         html_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
         assets: vec![mk("Other-amd64.AppImage"), mk("NyaaCord-x86_64.AppImage")],
      };
      assert_eq!(
         select_appimage_asset(&release).expect("selected").name,
         "NyaaCord-x86_64.AppImage"
      );

      let fallback = GitHubRelease {
         tag_name: "v0.2.0".to_owned(),
         draft: false,
         html_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
         assets: vec![
            mk("NyaaCord-aarch64.AppImage"),
            mk("NyaaCord-amd64.AppImage"),
         ],
      };
      assert_eq!(
         select_appimage_asset(&fallback).expect("selected").name,
         "NyaaCord-amd64.AppImage"
      );

      let single = GitHubRelease {
         tag_name: "v0.2.0".to_owned(),
         draft: false,
         html_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
         assets: vec![mk("NyaaCord.AppImage"), mk("SHA256SUMS.txt")],
      };
      assert_eq!(
         select_appimage_asset(&single).expect("selected").name,
         "NyaaCord.AppImage"
      );

      let ambiguous = GitHubRelease {
         tag_name: "v0.2.0".to_owned(),
         draft: false,
         html_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
         assets: vec![mk("A.AppImage"), mk("B.AppImage")],
      };
      assert!(select_appimage_asset(&ambiguous).is_err());
   }

   #[test]
   fn https_api_rejects_loopback_http_release_urls() {
      assert!(
         validate_release_url(
            "https://github.com/CateDesu/NyaaCord/releases/download/v0.2.0/NyaaCord.AppImage",
            http::UrlScheme::Https
         )
         .is_ok()
      );
      assert!(
         validate_release_url(
            "http://127.0.0.1:8080/evil.AppImage",
            http::UrlScheme::Https
         )
         .is_err()
      );
      assert!(
         validate_release_url(
            "http://127.0.0.1:8080/local.AppImage",
            http::UrlScheme::LoopbackHttp
         )
         .is_ok()
      );
   }

   #[test]
   fn selects_windows_exe_assets() {
      let mk = |name: &str| GitHubAsset {
         name: name.to_owned(),
         browser_download_url: "https://example.com/a".to_owned(),
         size: 1,
         digest: None,
      };

      let release = GitHubRelease {
         tag_name: "v0.2.0".to_owned(),
         draft: false,
         html_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
         assets: vec![
            mk("NyaaCord-x86_64.AppImage"),
            mk("NyaaCord-x86_64.exe"),
            mk("SHA256SUMS.txt"),
         ],
      };
      assert_eq!(
         select_asset_for(&release, TargetPlatform::WindowsExe)
            .expect("selected")
            .name,
         "NyaaCord-x86_64.exe"
      );

      let fallback = GitHubRelease {
         tag_name: "v0.2.0".to_owned(),
         draft: false,
         html_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
         assets: vec![mk("NyaaCord-arm64.exe"), mk("NyaaCord-win-x64.exe")],
      };
      assert_eq!(
         select_asset_for(&fallback, TargetPlatform::WindowsExe)
            .expect("selected")
            .name,
         "NyaaCord-win-x64.exe"
      );

      let missing = GitHubRelease {
         tag_name: "v0.2.0".to_owned(),
         draft: false,
         html_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
         assets: vec![mk("NyaaCord-x86_64.AppImage")],
      };
      assert!(matches!(
         select_asset_for(&missing, TargetPlatform::WindowsExe),
         Err(Error::NoWindowsAsset { .. })
      ));
   }

   #[test]
   fn resolves_api_base_and_sidecar_checksums() {
      let cfg = config_from_values(
         Some("CateDesu/NyaaCord"),
         None,
         Some("http://127.0.0.1:8080"),
      )
      .expect("loopback api base");
      assert_eq!(
         cfg.api_url.as_deref(),
         Some("http://127.0.0.1:8080/repos/CateDesu/NyaaCord/releases/latest")
      );

      let cfg_direct = config_from_values(
         Some("CateDesu/NyaaCord"),
         None,
         Some("http://127.0.0.1:8080/latest"),
      )
      .expect("direct api base");
      assert_eq!(
         cfg_direct.api_url.as_deref(),
         Some("http://127.0.0.1:8080/latest")
      );

      assert!(config_from_values(None, None, Some("http://example.com")).is_err());

      let hash = "c".repeat(64);
      assert_eq!(
         parse_single_sha256_file(&format!("{hash}\n"), "NyaaCord-x86_64.exe"),
         Some(hash.clone())
      );
      assert_eq!(
         parse_single_sha256_file(
            &format!("{hash}  NyaaCord-x86_64.exe\n"),
            "NyaaCord-x86_64.exe"
         ),
         Some(hash)
      );
   }
}
