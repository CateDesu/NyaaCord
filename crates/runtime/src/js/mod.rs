pub mod background;
mod hooks;
pub mod http;
mod intercept;
pub mod messages;
mod settings;
mod storage;

use std::{
   borrow::Cow,
   cell::RefCell,
   collections::{BTreeMap, BTreeSet, HashMap},
   fs,
   io::ErrorKind,
   sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
   },
};

use log::Level;
use phoenix_sigs::registry::{EventId, GatewayRegistry};
use rquickjs::{
   CatchResultExt, CaughtError, Context, Ctx, Exception, Function, JsLifetime, Object, Runtime,
   Value,
   function::{Opt, Rest},
};

use crate::{
   Chosen, Error, Header, Plugin, PluginInfo, Removed, STATE, Setting, State, discord, gateway,
   io_error,
   js::{
      hooks::{Hook, add_gateway_api},
      http::{discord_object, start_fetch},
      messages::messages_object,
      settings::{Definition, settings_object},
      storage::{storage, stores_object},
   },
};

pub struct Engine {
   context: Context,
   runtime: Runtime,
   plugins: BTreeMap<String, Loaded>,
   choices_dirty: Arc<AtomicBool>,
   #[cfg(test)]
   choices_path: Option<std::path::PathBuf>,
}

impl Drop for Engine {
   fn drop(&mut self) {
      self.context.with(|ctx| {
         let _ = ctx.globals().remove("phoenix");
         let _ = ctx.remove_userdata::<Registry<'_>>();
      });
      self.runtime.run_gc();
   }
}

struct Loaded {
   source: String,
   /// Original bundled source shipped with the program, if any.
   bundled: Option<String>,
   enabled: bool,
   error: Option<String>,
   /// Read from `home/plugins`, so the user can remove or restore it.
   installed: bool,
}

type ChosenMap = BTreeMap<String, BTreeMap<String, Chosen>>;

/// Which plugins the user turned off or on against their default and how
/// they set each plugin's settings, kept in `home/plugins.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Choices<'a> {
   #[serde(default)]
   disabled: BTreeSet<String>,
   #[serde(default)]
   enabled: BTreeSet<String>,
   #[serde(default)]
   settings: Cow<'a, ChosenMap>,
   #[serde(default)]
   staff: bool,
   #[serde(default)]
   remote_shell: bool,
}

impl Default for Choices<'_> {
   fn default() -> Self {
      Self {
         disabled: BTreeSet::new(),
         enabled: BTreeSet::new(),
         settings: Cow::Owned(BTreeMap::new()),
         staff: false,
         remote_shell: false,
      }
   }
}

pub enum Before {
   Keep,
   Drop,
   Replace(String),
}

