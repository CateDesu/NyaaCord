//! phoenix on Desktop Phoenix: the access gate, the HTTP transport hooks, the
//! `Nyaacord` pages in User Settings, message accessories and the tray.

#![cfg(any(windows, all(target_os = "linux", target_arch = "x86_64")))]

mod accessories;
mod badges;
mod composer;
mod context_menu;
mod discord;
mod emoji;
mod fetch;
mod gpui;
mod js;
mod message;
mod modal;
mod os;
mod settings;
mod staff;
mod tray;
mod typing;

use std::{
   ffi::c_void,
   path::{Path, PathBuf},
   sync::{Arc, Mutex, PoisonError},
   time::Duration,
};

use phoenix_hook::image::Image;
use phoenix_runtime::{
   Error, Notice, Platform, Plugin, Started,
   discord::{Authorization, DiscordError},
   fetch::{FetchError, Request, Response},
   remote::{self, Done},
   store::{Rows, StoreError},
};
use phoenix_sigs::registry::GatewayRegistry;
use rquickjs::{Ctx, Object};

struct Desktop;

/// A `POST /ui/choose` or `POST /ui/sheet` body.
#[derive(serde::Deserialize)]
struct DialogRequest {
   title: Option<String>,
   message: Option<String>,
   text: Option<String>,
   monospace: Option<bool>,
   highlight: Option<bool>,
   options: Option<Vec<DialogItem>>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum DialogItem {
   Label(String),
   Item {
      label: String,
      #[serde(default)]
      danger: bool,
   },
}

#[derive(serde::Serialize)]
struct DialogAnswer {
   index: i32,
}

/// The target and its swapped gateway registry, once [`prepare`] hooked them.
static PREPARED: Mutex<Option<(Image<'static>, &'static GatewayRegistry)>> = Mutex::new(None);

/// Hooks Discord from `DllMain` on Windows or the `.init_array` constructor on
/// Linux, before its own code runs.
///
/// Desktop Phoenix settles on the access gate in its first frames and stops
/// reading the gateway registry and HTTP vtables once started, so hooks swapped
/// later from [`start`]'s thread never run. [`start`] picks up what this
/// hooked.
pub fn prepare() -> Result<(), Error> {
   phoenix_runtime::set_platform(&Desktop);
   let image = phoenix_runtime::target()?;
   open_access_gate(&image);
   staff::prepare(&image);
   settings::install(&image);
   modal::install(&image);
   composer::install(&image);
   accessories::install(&image);
   context_menu::install(&image);
   message::install(&image);
   badges::install(&image);
   emoji::install(&image);
   typing::install(&image);
   <os::Os as os::DesktopOs>::install(&image);
   let registry = phoenix_runtime::hook(&image);
   if registry.is_ok() {
      discord::install(&image);
      accessories::install_optimistic_delete(&image);
   }
   tray::install(&image);
   // The index is tens of MB and nothing reads it after startup.
   phoenix_sigs::desktop::release_call_index();
   *PREPARED.lock().unwrap_or_else(PoisonError::into_inner) = Some((image, registry?));
   Ok(())
}

/// Opens the access gate and hooks nothing else, so Discord runs as it would
/// without `Nyaacord`. For telling Discord's own behaviour from ours.
pub fn open_gate_only() -> Result<(), Error> {
   phoenix_runtime::set_platform(&Desktop);
   open_access_gate(&phoenix_runtime::target()?);
   Ok(())
}

/// Starts phoenix inside Desktop Phoenix, on what [`prepare`] hooked.
pub fn start(home: PathBuf, bundled: Vec<Plugin>) -> Result<Started, Error> {
   let prepared = PREPARED
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .take();
   if let Some((image, registry)) = prepared {
      return phoenix_runtime::start_hooked(&Desktop, image, registry, home, bundled);
   }
   let image = phoenix_runtime::target()?;
   let registry = phoenix_runtime::hook(&image)?;
   discord::install(&image);
   phoenix_runtime::start_hooked(&Desktop, image, registry, home, bundled)
}

/// Desktop Phoenix shows "This app isn't available for your account" from
/// `libdiscord_ops_surfaces::client_access::ClientAccessGate`, and blocks the
/// login on it. One function reads it (1 blocked, 2 no gate, also what a
/// missing value reads as), and the gate is only considered while it is not 2:
/// its small caller is `signed_in && access(..) != 2`. A separate evaluator
/// gates background services like DM notifications and sounds on the
/// `2026-11-phoenix-app-access` flag. Patching both lets the UI and background
/// services run normally.
fn open_access_gate(image: &Image<'static>) {
   /// `mov al, 2; ret`
   const GATE_ALLOWED: [u8; 3] = [0xB0, 0x02, 0xC3];
   /// `mov al, 1; ret`
   const EVALUATOR_ALLOWED: [u8; 3] = [0xB0, 0x01, 0xC3];

   match phoenix_sigs::desktop::access_gate(image) {
      Ok(access) => {
         // SAFETY: Discord's own code, written from `DllMain` or the
         // constructor before it runs.
         match unsafe { image.write_code(access, &GATE_ALLOWED) } {
            Ok(()) => log::info!("access gate: open"),
            Err(err) => log::error!("access gate: {err}"),
         }
      },
      Err(err) => log::warn!("access gate: {err}, leaving it"),
   }

   match phoenix_sigs::desktop::access_evaluator(image) {
      Ok(eval) => {
         // SAFETY: Discord's own code, written from `DllMain` or the
         // constructor before it runs.
         match unsafe { image.write_code(eval, &EVALUATOR_ALLOWED) } {
            Ok(()) => log::info!("access evaluator: open"),
            Err(err) => log::error!("access evaluator: {err}"),
         }
      },
      Err(err) => log::warn!("access evaluator: {err}, leaving it"),
   }
}

impl Platform for Desktop {
   fn send(&self, request: &Request) -> Result<Response, FetchError> {
      fetch::send(request)
   }

