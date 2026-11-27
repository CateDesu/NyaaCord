//! Tells the user when the site ships a newer loader or plugin bundles that
//! differ from the installed ones. The site publishes both in `version.json`
//! (`web/tools/bundle-plugins.ts`).
//!
//! Plugin updates download each changed `plugins/<name>.js`, verify its CRC-32
//! against `version.json` and hot-reload it. A loader update is the platform's
//! ([`crate::Platform::update_loader`]).

use std::{
   collections::BTreeMap,
   sync::{
      Mutex, PoisonError,
      atomic::{AtomicBool, Ordering},
   },
   thread,
};

use crate::Notice;
/// This build's version, `<UTC commit date>-<short hash>` (`build.rs`).
pub const VERSION: &str = env!("PHOENIX_VERSION");
/// This application build's release version (`NYAACORD_VERSION` in `build.rs`).
pub const APP_VERSION: &str = env!("NYAACORD_VERSION");
pub const SITE: &str = "https://sunbird.tools/";
pub const ASSETS: &str = "https://assets.sunbird.tools/";

/// Where `bundle-plugins.ts` reads the version from the loader the site ships.
#[used]
static MARKER: &str = concat!("phoenix-version:", env!("PHOENIX_VERSION"), ";");

#[derive(serde::Deserialize)]
struct Site {
   loader: String,
   plugins: BTreeMap<String, u32>,
}

/// Whole-package release metadata provided by a platform updater.
#[derive(Clone, Debug)]
pub struct AppRelease {
   pub version: String,
   pub tag_name: String,
   pub asset_name: String,
   pub download_url: String,
   pub size: u64,
   pub sha256: String,
   pub release_url: String,
}

/// What the site or release channel has that this install lacks.
#[derive(Clone)]
pub struct Update {
   /// The newer build or release version.
   pub loader: Option<String>,
   /// Installed plugins whose code differs from the site's.
   pub plugins: Vec<String>,
   /// Expected CRC-32 for each plugin in [`Self::plugins`].
   pub plugin_crcs: BTreeMap<String, u32>,
   /// Full app release metadata when checked via [`crate::Platform::check_app_update`].
   pub release: Option<AppRelease>,
}

/// A plugin the site offers, as `plugins/index.json` lists it.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub struct Listed {
   pub name: String,
   pub description: String,
   #[serde(default)]
   pub warning: Option<String>,
}

/// The site's plugin list, fetched when the install page first opens.
#[derive(Clone)]
pub enum Catalog {
   Unfetched,
   Loading,
   Loaded(Vec<Listed>),
   Failed(String),
}

static CATALOG: Mutex<Catalog> = Mutex::new(Catalog::Unfetched);
static FOUND: Mutex<Option<Update>> = Mutex::new(None);
static STATUS: Mutex<Option<String>> = Mutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);

/// The installed version reported by the active platform.
#[must_use]
pub fn app_version() -> &'static str {
   crate::platform().map_or(VERSION, <dyn crate::Platform>::app_version)
}

/// Clears the cached pending update and redraws settings.
pub fn clear_found() {
   *FOUND.lock().unwrap_or_else(PoisonError::into_inner) = None;
   crate::refresh_settings();
}

