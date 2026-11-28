pub mod background;
mod hooks;
pub mod http;
mod intercept;
mod lockdown;
pub mod messages;
mod settings;
mod storage;

#[doc(hidden)]
pub use settings::settings_object;

use std::{
   borrow::Cow,
   cell::{Cell, RefCell},
   collections::{BTreeMap, BTreeSet, HashMap},
   fs,
   io::ErrorKind,
   sync::{
      Arc,
      atomic::{AtomicBool, AtomicU64, Ordering},
   },
   time::{Duration, Instant},
};

use log::Level;
use phoenix_sigs::registry::{EventId, GatewayRegistry};
use rquickjs::{
   CatchResultExt, CaughtError, Context, Ctx, Exception, Function, JsLifetime, Object, Runtime,
   Value,
   function::{Opt, Rest},
};

use crate::{
   Chosen, Error, Header, Plugin, PluginInfo, PluginState, Removed, STATE, Setting, State, discord,
   gateway, io_error,
   js::{
      hooks::{Hook, add_gateway_api},
      http::{discord_object, start_fetch},
      lockdown::lockdown,
      messages::messages_object,
      settings::Definition,
      storage::{storage, stores_object},
   },
};

/// How long one plugin callback or promise job runs before `QuickJS`
/// interrupts it. The hooks run on Discord's own threads, so a spinning plugin
/// freezes the app for this long.
const BUDGET: Duration = Duration::from_secs(1);

/// The heap every plugin shares. Past it allocations throw, so a runaway
/// plugin fails before the OS kills Discord for its memory.
const HEAP_LIMIT: usize = 128 << 20;

/// Drains in a row that may run out of budget before [`Engine::starve`] runs.
const OVERRUNS: u32 = 3;

/// The name the debug shell's `phoenix` object goes by, which no plugin may
/// take.
const CONSOLE: &str = "console";

pub struct Engine {
   /// The debug shell's prompt, a realm of its own so its unrestricted
   /// `phoenix` never sits in the plugins' frozen globals. Dropped before
   /// `context` so any cross-context references it holds release first.
   console: Context,
   context: Context,
   runtime: Runtime,
   budget: Arc<Budget>,
   /// Drains in a row that ran out of budget with jobs left.
   overruns: Cell<u32>,
   plugins: BTreeMap<String, Loaded>,
   choices_dirty: Arc<AtomicBool>,
   choices_path: Option<std::path::PathBuf>,
}

impl Drop for Engine {
   fn drop(&mut self) {
      self.budget.restart();
      self.console.with(|ctx| {
         let _ = ctx.globals().remove("phoenix");
      });
      self.context.with(|ctx| {
         let _ = ctx.remove_userdata::<Registry<'_>>();
      });
      self.runtime.run_gc();
   }
}

/// The deadline `QuickJS`'s interrupt handler checks, which every entry into
/// the engine and every plugin callback pushes out to [`BUDGET`] from now.
struct Budget {
   origin: Instant,
   deadline: AtomicU64,
}

impl Budget {
   fn nanos(&self) -> u64 {
      u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
   }

   fn restart(&self) {
      let budget = u64::try_from(BUDGET.as_nanos()).unwrap_or(u64::MAX);
      self
         .deadline
         .store(self.nanos().saturating_add(budget), Ordering::Relaxed);
   }

   fn spent(&self) -> bool {
      self.nanos() > self.deadline.load(Ordering::Relaxed)
   }
}

struct Loaded {
   source: String,
   origin: Origin,
   state: PluginState,
}

enum Origin {
   Bundled,
   /// Read from `home/plugins`, so the user can delete it.
   Installed,
   /// Installed over a bundled plugin, whose source is put back when the copy
   /// is uninstalled.
   Replacing(String),
}

impl Loaded {
   const fn new(source: String, origin: Origin) -> Self {
      Self {
         source,
         origin,
         state: PluginState::Off,
      }
   }

   fn install(&mut self, source: String) {
      match &self.origin {
         Origin::Bundled if crate::same_plugin_source(&source, &self.source) => {},
         Origin::Bundled => {
            let old = std::mem::replace(&mut self.source, source);
            self.origin = Origin::Replacing(old);
         },
         Origin::Replacing(bundled) if crate::same_plugin_source(&source, bundled) => {
            let Origin::Replacing(bundled) = std::mem::replace(&mut self.origin, Origin::Bundled)
            else {
               unreachable!();
            };
            self.source = bundled;
         },
         Origin::Replacing(_) | Origin::Installed => {
            self.source = source;
         },
      }
   }