/// What plugins registered, kept in the runtime's userdata next to the
/// platform's own.
///
/// Callbacks stay JS values owned by the runtime, and the only thing plugins
/// can reach is their own `phoenix` object.
pub struct Registry<'js> {
   gateway: Object<'js>,
   handles: Vec<(EventId, Object<'js>)>,
   hooks: RefCell<HashMap<EventId, Vec<Hook<'js>>>>,
   defined: RefCell<BTreeMap<String, Vec<Definition<'js>>>>,
   /// Kept across unloads, so a reloaded plugin sees what the user chose.
   chosen: RefCell<ChosenMap>,
   /// `phoenix.onUnload` callbacks, run when the plugin is turned off,
   /// reloaded or removed.
   unloads: RefCell<HashMap<String, Vec<Function<'js>>>>,
   /// The resolve and reject of each promise a worker thread will settle, by
   /// id.
   pending: RefCell<HashMap<u64, (Function<'js>, Function<'js>)>>,
   /// `phoenix.discord.onRequest` interceptors, by id.
   request_hooks: RefCell<HashMap<u64, (String, Function<'js>)>>,
   /// `phoenix.discord.onResponse` interceptors, by id.
   response_hooks: RefCell<HashMap<u64, (String, Function<'js>)>>,
   /// `phoenix.messages.onSend` interceptors in registration order, by id.
   pub interceptors: RefCell<Vec<(u64, String, Function<'js>)>>,
   /// Set when JS writes to a setting returned by `phoenix.settings.define`.
   pub(super) choices_dirty: Arc<AtomicBool>,
}

// SAFETY: every field is bound to 'js, and Changed swaps exactly that lifetime.
unsafe impl<'js> JsLifetime<'js> for Registry<'js> {
   type Changed<'to> = Registry<'to>;
}

#[expect(clippy::needless_pass_by_value, reason = "used as a map_err callback")]
#[must_use]
pub fn js_error(err: CaughtError<'_>) -> Error {
   Error::Js(err.to_string())
}

#[must_use]
pub fn throw(ctx: &Ctx<'_>, err: &Error) -> rquickjs::Error {
   Exception::throw_message(ctx, &err.to_string())
}

fn state() -> &'static State {
   STATE
      .get()
      .expect("natives run after start() publishes the state")
}

impl Engine {
   pub fn new(events: &GatewayRegistry) -> Result<Self, Error> {
      let runtime = Runtime::new().map_err(|err| Error::Js(err.to_string()))?;
      runtime.set_max_stack_size(512 * 1024);
      let context = Context::full(&runtime).map_err(|err| Error::Js(err.to_string()))?;
      let choices_dirty = Arc::new(AtomicBool::new(false));
      context.with(|ctx| {
         let registry = registry(&ctx, events, Arc::clone(&choices_dirty))
            .catch(&ctx)
            .map_err(js_error)?;
         ctx.store_userdata(registry)
            .map_err(|err| Error::Js(err.to_string()))?;
         if let Some(platform) = crate::platform() {
            platform.init(&ctx).catch(&ctx).map_err(js_error)?;
         }
         Ok::<_, Error>(())
      })?;
      Ok(Self {
         runtime,
         context,
         plugins: BTreeMap::new(),
         choices_dirty,
         #[cfg(test)]
         choices_path: None,
      })
   }

   pub(super) fn take_choices_dirty(&self) -> bool {
      self.choices_dirty.swap(false, Ordering::AcqRel)
   }

   pub(super) fn drain_and_flush(&self) {
      drain(&self.runtime);
      if self.take_choices_dirty() {
         self.save_choices();
      }
   }

   /// Runs `work` in the engine's context, then the promise reactions it
   /// queued.
   pub fn with<R>(&self, work: impl FnOnce(Ctx<'_>) -> R) -> R {
      let result = self.context.with(work);
      self.drain_and_flush();
      result
   }

   #[must_use]
   pub fn source(&self, name: &str) -> Option<String> {
      self.plugins.get(name).map(|loaded| loaded.source.clone())
   }

   #[must_use]
   pub fn plugins(&self) -> Vec<PluginInfo> {
      self.context.with(|ctx| {
         let registry = ctx.userdata::<Registry<'_>>();
         let defined = registry.as_ref().map(|registry| registry.defined.borrow());
         let chosen = registry.as_ref().map(|registry| registry.chosen.borrow());
         self
            .plugins
            .iter()
            .map(|(name, loaded)| {
               let settings = defined
                  .as_ref()
                  .and_then(|defined| defined.get(name))
                  .into_iter()
                  .flatten()
                  .map(|definition| {
                     let picked = chosen
                        .as_ref()
                        .and_then(|chosen| chosen.get(name)?.get(&definition.key));
                     Setting {
                        key: definition.key.clone(),
                        title: definition.title.clone(),
                        description: definition.description.clone(),
                        control: definition.kind.current(picked),
                     }
                  })
                  .collect();
               PluginInfo {
                  name: name.clone(),
                  enabled: loaded.enabled,
                  error: loaded.error.clone(),
                  installed: loaded.installed,
                  bundled: loaded.bundled.is_some(),
                  header: Header::from(loaded.source.as_str()),
                  settings,
               }
            })
            .collect()
      })
   }

   /// Evaluates `name` with a fresh `phoenix` object, recording any error.
   fn eval(&mut self, name: &str) {
      let Some(loaded) = self.plugins.get_mut(name) else {
         return;
      };
      let outcome = self.context.with(|ctx| {
         let run = || -> rquickjs::Result<()> {
            let phoenix = api(&ctx, name)?;
            let wrapped = format!("(function (phoenix) {{\n{}\n}})", loaded.source);
            ctx.eval::<Function<'_>, _>(wrapped)?.call((phoenix,))
         };
         run().catch(&ctx).map_err(js_error)
      });
      match outcome {
         Ok(()) => {
            log::info!("loaded plugin {name}");
            loaded.error = None;
         },
         Err(err) => {
            log::error!("plugin {name} failed to load: {err}");
            loaded.error = Some(err.to_string());
         },
      }
      self.drain_and_flush();
   }

   /// Drops every hook `name` registered, and turns off events nobody else
   /// listens to.
   fn unload(&self, name: &str) {
      self.context.with(|ctx| {
         let Some(registry) = ctx.userdata::<Registry<'_>>() else {
            return;
         };
         let cleanups = registry
            .unloads
            .borrow_mut()
            .remove(name)
            .unwrap_or_default();
         for cleanup in cleanups {
            if let Err(err) = cleanup.call::<_, ()>(()).catch(&ctx) {
               log::error!("[{name}] onUnload: {err}");
            }
         }
         registry.defined.borrow_mut().remove(name);
         registry
            .request_hooks
            .borrow_mut()
            .retain(|_, (plugin, _)| plugin != name);
         discord::forget_request_hooks(name);
         registry
            .response_hooks
            .borrow_mut()
            .retain(|_, (plugin, _)| plugin != name);
         discord::forget_response_hooks(name);
         registry
            .interceptors
            .borrow_mut()
            .retain(|(_, plugin, _)| plugin != name);
         if let Some(platform) = crate::platform() {
            platform.unload(&ctx, name);
         }
         let mut hooks = registry.hooks.borrow_mut();
         for (event, list) in hooks.iter_mut() {
            list.retain(|hook| hook.plugin != name);
            if list.is_empty() {
               gateway::uninstall(*event);
            }
         }
         hooks.retain(|_, list| !list.is_empty());
      });
      self.drain_and_flush();
   }

   pub fn set_enabled(&mut self, name: &str, enabled: bool) {
      let Some(loaded) = self.plugins.get_mut(name) else {
         return;
      };
      if loaded.enabled == enabled {
         return;
      }
      loaded.enabled = enabled;
      loaded.error = None;
      if enabled {
         self.eval(name);
      } else {
         self.unload(name);
      }
      self.save_choices();
   }

   /// Adds `name` or replaces its source, then runs it unless the user turned
   /// it off.
   pub fn install(&mut self, name: &str, source: String) {
      let enabled = if let Some(loaded) = self.plugins.get_mut(name) {
         loaded.source = source;
         loaded.installed = true;
         loaded.enabled
      } else {
         self.plugins.insert(
            name.to_owned(),
            Loaded {
               source,
               bundled: None,
               enabled: true,
               error: None,
               installed: true,
            },
         );
         true
      };
      if enabled {
         self.unload(name);
         self.eval(name);
      }
   }

   /// Drops a user-installed plugin or restores a bundled plugin immediately
   /// while preserving its enabled state and saved settings.
   pub fn uninstall(&mut self, name: &str) -> Removed {
      self.unload(name);
      if let Some(loaded) = self.plugins.get_mut(name)
         && let Some(original) = loaded.bundled.clone()
      {
         loaded.source = original;
         loaded.installed = false;
         loaded.error = None;
         let enabled = loaded.enabled;
         if enabled {
            self.eval(name);
         }
         self.save_choices();
         return Removed::Restored;
      }
      self.plugins.remove(name);
      self.context.with(|ctx| {
         if let Some(registry) = ctx.userdata::<Registry<'_>>() {
            registry.chosen.borrow_mut().remove(name);
         }
      });
      self.save_choices();
      Removed::Removed
   }

   pub fn reload(&mut self, name: &str) {
      if self.plugins.get(name).is_some_and(|loaded| loaded.enabled) {
         self.unload(name);
         self.eval(name);
      }
   }

   pub fn save_choices(&self) {
      let _ = self.take_choices_dirty();
      #[cfg(test)]
      let path = self
         .choices_path
         .clone()
         .or_else(|| STATE.get().map(State::choices_path));
      #[cfg(not(test))]
      let path = STATE.get().map(State::choices_path);
      let Some(path) = path else {
         return;
      };
      let (mut disabled, mut enabled) = (BTreeSet::new(), BTreeSet::new());
      for (name, loaded) in &self.plugins {
         if Header::from(loaded.source.as_str()).default_off {
            if loaded.enabled {
               enabled.insert(name.clone());
            }
         } else if !loaded.enabled {
            disabled.insert(name.clone());
         }
      }
      let remote_shell = crate::remote_shell_enabled();
      let json = self.context.with(|ctx| {
         let registry = ctx.userdata::<Registry<'_>>();
         let chosen = registry.as_ref().map(|registry| registry.chosen.borrow());
         let empty = BTreeMap::new();
         serde_json::to_vec(&Choices {
            disabled,
            enabled,
            settings: Cow::Borrowed(chosen.as_deref().unwrap_or(&empty)),
            staff: crate::staff_enabled(),
            remote_shell,
         })
      });
      let write = || -> std::io::Result<()> {
         fs::create_dir_all(path.parent().expect("the choices path has a parent"))?;
         fs::write(&path, json?)
      };
      if let Err(err) = write() {
         log::error!("{}: {err}", path.display());
      }
   }

   /// The result as JSON when it has a JSON form, else as `String(result)`.
   /// The prompt sees a `phoenix` object of its own, named "console".
   pub fn evaluate(&self, code: &str) -> Result<String, Error> {
      let result = self.context.with(|ctx| {
         let run = || -> rquickjs::Result<String> {
            if !ctx.globals().contains_key("phoenix")? {
               let phoenix = api(&ctx, "console")?;
               if let Some(platform) = crate::platform() {
                  platform.console(&ctx, &phoenix)?;
               }
               ctx.globals().set("phoenix", phoenix)?;
            }
            let result = ctx.eval::<Value<'_>, _>(code)?;
            if let Some(text) = result.as_string() {
               return text.to_string();
            }
            match ctx.json_stringify(result.clone())? {
               Some(json) => json.to_string(),
               None => ctx
                  .globals()
                  .get::<_, Function<'_>>("String")?
                  .call((result,)),
            }
         };
         run().catch(&ctx).map_err(js_error)
      });
      self.drain_and_flush();
      result
   }
}