   fn query(&self, database: &Path, sql: &str, args: &[String]) -> Result<Rows, StoreError> {
      phoenix_runtime::store::sqlite(database, sql, args)
   }

   fn discord(
      &self,
      method: &str,
      path: &str,
      headers: Vec<(String, String)>,
      body: Vec<u8>,
   ) -> Result<Response, DiscordError> {
      discord::send(method, path, headers, body)
   }

   fn authorize(&self, request: &Authorization) -> Result<String, DiscordError> {
      discord::authorize(request)
   }

   fn init(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
      ctx.store_userdata(js::Desktop::default())
         .map_err(|err| rquickjs::Exception::throw_internal(ctx, &err.to_string()))?;
      Ok(())
   }

   fn api<'js>(
      &self,
      ctx: &Ctx<'js>,
      plugin: &str,
      api: &Object<'js>,
      messages: &Object<'js>,
   ) -> rquickjs::Result<()> {
      js::ui::desktop_patch_api(ctx, plugin, api, messages)
   }

   fn console<'js>(&self, ctx: &Ctx<'js>, phoenix: &Object<'js>) -> rquickjs::Result<()> {
      phoenix.set("native", js::native::object(ctx)?)
   }

   fn unload(&self, ctx: &Ctx<'_>, plugin: &str) {
      js::unload(ctx, plugin);
   }

   fn staff(&self, image: &'static Image<'static>) {
      staff::start(image);
   }

   fn gateway(&self, store: *mut c_void) {
      // SAFETY: every registry handler takes the store state first, and this
      // runs inside one.
      unsafe { staff::apply(store) };
   }

   fn started(&self, _first_run: bool, _plugins: &[String]) {
      let Some(state) = phoenix_runtime::state() else {
         return;
      };
      let _ = std::fs::create_dir_all(state.plugins_dir());
      remote::set_flag(true);
      // Desktop keeps the shell on, so any local process could reach `/eval`
      // and the session; only holders of this start's token get in.
      remote::require_token(&state.plugins_dir().with_file_name("debug-shell-token"));
      remote::ensure_started();
      phoenix_ui_desktop::start_tray(phoenix_ui_desktop::TrayConfig {
         dashboard_url: remote::dashboard_url(),
         plugins_dir: state.plugins_dir(),
         on_reload_all: Some(Arc::new(phoenix_runtime::reload_all)),
         on_check_updates: Some(Arc::new(phoenix_runtime::update::check_now)),
      });
      phoenix_ui_desktop::notify(
         "Nyaacord ready",
         Some("Open Sunbird in User Settings for plugins, or press Ctrl+Shift+I for DevTools"),
      );
      phoenix_runtime::update::check();
   }

   fn refresh(&self) {
      modal::force_redraw();
   }

   fn toast(&self, message: &str) {
      phoenix_ui_desktop::toast(message);
   }

   fn notify(&self, notice: Notice<'_>) {
      match notice {
         Notice::UpdateAvailable(update) => {
            let mut parts = Vec::new();
            if let Some(version) = &update.loader {
               parts.push(format!("Nyaacord v{version} is available"));
            }
            if !update.plugins.is_empty() {
               parts.push(format!("plugins: {}", update.plugins.join(", ")));
            }
            let body = if parts.is_empty() {
               "Open Settings -> Sunbird".to_owned()
            } else {
               parts.join(" · ")
            };
            phoenix_ui_desktop::notify("Nyaacord updates available", Some(&body));
         },
         Notice::PluginsUpdated(names) if !names.is_empty() => {
            let body = format!("Reloaded: {}", names.join(", "));
            phoenix_ui_desktop::notify("Plugins updated", Some(&body));
         },
         Notice::PluginsUpdated(_) => {},
      }
   }

   #[cfg(any(windows, target_os = "linux"))]
   fn app_version(&self) -> &'static str {
      phoenix_runtime::update::APP_VERSION
   }

   #[cfg(any(windows, target_os = "linux"))]
   fn check_app_update(
      &self,
   ) -> Option<Result<Option<phoenix_runtime::update::AppRelease>, String>> {
      let result = phoenix_updater::Config::from_env()
         .and_then(|cfg| phoenix_updater::check_latest(phoenix_runtime::update::APP_VERSION, &cfg))
         .map(|check| match check {
            phoenix_updater::CheckResult::UpToDate { .. } => None,
            phoenix_updater::CheckResult::Available(rel) => {
               Some(phoenix_runtime::update::AppRelease {
                  version: rel.version,
                  tag_name: rel.tag_name,
                  asset_name: rel.asset_name,
                  download_url: rel.download_url,
                  size: rel.size,
                  sha256: rel.sha256,
                  release_url: rel.release_url,
               })
            },
         })
         .map_err(|err| err.to_string());
      Some(result)
   }

   #[cfg(any(windows, target_os = "linux"))]
   fn updates_loader(&self) -> bool {
      target_package_path().is_some()
   }

   #[cfg(any(windows, target_os = "linux"))]
   fn update_loader(&self, version: &str, update: &phoenix_runtime::update::Update) {
      match run_package_update(version, update) {
         Ok(outcome) => {
            #[cfg(target_os = "linux")]
            sync_managed_version(&outcome.target, &outcome.version);
            phoenix_runtime::update::clear_found();

            #[cfg(windows)]
            {
               let ver = outcome.version;
               phoenix_runtime::update::finish(Some(format!(
                  "Updated to Nyaacord v{ver}. Restart Nyaacord to apply."
               )));
               phoenix_ui_desktop::notify(
                  "Nyaacord updated",
                  Some(&format!("Updated to v{ver}. Restart Nyaacord to apply.")),
               );
            }

            #[cfg(target_os = "linux")]
            {
               let no_relaunch =
                  std::env::var("NYAACORD_NO_RELAUNCH").is_ok_and(|value| value == "1");
               if no_relaunch {
                  let ver = outcome.version;
                  phoenix_runtime::update::finish(Some(format!(
                     "Updated to Nyaacord v{ver}. Restart Nyaacord to apply."
                  )));
                  phoenix_ui_desktop::notify(
                     "Nyaacord updated",
                     Some(&format!("Updated to v{ver}. Restart Nyaacord to apply.")),
                  );
                  return;
               }
               match phoenix_updater::spawn_relaunch_watchdog(
                  &outcome.target,
                  outcome.previous.as_deref(),
                  std::process::id(),
               ) {
                  Ok(()) => {
                     let ver = outcome.version;
                     phoenix_runtime::update::finish(Some(format!(
                        "Updated to Nyaacord v{ver}. Restarting..."
                     )));
                     phoenix_ui_desktop::notify(
                        "Nyaacord updated",
                        Some(&format!("Updated to v{ver}. Restarting...")),
                     );
                     std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_millis(500));
                        // SAFETY: signals this process to exit so the detached watchdog
                        // relaunches the updated AppImage.
                        unsafe {
                           libc::kill(libc::getpid(), libc::SIGTERM);
                        }
                        std::thread::sleep(Duration::from_secs(1));
                        std::process::exit(0);
                     });
                  },
                  Err(err) => {
                     log::warn!("relaunch watchdog: {err}");
                     let ver = outcome.version;
                     phoenix_runtime::update::finish(Some(format!(
                        "Updated to Nyaacord v{ver}. Restart Nyaacord to apply."
                     )));
                     phoenix_ui_desktop::notify(
                        "Nyaacord updated",
                        Some(&format!("Updated to v{ver}. Restart Nyaacord to apply.")),
                     );
                  },
               }
            }
         },
         Err(err) => {
            let msg = format!("Update failed: {err}");
            log::error!("{msg}");
            phoenix_runtime::update::finish(Some(msg.clone()));
            phoenix_ui_desktop::toast(&msg);
         },
      }
   }

   fn route(&self, request: &remote::Request) -> Option<remote::Reply> {
      let html = || {
         (
            "200 OK",
            "text/html; charset=utf-8",
            phoenix_ui_desktop::DASHBOARD_HTML.as_bytes().to_vec(),
         )
      };
      match (request.method.as_str(), request.path.as_str()) {
         ("GET", "/ui") => Some(html()),
         ("GET", "/") if request.accept.contains("text/html") => Some(html()),
         ("GET", "/icon.gif" | "/favicon.ico") => {
            Some(("200 OK", "image/gif", phoenix_ui_desktop::ICON_GIF.to_vec()))
         },
         ("POST", "/ui/devtools") => {
            phoenix_ui_desktop::open_devtools();
            Some(remote::ok_json(&Done::Started))
         },
         ("POST", "/ui/choose") => Some(dialog(&request.body, false)),
         ("POST", "/ui/sheet") => Some(dialog(&request.body, true)),
         _ => None,
      }
   }

   fn routes(&self) -> &'static str {
      ", GET /ui, GET /icon.gif, POST /ui/devtools, POST /ui/{choose,sheet}"
   }
}