   const fn bundled_source(&self) -> Option<&str> {
      match &self.origin {
         Origin::Bundled => Some(self.source.as_str()),
         Origin::Replacing(bundled) => Some(bundled.as_str()),
         Origin::Installed => None,
      }
   }
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
   budget: Arc<Budget>,
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

/// A plugin's source as the body of a function of its `phoenix` object.
///
/// The realm's `Function` constructor parses it as a function body, so the
/// source cannot close the function early and declare script-level bindings
/// every other plugin's free names would resolve to.
pub fn compile<'js>(ctx: &Ctx<'js>, source: &str) -> rquickjs::Result<Function<'js>> {
   ctx.globals()
      .get::<_, Function<'js>>("Function")?
      .call(("phoenix", source))
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
      runtime.set_memory_limit(HEAP_LIMIT);
      let budget = Arc::new(Budget {
         origin: Instant::now(),
         deadline: AtomicU64::new(0),
      });
      let watched = Arc::clone(&budget);
      runtime.set_interrupt_handler(Some(Box::new(move || watched.spent())));
      let context = Context::full(&runtime).map_err(|err| Error::Js(err.to_string()))?;
      let console = Context::full(&runtime).map_err(|err| Error::Js(err.to_string()))?;
      let choices_dirty = Arc::new(AtomicBool::new(false));
      budget.restart();
      context.with(|ctx| {
         let registry = registry(
            &ctx,
            events,
            Arc::clone(&budget),
            Arc::clone(&choices_dirty),
         )
         .catch(&ctx)
         .map_err(js_error)?;
         let mut extra = Vec::with_capacity(registry.handles.len() + 1);
         extra.push(registry.gateway.clone().into_value());
         for (_, handle) in &registry.handles {
            extra.push(handle.clone().into_value());
         }
         ctx.store_userdata(registry)
            .map_err(|err| Error::Js(err.to_string()))?;
         if let Some(platform) = crate::platform() {
            platform.init(&ctx).catch(&ctx).map_err(js_error)?;
         }
         lockdown(&ctx, extra).catch(&ctx).map_err(js_error)
      })?;
      Ok(Self {
         console,
         context,
         runtime,
         budget,
         overruns: Cell::new(0),
         plugins: BTreeMap::new(),
         choices_dirty,
         choices_path: None,
      })
   }

   #[doc(hidden)]
   pub fn set_choices_path(&mut self, path: std::path::PathBuf) {
      self.choices_path = Some(path);
   }

   pub(super) fn take_choices_dirty(&self) -> bool {
      self.choices_dirty.swap(false, Ordering::AcqRel)
   }

   /// Runs promise reactions until none are left. `QuickJS` queues them, and
   /// nothing else runs the queue. They share one budget, and the jobs left
   /// when it runs out wait for the next entry. Only a queue still growing
   /// after [`OVERRUNS`] entries is starved, since starving fails every
   /// plugin's pending jobs.
   fn drain(&self) {
      self.budget.restart();
      while !self.budget.spent() {
         match self.runtime.execute_pending_job() {
            Ok(true) => {},
            Ok(false) => {
               self.overruns.set(0);
               return;
            },
            Err(err) => log::error!("a promise job threw: {err}"),
         }
      }
      if !self.runtime.is_job_pending() {
         self.overruns.set(0);
         return;
      }
      let overruns = self.overruns.get() + 1;
      if overruns < OVERRUNS {
         self.overruns.set(overruns);
         log::warn!("promise jobs ran past {BUDGET:?}, the rest wait for the next call");
         return;
      }
      self.overruns.set(0);
      self.starve();
   }

   /// Runs out the queue with every allocation failing. A job that would
   /// queue another cannot, so the queue only shrinks, where a promise that
   /// queues two more on every run would outgrow the budget forever. A cap at
   /// the heap's current size let each job reuse what the last one freed.
   fn starve(&self) {
      self.runtime.set_memory_limit(1);
      let mut failed = 0_u64;
      loop {
         match self.runtime.execute_pending_job() {
            Ok(true) => {},
            Ok(false) => break,
            Err(_) => failed += 1,
         }
      }
      self.runtime.set_memory_limit(HEAP_LIMIT);
      log::error!("promise jobs ran past {BUDGET:?}, {failed} more failed to finish");
   }

   pub(super) fn drain_and_flush(&self) {
      self.drain();
      if self.take_choices_dirty() {
         self.save_choices();
      }
   }

   /// Runs `work` in the engine's context, then the promise reactions it
   /// queued.
   pub fn with<R>(&self, work: impl FnOnce(Ctx<'_>) -> R) -> R {
      self.budget.restart();
      let result = self.context.with(work);
      self.drain_and_flush();
      result
   }

   #[must_use]
   pub fn source(&self, name: &str) -> Option<String> {
      self.plugins.get(name).map(|loaded| loaded.source.clone())
   }

   #[must_use]
   pub fn bundled_source(&self, name: &str) -> Option<String> {
      self
         .plugins
         .get(name)
         .and_then(|loaded| loaded.bundled_source().map(str::to_owned))
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
                  enabled: loaded.state.is_on(),
                  error: loaded.state.error().map(str::to_owned),
                  installed: !matches!(loaded.origin, Origin::Bundled),
                  bundled: !matches!(loaded.origin, Origin::Installed),
                  header: Header::from(loaded.source.as_str()),
                  settings,
               }
            })
            .collect()
      })
   }

   /// Evaluates `name` with a fresh `phoenix` object, recording any error.
   fn eval(&mut self, name: &str) {
      let Some(source) = self.plugins.get(name).map(|loaded| loaded.source.clone()) else {
         return;
      };
      let source = source.strip_prefix('\u{FEFF}').unwrap_or(&source);
      let outcome = if name == CONSOLE {
         Err(Error::Js(format!(
            "the name {CONSOLE} belongs to the debug shell"
         )))
      } else {
         self.with(|ctx| {
            let run = || -> rquickjs::Result<()> {
               let phoenix = api(&ctx, name)?;
               compile(&ctx, source)?.call((phoenix,))
            };
            run().catch(&ctx).map_err(js_error)
         })
      };
      let Some(loaded) = self.plugins.get_mut(name) else {
         return;
      };
      match outcome {
         Ok(()) => {
            log::info!("loaded plugin {name}");
            loaded.state = PluginState::On;
         },
         Err(err) => {
            log::error!("plugin {name} failed to load: {err}");
            loaded.state = PluginState::Failed(err.to_string());
         },
      }
   }

   /// Drops every hook `name` registered, and turns off events nobody else
   /// listens to.
   fn unload(&self, name: &str) {
      if name == CONSOLE {
         return;
      }
      self.with(|ctx| {
         let Some(registry) = ctx.userdata::<Registry<'_>>() else {
            return;
         };
         let cleanups = registry
            .unloads
            .borrow_mut()
            .remove(name)
            .unwrap_or_default();
         for cleanup in cleanups {
            restart_budget(&ctx);
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
   }

   pub fn set_enabled(&mut self, name: &str, enabled: bool) {
      let Some(loaded) = self.plugins.get_mut(name) else {
         return;
      };
      if loaded.state.is_on() == enabled {
         return;
      }
      loaded.state = PluginState::from_switch(enabled);
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
         loaded.install(source);
         loaded.state.is_on()
      } else {
         let mut loaded = Loaded::new(source, Origin::Installed);
         loaded.state = PluginState::On;
         self.plugins.insert(name.to_owned(), loaded);
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
      let restored = self.plugins.get_mut(name).and_then(|loaded| {
         match std::mem::replace(&mut loaded.origin, Origin::Bundled) {
            Origin::Replacing(bundled) => {
               loaded.source = bundled;
               let enabled = loaded.state.is_on();
               loaded.state = PluginState::from_switch(enabled);
               Some(enabled)
            },
            Origin::Bundled => {
               let enabled = loaded.state.is_on();
               loaded.state = PluginState::from_switch(enabled);
               Some(enabled)
            },
            Origin::Installed => None,
         }
      });
      let outcome = if let Some(enabled) = restored {
         if enabled {
            self.eval(name);
         }
         Removed::Restored
      } else {
         self.plugins.remove(name);
         if name != CONSOLE {
            self.context.with(|ctx| {
               if let Some(registry) = ctx.userdata::<Registry<'_>>() {
                  registry.chosen.borrow_mut().remove(name);
               }
            });
         }
         Removed::Removed
      };
      self.save_choices();
      outcome
   }

   pub fn reload(&mut self, name: &str) {
      if self
         .plugins
         .get(name)
         .is_some_and(|loaded| loaded.state.is_on())
      {
         self.unload(name);
         self.eval(name);
      }
   }

   pub fn save_choices(&self) {
      let _ = self.take_choices_dirty();
      let path = self
         .choices_path
         .clone()
         .or_else(|| STATE.get().map(State::choices_path));
      let Some(path) = path else {
         return;
      };
      let (mut disabled, mut enabled) = (BTreeSet::new(), BTreeSet::new());
      for (name, loaded) in &self.plugins {
         if Header::from(loaded.source.as_str()).default_off {
            if loaded.state.is_on() {
               enabled.insert(name.clone());
            }
         } else if !loaded.state.is_on() {
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
         if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
         {
            fs::create_dir_all(parent)?;
         }
         fs::write(&path, json?)
      };
      if let Err(err) = write() {
         log::error!("{}: {err}", path.display());
      }
   }

   /// The result as JSON when it has a JSON form, else as `String(result)`.
   /// The prompt sees a `phoenix` object of its own, named "console".
   pub fn evaluate(&self, code: &str) -> Result<String, Error> {
      self.budget.restart();
      let result = self.console.with(|ctx| {
         let run = || -> rquickjs::Result<String> {
            if !ctx.globals().contains_key("phoenix")? {
               let phoenix = api(&ctx, CONSOLE)?;
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

/// Gives the plugin callback about to run a budget of its own, so one that
/// spins does not take the next plugin's time.
pub fn restart_budget(ctx: &Ctx<'_>) {
   if let Some(registry) = ctx.userdata::<Registry<'_>>() {
      registry.budget.restart();
   }
}

/// One handle per live registry entry, frozen by [`lockdown`]. Plugins compare
/// nothing and parse nothing, the runtime recognizes a handle by identity.
fn registry<'js>(
   ctx: &Ctx<'js>,
   events: &GatewayRegistry,
   budget: Arc<Budget>,
   choices_dirty: Arc<AtomicBool>,
) -> rquickjs::Result<Registry<'js>> {
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
      gateway.set(key, handle.clone())?;
      handles.push((event, handle));
   }
   Ok(Registry {
      budget,
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

/// The name and source of every `*.js` file in `dir`. A file that cannot be
/// read is logged and left out, so one bad download keeps no other plugin off.
fn installed(dir: &std::path::Path) -> Result<Vec<(String, String)>, Error> {
   let paths = match fs::read_dir(dir) {
      Ok(entries) => entries
         .collect::<Result<Vec<_>, _>>()
         .map_err(io_error(dir))?,
      Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
      Err(err) => return Err(io_error(dir)(err)),
   };
   let mut found = Vec::new();
   for path in paths.iter().map(fs::DirEntry::path) {
      let Some(name) = path
         .file_stem()
         .and_then(|stem| stem.to_str())
         .filter(|_| path.extension().is_some_and(|ext| ext == "js"))
      else {
         continue;
      };
      match fs::read_to_string(&path) {
         Ok(source) => found.push((name.to_owned(), source)),
         Err(err) => log::error!("{}: {err}, loading the other plugins", path.display()),
      }
   }
   Ok(found)
}

/// Reconciles `bundled` plugins with `*.js` files in `plugins_dir`.
/// Removes identical or older disk copies that would shadow a bundled copy,
/// while preserving local edits, newer overrides, unversioned files and third
/// party plugins.
fn merge_plugins(
   bundled: Vec<Plugin>,
   plugins_dir: &std::path::Path,
) -> Result<BTreeMap<String, Loaded>, Error> {
   let mut plugins = bundled
      .into_iter()
      .map(|plugin| (plugin.name, Loaded::new(plugin.source, Origin::Bundled)))
      .collect::<BTreeMap<String, Loaded>>();
   for (name, source) in installed(plugins_dir)? {
      match plugins.get_mut(&name) {
         Some(loaded) => {
            if crate::same_plugin_source(&source, &loaded.source) {
               log::info!("removing redundant disk copy of bundled plugin {name}");
               let file = plugins_dir.join(format!("{name}.js"));
               if let Err(err) = fs::remove_file(&file) {
                  log::warn!("{}: {err}", file.display());
               }
               continue;
            }
            let disk_ver = Header::from(source.as_str()).version;
            let bundled_ver = Header::from(loaded.source.as_str()).version;
            if let (Some(disk_ver), Some(bundled_ver)) = (disk_ver, bundled_ver)
               && crate::compare_versions(&disk_ver, &bundled_ver)
                  == Some(std::cmp::Ordering::Less)
            {
               log::info!(
                  "upgrading stale override {name} from {disk_ver} to bundled {bundled_ver}"
               );
               let file = plugins_dir.join(format!("{name}.js"));
               if let Err(err) = fs::remove_file(&file) {
                  log::warn!("{}: {err}", file.display());
               }
               continue;
            }
            loaded.install(source);
         },
         None => {
            plugins.insert(name, Loaded::new(source, Origin::Installed));
         },
      }
   }
   Ok(plugins)
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

   let mut plugins = merge_plugins(bundled, &state.plugins_dir())?;
   for (name, loaded) in &mut plugins {
      let enabled = if Header::from(loaded.source.as_str()).default_off {
         choices.enabled.contains(name)
      } else {
         !choices.disabled.contains(name)
      };
      loaded.state = PluginState::from_switch(enabled);
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
   engine.plugins = plugins;
   let enabled = engine
      .plugins
      .iter()
      .filter(|(_, loaded)| loaded.state.is_on())
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
            .is_some_and(|loaded| matches!(loaded.state, PluginState::On))
      })
      .collect())
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::Control;

   fn engine_with_bundled(name: &str, source: &str, enabled: bool) -> Engine {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      let mut loaded = Loaded::new(source.to_owned(), Origin::Bundled);
      loaded.state = PluginState::from_switch(enabled);
      engine.plugins.insert(name.to_owned(), loaded);
      if enabled {
         engine.eval(name);
      }
      engine
   }

   fn setting_control(info: &PluginInfo, key: &str) -> Option<Control> {
      info
         .settings
         .iter()
         .find(|setting| setting.key == key)
         .map(|setting| setting.control.clone())
   }

   #[test]
   fn a_file_that_cannot_be_read_leaves_the_other_plugins() {
      let dir = std::env::temp_dir().join(format!("phoenix-installed-{}", std::process::id()));
      let _ = fs::remove_dir_all(&dir);
      fs::create_dir_all(&dir).unwrap();
      fs::write(dir.join("works.js"), "const works = 1;").unwrap();
      fs::write(dir.join("broken.js"), [0xFF, 0xFE, 0x00]).unwrap();
      fs::write(dir.join("notes.txt"), "not a plugin").unwrap();
      let found = installed(&dir);
      fs::remove_dir_all(&dir).unwrap();
      assert_eq!(
         found.unwrap(),
         [("works".to_owned(), "const works = 1;".to_owned())]
      );
   }

   #[test]
   fn a_missing_directory_has_no_plugins() {
      let dir = std::env::temp_dir().join("phoenix-installed-missing-directory");
      assert_eq!(installed(&dir).unwrap(), Vec::<(String, String)>::new());
   }

   #[test]
   fn uninstall_restores_bundled_plugin_and_preserves_state_and_settings() {
      let bundled = r#"
         const s = phoenix.settings.define({
            mode: { type: "text", title: "Mode", default: "a" },
            tag: { type: "text", title: "Tag", default: "" },
            seen: { type: "text", title: "Seen", default: "" }
         });
         s.tag = "bundled";
         s.seen = s.mode;
      "#;
      let overridden = r#"
         const s = phoenix.settings.define({
            mode: { type: "text", title: "Mode", default: "a" },
            tag: { type: "text", title: "Tag", default: "" },
            seen: { type: "text", title: "Seen", default: "" }
         });
         s.tag = "override";
         s.seen = s.mode;
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
      assert!(info.error.is_none());
      assert_eq!(
         setting_control(&info, "tag"),
         Some(Control::Text("override".to_owned()))
      );
      assert_eq!(
         setting_control(&info, "seen"),
         Some(Control::Text("custom".to_owned()))
      );

      assert_eq!(engine.uninstall("demo"), Removed::Restored);

      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "demo")
         .unwrap();
      assert!(!info.installed);
      assert!(info.bundled);
      assert!(info.enabled);
      assert!(info.error.is_none());
      assert_eq!(engine.source("demo").as_deref(), Some(bundled));
      assert_eq!(
         setting_control(&info, "tag"),
         Some(Control::Text("bundled".to_owned()))
      );
      assert_eq!(
         setting_control(&info, "seen"),
         Some(Control::Text("custom".to_owned()))
      );
   }

   #[test]
   fn uninstall_restores_disabled_bundled_plugin_without_running_it() {
      let bundled = r#"
         const s = phoenix.settings.define({
            ran: { type: "number", title: "Ran", default: 0 }
         });
         s.ran = 1;
      "#;
      let overridden = r#"
         const s = phoenix.settings.define({
            ran: { type: "number", title: "Ran", default: 0 }
         });
         s.ran = 99;
      "#;

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
      assert!(info.settings.is_empty());
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
            const s = phoenix.settings.define({
               flag: { type: "switch", title: "Flag", default: false },
               seen: { type: "text", title: "Seen", default: "" }
            });
            s.seen = String(s.flag);
         "#
         .to_owned(),
      );
      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "custom")
         .unwrap();
      assert_eq!(
         setting_control(&info, "seen"),
         Some(Control::Text("false".to_owned()))
      );
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
            const s = phoenix.settings.define({
               limit: { type: "number", title: "Limit", default: 10 },
               checked: { type: "text", title: "Checked", default: "" }
            });
            s.limit = 42.5;
            const results = [];
            for (const bad of [NaN, Infinity, -Infinity]) {
               try {
                  s.limit = bad;
                  results.push("allowed");
               } catch (e) {
                  results.push((e instanceof RangeError) + ":" + e.message);
               }
            }
            s.checked = results.join("|");
         "#
         .to_owned(),
      );

      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "numtest")
         .unwrap();
      assert!(info.error.is_none());
      assert_eq!(setting_control(&info, "limit"), Some(Control::Number(42.5)));
      let Some(Control::Text(checked)) = setting_control(&info, "checked") else {
         panic!("missing checked setting");
      };
      for part in checked.split('|') {
         assert!(
            part.starts_with("true:") && part.contains("finite number expected"),
            "unexpected rejection outcome: {part}"
         );
      }

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
         .evaluate(
            r#"
               const cs = phoenix.settings.define({
                  mode: { type: "text", title: "Mode", default: "init" },
                  count: { type: "number", title: "Count", default: 0 }
               });
               cs.mode = "evaluated";
               cs.count = 9;
            "#,
         )
         .expect("evaluate setting update");
      let after_eval: Choices<'_> =
         serde_json::from_slice(&fs::read(&choices_path).expect("plugins.json after evaluate"))
            .expect("valid json after evaluate");
      assert_eq!(
         after_eval.settings.get(CONSOLE).and_then(|m| m.get("mode")),
         Some(&Chosen::Text("evaluated".to_owned()))
      );
      assert_eq!(
         after_eval
            .settings
            .get(CONSOLE)
            .and_then(|m| m.get("count")),
         Some(&Chosen::Number(9.0))
      );

      let _ = fs::remove_dir_all(&dir);
   }

   #[test]
   fn reserves_console_name_for_debug_shell() {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.install(CONSOLE, "const x = 1;".to_owned());
      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == CONSOLE)
         .unwrap();
      assert!(info.enabled);
      assert!(
         info
            .error
            .as_deref()
            .is_some_and(|err| err.contains("debug shell")),
         "expected reserved console error, got {:?}",
         info.error
      );
   }

   #[test]
   fn lockdown_freezes_shared_builtins_and_allows_error_subclasses() {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.install(
         "probe",
         r#"
            "use strict";
            const s = phoenix.settings.define({
               globalFrozen: { type: "switch", title: "Global", default: false },
               protoFrozen: { type: "switch", title: "Proto", default: false },
               subclassOk: { type: "switch", title: "Subclass", default: false }
            });
            try { globalThis.leaked = 1; } catch (_) { s.globalFrozen = true; }
            try { String.prototype.replace = () => "hijacked"; } catch (_) { s.protoFrozen = true; }
            class CustomError extends Error {
               constructor(msg) {
                  super(msg);
                  this.name = "CustomError";
               }
            }
            const err = new CustomError("boom");
            s.subclassOk = err.name === "CustomError" && err.message === "boom";
         "#
         .to_owned(),
      );
      let info = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "probe")
         .unwrap();
      assert!(info.error.is_none(), "probe failed: {:?}", info.error);
      assert_eq!(
         setting_control(&info, "globalFrozen"),
         Some(Control::Switch(true))
      );
      assert_eq!(
         setting_control(&info, "protoFrozen"),
         Some(Control::Switch(true))
      );
      assert_eq!(
         setting_control(&info, "subclassOk"),
         Some(Control::Switch(true))
      );
      // Debug shell runs in its own unfrozen realm with `phoenix`.
      assert_eq!(engine.evaluate("typeof phoenix").unwrap(), "object");
   }

   #[test]
   fn interrupts_runaway_plugin_and_caps_heap() {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.install("spin", "while (true) {}".to_owned());
      let spin = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "spin")
         .unwrap();
      assert!(
         spin.error.is_some(),
         "infinite loop plugin must be interrupted"
      );

      engine.install("oom", "new Uint8Array(256 * 1024 * 1024);".to_owned());
      let oom = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "oom")
         .unwrap();
      assert!(oom.error.is_some(), "allocation past 128 MiB must fail");

      // Engine stays healthy for later plugins.
      engine.install(
         "ok",
         r#"phoenix.settings.define({ alive: { type: "switch", title: "Alive", default: true } });"#
            .to_owned(),
      );
      let ok = engine
         .plugins()
         .into_iter()
         .find(|p| p.name == "ok")
         .unwrap();
      assert!(ok.error.is_none(), "healthy plugin failed: {:?}", ok.error);
   }

   #[test]
   fn console_plugin_lifecycle_preserves_debug_shell_state_and_drops_cleanly() {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine
         .evaluate(
            r#"
               globalThis.keptGateway = phoenix.gateway;
               const cs = phoenix.settings.define({
                  note: { type: "text", title: "Note", default: "init" }
               });
               cs.note = "from-console";
               globalThis.consoleSettings = cs;
            "#,
         )
         .expect("console setup");

      engine.install(CONSOLE, "const x = 1;".to_owned());
      assert_eq!(engine.uninstall(CONSOLE), Removed::Removed);
      assert_eq!(
         engine.evaluate("globalThis.consoleSettings.note").unwrap(),
         "from-console"
      );
      // Dropping `engine` after console captured `phoenix.gateway` must stay clean.
      drop(engine);
   }

   #[test]
   fn starves_self_requeueing_promises_after_overruns_and_recovers() {
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.install(
         "bomb",
         "Promise.resolve().then(function loop() { return Promise.resolve().then(loop); });"
            .to_owned(),
      );
      // Two more entries push `overruns` to OVERRUNS and starve the queue.
      for _ in 0..OVERRUNS {
         engine.with(|_| {});
      }
      assert!(!engine.runtime.is_job_pending());
      assert_eq!(engine.overruns.get(), 0);
      assert_eq!(engine.evaluate("1 + 1").unwrap(), "2");
   }

   fn temp_plugins_dir(label: &str) -> std::path::PathBuf {
      static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
      let n = SEQ.fetch_add(1, Ordering::Relaxed);
      std::env::temp_dir().join(format!("nyaacord-merge-{label}-{}-{n}", std::process::id()))
   }

   #[test]
   fn reconciles_disk_overrides_against_bundled_plugins() {
      let dir = temp_plugins_dir("reconcile");
      let _ = fs::remove_dir_all(&dir);
      fs::create_dir_all(&dir).unwrap();

      let bundled_silent = "// @version 2.0.0\nconst mode = 'bundled-v2';\n";
      let stale_silent = "// @version 1.0.0\nconst mode = 'disk-v1';\n";
      fs::write(dir.join("silent-typing.js"), stale_silent).unwrap();

      let bundled_identical = "// @version 1.3.0\nconst same = true;\n";
      fs::write(dir.join("fix-embeds.js"), bundled_identical).unwrap();

      let bundled_edited = "// @version 2.0.0\nconst flag = 'stock';\n";
      let edited_disk = "// @version 2.0.0\nconst flag = 'custom-edit';\n";
      fs::write(dir.join("edited.js"), edited_disk).unwrap();

      let bundled_newer = "// @version 2.0.0\nconst rev = 200;\n";
      let newer_disk = "// @version 2.1.0\nconst rev = 210;\n";
      fs::write(dir.join("newer.js"), newer_disk).unwrap();

      let bundled_unversioned = "// @version 2.0.0\nconst v = 2;\n";
      let custom_unversioned = "const v = 'custom-no-header';\n";
      fs::write(dir.join("unversioned.js"), custom_unversioned).unwrap();

      let third_party = "// @version 1.0.0\nconst ext = true;\n";
      fs::write(dir.join("third-party.js"), third_party).unwrap();

      let bundled_unreadable = "// @version 0.1.3\nconst ok = true;\n";
      fs::write(dir.join("clear-urls.js"), [0xFF, 0xFE, 0x00]).unwrap();

      let bundled = vec![
         Plugin {
            name: "silent-typing".to_owned(),
            source: bundled_silent.to_owned(),
         },
         Plugin {
            name: "fix-embeds".to_owned(),
            source: bundled_identical.to_owned(),
         },
         Plugin {
            name: "edited".to_owned(),
            source: bundled_edited.to_owned(),
         },
         Plugin {
            name: "newer".to_owned(),
            source: bundled_newer.to_owned(),
         },
         Plugin {
            name: "unversioned".to_owned(),
            source: bundled_unversioned.to_owned(),
         },
         Plugin {
            name: "clear-urls".to_owned(),
            source: bundled_unreadable.to_owned(),
         },
      ];

      let merged = merge_plugins(bundled, &dir).expect("merge_plugins");

      // Older disk copy 1.0.0 is removed and bundled 2.0.0 loads.
      let silent = merged.get("silent-typing").expect("silent-typing");
      assert!(matches!(silent.origin, Origin::Bundled));
      assert_eq!(silent.source, bundled_silent);
      assert!(!dir.join("silent-typing.js").exists());

      // Identical disk copy 1.3.0 is removed and bundled copy loads.
      let fix_embeds = merged.get("fix-embeds").expect("fix-embeds");
      assert!(matches!(fix_embeds.origin, Origin::Bundled));
      assert_eq!(fix_embeds.source, bundled_identical);
      assert!(!dir.join("fix-embeds.js").exists());

      // Local edit at equal version 2.0.0 is preserved as Replacing.
      let edited = merged.get("edited").expect("edited");
      assert!(matches!(edited.origin, Origin::Replacing(ref b) if b == bundled_edited));
      assert_eq!(edited.source, edited_disk);
      assert!(dir.join("edited.js").exists());

      // Newer disk override 2.1.0 is preserved as Replacing.
      let newer = merged.get("newer").expect("newer");
      assert!(matches!(newer.origin, Origin::Replacing(ref b) if b == bundled_newer));
      assert_eq!(newer.source, newer_disk);
      assert!(dir.join("newer.js").exists());

      // Unversioned disk override is preserved as Replacing.
      let unversioned = merged.get("unversioned").expect("unversioned");
      assert!(matches!(unversioned.origin, Origin::Replacing(ref b) if b == bundled_unversioned));
      assert_eq!(unversioned.source, custom_unversioned);
      assert!(dir.join("unversioned.js").exists());

      // Non-bundled third party plugin is preserved as Installed.
      let tp = merged.get("third-party").expect("third-party");
      assert!(matches!(tp.origin, Origin::Installed));
      assert_eq!(tp.source, third_party);
      assert!(dir.join("third-party.js").exists());

      // Unreadable disk file falls back to bundled copy.
      let clear_urls = merged.get("clear-urls").expect("clear-urls");
      assert!(matches!(clear_urls.origin, Origin::Bundled));
      assert_eq!(clear_urls.source, bundled_unreadable);

      // Verify Engine::plugins and bundled_source reflect merged origins.
      let mut engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.plugins = merged;
      let infos = engine.plugins();
      let find = |name: &str| infos.iter().find(|p| p.name == name).unwrap();
      assert!(!find("silent-typing").installed && find("silent-typing").bundled);
      assert!(!find("fix-embeds").installed && find("fix-embeds").bundled);
      assert!(find("edited").installed && find("edited").bundled);
      assert!(find("newer").installed && find("newer").bundled);
      assert!(find("unversioned").installed && find("unversioned").bundled);
      assert!(find("third-party").installed && !find("third-party").bundled);
      assert_eq!(engine.bundled_source("edited").as_deref(), Some(bundled_edited));
      assert_eq!(engine.bundled_source("third-party"), None);

      let _ = fs::remove_dir_all(&dir);
   }

   #[test]
   fn saving_bundled_identical_source_and_uninstalling_bundled_stay_clean() {
      let bundled = r#"
         const s = phoenix.settings.define({
            mode: { type: "text", title: "Mode", default: "a" }
         });
      "#;
      let edited = r#"
         const s = phoenix.settings.define({
            mode: { type: "text", title: "Mode", default: "b" }
         });
      "#;

      let mut engine = engine_with_bundled("demo", bundled, true);
      engine.set_setting("demo", "mode", Chosen::Text("custom".to_owned()));

      // Saving source identical to bundled keeps Origin::Bundled.
      engine.install("demo", bundled.replace('\n', "\r\n"));
      let info = engine.plugins().into_iter().find(|p| p.name == "demo").unwrap();
      assert!(!info.installed && info.bundled && info.enabled);

      // Editing creates an override, then saving bundled source again restores Bundled.
      engine.install("demo", edited.to_owned());
      let info = engine.plugins().into_iter().find(|p| p.name == "demo").unwrap();
      assert!(info.installed && info.bundled);

      engine.install("demo", bundled.to_owned());
      let info = engine.plugins().into_iter().find(|p| p.name == "demo").unwrap();
      assert!(!info.installed && info.bundled && info.enabled);
      assert_eq!(engine.source("demo").as_deref(), Some(bundled));
      assert_eq!(
         setting_control(&info, "mode"),
         Some(Control::Text("custom".to_owned()))
      );

      // Calling uninstall on a bundled plugin restores without deleting it or its settings.
      assert_eq!(engine.uninstall("demo"), Removed::Restored);
      let info = engine.plugins().into_iter().find(|p| p.name == "demo").unwrap();
      assert!(!info.installed && info.bundled && info.enabled);
      assert_eq!(
         setting_control(&info, "mode"),
         Some(Control::Text("custom".to_owned()))
      );
   }
}
