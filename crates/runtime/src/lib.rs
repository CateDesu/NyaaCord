//! The plugin runtime the loader starts.
//!
//! [`start`] picks the target library among the loaded images, boots `QuickJS`
//! with the `phoenix` API, and evaluates every bundle in `<home>/plugins`.
//! Every gateway registry entry points at phoenix's detour from the start, and
//! a plugin's hook turns its event on.

use std::{
   path::{Path, PathBuf},
   sync::{
      Mutex, OnceLock,
      atomic::{AtomicBool, Ordering},
   },
};

use phoenix_hook::image::{BuildId, Image};
use phoenix_sigs::{
   LIBRARIES,
   registry::{self, GatewayRegistry},
};

pub mod console;
pub mod discord;
pub mod fetch;
pub mod gateway;
pub mod highlight;
pub mod js;
pub mod local;
mod platform;
pub mod remote;
pub mod store;
pub mod update;

pub use platform::{Notice, Platform};

#[derive(Debug, flub::Error)]
pub enum Error {
   #[error("no loaded library is one of {LIBRARIES:?}")]
   NoTarget,
   #[error("the runtime is already running")]
   AlreadyStarted,
   #[error("the runtime has not started")]
   NotStarted,
   #[error("{path}: {source}")]
   Io {
      path: PathBuf,
      #[source]
      source: std::io::Error,
   },
   #[error("javascript: {0}")]
   Js(String),
   #[error(transparent)]
   Registry(#[from] registry::Error),
   #[error("{event}: the registry slot held {found:#x}, the walk read {expected:#x}")]
   SwapConflict {
      event: String,
      expected: usize,
      found: usize,
   },
   #[error(transparent)]
   Hook(#[from] phoenix_hook::Error),
   #[error("{url}: plugins install from https:// URLs ending in a name like plugin.js")]
   PluginUrl { url: String },
   #[error("{name}: plugin files are .js files named with letters, digits, - or _")]
   PluginFile { name: String },
   #[error("{url}: the download is not UTF-8 text")]
   NotUtf8 { url: String },
   #[error("invalid plugin: {reason}")]
   InvalidPlugin { reason: String },
   #[error(transparent)]
   Fetch(#[from] fetch::FetchError),
   #[error("NyaaCord needs runtime version {version}, the site's plugins need it")]
   LoaderOutdated { version: String },
}

pub struct State {
   pub image: Image<'static>,
   pub home: PathBuf,
   js: Mutex<js::Engine>,
}

static STATE: OnceLock<State> = OnceLock::new();
static PLATFORM: OnceLock<&'static dyn Platform> = OnceLock::new();
/// Phoenix's Staff Settings page reads `(user.flags | flag_override) & 1`, and
/// the platforms set that bit while this is on.
static STAFF: AtomicBool = AtomicBool::new(false);

/// The running runtime, once [`start`] published it.
#[must_use]
pub fn state() -> Option<&'static State> {
   STATE.get()
}

/// The platform [`start`] runs on.
#[must_use]
pub fn platform() -> Option<&'static dyn Platform> {
   PLATFORM.get().copied()
}

/// Sets the platform ahead of [`start`], for hooks that run before it.
pub fn set_platform(platform: &'static dyn Platform) {
   let _ = PLATFORM.set(platform);
}

/// A plugin bundle, an esbuild IIFE that receives the `phoenix` API.
pub struct Plugin {
   pub name: String,
   pub source: String,
}

/// A plugin as the settings page shows it.
#[derive(Clone, Debug)]
pub struct PluginInfo {
   pub name: String,
   pub enabled: bool,
   pub error: Option<String>,
   pub installed: bool,
   pub bundled: bool,
   pub header: Header,
   pub settings: Vec<Setting>,
}

/// Outcome of uninstalling a plugin file from `home/plugins`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
   /// A user-installed plugin was removed completely.
   Removed,
   /// A user override was removed and the bundled plugin was restored.
   Restored,
}

/// The `// @key value` lines a bundle starts with.
#[derive(Clone, Debug, Default)]
pub struct Header {
   pub description: Option<String>,
   pub version: Option<String>,
   pub author: Option<String>,
   /// `// @enabled false`, for plugins that stay off until the user turns
   /// them on.
   pub default_off: bool,
}