/// Opens a native choose or sheet dialog for the dashboard and blocks this
/// request's thread until it is answered.
fn dialog(body: &[u8], sheet: bool) -> remote::Reply {
   let Ok(request) = serde_json::from_slice::<DialogRequest>(body) else {
      return remote::err("400 Bad Request", "expected a JSON dialog request");
   };
   let options = request
      .options
      .unwrap_or_default()
      .into_iter()
      .map(|item| match item {
         DialogItem::Label(label) => modal::ChoiceOption {
            label,
            danger: false,
         },
         DialogItem::Item { label, danger } => modal::ChoiceOption { label, danger },
      })
      .collect::<Vec<_>>();
   let content = if sheet {
      request.text.map(|text| modal::ModalSection::Code {
         title: None,
         text,
         monospace: request.monospace.unwrap_or_default(),
         highlight: request.highlight.unwrap_or_default(),
      })
   } else {
      request
         .message
         .map(|text| modal::ModalSection::Text { title: None, text })
   };
   if content.is_none() && options.is_empty() {
      return remote::err(
         "400 Bad Request",
         if sheet {
            "a sheet needs text"
         } else {
            "a choose dialog needs a message or options"
         },
      );
   }
   let title = request.title.unwrap_or_else(|| "Nyaacord".to_owned());
   let index = modal::ask(title, content, options, Duration::from_mins(2));
   remote::ok_json(&DialogAnswer { index })
}