/// The update the last check found, if any.
pub fn found() -> Option<Update> {
   FOUND.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Human-readable status of the current or last update attempt, if any.
pub fn status() -> Option<String> {
   STATUS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .clone()
}

/// Whether an update download/install is currently running.
pub fn busy() -> bool {
   BUSY.load(Ordering::Acquire)
}

struct BusyGuard;

impl Drop for BusyGuard {
   fn drop(&mut self) {
      if BUSY.swap(false, Ordering::AcqRel) {
         crate::refresh_settings();
      }
   }
}

pub fn set_status(message: Option<String>) {
   *STATUS.lock().unwrap_or_else(PoisonError::into_inner) = message;
   crate::refresh_settings();
}

pub fn catalog() -> Catalog {
   CATALOG
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .clone()
}

/// Fetches the site's plugin list in the background, unless it is loaded or
/// on its way. A failed fetch is retried on the next call.
pub fn fetch_catalog() {
   {
      let mut catalog = CATALOG.lock().unwrap_or_else(PoisonError::into_inner);
      if matches!(*catalog, Catalog::Loading | Catalog::Loaded(_)) {
         return;
      }
      *catalog = Catalog::Loading;
   }
   thread::spawn(|| {
      let fetched = crate::fetch::get(&format!("{ASSETS}plugins/index.json"))
         .map_err(|err| err.to_string())
         .and_then(|body| {
            serde_json::from_slice::<Vec<Listed>>(&body).map_err(|err| err.to_string())
         });
      *CATALOG.lock().unwrap_or_else(PoisonError::into_inner) = match fetched {
         Ok(listed) => Catalog::Loaded(listed),
         Err(err) => {
            log::info!("plugin list: {err}");
            Catalog::Failed(err)
         },
      };
      crate::refresh_settings();
   });
}

/// Installs a plugin the site lists. Site plugins can call API an older loader
/// lacks, so platforms with an in-app loader updater wait until the loader is
/// current.
pub fn install_listed(name: &str) -> Result<String, crate::Error> {
   let updates_loader = crate::platform().is_some_and(<dyn crate::Platform>::updates_loader);
   if updates_loader
      && let Some(update) = found()
      && update.release.is_none()
      && let Some(version) = update.loader
   {
      return Err(crate::Error::LoaderOutdated { version });
   }
   crate::install(&format!("{ASSETS}plugins/{name}.js"))
}

/// Asks the platform or site what this install lacks, and remembers it for
/// [`found`]. `None` means up to date.
fn query() -> Result<Option<Update>, String> {
   let update =
      if let Some(res) = crate::platform().and_then(<dyn crate::Platform>::check_app_update) {
         res?.map(|rel| Update {
            loader: Some(rel.version.clone()),
            plugins: Vec::new(),
            plugin_crcs: BTreeMap::new(),
            release: Some(rel),
         })
      } else {
         let body =
            crate::fetch::get(&format!("{ASSETS}version.json")).map_err(|err| err.to_string())?;
         let site =
            serde_json::from_slice::<Site>(&body).map_err(|err| format!("version.json: {err}"))?;
         let loader = (site.loader.as_str() > VERSION).then_some(site.loader);
         // Plugins come along with a loader update, so one tap brings both.
         let mut plugins = Vec::new();
         let mut plugin_crcs = BTreeMap::new();
         for plugin in crate::plugins() {
            if let Some(&crc) = site.plugins.get(&plugin.name)
               && crate::source(&plugin.name)
                  .is_some_and(|source| crc32fast::hash(source.as_bytes()) != crc)
            {
               plugin_crcs.insert(plugin.name.clone(), crc);
               plugins.push(plugin.name);
            }
         }
         (loader.is_some() || !plugins.is_empty()).then_some(Update {
            loader,
            plugins,
            plugin_crcs,
            release: None,
         })
      };
   FOUND
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .clone_from(&update);
   crate::refresh_settings();
   Ok(update)
}

/// Asks the site in the background. Failures (offline) are only logged.
pub fn check() {
   if !begin() {
      log::info!("update check: already running");
      return;
   }
   thread::spawn(|| {
      let guard = BusyGuard;
      let update = match query() {
         Ok(Some(update)) => update,
         Ok(None) => {
            let cur = app_version();
            log::info!("up to date ({cur})");
            return;
         },
         Err(err) => {
            log::info!("update check: {err}");
            return;
         },
      };
      log::info!(
         "update available: loader {:?}, plugins {:?}",
         update.loader,
         update.plugins
      );
      drop(guard);
      if let Some(platform) = crate::platform() {
         platform.notify(Notice::UpdateAvailable(&update));
      }
   });
}

/// Checks the site on demand for a user click, updating status and showing a
/// toast on completion.
pub fn check_now() {
   if !begin() {
      log::info!("update check: already running");
      crate::toast("Update check already in progress...");
      return;
   }
   set_status(Some("Checking for updates...".to_owned()));
   thread::spawn(|| {
      let _guard = BusyGuard;
      match query() {
         Ok(Some(update)) => {
            log::info!(
               "update available: loader {:?}, plugins {:?}",
               update.loader,
               update.plugins
            );
            finish(None);
            if let Some(platform) = crate::platform() {
               platform.notify(Notice::UpdateAvailable(&update));
            }
         },
         Ok(None) => {
            let cur = app_version();
            log::info!("up to date ({cur})");
            finish(Some(format!("Nyaacord v{cur} is up to date.")));
            crate::toast("Nyaacord is up to date");
         },
         Err(err) => {
            log::info!("update check: {err}");
            let msg = format!("Update check failed: {err}");
            finish(Some(msg.clone()));
            crate::toast(&msg);
         },
      }
   });
}

/// Checks for updates again and applies what this install lacks.
///
/// When a loader or package update is available and the platform supports
/// [`crate::Platform::updates_loader`], hands off to
/// [`crate::Platform::update_loader`], which must call [`finish`] on every exit
/// path so the busy slot is released.
pub fn apply() {
   if !begin() {
      log::info!("update apply: already running");
      crate::toast("Update already in progress...");
      return;
   }
   set_status(Some("Checking for updates...".to_owned()));
   thread::spawn(|| {
      let _guard = BusyGuard;
      // The last check ran when Discord started, and a release may have shipped
      // since, so what it found can be out of date by the time this runs.
      let update = match query() {
         Ok(update) => update,
         Err(err) => {
            if let Some(cached) = found() {
               log::info!("update apply: {err}, using the last check");
               Some(cached)
            } else {
               log::info!("update apply: {err}");
               let msg = format!("Update check failed: {err}");
               finish(Some(msg.clone()));
               crate::toast(&msg);
               return;
            }
         },
      };
      let Some(update) = update else {
         let cur = app_version();
         log::info!("update apply: up to date ({cur})");
         finish(Some(format!("Nyaacord v{cur} is up to date.")));
         crate::toast("Nyaacord is up to date");
         return;
      };
      // The site's plugins are built against its loader and can call API this
      // one lacks, so with a loader update they wait in `plugins.next/` for it.
      if let Some(version) = &update.loader
         && let Some(platform) = crate::platform()
      {
         if platform.updates_loader() {
            platform.update_loader(version, &update);
            return;
         }
         if update.plugins.is_empty() {
            let msg = if update.release.is_some() {
               format!("Nyaacord v{version} is available.")
            } else {
               format!("Upstream build {version} is available.")
            };
            finish(Some(msg.clone()));
            crate::toast(&msg);
            return;
         }
      }
      let mut updated_plugins = Vec::new();
      let mut failed = Vec::new();
      if !update.plugins.is_empty() {
         set_status(Some(format!(
            "Updating {} plugin{}...",
            update.plugins.len(),
            if update.plugins.len() == 1 { "" } else { "s" }
         )));
         for name in &update.plugins {
            let expected_crc = update.plugin_crcs.get(name).copied();
            match update_plugin(name, expected_crc) {
               Ok(()) => {
                  log::info!("ota updated plugin {name}");
                  updated_plugins.push(name.clone());
               },
               Err(err) => {
                  log::error!("ota update {name} failed: {err}");
                  failed.push(format!("{name}: {err}"));
               },
            }
         }
         // Drop successfully updated plugins from FOUND.
         {
            let mut guard = FOUND.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(pending) = guard.as_mut() {
               pending
                  .plugins
                  .retain(|name| !updated_plugins.contains(name));
               pending
                  .plugin_crcs
                  .retain(|name, _| !updated_plugins.contains(name));
               if pending.loader.is_none() && pending.plugins.is_empty() {
                  *guard = None;
               }
            }
         }
      }

      // A failed plugin used to go unreported whenever the loader update below
      // still had to run, since only `update.loader.is_none()` printed it.
      // The slot is released either way, and the failure keeps its line.
      if failed.is_empty() {
         finish(None);
      } else {
         let msg = format!("Plugin update failed: {}", failed.join("; "));
         log::error!("{msg}");
         finish(Some(msg.clone()));
         crate::toast(&msg);
      }
      if !updated_plugins.is_empty()
         && let Some(platform) = crate::platform()
      {
         platform.notify(Notice::PluginsUpdated(&updated_plugins));
      }
   });
}

fn update_plugin(name: &str, expected_crc: Option<u32>) -> Result<(), String> {
   let source = download_plugin(name, expected_crc)?;
   crate::save_source(name, source).map_err(|err| err.to_string())
}

/// The site's `plugins/<name>.js`, checked against its CRC-32.
pub fn download_plugin(name: &str, expected_crc: Option<u32>) -> Result<String, String> {
   let url = format!("{ASSETS}plugins/{name}.js");
   let bytes = crate::fetch::get(&url).map_err(|err| format!("fetch: {err}"))?;
   if let Some(expected) = expected_crc {
      let actual = crc32fast::hash(&bytes);
      if actual != expected {
         return Err(format!(
            "CRC mismatch (got {actual:#x}, expected {expected:#x})"
         ));
      }
   }
   String::from_utf8(bytes).map_err(|err| format!("invalid UTF-8: {err}"))
}

/// Claims the update slot. False when an update already runs.
pub fn begin() -> bool {
   !BUSY.swap(true, Ordering::AcqRel)
}

/// Releases the update slot with `status` shown.
///
/// Writes `STATUS` before releasing `BUSY` so any reader observing `busy() == false`
/// sees the final status line immediately.
pub fn finish(status: Option<String>) {
   *STATUS.lock().unwrap_or_else(PoisonError::into_inner) = status;
   BUSY.store(false, Ordering::Release);
   crate::refresh_settings();
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn finish_publishes_status_before_releasing_busy() {
      assert!(begin());
      assert!(busy());
      assert!(!begin());
      finish(Some("done".to_owned()));
      assert!(!busy());
      assert_eq!(status().as_deref(), Some("done"));
      finish(None);
      assert_eq!(status(), None);
   }
}