impl From<&str> for Header {
   fn from(source: &str) -> Self {
      let mut header = Self::default();
      for line in source
         .lines()
         .map(str::trim)
         .filter(|line| !line.is_empty())
      {
         let Some(comment) = line.strip_prefix("//") else {
            break;
         };
         let Some((key, value)) = comment
            .trim()
            .strip_prefix('@')
            .and_then(|tag| tag.split_once(' '))
         else {
            continue;
         };
         let value = value.trim();
         match key {
            "description" => header.description = Some(value.to_owned()),
            "version" => header.version = Some(value.to_owned()),
            "author" => header.author = Some(value.to_owned()),
            "enabled" => header.default_off = value == "false",
            _ => {},
         }
      }
      header
   }
}

/// A plugin setting as the user currently has it.
#[derive(Clone, Debug)]
pub struct Setting {
   pub key: String,
   pub title: String,
   pub description: Option<String>,
   pub control: Control,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Control {
   Switch(bool),
   Text(String),
   /// Text shown masked, and redacted from the plugin's log lines.
   Secret(String),
   Select {
      options: Vec<Choice>,
      chosen: String,
   },
   Number(f64),
   Button,
}

/// One option of a select setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
   pub value: String,
   pub label: String,
}

/// A value the user picked for a setting, as `plugins.json` stores it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Chosen {
   Switch(bool),
   Number(f64),
   Text(String),
}

pub fn engine() -> Option<std::sync::MutexGuard<'static, js::Engine>> {
   Some(
      STATE
         .get()?
         .js
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner),
   )
}

/// Runs matching `phoenix.discord.onRequest` hooks on an outgoing Discord API
/// request.
#[must_use]
pub fn intercept_request(
   matched: &[u64],
   method: &str,
   path: &str,
   body: &[u8],
) -> discord::RequestAction {
   engine().map_or(discord::RequestAction::Pass, |engine| {
      engine.intercept_request(matched, method, path, body)
   })
}

/// Runs matching `phoenix.discord.onResponse` hooks (sync or async) on an
/// incoming Discord API reply and delivers the final status and body through
/// `done`.
pub fn intercept_response(
   matched: Vec<u64>,
   method: String,
   path: String,
   status: i32,
   body: Vec<u8>,
   done: std::sync::mpsc::Sender<(i32, Vec<u8>)>,
) {
   if let Some(engine) = engine() {
      engine.intercept_response(matched, method, path, status, body, done);
   } else {
      let _ = done.send((status, body));
   }
}

/// Redraws the platform's settings pages.
pub fn refresh_settings() {
   if let Some(platform) = platform() {
      platform.refresh();
   }
}

pub fn toast(message: &str) {
   if let Some(platform) = platform() {
      platform.toast(message);
   }
}

/// Every plugin found at start, enabled or not.
#[must_use]
pub fn plugins() -> Vec<PluginInfo> {
   engine().map_or_else(Vec::new, |engine| engine.plugins())
}

/// Whether the staff unlock is on.
#[must_use]
pub fn staff_enabled() -> bool {
   STAFF.load(Ordering::Acquire)
}

/// Turns the staff unlock on or off and remembers the choice across restarts.
pub fn set_staff_enabled(enabled: bool) {
   STAFF.store(enabled, Ordering::Release);
   if let Some(engine) = engine() {
      engine.save_choices();
   }
}

/// Whether the localhost debug shell (`127.0.0.1:4242`) is enabled.
#[must_use]
pub fn remote_shell_enabled() -> bool {
   remote::enabled()
}

pub fn set_remote_shell_flag(enabled: bool) {
   remote::set_flag(enabled);
}

/// Turns the localhost debug shell on or off and remembers the choice.
pub fn set_remote_shell_enabled(enabled: bool) {
   remote::set_enabled(enabled);
   if let Some(engine) = engine() {
      engine.save_choices();
   }
}