#[cfg(target_os = "linux")]
fn xdg_sunbird_dir() -> Option<PathBuf> {
   let data_home = std::env::var_os("XDG_DATA_HOME")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from)
      .or_else(|| {
         std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(".local/share"))
      })?;
   Some(data_home.join("sunbird"))
}

#[cfg(target_os = "linux")]
fn is_nyaacord_appimage_env(appimage: &Path, has_nyaacord_appdir: bool) -> bool {
   has_nyaacord_appdir
      || appimage
         .file_name()
         .and_then(|name| name.to_str())
         .is_some_and(|name| name.to_ascii_lowercase().contains("nyaacord"))
}

#[cfg(target_os = "linux")]
fn resolve_target_appimage(
   managed: Option<PathBuf>,
   appimage: Option<PathBuf>,
   has_nyaacord_appdir: bool,
   sunbird_dir: Option<PathBuf>,
) -> Option<PathBuf> {
   // Prefer NYAACORD_MANAGED_APPIMAGE over APPIMAGE so local_loader installs
   // launched via `sunbird` never overwrite the user's base discord-phoenix AppImage.
   if let Some(path) = managed
      && !path.as_os_str().is_empty()
      && !phoenix_updater::is_mount_path(&path)
   {
      return Some(path);
   }
   if let Some(path) = appimage
      && !path.as_os_str().is_empty()
      && is_nyaacord_appimage_env(&path, has_nyaacord_appdir)
      && !phoenix_updater::is_mount_path(&path)
   {
      return Some(path);
   }
   let dir = sunbird_dir?;
   let candidate = dir.join("Nyaacord-x86_64.AppImage");
   if (candidate.exists() || dir.is_dir()) && !phoenix_updater::is_mount_path(&candidate) {
      return Some(candidate);
   }
   None
}