/// Runs promise reactions until none are left. `QuickJS` queues them, and
/// nothing else runs the queue.
pub fn drain(runtime: &Runtime) {
   loop {
      match runtime.execute_pending_job() {
         Ok(true) => {},
         Ok(false) => break,
         Err(err) => log::error!("a promise job threw: {err}"),
      }
   }
}

/// One frozen handle per live registry entry. Plugins compare nothing and parse
/// nothing, the runtime recognizes a handle by identity.
fn registry<'js>(
   ctx: &Ctx<'js>,
   events: &GatewayRegistry,
   choices_dirty: Arc<AtomicBool>,
) -> rquickjs::Result<Registry<'js>> {
   let freeze = ctx
      .globals()
      .get::<_, Object>("Object")?
      .get::<_, Function>("freeze")?;
   let gateway = Object::new(ctx.clone())?;
   let mut handles = Vec::new();
   for (event, entry) in events.iter() {
      let key = camel_case(&entry.name);
      let handle = Object::new(ctx.clone())?;
      let label = format!("gateway.{key}");
      handle.set(
         "toString",
         Function::new(ctx.clone(), move || label.clone())?,
      )?;
      let handle = freeze.call::<_, Object<'js>>((handle,))?;
      gateway.set(key, handle.clone())?;
      handles.push((event, handle));
   }
   let gateway = freeze.call((gateway,))?;
   Ok(Registry {
      gateway,
      handles,
      hooks: RefCell::default(),
      defined: RefCell::default(),
      chosen: RefCell::default(),
      unloads: RefCell::default(),
      pending: RefCell::default(),
      request_hooks: RefCell::default(),
      response_hooks: RefCell::default(),
      interceptors: RefCell::default(),
      choices_dirty,
   })
}

