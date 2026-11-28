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
      Mutex, MutexGuard, PoisonError,
      atomic::{AtomicU64, Ordering},
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
static PROGRESS: Mutex<Progress> = Mutex::new(Progress::Idle);
static NEXT_SLOT: AtomicU64 = AtomicU64::new(1);

/// What the single update slot is doing.
enum Progress {
   Idle,
   Running { id: u64, status: Option<String> },
   Finished(String),
}

fn progress() -> MutexGuard<'static, Progress> {
   PROGRESS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Internal claim on the update slot. Dropping it releases a still-running
/// slot with the same id to `Idle`, or publishes the line left by
/// [`Slot::finish`], while leaving any finalized or newer slot intact.
struct Slot {
   id: u64,
   result: Option<String>,
}

impl Slot {
   fn claim(initial: Option<String>) -> Option<Self> {
      let mut state = progress();
      if matches!(*state, Progress::Running { .. }) {
         return None;
      }
      let id = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
      let refresh = initial.is_some();
      *state = Progress::Running {
         id,
         status: initial,
      };
      drop(state);
      if refresh {
         crate::refresh_settings();
      }
      Some(Self { id, result: None })
   }

   fn step(&self, step: impl Into<String>) {
      let mut state = progress();
      if !matches!(*state, Progress::Running { id, .. } if id == self.id) {
         return;
      }
      *state = Progress::Running {
         id: self.id,
         status: Some(step.into()),
      };
      drop(state);
      crate::refresh_settings();
   }

   fn finish(mut self, result: Option<String>) {
      self.result = result;
   }
}

impl Drop for Slot {
   fn drop(&mut self) {
      let mut state = progress();
      let owned = matches!(*state, Progress::Running { id, .. } if id == self.id);
      let changed = match self.result.take() {
         Some(line) if owned => {
            *state = Progress::Finished(line);
            true
         },
         None if owned => {
            *state = Progress::Idle;
            true
         },
         _ => false,
      };
      drop(state);
      if changed {
         crate::refresh_settings();
      }
   }
}

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

/// The step the running update shows, or the line the last one left.
#[must_use]
pub fn status() -> Option<String> {
   match &*progress() {
      Progress::Idle => None,
      Progress::Running { status, .. } => status.clone(),
      Progress::Finished(line) => Some(line.clone()),
   }
}

/// Whether an update holds the slot.
#[must_use]
pub fn busy() -> bool {
   matches!(*progress(), Progress::Running { .. })
}

/// Reads `busy` and `status` together under one lock.
#[must_use]
pub fn snapshot() -> (bool, Option<String>) {
   match &*progress() {
      Progress::Idle => (false, None),
      Progress::Running { status, .. } => (true, status.clone()),
      Progress::Finished(line) => (false, Some(line.clone())),
   }
}