#[cfg(target_os = "linux")]
fn target_appimage_path() -> Option<PathBuf> {
   let managed = std::env::var_os("NYAACORD_MANAGED_APPIMAGE")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from);
   let appimage = std::env::var_os("APPIMAGE")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from);
   let has_nyaacord_appdir =
      std::env::var_os("NYAACORD_APPDIR").is_some_and(|value| !value.is_empty());
   resolve_target_appimage(managed, appimage, has_nyaacord_appdir, xdg_sunbird_dir())
}

#[cfg(target_os = "linux")]
fn sync_managed_version(target: &Path, version: &str) {
   let Some(dir) = target.parent() else {
      return;
   };
   let launcher_matches = std::env::var_os("NYAACORD_LAUNCHER")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from)
      .and_then(|launcher| launcher.parent().map(Path::to_path_buf))
      .is_some_and(|launcher_dir| launcher_dir == dir);
   let managed = launcher_matches || (dir.join("sunbird").exists() && dir.join("version").exists());
   if managed && let Err(err) = std::fs::write(dir.join("version"), format!("{version}\n")) {
      log::warn!("updating managed version file: {err}");
   }
}

#[cfg(any(windows, test))]
fn resolve_target_windows_exe(
   managed: Option<PathBuf>,
   local_app_data: Option<PathBuf>,
   dir_exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
   if let Some(path) = managed
      && !path.as_os_str().is_empty()
   {
      return Some(path);
   }
   let base = local_app_data.filter(|dir| !dir.as_os_str().is_empty())?;
   let dir = base.join("Nyaacord");
   let exe = dir.join("Nyaacord.exe");
   if dir_exists(&exe) || dir_exists(&dir) {
      return Some(exe);
   }
   None
}

#[cfg(windows)]
fn target_package_path() -> Option<PathBuf> {
   let managed = std::env::var_os("NYAACORD_MANAGED_EXE")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from);
   let local = std::env::var_os("LOCALAPPDATA")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from);
   resolve_target_windows_exe(managed, local, Path::exists)
}

#[cfg(target_os = "linux")]
fn target_package_path() -> Option<PathBuf> {
   target_appimage_path()
}