/// `MESSAGE_CREATE` to `messageCreate`, the key plugins and `phoenix.d.ts` use.
fn camel_case(name: &str) -> String {
   name
      .split('_')
      .enumerate()
      .map(|(index, word)| {
         let lower = word.to_ascii_lowercase();
         let mut chars = lower.chars();
         match chars.next() {
            Some(first) if index > 0 => first.to_ascii_uppercase().to_string() + chars.as_str(),
            _ => lower,
         }
      })
      .collect()
}

/// The `phoenix` object one plugin receives.
fn api<'js>(ctx: &Ctx<'js>, plugin: &str) -> rquickjs::Result<Object<'js>> {
   let api = Object::new(ctx.clone())?;
   let gateway = ctx
      .userdata::<Registry<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
   api.set("gateway", gateway.gateway.clone())?;
   drop(gateway);

   add_gateway_api(ctx, plugin, &api)?;

   let messages = messages_object(ctx)?;
   if let Some(platform) = crate::platform() {
      platform.api(ctx, plugin, &api, &messages)?;
   }
   api.set("messages", messages)?;
   api.set("log", log_object(ctx, plugin)?)?;
   api.set("storage", storage(ctx, plugin)?)?;
   api.set("settings", settings_object(ctx, plugin)?)?;
   let name = plugin.to_owned();
   api.set(
      "onUnload",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>, cleanup: Function<'js>| {
         ctx.userdata::<Registry<'js>>()
            .ok_or_else(|| Exception::throw_internal(&ctx, "no registry"))?
            .unloads
            .borrow_mut()
            .entry(name.clone())
            .or_default()
            .push(cleanup);
         Ok::<_, rquickjs::Error>(())
      })?,
   )?;
   api.set("stores", stores_object(ctx)?)?;
   api.set("discord", discord_object(ctx, plugin)?)?;
   api.set(
      "fetch",
      Function::new(
         ctx.clone(),
         |ctx: Ctx<'js>, url: String, init: Opt<Object<'js>>| start_fetch(&ctx, url, init.0),
      )?,
   )?;
   Ok(api)
}