pub fn set_status(message: Option<String>) {
   let mut state = progress();
   *state = match (&*state, message) {
      (&Progress::Running { id, .. }, status) => Progress::Running { id, status },
      (_, Some(msg)) => Progress::Finished(msg),
      (_, None) => Progress::Idle,
   };
   drop(state);
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

/// Whether a bundled plugin source already matches or is at least as new as
/// a remote catalog or OTA copy.
#[must_use]
fn bundled_supersedes(bundled_source: &str, remote_source: &str) -> bool {
   if crate::same_plugin_source(bundled_source, remote_source) {
      return true;
   }
   let bundled = crate::Header::from(bundled_source);
   let remote = crate::Header::from(remote_source);
   match (bundled.version.as_deref(), remote.version.as_deref()) {
      (Some(bundled_ver), Some(remote_ver)) => matches!(
         crate::compare_versions(bundled_ver, remote_ver),
         Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
      ),
      _ => false,
   }
}

/// Whether the active local plugin copy already matches or is strictly newer
/// than a remote catalog or OTA copy.
#[must_use]
fn active_supersedes(active_source: &str, remote_source: &str) -> bool {
   if crate::same_plugin_source(active_source, remote_source) {
      return true;
   }
   let active = crate::Header::from(active_source);
   let remote = crate::Header::from(remote_source);
   match (active.version.as_deref(), remote.version.as_deref()) {
      (Some(active_ver), Some(remote_ver)) => {
         crate::compare_versions(active_ver, remote_ver) == Some(std::cmp::Ordering::Greater)
      },
      _ => false,
   }
}

/// Persists a catalog or OTA plugin unless a bundled or active local copy
/// already supersedes it. Returns `true` when the remote copy was saved.
fn install_catalog_source(
   name: &str,
   source: String,
   current: Option<&str>,
   bundled: Option<&str>,
   save: impl FnOnce(&str, String) -> Result<(), crate::Error>,
) -> Result<bool, crate::Error> {
   if let Some(bundled) = bundled
      && bundled_supersedes(bundled, &source)
   {
      log::info!("skipping catalog write for {name}: bundled copy is already up to date");
      return Ok(false);
   }
   if let Some(current) = current
      && active_supersedes(current, &source)
   {
      log::info!("skipping catalog write for {name}: active copy is already newer");
      return Ok(false);
   }
   save(name, source)?;
   Ok(true)
}

struct InstalledPlugin {
   name: String,
   source: String,
   bundled: Option<String>,
}

/// Filters `site_plugins` down to entries that should be reported as available
/// updates, skipping bundled copies that are equal or newer and active local
/// overrides that are already newer than the site copy.
fn pending_plugin_updates(
   site_plugins: &BTreeMap<String, u32>,
   installed: &[InstalledPlugin],
   mut fetch_remote: impl FnMut(&str, u32) -> Result<String, String>,
) -> (Vec<String>, BTreeMap<String, u32>) {
   let mut plugins = Vec::new();
   let mut plugin_crcs = BTreeMap::new();
   for plugin in installed {
      let Some(&crc) = site_plugins.get(&plugin.name) else {
         continue;
      };
      if crc32fast::hash(plugin.source.as_bytes()) == crc {
         continue;
      }
      if plugin
         .bundled
         .as_deref()
         .is_some_and(|bundled| crc32fast::hash(bundled.as_bytes()) == crc)
      {
         continue;
      }
      match fetch_remote(&plugin.name, crc) {
         Ok(remote_source) => {
            if plugin
               .bundled
               .as_deref()
               .is_some_and(|bundled| bundled_supersedes(bundled, &remote_source))
            {
               log::info!(
                  "skipping update for {}: bundled copy is already up to date",
                  plugin.name
               );
               continue;
            }
            if active_supersedes(&plugin.source, &remote_source) {
               log::info!(
                  "skipping update for {}: active copy is already newer",
                  plugin.name
               );
               continue;
            }
         },
         Err(err) => {
            log::info!("update check: skipping {} ({err})", plugin.name);
            continue;
         },
      }
      plugin_crcs.insert(plugin.name.clone(), crc);
      plugins.push(plugin.name.clone());
   }
   (plugins, plugin_crcs)
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
   let (fetch_url, normalized_name) =
      crate::normalize_plugin_url(&format!("{ASSETS}plugins/{name}.js"))?;
   let body = crate::fetch::get(&fetch_url)?;
   let source = String::from_utf8(body).map_err(|_| crate::Error::NotUtf8 { url: fetch_url })?;
   let current = crate::source(&normalized_name);
   let bundled = crate::bundled_source(&normalized_name);
   let _ = install_catalog_source(
      &normalized_name,
      source,
      current.as_deref(),
      bundled.as_deref(),
      crate::save_source,
   )?;
   Ok(normalized_name)
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
         // Snapshot plugins before any network fetch so the engine lock is not held.
         let installed = crate::engine().map_or_else(Vec::new, |engine| {
            engine
               .plugins()
               .into_iter()
               .filter_map(|plugin| {
                  let source = engine.source(&plugin.name)?;
                  let bundled = engine.bundled_source(&plugin.name);
                  Some(InstalledPlugin {
                     name: plugin.name,
                     source,
                     bundled,
                  })
               })
               .collect()
         });
         let (plugins, plugin_crcs) =
            pending_plugin_updates(&site.plugins, &installed, |name, crc| {
               download_plugin(name, Some(crc))
            });
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
   let Some(slot) = Slot::claim(None) else {
      log::info!("update check: already running");
      return;
   };
   thread::spawn(move || {
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
      drop(slot);
      if let Some(platform) = crate::platform() {
         platform.notify(Notice::UpdateAvailable(&update));
      }
   });
}

/// Checks the site on demand for a user click, updating status and showing a
/// toast on completion.
pub fn check_now() {
   let Some(slot) = Slot::claim(Some("Checking for updates...".to_owned())) else {
      log::info!("update check: already running");
      crate::toast("Update check already in progress...");
      return;
   };
   thread::spawn(move || match query() {
      Ok(Some(update)) => {
         log::info!(
            "update available: loader {:?}, plugins {:?}",
            update.loader,
            update.plugins
         );
         drop(slot);
         if let Some(platform) = crate::platform() {
            platform.notify(Notice::UpdateAvailable(&update));
         }
      },
      Ok(None) => {
         let cur = app_version();
         log::info!("up to date ({cur})");
         slot.finish(Some(format!("NyaaCord v{cur} is up to date.")));
         crate::toast("NyaaCord is up to date");
      },
      Err(err) => {
         log::info!("update check: {err}");
         let msg = format!("Update check failed: {err}");
         slot.finish(Some(msg.clone()));
         crate::toast(&msg);
      },
   });
}

/// Checks for updates again and applies what this install lacks.
///
/// When a loader or package update is available and the platform supports
/// [`crate::Platform::updates_loader`], hands off to
/// [`crate::Platform::update_loader`], which should call [`finish`] on exit
/// to publish its final status line.
pub fn apply() {
   let Some(slot) = Slot::claim(Some("Checking for updates...".to_owned())) else {
      log::info!("update apply: already running");
      crate::toast("Update already in progress...");
      return;
   };
   thread::spawn(move || {
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
               slot.finish(Some(msg.clone()));
               crate::toast(&msg);
               return;
            }
         },
      };
      let Some(update) = update else {
         let cur = app_version();
         log::info!("update apply: up to date ({cur})");
         slot.finish(Some(format!("NyaaCord v{cur} is up to date.")));
         crate::toast("NyaaCord is up to date");
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
               format!("NyaaCord v{version} is available.")
            } else {
               format!("Upstream build {version} is available.")
            };
            slot.finish(Some(msg.clone()));
            crate::toast(&msg);
            return;
         }
      }
      let mut updated_plugins = Vec::new();
      let mut resolved_plugins = Vec::new();
      let mut failed = Vec::new();
      if !update.plugins.is_empty() {
         slot.step(format!(
            "Updating {} plugin{}...",
            update.plugins.len(),
            if update.plugins.len() == 1 { "" } else { "s" }
         ));
         for name in &update.plugins {
            let expected_crc = update.plugin_crcs.get(name).copied();
            match update_plugin(name, expected_crc) {
               Ok(true) => {
                  log::info!("ota updated plugin {name}");
                  updated_plugins.push(name.clone());
                  resolved_plugins.push(name.clone());
               },
               Ok(false) => {
                  resolved_plugins.push(name.clone());
               },
               Err(err) => {
                  log::error!("ota update {name} failed: {err}");
                  failed.push(format!("{name}: {err}"));
               },
            }
         }
         drop_resolved_plugins(&resolved_plugins);
      }

      // A failed plugin used to go unreported whenever the loader update below
      // still had to run, since only `update.loader.is_none()` printed it.
      // The slot is released either way, and the failure keeps its line.
      if failed.is_empty() {
         drop(slot);
      } else {
         let msg = format!("Plugin update failed: {}", failed.join("; "));
         log::error!("{msg}");
         slot.finish(Some(msg.clone()));
         crate::toast(&msg);
      }
      if !updated_plugins.is_empty()
         && let Some(platform) = crate::platform()
      {
         platform.notify(Notice::PluginsUpdated(&updated_plugins));
      }
   });
}