#[cfg(any(windows, target_os = "linux"))]
fn run_package_update(
   version: &str,
   update: &phoenix_runtime::update::Update,
) -> Result<phoenix_updater::ApplyOutcome, String> {
   let target =
      target_package_path().ok_or_else(|| "no writable launcher path found".to_owned())?;
   let cfg = phoenix_updater::Config::from_env().map_err(|err| err.to_string())?;
   let release = match &update.release {
      Some(rel) => phoenix_updater::ReleaseInfo {
         version: rel.version.clone(),
         tag_name: rel.tag_name.clone(),
         asset_name: rel.asset_name.clone(),
         download_url: rel.download_url.clone(),
         size: rel.size,
         sha256: rel.sha256.clone(),
         release_url: rel.release_url.clone(),
      },
      None => phoenix_updater::fetch_latest(&cfg).map_err(|err| err.to_string())?,
   };
   let display_version = if release.version.is_empty() {
      version
   } else {
      release.version.as_str()
   };
   phoenix_runtime::update::set_status(Some(format!("Downloading Nyaacord v{display_version}...")));
   let mut last_pct: Option<u64> = None;
   let mut last_mb: u64 = 0;
   let mut on_progress = |downloaded: u64, total: Option<u64>| {
      let known_total = total
         .filter(|&bytes| bytes > 0)
         .or_else(|| (release.size > 0).then_some(release.size));
      if let Some(total_bytes) = known_total {
         let pct = downloaded
            .saturating_mul(100)
            .checked_div(total_bytes)
            .unwrap_or(0)
            .min(100);
         if last_pct.is_none_or(|prev| pct >= prev + 5 || (pct == 100 && prev != 100)) {
            last_pct = Some(pct);
            phoenix_runtime::update::set_status(Some(format!(
               "Downloading Nyaacord v{display_version} ({pct}%)..."
            )));
         }
      } else {
         let mb = downloaded / (1024 * 1024);
         if mb >= last_mb + 5 {
            last_mb = mb;
            phoenix_runtime::update::set_status(Some(format!(
               "Downloading Nyaacord v{display_version} ({mb} MiB)..."
            )));
         }
      }
   };
   phoenix_updater::apply_update(&release, &target, &cfg, &mut on_progress)
      .map_err(|err| err.to_string())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
   use super::*;

   #[test]
   fn detects_appimage_mount_paths() {
      assert!(phoenix_updater::is_mount_path(Path::new(
         "/tmp/.mount_Nyaacord1234/AppRun"
      )));
      assert!(phoenix_updater::is_mount_path(Path::new(
         "/home/cate/.t3/tmp/.mount_Nyaacord1234/AppRun"
      )));
      assert!(phoenix_updater::is_mount_path(Path::new("/tmp/.mount_abc")));
      assert!(!phoenix_updater::is_mount_path(Path::new(
         "/home/cate/.local/share/sunbird/Nyaacord-x86_64.AppImage"
      )));
      assert!(!phoenix_updater::is_mount_path(Path::new(
         "/tmp/nyaacord-test/Nyaacord-x86_64.AppImage"
      )));
   }

   #[test]
   fn resolves_target_appimage_without_touching_base_discord() {
      let managed = PathBuf::from("/home/cate/.local/share/sunbird/Nyaacord-x86_64.AppImage");
      let discord = PathBuf::from("/home/cate/Applications/discord-phoenix.AppImage");
      let custom = PathBuf::from("/home/cate/Apps/custom-chat.AppImage");

      // Managed AppImage takes precedence over stock discord-phoenix APPIMAGE.
      assert_eq!(
         resolve_target_appimage(Some(managed.clone()), Some(discord.clone()), false, None),
         Some(managed)
      );

      // Stock discord-phoenix APPIMAGE without NYAACORD_APPDIR is ignored.
      assert_eq!(
         resolve_target_appimage(None, Some(discord), false, None),
         None
      );

      // Standalone renamed Nyaacord AppImage with NYAACORD_APPDIR is accepted.
      assert_eq!(
         resolve_target_appimage(None, Some(custom.clone()), true, None),
         Some(custom)
      );

      // Mount paths are never selected as targets.
      assert_eq!(
         resolve_target_appimage(
            Some(PathBuf::from("/var/tmp/.mount_Nyaaco1234/AppRun")),
            None,
            false,
            None
         ),
         None
      );
   }

   #[test]
   fn resolves_target_windows_exe_paths() {
      let explicit = PathBuf::from(r"D:\Portable\Nyaacord-x86_64.exe");
      let local = PathBuf::from(r"C:\Users\cate\AppData\Local");

      assert_eq!(
         resolve_target_windows_exe(Some(explicit.clone()), Some(local.clone()), |_| true),
         Some(explicit)
      );
      assert_eq!(
         resolve_target_windows_exe(None, Some(local.clone()), |path| {
            path == local.join("Nyaacord")
         }),
         Some(local.join("Nyaacord").join("Nyaacord.exe"))
      );
      assert_eq!(
         resolve_target_windows_exe(None, Some(local), |_| false),
         None
      );
   }
}