/// `phoenix.log`, whose lines carry the plugin's name.
fn log_object<'js>(ctx: &Ctx<'js>, plugin: &str) -> rquickjs::Result<Object<'js>> {
   let log = Object::new(ctx.clone())?;
   for (method, level) in [
      ("debug", Level::Debug),
      ("info", Level::Info),
      ("warn", Level::Warn),
      ("error", Level::Error),
   ] {
      let name = plugin.to_owned();
      log.set(
         method,
         Function::new(ctx.clone(), move |ctx: Ctx<'js>, args: Rest<Value<'js>>| {
            let parts = args
               .0
               .into_iter()
               .map(|arg| {
                  if let Some(text) = arg.as_string() {
                     return text.to_string();
                  }
                  ctx.json_stringify(arg)?
                     .map_or_else(|| Ok("undefined".to_owned()), |json| json.to_string())
               })
               .collect::<rquickjs::Result<Vec<_>>>()?;
            log::log!(level, "[{name}] {}", parts.join(" "));
            Ok::<_, rquickjs::Error>(())
         })?,
      )?;
   }
   Ok(log)
}

/// Evaluates `bundled` plus every `*.js` file in the plugins directory, in name
/// order, skipping the ones the user turned off.
#[expect(
   clippy::significant_drop_tightening,
   reason = "plugins load under one engine lock so no gateway event sees a half-loaded set"
)]
pub fn load_plugins(state: &'static State, bundled: Vec<Plugin>) -> Result<Vec<String>, Error> {
   let choices_path = state.choices_path();
   let choices = match fs::read(&choices_path) {
      Ok(bytes) => serde_json::from_slice::<Choices>(&bytes).unwrap_or_else(|err| {
         log::error!("{}: {err}", choices_path.display());
         Choices::default()
      }),
      Err(err) if err.kind() == ErrorKind::NotFound => Choices::default(),
      Err(err) => return Err(io_error(&choices_path)(err)),
   };
   crate::STAFF.store(choices.staff, std::sync::atomic::Ordering::Release);
   crate::set_remote_shell_flag(choices.remote_shell);
   if let Some(platform) = crate::platform() {
      platform.staff(&state.image);
   }

   let dir = state.plugins_dir();
   let mut bundled_sources = BTreeMap::new();
   let mut sources = BTreeMap::new();
   for plugin in bundled {
      bundled_sources.insert(plugin.name.clone(), plugin.source.clone());
      sources.insert(plugin.name, (plugin.source, false));
   }
   let paths = match fs::read_dir(&dir) {
      Ok(entries) => entries
         .collect::<Result<Vec<_>, _>>()
         .map_err(io_error(&dir))?,
      Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
      Err(err) => return Err(io_error(&dir)(err)),
   };
   for path in paths.iter().map(fs::DirEntry::path) {
      let Some(name) = path
         .file_stem()
         .and_then(|stem| stem.to_str())
         .filter(|_| path.extension().is_some_and(|ext| ext == "js"))
      else {
         continue;
      };
      let source = fs::read_to_string(&path).map_err(io_error(&path))?;
      sources.insert(name.to_owned(), (source, true));
   }

   let mut engine = state
      .js
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
   engine.context.with(|ctx| {
      if let Some(registry) = ctx.userdata::<Registry<'_>>() {
         *registry.chosen.borrow_mut() = choices.settings.into_owned();
      }
   });
   engine.plugins = sources
      .into_iter()
      .map(|(name, (source, installed))| {
         let enabled = if Header::from(source.as_str()).default_off {
            choices.enabled.contains(&name)
         } else {
            !choices.disabled.contains(&name)
         };
         let bundled = bundled_sources.remove(&name);
         (
            name,
            Loaded {
               source,
               bundled,
               enabled,
               error: None,
               installed,
            },
         )
      })
      .collect();
   let enabled = engine
      .plugins
      .iter()
      .filter(|(_, loaded)| loaded.enabled)
      .map(|(name, _)| name.clone())
      .collect::<Vec<String>>();
   for name in &enabled {
      engine.eval(name);
   }
   Ok(enabled
      .into_iter()
      .filter(|name| {
         engine
            .plugins
            .get(name)
            .is_some_and(|loaded| loaded.error.is_none())
      })
      .collect())
}