/// Runs `code` in the plugin runtime's global scope, logs it, and returns its
/// formatted result or error.
pub fn evaluate_capture(code: &str) -> Result<String, String> {
   log::info!("> {code}");
   let result = engine().map(|engine| engine.evaluate(code));
   match result {
      Some(Ok(result)) => {
         log::info!("{result}");
         Ok(result)
      },
      Some(Err(err)) => {
         let message = err.to_string();
         log::error!("{message}");
         Err(message)
      },
      None => {
         let message = "the runtime is not running".to_owned();
         log::error!("{message}");
         Err(message)
      },
   }
}

/// Turns a plugin on or off and remembers the choice across restarts.
pub fn set_enabled(name: &str, enabled: bool) {
   if let Some(mut engine) = engine() {
      engine.set_enabled(name, enabled);
   }
}

/// Sets one of a plugin's settings and remembers it across restarts.
pub fn set_setting(plugin: &str, key: &str, value: Chosen) {
   if let Some(engine) = engine() {
      engine.set_setting(plugin, key, value);
   }
}

/// Runs a button setting's `onPress`.
pub fn press(plugin: &str, key: &str) {
   if let Some(engine) = engine() {
      engine.press(plugin, key);
   }
}

/// Runs `code` in the plugin runtime's global scope and logs it with its
/// result.
pub fn evaluate(code: &str) {
   log::info!("> {code}");
   let result = engine().map(|engine| engine.evaluate(code));
   match result {
      Some(Ok(result)) => log::info!("{result}"),
      Some(Err(err)) => log::error!("{err}"),
      None => log::error!("the runtime is not running"),
   }
}

/// Normalizes a plugin URL and extracts its plugin name.
///
/// Rewrites GitHub `/blob/` and `/raw/` links to `raw.githubusercontent.com`
/// and `gist.github.com/.../raw/...` links to `gist.githubusercontent.com`.
pub fn normalize_plugin_url(raw: &str) -> Result<(String, String), Error> {
   let trimmed = raw.trim();
   let bad_url = || Error::PluginUrl {
      url: raw.to_owned(),
   };
   if trimmed.is_empty() || trimmed.bytes().any(|b| b <= 0x20 || b == 0x7F) {
      return Err(bad_url());
   }
   let rest = trimmed.strip_prefix("https://").ok_or_else(bad_url)?;
   let without_frag = rest.split(['?', '#']).next().unwrap_or_default();
   let (host, path) = without_frag.split_once('/').ok_or_else(bad_url)?;
   if host.is_empty() || host.contains('@') || path.is_empty() {
      return Err(bad_url());
   }

   let normalized = if host.eq_ignore_ascii_case("github.com") {
      let mut parts = path.splitn(4, '/');
      let (Some(owner), Some(repo), Some(kind), Some(tail)) =
         (parts.next(), parts.next(), parts.next(), parts.next())
      else {
         return Err(bad_url());
      };
      if owner.is_empty()
         || repo.is_empty()
         || !matches!(kind, "blob" | "raw")
         || !tail.contains('/')
      {
         return Err(bad_url());
      }
      format!("https://raw.githubusercontent.com/{owner}/{repo}/{tail}")
   } else if host.eq_ignore_ascii_case("gist.github.com") {
      let segments: Vec<&str> = path.split('/').collect();
      let raw_pos = segments.iter().position(|&seg| seg == "raw");
      if raw_pos.is_none_or(|idx| idx == 0 || idx + 1 >= segments.len())
         || segments.iter().any(|seg| seg.is_empty())
      {
         return Err(bad_url());
      }
      format!("https://gist.githubusercontent.com/{path}")
   } else {
      trimmed.to_owned()
   };

   let norm_path = normalized
      .strip_prefix("https://")
      .unwrap_or_default()
      .split(['?', '#'])
      .next()
      .unwrap_or_default();
   let name = norm_path
      .rsplit('/')
      .next()
      .and_then(plugin_name)
      .map(str::to_owned)
      .ok_or_else(bad_url)?;

   Ok((normalized, name))
}

fn looks_like_html(trimmed: &str) -> bool {
   let prefix: String = trimmed.chars().take(16).collect();
   let lower = prefix.to_ascii_lowercase();
   lower.starts_with("<!doctype html")
      || lower.starts_with("<html")
      || lower.starts_with("<head")
      || lower.starts_with("<body")
      || lower.starts_with("<script")
}