fn drop_resolved_plugins(resolved: &[String]) {
   let mut guard = FOUND.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(pending) = guard.as_mut() {
      pending.plugins.retain(|name| !resolved.contains(name));
      pending
         .plugin_crcs
         .retain(|name, _| !resolved.contains(name));
      if pending.loader.is_none() && pending.plugins.is_empty() {
         *guard = None;
      }
   }
}

fn update_plugin(name: &str, expected_crc: Option<u32>) -> Result<bool, String> {
   let source = download_plugin(name, expected_crc)?;
   let current = crate::source(name);
   let bundled = crate::bundled_source(name);
   install_catalog_source(
      name,
      source,
      current.as_deref(),
      bundled.as_deref(),
      crate::save_source,
   )
   .map_err(|err| err.to_string())
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
#[must_use]
pub fn begin() -> bool {
   let mut state = progress();
   if matches!(*state, Progress::Running { .. }) {
      return false;
   }
   let id = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
   *state = Progress::Running { id, status: None };
   true
}

/// Releases the update slot with `status` shown.
pub fn finish(status: Option<String>) {
   *progress() = status.map_or(Progress::Idle, Progress::Finished);
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
      assert_eq!(snapshot(), (false, Some("done".to_owned())));
      finish(None);
      assert_eq!(status(), None);
      assert_eq!(snapshot(), (false, None));

      // Dropping an unfinished slot releases a still-running update without
      // clobbering a final status already written by a platform updater.
      let slot = Slot::claim(Some("Checking...".to_owned())).expect("slot");
      assert_eq!(snapshot(), (true, Some("Checking...".to_owned())));
      assert!(Slot::claim(None).is_none());
      slot.step("Downloading...");
      assert_eq!(snapshot(), (true, Some("Downloading...".to_owned())));
      finish(Some("Updated".to_owned()));
      drop(slot);
      assert_eq!(snapshot(), (false, Some("Updated".to_owned())));

      // Dropping an old slot after a newer update starts must leave the newer
      // slot running and untouched.
      let old = Slot::claim(Some("Old".to_owned())).expect("old slot");
      finish(Some("Old done".to_owned()));
      let newer = Slot::claim(Some("Newer".to_owned())).expect("newer slot");
      drop(old);
      assert_eq!(snapshot(), (true, Some("Newer".to_owned())));
      drop(newer);
      assert_eq!(snapshot(), (false, None));
   }

   #[test]
   fn catalog_install_skips_older_or_equal_bundled_and_never_downgrades_overrides() {
      let bundled_silent = "// @version 2.0.0\nconst bundled = true;\n";
      let remote_older_silent = "// @version 1.0.0\nconst bundled = false;\n";
      let bundled_fix = "// @version 1.3.0\nconst fix = 'local-build';\n";
      let remote_equal_fix = "// @version 1.3.0\nconst fix = 'site-build';\n";
      let bundled_clear = "// @version 0.1.3\nconst clear = 3;\n";
      let remote_newer_clear = "// @version 0.1.4\nconst clear = 4;\n";
      let override_newer = "// @version 2.1.0\nconst custom = 210;\n";
      let remote_mid = "// @version 2.0.5\nconst remote = 205;\n";
      let remote_upgrade = "// @version 2.2.0\nconst remote = 220;\n";
      let remote_third_party = "// @version 1.1.0\nconst ext = 2;\n";

      let mut saved = Vec::new();
      for (name, remote, current, bundled) in [
         (
            "silent-typing",
            remote_older_silent,
            Some(bundled_silent),
            Some(bundled_silent),
         ),
         (
            "fix-embeds",
            remote_equal_fix,
            Some(bundled_fix),
            Some(bundled_fix),
         ),
         ("identical", bundled_fix, Some(bundled_fix), Some(bundled_fix)),
         // Active 2.1.0 override over bundled 2.0.0 must not downgrade to 2.0.5.
         (
            "override-no-downgrade",
            remote_mid,
            Some(override_newer),
            Some(bundled_silent),
         ),
         // Non-bundled 2.1.0 must not downgrade to 2.0.5 either.
         ("third-party-no-downgrade", remote_mid, Some(override_newer), None),
      ] {
         assert!(
            !install_catalog_source(name, remote.to_owned(), current, bundled, |n, src| {
               saved.push((n.to_owned(), src));
               Ok(())
            })
            .unwrap(),
            "expected {name} to be skipped"
         );
      }
      assert_eq!(saved, Vec::new());

      for (name, remote, current, bundled) in [
         (
            "clear-urls",
            remote_newer_clear,
            Some(bundled_clear),
            Some(bundled_clear),
         ),
         // Active 2.1.0 override over bundled 2.0.0 upgrades to remote 2.2.0.
         (
            "override-upgrade",
            remote_upgrade,
            Some(override_newer),
            Some(bundled_silent),
         ),
         ("third-party", remote_third_party, None, None),
      ] {
         assert!(
            install_catalog_source(name, remote.to_owned(), current, bundled, |n, src| {
               saved.push((n.to_owned(), src));
               Ok(())
            })
            .unwrap(),
            "expected {name} to be saved"
         );
      }
      assert_eq!(saved.len(), 3);
      assert_eq!(saved[0].0, "clear-urls");
      assert_eq!(saved[1].0, "override-upgrade");
      assert_eq!(saved[2].0, "third-party");
   }

   #[test]
   fn update_check_skips_older_or_equal_bundled_and_newer_local_overrides() {
      let bundled_silent = "// @version 2.0.0\nconst bundled = true;\n";
      let remote_older_silent = "// @version 1.0.0\nconst bundled = false;\n";
      let bundled_fix = "// @version 1.3.0\nconst fix = 'local-build';\n";
      let remote_equal_fix = "// @version 1.3.0\nconst fix = 'site-build';\n";
      let bundled_clear = "// @version 0.1.3\nconst clear = 3;\n";
      let remote_newer_clear = "// @version 0.1.4\nconst clear = 4;\n";
      let override_210 = "// @version 2.1.0\nconst override = 210;\n";
      let remote_205 = "// @version 2.0.5\nconst remote = 205;\n";
      let remote_220 = "// @version 2.2.0\nconst remote = 220;\n";
      let installed_third_party = "// @version 1.0.0\nconst ext = 1;\n";
      let remote_third_party = "// @version 1.1.0\nconst ext = 2;\n";

      let entry = |name: &str, source: &str, bundled: Option<&str>| InstalledPlugin {
         name: name.to_owned(),
         source: source.to_owned(),
         bundled: bundled.map(str::to_owned),
      };
      let site_plugins = BTreeMap::from([
         (
            "silent-typing".to_owned(),
            crc32fast::hash(remote_older_silent.as_bytes()),
         ),
         (
            "fix-embeds".to_owned(),
            crc32fast::hash(remote_equal_fix.as_bytes()),
         ),
         (
            "clear-urls".to_owned(),
            crc32fast::hash(remote_newer_clear.as_bytes()),
         ),
         (
            "override-hold".to_owned(),
            crc32fast::hash(remote_205.as_bytes()),
         ),
         (
            "override-upgrade".to_owned(),
            crc32fast::hash(remote_220.as_bytes()),
         ),
         (
            "third-party-hold".to_owned(),
            crc32fast::hash(remote_205.as_bytes()),
         ),
         (
            "third-party".to_owned(),
            crc32fast::hash(remote_third_party.as_bytes()),
         ),
      ]);
      let installed = vec![
         entry("silent-typing", bundled_silent, Some(bundled_silent)),
         entry("fix-embeds", bundled_fix, Some(bundled_fix)),
         entry("clear-urls", bundled_clear, Some(bundled_clear)),
         entry("override-hold", override_210, Some(bundled_silent)),
         entry("override-upgrade", override_210, Some(bundled_silent)),
         entry("third-party-hold", override_210, None),
         entry("third-party", installed_third_party, None),
      ];

      let (pending, crcs) = pending_plugin_updates(&site_plugins, &installed, |name, _| {
         match name {
            "silent-typing" => Ok(remote_older_silent.to_owned()),
            "fix-embeds" => Ok(remote_equal_fix.to_owned()),
            "clear-urls" => Ok(remote_newer_clear.to_owned()),
            "override-hold" | "third-party-hold" => Ok(remote_205.to_owned()),
            "override-upgrade" => Ok(remote_220.to_owned()),
            "third-party" => Ok(remote_third_party.to_owned()),
            other => Err(format!("unexpected fetch for {other}")),
         }
      });

      assert_eq!(
         pending,
         vec![
            "clear-urls".to_owned(),
            "override-upgrade".to_owned(),
            "third-party".to_owned(),
         ]
      );
      assert_eq!(
         crcs.get("clear-urls").copied(),
         Some(crc32fast::hash(remote_newer_clear.as_bytes()))
      );
      assert_eq!(
         crcs.get("override-upgrade").copied(),
         Some(crc32fast::hash(remote_220.as_bytes()))
      );
      assert_eq!(
         crcs.get("third-party").copied(),
         Some(crc32fast::hash(remote_third_party.as_bytes()))
      );
      for skipped in [
         "silent-typing",
         "fix-embeds",
         "override-hold",
         "third-party-hold",
      ] {
         assert!(!crcs.contains_key(skipped), "{skipped} should not be pending");
      }
   }
}