#[cfg(test)]
mod tests {
   use super::*;

   fn engine_with_bundled(name: &str, source: &str, enabled: bool) -> Engine {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.plugins.insert(
         name.to_owned(),
         Loaded {
            source: source.to_owned(),
            bundled: Some(source.to_owned()),
            enabled,
            error: None,
            installed: false,
         },
      );
      if enabled {
         engine.eval(name);
      }
      engine
   }

   #[test]
   fn uninstall_restores_bundled_plugin_and_preserves_state_and_settings() {
      let bundled = r#"
         globalThis.__tag = "bundled";
         const s = phoenix.settings.define({
            mode: { type: "text", title: "Mode", default: "a" }
         });
         globalThis.__mode = s.mode;
      "#;
      let overridden = r#"
         globalThis.__tag = "override";
         const s = phoenix.settings.define({
            mode: { type: "text", title: "Mode", default: "a" }
         });
         globalThis.__mode = s.mode;
      "#;

      let mut engine = engine_with_bundled("demo", bundled, true);
      engine.set_setting("demo", "mode", Chosen::Text("custom".to_owned()));
      engine.install("demo", overridden.to_owned());

      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "demo")
         .unwrap();
      assert!(info.installed);
      assert!(info.bundled);
      assert!(info.enabled);
      assert_eq!(engine.evaluate("globalThis.__tag").unwrap(), "override");
      assert_eq!(engine.evaluate("globalThis.__mode").unwrap(), "custom");