fn looks_like_esm(trimmed: &str) -> bool {
   trimmed.lines().any(|line| {
      let line = line.trim_start();
      if line.starts_with("//") || line.starts_with("/*") || line.starts_with('*') {
         return false;
      }
      line.starts_with("import ")
         || line.starts_with("import{")
         || line.starts_with("import*")
         || line.starts_with("import\"")
         || line.starts_with("import'")
         || line.starts_with("export ")
         || line.starts_with("export{")
         || line.starts_with("export*")
   })
}

fn looks_like_typescript(trimmed: &str) -> bool {
   trimmed.lines().any(|line| {
      let line = line.trim_start();
      if line.starts_with("//") || line.starts_with("/*") || line.starts_with('*') {
         return false;
      }
      line.starts_with("interface ")
         || line.starts_with("type ")
         || line.starts_with("enum ")
         || line.starts_with("declare ")
         || line.contains(": string")
         || line.contains(": number")
         || line.contains(": boolean")
         || line.contains(" as const")
   })
}

fn compile_only_js(ctx: &rquickjs::Ctx<'_>, code: &str) -> Result<(), String> {
   use rquickjs::qjs;

   let Ok(c_code) = std::ffi::CString::new(code) else {
      return Err("plugin contains a NUL byte".to_owned());
   };
   let Ok(code_len) = qjs::size_t::try_from(code.len()) else {
      return Err("plugin is too large".to_owned());
   };
   let raw = ctx.as_raw().as_ptr();
   #[expect(clippy::cast_possible_wrap, reason = "QuickJS eval flags fit in c_int")]
   let flags = (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as std::ffi::c_int;
   // SAFETY: `raw` is a live `JSContext`, `c_code` is NUL-terminated with
   // matching byte length, and `JS_EVAL_FLAG_COMPILE_ONLY` parses and compiles
   // bytecode without executing any code.
   let val = unsafe {
      qjs::JS_Eval(
         raw,
         c_code.as_ptr(),
         code_len,
         c"<plugin-check>".as_ptr(),
         flags,
      )
   };
   // SAFETY: inspect the returned `JSValue` tag on this context.
   let is_err = unsafe { qjs::JS_VALUE_GET_TAG(val) == qjs::JS_TAG_EXCEPTION };
   if is_err {
      let caught = ctx.catch();
      let err = caught
         .as_exception()
         .and_then(rquickjs::Exception::message)
         .unwrap_or_else(|| format!("{caught:?}"));
      return Err(err);
   }
   // SAFETY: `val` is an owned non-exception bytecode value from `JS_Eval`.
   unsafe { qjs::JS_FreeValue(raw, val) };
   Ok(())
}

/// Validates plugin JavaScript inside an isolated temporary `QuickJS` context
/// without executing any of its code.
pub fn validate_plugin_source(source: &str) -> Result<(), Error> {
   let body = source.strip_prefix('\u{FEFF}').unwrap_or(source);
   let trimmed = body.trim();
   if trimmed.is_empty() {
      return Err(Error::InvalidPlugin {
         reason: "file is empty".to_owned(),
      });
   }
   if looks_like_html(trimmed) {
      return Err(Error::InvalidPlugin {
         reason: "downloaded an HTML page instead of a JavaScript plugin".to_owned(),
      });
   }
   if (trimmed.starts_with('{') || trimmed.starts_with('['))
      && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
   {
      return Err(Error::InvalidPlugin {
         reason: "file is JSON data, not a JavaScript plugin".to_owned(),
      });
   }

   let runtime = rquickjs::Runtime::new().map_err(|err| Error::InvalidPlugin {
      reason: format!("validator runtime failed: {err}"),
   })?;
   runtime.set_max_stack_size(256 * 1024);
   runtime.set_memory_limit(16 * 1024 * 1024);
   runtime.set_interrupt_handler(Some(Box::new(|| true)));
   let context = rquickjs::Context::full(&runtime).map_err(|err| Error::InvalidPlugin {
      reason: format!("validator context failed: {err}"),
   })?;

   // Compile-only checks across three distinct function wrappers: zero code
   // runs, and any closing-brace breakout fails at least one wrapper's grammar.
   let wrappers = [
      format!("(function (phoenix) {{\n{body}\n}})"),
      format!("function __nyaacord_check(phoenix) {{\n{body}\n}}"),
      format!("({{ __nyaacord_check(phoenix) {{\n{body}\n}} }})"),
   ];
   let check = context.with(|ctx| {
      for wrapped in &wrappers {
         compile_only_js(&ctx, wrapped)?;
      }
      Ok::<(), String>(())
   });

   if let Err(err) = check {
      let reason = if looks_like_esm(trimmed) {
         format!("ES modules (import/export) must be bundled into a single .js file ({err})")
      } else if looks_like_typescript(trimmed) {
         format!("TypeScript syntax must be compiled to plain JavaScript first ({err})")
      } else {
         format!("JavaScript syntax error: {err}")
      };
      return Err(Error::InvalidPlugin { reason });
   }
   Ok(())
}

fn write_validated_source(plugins_dir: &Path, name: &str, source: &str) -> Result<(), Error> {
   validate_plugin_source(source)?;
   let file = plugins_dir.join(format!("{name}.js"));
   let write = || {
      std::fs::create_dir_all(plugins_dir)?;
      std::fs::write(&file, source)
   };
   write().map_err(io_error(&file))
}

/// Downloads a plugin bundle into `home/plugins` and loads it, replacing a
/// plugin of the same name. Returns the plugin's name.
pub fn install(url: &str) -> Result<String, Error> {
   let (fetch_url, name) = normalize_plugin_url(url)?;
   let body = fetch::get(&fetch_url)?;
   let source = String::from_utf8(body).map_err(|_| Error::NotUtf8 { url: fetch_url })?;
   save_source(&name, source)?;
   Ok(name)
}

/// The plugin name a file name like `plugin.js` gives.
fn plugin_name(file: &str) -> Option<&str> {
   file.strip_suffix(".js").filter(|name| {
      !name.is_empty()
         && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
   })
}

/// Installs a bundle the user picked from the device, named by its file name.
/// Returns the plugin's name.
pub fn install_file(file: &str, bytes: Vec<u8>) -> Result<String, Error> {
   let name = plugin_name(file).ok_or_else(|| Error::PluginFile {
      name: file.to_owned(),
   })?;
   let source = String::from_utf8(bytes).map_err(|_| Error::NotUtf8 {
      url: file.to_owned(),
   })?;
   save_source(name, source)?;
   Ok(name.to_owned())
}

/// Validates and writes `source` to `home/plugins/<name>.js` and loads it,
/// replacing the plugin's code, bundled or not.
pub fn save_source(name: &str, source: String) -> Result<(), Error> {
   let source = if source.starts_with('\u{FEFF}') {
      source['\u{FEFF}'.len_utf8()..].to_owned()
   } else {
      source
   };
   let state = STATE.get().ok_or(Error::NotStarted)?;
   write_validated_source(&state.plugins_dir(), name, &source)?;
   if let Some(mut engine) = engine() {
      engine.install(name, source);
   }
   Ok(())
}

/// A plugin's current code.
#[must_use]
pub fn source(name: &str) -> Option<String> {
   engine()?.source(name)
}

/// Deletes a plugin installed into `home/plugins`, restoring the bundled copy
/// immediately when one exists.
pub fn uninstall(name: &str) -> Result<Removed, Error> {
   let state = STATE.get().ok_or(Error::NotStarted)?;
   let file = state.plugins_dir().join(format!("{name}.js"));
   std::fs::remove_file(&file).map_err(io_error(&file))?;
   let outcome = engine().map_or(Removed::Removed, |mut engine| engine.uninstall(name));
   Ok(outcome)
}

/// Evaluates an enabled plugin again with its hooks cleared.
pub fn reload(name: &str) {
   if let Some(mut engine) = engine() {
      engine.reload(name);
   }
}

/// Reloads every currently enabled plugin.
pub fn reload_all() {
   let names = plugins()
      .into_iter()
      .filter(|plugin| plugin.enabled)
      .map(|plugin| plugin.name)
      .collect::<Vec<_>>();
   for name in names {
      reload(&name);
   }
}

/// The first loaded library named in [`LIBRARIES`].
pub fn target() -> Result<Image<'static>, Error> {
   // SAFETY: the target is the host app's own library, which stays mapped for
   // the life of the process, and scans run before the host's threads touch its
   // data.
   let images = unsafe { Image::loaded() };
   images
      .into_iter()
      .find(|image| LIBRARIES.contains(&image.file_name()))
      .ok_or(Error::NoTarget)
}