      assert_eq!(engine.uninstall("demo"), Removed::Restored);

      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "demo")
         .unwrap();
      assert!(!info.installed);
      assert!(info.bundled);
      assert!(info.enabled);
      assert_eq!(engine.source("demo").as_deref(), Some(bundled));
      assert_eq!(engine.evaluate("globalThis.__tag").unwrap(), "bundled");
      assert_eq!(engine.evaluate("globalThis.__mode").unwrap(), "custom");
   }

   #[test]
   fn uninstall_restores_disabled_bundled_plugin_without_running_it() {
      let bundled = "globalThis.__ran = (globalThis.__ran || 0) + 1;";
      let overridden = "globalThis.__ran = 99;";

      let mut engine = engine_with_bundled("demo", bundled, false);
      engine.install("demo", overridden.to_owned());
      assert_eq!(engine.uninstall("demo"), Removed::Restored);

      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "demo")
         .unwrap();
      assert!(!info.installed);
      assert!(info.bundled);
      assert!(!info.enabled);
      assert_eq!(engine.source("demo").as_deref(), Some(bundled));
      assert_eq!(
         engine.evaluate("typeof globalThis.__ran").unwrap(),
         "undefined"
      );
   }

   #[test]
   fn uninstall_removes_non_bundled_plugin_and_clears_settings() {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.install(
         "custom",
         r#"phoenix.settings.define({ flag: { type: "switch", title: "Flag", default: false } });"#
            .to_owned(),
      );
      engine.set_setting("custom", "flag", Chosen::Switch(true));

      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "custom")
         .unwrap();
      assert!(info.installed);
      assert!(!info.bundled);

      assert_eq!(engine.uninstall("custom"), Removed::Removed);
      assert!(engine.source("custom").is_none());
      assert!(engine.plugins().is_empty());

      // Reinstalling later must start from default settings, not stale ones.
      engine.install(
         "custom",
         r#"
            const s = phoenix.settings.define({ flag: { type: "switch", title: "Flag", default: false } });
            globalThis.__flag = s.flag;
         "#
         .to_owned(),
      );
      assert_eq!(engine.evaluate("globalThis.__flag").unwrap(), "false");
   }

   fn temp_choices_path(label: &str) -> std::path::PathBuf {
      static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
      let n = SEQ.fetch_add(1, Ordering::Relaxed);
      std::env::temp_dir()
         .join(format!("nyaacord-js-{label}-{}-{n}", std::process::id()))
         .join("plugins.json")
   }

   #[test]
   fn rejects_non_finite_number_settings_and_preserves_valid_plugins_json() {
      let choices_path = temp_choices_path("non-finite");
      let dir = choices_path.parent().unwrap().to_path_buf();
      let _ = fs::remove_dir_all(&dir);

      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.choices_path = Some(choices_path.clone());
      engine.install(
         "numtest",
         r#"
            globalThis.__s = phoenix.settings.define({
               limit: { type: "number", title: "Limit", default: 10 }
            });
            globalThis.__s.limit = 42.5;
         "#
         .to_owned(),
      );

      for expr in [
         "globalThis.__s.limit = NaN",
         "globalThis.__s.limit = Infinity",
         "globalThis.__s.limit = -Infinity",
      ] {
         let err = engine
            .evaluate(expr)
            .expect_err("non-finite float must throw");
         assert!(
            err.to_string().contains("finite number expected"),
            "expected finite number error for {expr}, got {err}"
         );
         let is_range = engine
            .evaluate(&format!(
               "(() => {{ try {{ {expr}; return false; }} catch (e) {{ return e instanceof RangeError; }} }})()"
            ))
            .expect("catch RangeError");
         assert_eq!(is_range, "true", "expected RangeError for {expr}");
      }

      assert_eq!(engine.evaluate("globalThis.__s.limit").unwrap(), "42.5");
      let bytes = fs::read(&choices_path).expect("plugins.json written");
      let parsed: Choices<'_> =
         serde_json::from_slice(&bytes).expect("plugins.json must remain valid JSON");
      assert_eq!(
         parsed.settings.get("numtest").and_then(|m| m.get("limit")),
         Some(&Chosen::Number(42.5))
      );

      let _ = fs::remove_dir_all(&dir);
   }

   #[test]
   fn flushes_settings_mutations_from_press_and_evaluate() {
      let choices_path = temp_choices_path("flush-entry");
      let dir = choices_path.parent().unwrap().to_path_buf();
      let _ = fs::remove_dir_all(&dir);

      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.choices_path = Some(choices_path.clone());
      engine.install(
         "flushdemo",
         r#"
            const s = phoenix.settings.define({
               mode: { type: "text", title: "Mode", default: "init" },
               count: { type: "number", title: "Count", default: 0 },
               bump: {
                  type: "button",
                  title: "Bump",
                  onPress() {
                     s.mode = "pressed";
                     s.count = 7;
                  }
               }
            });
            globalThis.__flushDemo = s;
         "#
         .to_owned(),
      );

      assert!(!choices_path.exists());

      engine.press("flushdemo", "bump");
      let after_press: Choices<'_> =
         serde_json::from_slice(&fs::read(&choices_path).expect("plugins.json after press"))
            .expect("valid json after press");
      assert_eq!(
         after_press
            .settings
            .get("flushdemo")
            .and_then(|m| m.get("mode")),
         Some(&Chosen::Text("pressed".to_owned()))
      );
      assert_eq!(
         after_press
            .settings
            .get("flushdemo")
            .and_then(|m| m.get("count")),
         Some(&Chosen::Number(7.0))
      );

      engine
         .evaluate("globalThis.__flushDemo.mode = 'evaluated'; globalThis.__flushDemo.count = 9;")
         .expect("evaluate setting update");
      let after_eval: Choices<'_> =
         serde_json::from_slice(&fs::read(&choices_path).expect("plugins.json after evaluate"))
            .expect("valid json after evaluate");
      assert_eq!(
         after_eval
            .settings
            .get("flushdemo")
            .and_then(|m| m.get("mode")),
         Some(&Chosen::Text("evaluated".to_owned()))
      );
      assert_eq!(
         after_eval
            .settings
            .get("flushdemo")
            .and_then(|m| m.get("count")),
         Some(&Chosen::Number(9.0))
      );

      let _ = fs::remove_dir_all(&dir);
   }
}