/// Points the gateway registry's handlers at phoenix.
pub fn hook(image: &Image<'static>) -> Result<&'static GatewayRegistry, Error> {
   let walked = GatewayRegistry::locate(image)?;
   log::info!("gateway registry: {} events", walked.iter().count());
   gateway::swap(image, walked)
}

/// What [`start`] found and loaded.
pub struct Started {
   pub target: String,
   pub build: Option<BuildId>,
   pub plugins: Vec<String>,
}

/// Starts phoenix on `platform` against the first loaded library named in
/// [`LIBRARIES`]. `bundled` plugins load first, and a file in `home/plugins`
/// with the same name replaces one.
pub fn start(
   platform: &'static dyn Platform,
   home: PathBuf,
   bundled: Vec<Plugin>,
) -> Result<Started, Error> {
   let image = target()?;
   let registry = hook(&image)?;
   start_hooked(platform, image, registry, home, bundled)
}

/// [`start`] for a target whose registry [`hook`] already swapped.
pub fn start_hooked(
   platform: &'static dyn Platform,
   image: Image<'static>,
   registry: &'static GatewayRegistry,
   home: PathBuf,
   bundled: Vec<Plugin>,
) -> Result<Started, Error> {
   set_platform(platform);
   let engine = js::Engine::new(registry)?;
   let state = State {
      image,
      home,
      js: Mutex::new(engine),
   };
   STATE.set(state).map_err(|_| Error::AlreadyStarted)?;
   let state = STATE.get().expect("STATE was set above");

   let first_run = !state.choices_path().exists();
   platform.before_plugins(&state.home);
   let plugins = js::load_plugins(state, bundled)?;
   remote::ensure_started();
   platform.started(first_run, &plugins);
   Ok(Started {
      target: state.image.path.clone(),
      build: state.image.build_id,
      plugins,
   })
}

impl State {
   pub fn storage_path(&self, plugin: &str) -> PathBuf {
      self.home.join("storage").join(format!("{plugin}.json"))
   }

   pub fn choices_path(&self) -> PathBuf {
      self.home.join("plugins.json")
   }

   /// The app's data directory holding both phoenix's home and Phoenix's
   /// `store/<account>/cache.sqlite`.
   pub fn files_dir(&self) -> PathBuf {
      self
         .home
         .parent()
         .map_or_else(|| self.home.clone(), Path::to_path_buf)
   }

   pub fn plugins_dir(&self) -> PathBuf {
      self.home.join("plugins")
   }
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
   move |source| Error::Io {
      path: path.to_owned(),
      source,
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn normalizes_github_and_gist_plugin_urls() {
      assert_eq!(
         normalize_plugin_url("https://example.com/plugins/my-plugin.js").unwrap(),
         (
            "https://example.com/plugins/my-plugin.js".to_owned(),
            "my-plugin".to_owned()
         )
      );
      assert_eq!(
         normalize_plugin_url(
            "https://github.com/CateDesu/NyaaCord/blob/main/plugins/demo_1.js?plain=1#L4"
         )
         .unwrap(),
         (
            "https://raw.githubusercontent.com/CateDesu/NyaaCord/main/plugins/demo_1.js".to_owned(),
            "demo_1".to_owned()
         )
      );
      assert_eq!(
         normalize_plugin_url("https://github.com/owner/repo/raw/refs/heads/main/foo.js").unwrap(),
         (
            "https://raw.githubusercontent.com/owner/repo/refs/heads/main/foo.js".to_owned(),
            "foo".to_owned()
         )
      );
      assert_eq!(
         normalize_plugin_url("https://gist.github.com/user/abc123/raw/rev1/helper.js").unwrap(),
         (
            "https://gist.githubusercontent.com/user/abc123/raw/rev1/helper.js".to_owned(),
            "helper".to_owned()
         )
      );
      assert_eq!(
         normalize_plugin_url("https://gist.github.com/user/abc123/raw/helper.js").unwrap(),
         (
            "https://gist.githubusercontent.com/user/abc123/raw/helper.js".to_owned(),
            "helper".to_owned()
         )
      );
      assert_eq!(
         normalize_plugin_url("https://gist.githubusercontent.com/user/abc123/raw/helper.js")
            .unwrap(),
         (
            "https://gist.githubusercontent.com/user/abc123/raw/helper.js".to_owned(),
            "helper".to_owned()
         )
      );

      for bad in [
         "",
         "http://example.com/plugin.js",
         "https://example.com/plugin.ts",
         "https://example.com/plugin.html",
         "https://github.com/owner/repo",
         "https://github.com/owner/repo/blob/onlyfile.js",
         "https://github.com/owner/repo/tree/main/plugin.js",
         "https://gist.github.com/user/abc123/plugin.js",
         "https://example.com/bad name.js",
         "https:///plugin.js",
      ] {
         assert!(
            matches!(normalize_plugin_url(bad), Err(Error::PluginUrl { .. })),
            "expected {bad:?} to be rejected"
         );
      }
   }

   #[test]
   fn validates_plugin_javascript_and_rejects_bad_inputs() {
      assert!(validate_plugin_source("phoenix.log.info('ok');").is_ok());
      assert!(
         validate_plugin_source("\u{FEFF}// @version 1.0\n(() => { const x = 1; })();").is_ok()
      );
      assert!(validate_plugin_source("{ const scoped = 42; phoenix.log.info(scoped); }").is_ok());

      let check_err = |src: &str, needle: &str| {
         let err = validate_plugin_source(src).expect_err("expected validation failure");
         let msg = err.to_string();
         assert!(
            msg.to_ascii_lowercase()
               .contains(&needle.to_ascii_lowercase()),
            "expected {needle:?} in error {msg:?} for input {src:?}"
         );
      };

      check_err("   \n\t  ", "empty");
      check_err("<!DOCTYPE html><html><body>404</body></html>", "html");
      check_err("<html><head></head></html>", "html");
      check_err(r#"{"name": "not-a-plugin", "version": "1.0.0"}"#, "json");
      check_err("[1, 2, 3]", "json");
      check_err(
         "import foo from './foo.js';\nconsole.log(foo);",
         "es module",
      );
      check_err("export default function () {}", "es module");
      check_err(
         "interface Foo { a: number }\nconst x: string = 'hi';",
         "typescript",
      );
      check_err("const x: number = 123;", "typescript");
      check_err("function broken( {", "syntax error");
      // Breakout attempts must fail parsing without executing.
      check_err("}); while (true) {} ({", "syntax error");
      check_err("}); throw new Error('executed'); ({", "syntax error");
      check_err("}; while (true) {} {", "syntax error");
      check_err("}), while (true) {}, ({", "syntax error");
      check_err("}+({", "syntax error");
   }

   #[test]
   fn invalid_source_never_clobbers_existing_plugin_file() {
      let dir = std::env::temp_dir().join(format!("nyaacord-validate-test-{}", std::process::id()));
      let _ = std::fs::remove_dir_all(&dir);
      let original = "// @version 1.0\nphoenix.log.info('working');\n";
      write_validated_source(&dir, "demo", original).expect("initial write");
      let file = dir.join("demo.js");
      assert_eq!(std::fs::read_to_string(&file).unwrap(), original);

      let err = write_validated_source(&dir, "demo", "<!DOCTYPE html><html>oops</html>");
      assert!(matches!(err, Err(Error::InvalidPlugin { .. })));
      assert_eq!(std::fs::read_to_string(&file).unwrap(), original);

      let err = write_validated_source(&dir, "demo", "function broken( {");
      assert!(matches!(err, Err(Error::InvalidPlugin { .. })));
      assert_eq!(std::fs::read_to_string(&file).unwrap(), original);

      let _ = std::fs::remove_dir_all(&dir);
   }
}
