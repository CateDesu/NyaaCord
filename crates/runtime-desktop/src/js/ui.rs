//! Desktop `phoenix.ui` bindings backed by [`phoenix_ui_desktop`] and native
//! GPUI message accessories ([`crate::accessories`]).
//!
//! Native clipboard, toasts, notifications, and `addMessageAccessory` run live
//! on Desktop Phoenix. Remaining mobile-only UI-op slot hooks return harmless
//! no-op removers so cross-platform plugins load cleanly without throwing.

use std::{
   sync::{
      Arc,
      atomic::{AtomicU64, Ordering},
   },
   time::Duration,
};

use phoenix_runtime::{
   discord::RequestAction,
   js::{
      Engine, Registry,
      background::{Settlement, in_background},
      restart_budget,
   },
   local,
   store::{self, Cell},
};
use rquickjs::{
   Array, CatchResultExt, Ctx, Exception, Function, Object, Value,
   function::{Opt, This},
};
use serde_json::value::RawValue;

use crate::{
   accessories::{self, RenderedAccessory},
   composer::{self, ComposerButtonMeta},
   context_menu::{self, ActionMeta},
   emoji,
   js::Desktop,
   modal::{self, ModalButton, ModalSection, PageModalSpec},
   typing,
};

#[derive(Clone)]
pub struct DesktopAccessory<'js> {
   pub id: u64,
   pub plugin: String,
   pub priority: i32,
   pub render: Function<'js>,
}

#[derive(Clone)]
pub struct DesktopAction<'js> {
   pub id: u64,
   pub plugin: String,
   pub label: String,
   pub icon: Option<String>,
   pub danger: bool,
   pub priority: i32,
   pub visible: Option<Function<'js>>,
   pub on_press: Function<'js>,
}

impl DesktopAction<'_> {
   pub fn meta(&self) -> ActionMeta {
      ActionMeta {
         id: self.id,
         label: self.label.clone(),
         icon: self.icon.clone(),
         danger: self.danger,
         priority: self.priority,
         has_visible: self.visible.is_some(),
      }
   }
}

#[derive(Clone)]
pub struct DesktopComposerButton<'js> {
   pub id: u64,
   pub plugin: String,
   pub icon: String,
   pub label: String,
   pub tint: Option<u32>,
   pub priority: i32,
   pub on_press: Function<'js>,
}

impl DesktopComposerButton<'_> {
   pub fn meta(&self) -> ComposerButtonMeta {
      ComposerButtonMeta {
         id: self.id,
         icon: self.icon.clone(),
         label: self.label.clone(),
         tint: self.tint,
         priority: self.priority,
      }
   }
}

fn noop_handle<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Function<'js>> {
   let remove = Function::new(ctx.clone(), || {})?;
   remove.set("update", Function::new(ctx.clone(), || {})?)?;
   remove.set("close", Function::new(ctx.clone(), || {})?)?;
   Ok(remove)
}

fn parse_rendered(value: &Value<'_>, priority: i32) -> Option<RenderedAccessory> {
   if value.is_null() || value.is_undefined() || value.as_bool() == Some(false) {
      return None;
   }
   if let Some(s) = value.as_string() {
      let text = s.to_string().ok()?;
      if text.is_empty() {
         return None;
      }
      return Some(RenderedAccessory {
         priority,
         text,
         color: None,
         tint: None,
      });
   }
   if let Some(obj) = value.as_object() {
      let text = obj
         .get::<_, Option<String>>("text")
         .ok()
         .flatten()
         .unwrap_or_default();
      let color = obj
         .get::<_, Option<f64>>("color")
         .ok()
         .flatten()
         .map(to_argb);
      let tint = obj
         .get::<_, Option<f64>>("tint")
         .ok()
         .flatten()
         .map(to_argb);
      if text.is_empty() && tint.is_none() {
         return None;
      }
      return Some(RenderedAccessory {
         priority,
         text,
         color,
         tint,
      });
   }
   None
}

fn evaluate_accessories_in_ctx<'js>(
   ctx: &Ctx<'js>,
   message_id: u64,
   epoch: u64,
) -> Vec<RenderedAccessory> {
   let Some(registry) = ctx.userdata::<Desktop<'js>>() else {
      return Vec::new();
   };
   let snapshot: Vec<DesktopAccessory<'js>> = registry.accessories.borrow().clone();
   drop(registry);
   if snapshot.is_empty() {
      return Vec::new();
   }

   let id_str = message_id.to_string();
   let Ok(arg) = Object::new(ctx.clone()) else {
      return Vec::new();
   };
   let _ = arg.set("messageId", id_str.as_str());
   let _ = arg.set("channelId", Value::new_null(ctx.clone()));
   let _ = arg.set("currentUserId", Value::new_null(ctx.clone()));
   if let Ok(reply) = Function::new(ctx.clone(), String::new) {
      let _ = arg.set("reply", reply);
   }

   let mut out = Vec::new();
   for acc in snapshot {
      restart_budget(ctx);
      match acc.render.call::<_, Value<'_>>((arg.clone(),)).catch(ctx) {
         Ok(val) if val.is_promise() => {
            if let Some(promise) = val.as_object()
               && let Ok(then) = promise.get::<_, Function<'_>>("then")
            {
               let mid = message_id;
               let acc_id = acc.id;
               let prio = acc.priority;
               let err_plugin = acc.plugin.clone();
               let on_resolve =
                  Function::new(ctx.clone(), move |ctx: Ctx<'js>, resolved: Value<'js>| {
                     let active = ctx.userdata::<Desktop<'js>>().is_some_and(|registry| {
                        registry
                           .accessories
                           .borrow()
                           .iter()
                           .any(|entry| entry.id == acc_id)
                     });
                     if !active {
                        return;
                     }
                     if let Some(item) = parse_rendered(&resolved, prio) {
                        let mut all = evaluate_sync_only(&ctx, mid, Some((prio, item)));
                        all.sort_by_key(|a| a.priority);
                        accessories::set_rendered_for_epoch(mid, epoch, all);
                     }
                  });
               let on_reject = Function::new(ctx.clone(), move |reason: Value<'js>| {
                  log::error!("[{err_plugin}] addMessageAccessory promise: {reason:?}");
               });
               if let (Ok(on_resolve), Ok(on_reject)) = (on_resolve, on_reject)
                  && let Err(err) = then
                     .call::<_, Value<'_>>((This(promise.clone()), on_resolve, on_reject))
                     .catch(ctx)
               {
                  log::error!("[{}] addMessageAccessory promise: {err}", acc.plugin);
               }
            }
         },
         Ok(val) => {
            if let Some(rendered) = parse_rendered(&val, acc.priority) {
               out.push(rendered);
            }
         },
         Err(err) => {
            log::error!("[{}] addMessageAccessory render: {err}", acc.plugin);
         },
      }
   }
   out
}

fn evaluate_sync_only<'js>(
   ctx: &Ctx<'js>,
   message_id: u64,
   extra: Option<(i32, RenderedAccessory)>,
) -> Vec<RenderedAccessory> {
   let mut out = Vec::new();
   if let Some((_, item)) = extra {
      out.push(item);
   }
   let Some(registry) = ctx.userdata::<Desktop<'js>>() else {
      return out;
   };
   let snapshot: Vec<DesktopAccessory<'js>> = registry.accessories.borrow().clone();
   drop(registry);

   let id_str = message_id.to_string();
   let Ok(arg) = Object::new(ctx.clone()) else {
      return out;
   };
   let _ = arg.set("messageId", id_str.as_str());
   let _ = arg.set("channelId", Value::new_null(ctx.clone()));
   let _ = arg.set("currentUserId", Value::new_null(ctx.clone()));

   for acc in snapshot {
      restart_budget(ctx);
      match acc.render.call::<_, Value<'_>>((arg.clone(),)).catch(ctx) {
         Ok(val)
            if !val.is_promise()
               && let Some(rendered) = parse_rendered(&val, acc.priority)
               && !out.contains(&rendered) =>
         {
            out.push(rendered);
         },
         Ok(_) => {},
         Err(err) => log::error!("[{}] addMessageAccessory render: {err}", acc.plugin),
      }
   }
   out
}

/// Evaluates every registered desktop `addMessageAccessory` for `message_id`.
pub fn render_accessories(engine: &Engine, message_id: u64, epoch: u64) -> Vec<RenderedAccessory> {
   engine.with(|ctx| evaluate_accessories_in_ctx(&ctx, message_id, epoch))
}

#[expect(
   clippy::cast_possible_truncation,
   clippy::cast_sign_loss,
   reason = "ARGB colors are 0..=0xFFFFFFFF, and `as` saturates the rest"
)]
const fn to_argb(color: f64) -> u32 {
   color as u32
}

fn add_message_accessory<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   options: &Object<'js>,
) -> rquickjs::Result<Function<'js>> {
   static NEXT_ID: AtomicU64 = AtomicU64::new(1);

   let render = options.get::<_, Function<'js>>("render")?;
   let priority = options.get::<_, Option<i32>>("priority")?.unwrap_or(0);
   let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

   let count = {
      let registry = ctx
         .userdata::<Desktop<'js>>()
         .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
      let mut list = registry.accessories.borrow_mut();
      list.push(DesktopAccessory {
         id,
         plugin: plugin.to_owned(),
         priority,
         render,
      });
      list.len()
   };
   accessories::set_active_count(count);
   accessories::clear_cache();

   let remove = Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
      if let Some(registry) = ctx.userdata::<Desktop<'js>>() {
         let mut list = registry.accessories.borrow_mut();
         list.retain(|entry| entry.id != id);
         accessories::set_active_count(list.len());
         accessories::clear_cache();
         accessories::invalidate_discord_windows();
      }
   })?;
   remove.set(
      "update",
      Function::new(ctx.clone(), |ctx: Ctx<'js>, message_id: Opt<String>| {
         if let Some(mid) = message_id.0.and_then(|s| s.parse::<u64>().ok()) {
            let epoch = accessories::cache_epoch();
            let items = evaluate_accessories_in_ctx(&ctx, mid, epoch);
            accessories::set_rendered(mid, items);
         } else {
            accessories::clear_cache();
            accessories::invalidate_discord_windows();
         }
      })?,
   )?;
   Ok(remove)
}

fn cached_message_and_me(channel_id: u64, message_id: u64) -> (Option<String>, Option<String>) {
   let Some(state) = phoenix_runtime::state() else {
      return (None, None);
   };
   let Ok(db) = store::database(&state.files_dir()) else {
      return (None, None);
   };
   // Phoenix keys messages `<channel>:<message>`, each zero-padded to 20 digits.
   let rows = if channel_id != 0 {
      store::query(
         &db,
         "SELECT v FROM messages WHERE k = ?",
         &[format!("{channel_id:020}:{message_id:020}")],
      )
   } else {
      store::query(
         &db,
         "SELECT v FROM messages WHERE k LIKE ? LIMIT 1",
         &[format!("%:{message_id:020}")],
      )
   };
   let msg_json = rows
      .ok()
      .and_then(|rows| rows.first("v").and_then(Cell::text).map(str::to_owned));
   let me_id = store::meta(&db, "me")
      .ok()
      .flatten()
      .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
      .and_then(|val| {
         val.get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
      });
   (msg_json, me_id)
}

fn build_message_context<'js>(
   ctx: &Ctx<'js>,
   message_id: u64,
   channel_id: u64,
   include_message: bool,
) -> rquickjs::Result<Object<'js>> {
   let arg = Object::new(ctx.clone())?;
   let id_str = message_id.to_string();
   arg.set("messageId", id_str.as_str())?;
   if channel_id != 0 {
      let ch_str = channel_id.to_string();
      arg.set("channelId", ch_str.as_str())?;
   } else {
      arg.set("channelId", Value::new_null(ctx.clone()))?;
   }
   let (msg_json, me_id) = if include_message {
      let memory = context_menu::menu_message(message_id);
      let (cached, me) = cached_message_and_me(channel_id, message_id);
      (memory.or(cached), me)
   } else {
      (None, None)
   };
   if let Some(json) = msg_json
      && let Ok(parsed) = ctx.json_parse(json)
   {
      arg.set("message", parsed)?;
   } else {
      arg.set("message", Value::new_null(ctx.clone()))?;
   }
   if let Some(me) = me_id {
      arg.set("currentUserId", me.as_str())?;
   } else {
      arg.set("currentUserId", Value::new_null(ctx.clone()))?;
   }
   if let Ok(reply) = Function::new(ctx.clone(), String::new) {
      arg.set("reply", reply)?;
   }
   Ok(arg)
}

/// Evaluates an `addMessageAction`'s synchronous `visible(ctx)` predicate.
pub fn check_action_visible(
   engine: &Engine,
   action_id: u64,
   message_id: u64,
   channel_id: u64,
) -> bool {
   engine.with(|ctx| {
      let Some((plugin, visible)) = ctx.userdata::<Desktop<'_>>().and_then(|registry| {
         let list = registry.actions.borrow();
         let entry = list.iter().find(|e| e.id == action_id)?;
         Some((entry.plugin.clone(), entry.visible.clone()?))
      }) else {
         return true;
      };
      let Ok(arg) = build_message_context(&ctx, message_id, channel_id, true) else {
         return true;
      };
      restart_budget(&ctx);
      match visible.call::<_, Value<'_>>((arg,)).catch(&ctx) {
         Ok(val) if val.is_promise() => true,
         Ok(val) => val.as_bool().unwrap_or(true),
         Err(err) => {
            log::error!("[{plugin}] addMessageAction visible: {err}");
            false
         },
      }
   })
}

/// Invokes a registered `addMessageAction`'s `onPress(ctx)` callback.
pub fn invoke_message_action(engine: &Engine, action_id: u64, message_id: u64, channel_id: u64) {
   engine.with(|ctx| {
      let Some((plugin, on_press)) = ctx.userdata::<Desktop<'_>>().and_then(|registry| {
         let list = registry.actions.borrow();
         let entry = list.iter().find(|e| e.id == action_id)?;
         Some((entry.plugin.clone(), entry.on_press.clone()))
      }) else {
         return;
      };
      let Ok(arg) = build_message_context(&ctx, message_id, channel_id, true) else {
         return;
      };
      restart_budget(&ctx);
      if let Err(err) = on_press.call::<_, Value<'_>>((arg,)).catch(&ctx) {
         log::error!("[{plugin}] addMessageAction onPress: {err}");
      }
   });
}

fn add_message_action<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   options: &Object<'js>,
) -> rquickjs::Result<Function<'js>> {
   static NEXT_ID: AtomicU64 = AtomicU64::new(1);

   let label = options.get::<_, String>("label")?;
   let icon = options.get::<_, Option<String>>("icon")?;
   let danger = options.get::<_, Option<bool>>("danger")?.unwrap_or(false);
   let priority = options.get::<_, Option<i32>>("priority")?.unwrap_or(0);
   let visible = options.get::<_, Option<Function<'js>>>("visible")?;
   let on_press = options.get::<_, Function<'js>>("onPress")?;
   let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

   let meta = {
      let registry = ctx
         .userdata::<Desktop<'js>>()
         .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
      let mut list = registry.actions.borrow_mut();
      list.push(DesktopAction {
         id,
         plugin: plugin.to_owned(),
         label,
         icon,
         danger,
         priority,
         visible,
         on_press,
      });
      list.iter().map(DesktopAction::meta).collect()
   };
   context_menu::sync_actions(meta);

   let remove = Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
      if let Some(registry) = ctx.userdata::<Desktop<'js>>() {
         let mut list = registry.actions.borrow_mut();
         list.retain(|entry| entry.id != id);
         let meta = list.iter().map(DesktopAction::meta).collect();
         context_menu::sync_actions(meta);
      }
   })?;
   remove.set("update", Function::new(ctx.clone(), || {})?)?;
   Ok(remove)
}

fn cached_me_id() -> Option<String> {
   let state = phoenix_runtime::state()?;
   let db = store::database(&state.files_dir()).ok()?;
   store::meta(&db, "me")
      .ok()
      .flatten()
      .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
      .and_then(|val| {
         val.get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
      })
}

fn build_ui_context<'js>(ctx: &Ctx<'js>, channel_id: u64) -> rquickjs::Result<Object<'js>> {
   let arg = Object::new(ctx.clone())?;
   let ch_opt = (channel_id != 0).then(|| channel_id.to_string());
   if let Some(ch) = &ch_opt {
      arg.set("channelId", ch.as_str())?;
   } else {
      arg.set("channelId", Value::new_null(ctx.clone()))?;
   }
   if let Some(me) = cached_me_id() {
      arg.set("currentUserId", me.as_str())?;
   } else {
      arg.set("currentUserId", Value::new_null(ctx.clone()))?;
   }
   let reply_channel = ch_opt;
   let reply = Function::new(ctx.clone(), move |text: String| -> String {
      let Some(ch) = reply_channel.as_deref() else {
         return String::new();
      };
      let Ok(embeds) = RawValue::from_string("[]".to_owned()) else {
         return String::new();
      };
      let options = local::Options {
         author: "NyaaCord",
         guild_id: None,
         embeds: &embeds,
      };
      local::add(ch, &text, &options).unwrap_or_default()
   })?;
   arg.set("reply", reply)?;
   Ok(arg)
}

/// Invokes a registered `addComposerButton`'s `onPress(ctx)` callback on the
/// `QuickJS` worker thread.
pub fn invoke_composer_button(engine: &Engine, button_id: u64, channel_id: u64) {
   engine.with(|ctx| {
      let Some((plugin, on_press)) = ctx.userdata::<Desktop<'_>>().and_then(|registry| {
         let list = registry.composer_buttons.borrow();
         let entry = list.iter().find(|e| e.id == button_id)?;
         Some((entry.plugin.clone(), entry.on_press.clone()))
      }) else {
         return;
      };
      let Ok(arg) = build_ui_context(&ctx, channel_id) else {
         return;
      };
      restart_budget(&ctx);
      if let Err(err) = on_press.call::<_, Value<'_>>((arg,)).catch(&ctx) {
         log::error!("[{plugin}] addComposerButton onPress: {err}");
      }
   });
}

fn add_composer_button<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   options: &Object<'js>,
) -> rquickjs::Result<Function<'js>> {
   static NEXT_ID: AtomicU64 = AtomicU64::new(1);

   let icon = options.get::<_, String>("icon")?;
   let label = options.get::<_, String>("label")?;
   let tint = options.get::<_, Option<f64>>("tint")?.map(to_argb);
   let priority = options.get::<_, Option<i32>>("priority")?.unwrap_or(0);
   let on_press = options.get::<_, Function<'js>>("onPress")?;
   let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

   let meta = {
      let registry = ctx
         .userdata::<Desktop<'js>>()
         .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
      let mut list = registry.composer_buttons.borrow_mut();
      list.push(DesktopComposerButton {
         id,
         plugin: plugin.to_owned(),
         icon,
         label,
         tint,
         priority,
         on_press,
      });
      list.iter().map(DesktopComposerButton::meta).collect()
   };
   composer::sync_buttons(meta);

   let remove = Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
      if let Some(registry) = ctx.userdata::<Desktop<'js>>() {
         let mut list = registry.composer_buttons.borrow_mut();
         let before = list.len();
         list.retain(|entry| entry.id != id);
         if list.len() != before {
            let meta = list.iter().map(DesktopComposerButton::meta).collect();
            composer::sync_buttons(meta);
         }
      }
   })?;
   remove.set(
      "update",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>, patch: Opt<Value<'js>>| {
         let Some(object) = patch.0.as_ref().and_then(Value::as_object) else {
            return;
         };
         let Some(registry) = ctx.userdata::<Desktop<'js>>() else {
            return;
         };
         let mut list = registry.composer_buttons.borrow_mut();
         let Some(entry) = list.iter_mut().find(|entry| entry.id == id) else {
            return;
         };
         if let Ok(Some(icon)) = object.get::<_, Option<String>>("icon") {
            entry.icon = icon;
         }
         if let Ok(Some(label)) = object.get::<_, Option<String>>("label") {
            entry.label = label;
         }
         if let Ok(value) = object.get::<_, Value<'_>>("tint") {
            if value.is_null() {
               entry.tint = None;
            } else if let Some(color) = value.as_number() {
               entry.tint = Some(to_argb(color));
            }
         }
         let meta = list.iter().map(DesktopComposerButton::meta).collect();
         drop(list);
         composer::sync_buttons(meta);
      })?,
   )?;
   Ok(remove)
}

fn invoke_page_button(button_id: u64) {
   let Some(engine) = phoenix_runtime::engine() else {
      return;
   };
   engine.with(|ctx| {
      let Some((plugin, on_press)) = ctx.userdata::<Desktop<'_>>().and_then(|registry| {
         let map = registry.page_buttons.borrow();
         let (_, plugin, func) = map.get(&button_id)?;
         Some((plugin.clone(), func.clone()))
      }) else {
         return;
      };
      if let Err(err) = on_press.call::<_, Value<'_>>(()).catch(&ctx) {
         log::error!("[{plugin}] openPage button onPress: {err}");
      }
   });
}

fn open_page<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   options: &Object<'js>,
) -> rquickjs::Result<Object<'js>> {
   static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);
   static NEXT_BTN: AtomicU64 = AtomicU64::new(1);

   let page_id = NEXT_PAGE.fetch_add(1, Ordering::Relaxed);
   let title = options
      .get::<_, Option<String>>("title")?
      .unwrap_or_else(|| "NyaaCord".to_owned());
   let sections_js = options.get::<_, Option<Array<'js>>>("sections")?;

   let mut sections = Vec::new();
   let mut new_buttons = Vec::new();
   if let Some(arr) = sections_js {
      for entry in arr.iter::<Object<'js>>() {
         let Ok(sec) = entry else {
            continue;
         };
         let kind = sec.get::<_, Option<String>>("type")?.unwrap_or_default();
         let sec_title = sec.get::<_, Option<String>>("title")?;
         match kind.as_str() {
            "buttons" => {
               let mut buttons = Vec::new();
               if let Some(btns) = sec.get::<_, Option<Array<'js>>>("buttons")? {
                  for item in btns.iter::<Object<'js>>() {
                     let Ok(btn) = item else {
                        continue;
                     };
                     let Ok(label) = btn.get::<_, String>("label") else {
                        continue;
                     };
                     let danger = btn.get::<_, Option<bool>>("danger")?.unwrap_or(false);
                     let on_press = btn.get::<_, Option<Function<'js>>>("onPress")?;
                     let btn_id = NEXT_BTN.fetch_add(1, Ordering::Relaxed);
                     if let Some(func) = on_press {
                        new_buttons.push((btn_id, func));
                     }
                     buttons.push(ModalButton {
                        label,
                        danger,
                        on_press: Arc::new(move || invoke_page_button(btn_id)),
                     });
                  }
               }
               sections.push(ModalSection::Buttons {
                  title: sec_title,
                  buttons,
               });
            },
            "code" => {
               let text = sec.get::<_, Option<String>>("text")?.unwrap_or_default();
               let monospace = sec.get::<_, Option<bool>>("monospace")?.unwrap_or(true);
               let highlight = sec.get::<_, Option<bool>>("highlight")?.unwrap_or(false);
               sections.push(ModalSection::Code {
                  title: sec_title,
                  text,
                  monospace,
                  highlight,
               });
            },
            "text" => {
               let text = sec.get::<_, Option<String>>("text")?.unwrap_or_default();
               sections.push(ModalSection::Text {
                  title: sec_title,
                  text,
               });
            },
            _ => {},
         }
      }
   }

   if let Some(registry) = ctx.userdata::<Desktop<'js>>() {
      let mut map = registry.page_buttons.borrow_mut();
      for (btn_id, func) in new_buttons {
         map.insert(btn_id, (page_id, plugin.to_owned(), func));
      }
   }

   let modal_id = modal::open_page(PageModalSpec { title, sections });
   let obj = Object::new(ctx.clone())?;
   obj.set(
      "close",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
         modal::close_page(modal_id);
         if let Some(registry) = ctx.userdata::<Desktop<'js>>() {
            registry
               .page_buttons
               .borrow_mut()
               .retain(|_, (pid, ..)| *pid != page_id);
         }
      })?,
   )?;
   Ok(obj)
}

/// Wires desktop `phoenix.ui`, `phoenix.messages.onSend` / `onEmojiPick`, and
/// `phoenix.commands` onto the plugin's API objects.
pub fn desktop_patch_api<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   api: &Object<'js>,
   messages: &Object<'js>,
) -> rquickjs::Result<()> {
   let name = plugin.to_owned();
   messages.set(
      "onSend",
      Function::new(
         ctx.clone(),
         move |ctx: Ctx<'js>, function: Function<'js>| on_send(&ctx, &name, function),
      )?,
   )?;
   let name = plugin.to_owned();
   messages.set(
      "onEmojiPick",
      Function::new(
         ctx.clone(),
         move |ctx: Ctx<'js>, _function: Opt<Value<'js>>| on_emoji_pick(&ctx, &name),
      )?,
   )?;
   let name = plugin.to_owned();
   messages.set(
      "suppressTyping",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
         suppress_typing(&ctx, &name)
      })?,
   )?;
   let commands = Object::new(ctx.clone())?;
   commands.set(
      "register",
      Function::new(ctx.clone(), |ctx: Ctx<'js>, _options: Opt<Value<'js>>| {
         noop_handle(&ctx)
      })?,
   )?;
   commands.set(
      "list",
      Function::new(ctx.clone(), |ctx: Ctx<'js>| rquickjs::Array::new(ctx))?,
   )?;
   api.set("commands", commands)?;
   api.set("ui", desktop_ui_object(ctx, plugin)?)
}

pub fn desktop_ui_object<'js>(ctx: &Ctx<'js>, plugin: &str) -> rquickjs::Result<Object<'js>> {
   let ui = Object::new(ctx.clone())?;
   ui.set(
      "toast",
      Function::new(ctx.clone(), |message: String| {
         phoenix_ui_desktop::toast(&message);
      })?,
   )?;
   ui.set(
      "copy",
      Function::new(ctx.clone(), |ctx: Ctx<'js>, text: String| {
         phoenix_ui_desktop::copy(&text)
            .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
      })?,
   )?;
   ui.set(
      "notify",
      Function::new(ctx.clone(), |ctx: Ctx<'js>, options: Object<'js>| {
         let title = options
            .get::<_, Option<String>>("title")?
            .unwrap_or_default();
         let body = options.get::<_, Option<String>>("body")?;
         phoenix_ui_desktop::notify(&title, body.as_deref());
         noop_handle(&ctx)
      })?,
   )?;

   let owner = plugin.to_owned();
   ui.set(
      "addMessageAccessory",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>, options: Object<'js>| {
         add_message_accessory(&ctx, &owner, &options)
      })?,
   )?;

   let action_owner = plugin.to_owned();
   ui.set(
      "addMessageAction",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>, options: Object<'js>| {
         add_message_action(&ctx, &action_owner, &options)
      })?,
   )?;

   let composer_owner = plugin.to_owned();
   ui.set(
      "addComposerButton",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>, options: Object<'js>| {
         add_composer_button(&ctx, &composer_owner, &options)
      })?,
   )?;

   let page_owner = plugin.to_owned();
   ui.set(
      "openPage",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>, options: Object<'js>| {
         open_page(&ctx, &page_owner, &options)
      })?,
   )?;

   // Graceful no-op shims for remaining mobile UI-op surfaces so cross-platform
   // plugins load without throwing on desktop.
   for slot_fn in [
      "addHeaderButton",
      "addProfileSection",
      "addProfileTab",
      "hide",
      "patchProps",
      "onEvent",
      "onNode",
      "mount",
      "dispatch",
      "update",
   ] {
      ui.set(
         slot_fn,
         Function::new(ctx.clone(), |ctx: Ctx<'js>| noop_handle(&ctx))?,
      )?;
   }
   for query_fn in ["find", "findDown"] {
      ui.set(
         query_fn,
         Function::new(ctx.clone(), |ctx: Ctx<'js>| rquickjs::Array::new(ctx))?,
      )?;
   }
   for nullable_fn in ["node", "root"] {
      ui.set(
         nullable_fn,
         Function::new(ctx.clone(), |ctx: Ctx<'js>| Value::new_null(ctx))?,
      )?;
   }
   ui.set(
      "choose",
      Function::new(ctx.clone(), |ctx: Ctx<'js>, options: Object<'js>| {
         choose_dialog(&ctx, &options)
      })?,
   )?;
   ui.set(
      "showSheet",
      Function::new(ctx.clone(), |ctx: Ctx<'js>, options: Object<'js>| {
         sheet_dialog(&ctx, &options)
      })?,
   )?;
   Ok(ui)
}

/// `phoenix.ui.choose` as a native Mana modal; the picked index, or -1 when it
/// was dismissed.
fn choose_dialog<'js>(
   ctx: &Ctx<'js>,
   options: &Object<'js>,
) -> rquickjs::Result<rquickjs::Promise<'js>> {
   let title = options.get::<_, Option<String>>("title")?;
   let message = options.get::<_, Option<String>>("message")?;
   let items = parse_choice_options(options)?;
   if message.is_none() && items.is_empty() {
      return Err(Exception::throw_type(
         ctx,
         "choose needs options or a message",
      ));
   }
   let body = message.map(|text| ModalSection::Text { title: None, text });
   let title = title.unwrap_or_else(|| "NyaaCord".to_owned());
   in_background(ctx, move || answer(title, body, items))
}

/// `phoenix.ui.showSheet` as a native Mana modal with the code viewer.
fn sheet_dialog<'js>(
   ctx: &Ctx<'js>,
   options: &Object<'js>,
) -> rquickjs::Result<rquickjs::Promise<'js>> {
   let title = options.get::<_, Option<String>>("title")?;
   let text = options.get::<_, String>("text")?;
   let monospace = options
      .get::<_, Option<bool>>("monospace")?
      .unwrap_or_default();
   let highlight = options
      .get::<_, Option<bool>>("highlight")?
      .unwrap_or_default();
   let items = parse_choice_options(options)?;
   let body = ModalSection::Code {
      title: None,
      text,
      monospace,
      highlight,
   };
   let title = title.unwrap_or_else(|| "NyaaCord".to_owned());
   in_background(ctx, move || answer(title, Some(body), items))
}

/// Opens a choose dialog and waits for the picked index, -1 when it is
/// dismissed or left open for ten minutes.
fn answer(
   title: String,
   body: Option<ModalSection>,
   items: Vec<modal::ChoiceOption>,
) -> Settlement {
   Settlement::Int(Ok(modal::ask(title, body, items, Duration::from_mins(10))))
}

fn parse_choice_options<'js>(options: &Object<'js>) -> rquickjs::Result<Vec<modal::ChoiceOption>> {
   Ok(options
      .get::<_, Option<Vec<Value<'js>>>>("options")?
      .unwrap_or_default()
      .into_iter()
      .filter_map(|option| {
         if let Some(label) = option.as_string() {
            return Some(modal::ChoiceOption {
               label: label.to_string().ok()?,
               danger: false,
            });
         }
         let option = option.as_object()?;
         Some(modal::ChoiceOption {
            label: option.get::<_, Option<String>>("label").ok().flatten()?,
            danger: option
               .get::<_, Option<bool>>("danger")
               .ok()
               .flatten()
               .unwrap_or_default(),
         })
      })
      .collect())
}

/// Runs registered `phoenix.messages.onSend` hooks against the JSON payload
/// of an outgoing `POST /channels/<channel_id>/messages` request on desktop.
pub fn intercept_send_body(engine: &Engine, channel_id: &str, body: &[u8]) -> RequestAction {
   engine.with(|ctx| {
      let Some(registry) = ctx.userdata::<Registry<'_>>() else {
         return RequestAction::Pass;
      };
      let interceptors = registry
         .interceptors
         .borrow()
         .iter()
         .map(|(_, plugin, function)| (plugin.clone(), function.clone()))
         .collect::<Vec<_>>();
      drop(registry);
      if interceptors.is_empty() {
         return RequestAction::Pass;
      }
      let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(body) else {
         return RequestAction::Pass;
      };
      let Some(original) = payload
         .get("content")
         .and_then(serde_json::Value::as_str)
         .map(str::to_owned)
      else {
         return RequestAction::Pass;
      };
      let Ok(argument) = Object::new(ctx.clone()) else {
         return RequestAction::Pass;
      };
      let _ = argument.set("channelId", channel_id);

      let mut value = original.clone();
      for (plugin, function) in interceptors {
         restart_budget(&ctx);
         match function
            .call::<_, Value<'_>>((value.clone(), argument.clone()))
            .catch(&ctx)
         {
            Ok(result) if result.as_bool() == Some(false) => {
               return RequestAction::Drop;
            },
            Ok(result) => {
               if let Some(text) = result.as_string().and_then(|text| text.to_string().ok()) {
                  value = text;
               }
            },
            Err(err) => log::error!("[{plugin}] onSend: {err}"),
         }
      }
      if value == original {
         return RequestAction::Pass;
      }
      if let Some(obj) = payload.as_object_mut() {
         obj.insert("content".to_owned(), serde_json::Value::String(value));
      }
      serde_json::to_vec(&payload).map_or(RequestAction::Pass, RequestAction::Replace)
   })
}

/// Counts the handler toward the emoji unlock. Desktop has no picker presses
/// to give it: the unlocked picker inserts the emoji itself.
fn on_emoji_pick<'js>(ctx: &Ctx<'js>, plugin: &str) -> rquickjs::Result<Function<'js>> {
   static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

   let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
   let registry = ctx
      .userdata::<Desktop<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
   registry
      .emoji_pickers
      .borrow_mut()
      .push((id, plugin.to_owned()));
   emoji::unlock(true);
   drop(registry);
   Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
      if let Some(registry) = ctx.userdata::<Desktop<'js>>() {
         let mut pickers = registry.emoji_pickers.borrow_mut();
         pickers.retain(|(existing, _)| *existing != id);
         emoji::unlock(!pickers.is_empty());
      }
   })
}

/// Suppresses Desktop Phoenix's local typing-start trigger until the returned
/// remover is called or the plugin unloads.
fn suppress_typing<'js>(ctx: &Ctx<'js>, plugin: &str) -> rquickjs::Result<Function<'js>> {
   static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

   let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
   let registry = ctx
      .userdata::<Desktop<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
   registry
      .typing_suppressors
      .borrow_mut()
      .push((id, plugin.to_owned()));
   typing::suppress(true);
   drop(registry);
   Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
      if let Some(registry) = ctx.userdata::<Desktop<'js>>() {
         let mut suppressors = registry.typing_suppressors.borrow_mut();
         suppressors.retain(|(existing, _)| *existing != id);
         typing::suppress(!suppressors.is_empty());
      }
   })
}

pub fn on_send<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   function: Function<'js>,
) -> rquickjs::Result<Function<'js>> {
   static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

   let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
   let registry = ctx
      .userdata::<Registry<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
   registry
      .interceptors
      .borrow_mut()
      .push((id, plugin.to_owned(), function));
   drop(registry);
   Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
      if let Some(registry) = ctx.userdata::<Registry<'js>>() {
         registry
            .interceptors
            .borrow_mut()
            .retain(|(existing, ..)| *existing != id);
      }
   })
}

#[cfg(test)]
mod tests {
   use phoenix_runtime::{
      Chosen,
      discord::RequestAction,
      js::{Engine, compile, settings_object},
   };
   use phoenix_sigs::registry::GatewayRegistry;
   use rquickjs::{CatchResultExt, Object};

   use super::*;

   fn desktop_engine() -> Engine {
      let engine = Engine::new(&GatewayRegistry::default()).expect("engine");
      engine.with(|ctx| {
         ctx.store_userdata(Desktop::default())
            .expect("store Desktop userdata");
      });
      engine
   }

   fn eval_desktop_plugin(engine: &Engine, name: &str, source: &str) {
      let err = engine.with(|ctx| {
         let run = || -> rquickjs::Result<()> {
            let api = Object::new(ctx.clone())?;
            let messages = Object::new(ctx.clone())?;
            desktop_patch_api(&ctx, name, &api, &messages)?;
            api.set("messages", messages)?;
            api.set("settings", settings_object(&ctx, name)?)?;
            compile(&ctx, source)?.call((api,))
         };
         run().catch(&ctx).err().map(|err| err.to_string())
      });
      assert!(err.is_none(), "plugin {name} failed: {err:?}");
   }

   fn drop_desktop_engine(engine: Engine) {
      engine.with(|ctx| {
         let _ = ctx.remove_userdata::<Desktop<'_>>();
      });
      composer::sync_buttons(Vec::new());
      drop(engine);
   }

   static ACC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

   #[test]
   fn async_message_accessory_resolves_and_sync_render_stays_healthy() {
      let _serial = ACC_TEST_LOCK
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      let engine = desktop_engine();
      eval_desktop_plugin(
         &engine,
         "async-acc",
         r#"
            let calls = 0;
            phoenix.ui.addMessageAccessory({
               priority: 1,
               render: () => Promise.resolve("from-promise"),
            });
            phoenix.ui.addMessageAccessory({
               priority: 2,
               render: () => {
                  calls += 1;
                  return "sync-" + calls;
               },
            });
         "#,
      );
      let epoch = accessories::mark_pending_for_test(123_456);
      let first = render_accessories(&engine, 123_456, epoch);
      assert_eq!(first.len(), 1);
      assert_eq!(first[0].text, "sync-1");
      // The worker finishing after drain must not clobber the async entry already
      // stored by `set_rendered_for_epoch`.
      assert!(!accessories::finish_pending(123_456, epoch, first));
      let cached = accessories::cached_for_test(123_456).expect("cached async+sync accessories");
      assert_eq!(
         cached
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
         vec!["from-promise", "sync-2"]
      );
      // The async callback invoked `evaluate_sync_only`, bumping `calls` to 2.
      let second = render_accessories(&engine, 123_457, epoch);
      assert_eq!(second.len(), 1);
      assert_eq!(second[0].text, "sync-3");
      drop_desktop_engine(engine);
   }

   #[test]
   fn stale_async_accessory_after_remove_does_not_repopulate_cache() {
      let _serial = ACC_TEST_LOCK
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      let engine = desktop_engine();
      eval_desktop_plugin(
         &engine,
         "async-unload",
         r#"
            let resolvePending;
            const remove = phoenix.ui.addMessageAccessory({
               priority: 1,
               render: () => new Promise((resolve) => { resolvePending = resolve; }),
            });
            phoenix.settings.define({
               finish: {
                  type: "button",
                  title: "Finish",
                  onPress() {
                     remove();
                     resolvePending("stale-after-remove");
                  }
               }
            });
         "#,
      );
      let epoch = accessories::mark_pending_for_test(999_001);
      let initial = render_accessories(&engine, 999_001, epoch);
      assert_eq!(initial, Vec::new());
      assert!(!accessories::finish_pending(999_001, epoch, initial));

      // Removing the accessory clears the cache and bumps the epoch; resolving
      // the older promise afterward must not resurrect the removed accessory.
      engine.press("async-unload", "finish");
      assert_eq!(accessories::cached_for_test(999_001), None);
      drop_desktop_engine(engine);
   }

   #[test]
   fn send_interceptors_isolate_budget_between_plugins() {
      let engine = desktop_engine();
      eval_desktop_plugin(
         &engine,
         "spinner",
         "phoenix.messages.onSend(() => { while (true) {} });",
      );
      eval_desktop_plugin(
         &engine,
         "rewriter",
         "phoenix.messages.onSend((text) => text + '!');",
      );
      let action = intercept_send_body(&engine, "42", br#"{"content":"hi"}"#);
      let RequestAction::Replace(bytes) = action else {
         panic!("expected healthy plugin after spinner to rewrite content");
      };
      let json: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
      assert_eq!(json["content"], "hi!");
      drop_desktop_engine(engine);
   }

   #[test]
   fn composer_button_settings_write_flushes_to_plugins_json() {
      let dir = std::env::temp_dir().join(format!(
         "nyaacord-composer-flush-{}",
         std::process::id()
      ));
      let choices_path = dir.join("plugins.json");
      let _ = std::fs::remove_dir_all(&dir);

      let mut engine = desktop_engine();
      engine.set_choices_path(choices_path.clone());
      eval_desktop_plugin(
         &engine,
         "silent-typing",
         r#"
            const settings = phoenix.settings.define({
               enabled: { type: "switch", title: "Silent typing", default: true }
            });
            phoenix.ui.addComposerButton({
               icon: "res://ic_mana_chat_eye",
               label: "Silent Typing",
               onPress() {
                  settings.enabled = !settings.enabled;
               }
            });
         "#,
      );

      // Seed plugins.json with an existing saved settings entry first.
      engine.set_setting("silent-typing", "enabled", Chosen::Switch(true));
      let initial: serde_json::Value = serde_json::from_slice(
         &std::fs::read(&choices_path).expect("plugins.json after set_setting"),
      )
      .expect("valid initial plugins.json");
      assert_eq!(initial["settings"]["silent-typing"]["enabled"], true);

      let button_id = engine.with(|ctx| {
         ctx.userdata::<Desktop<'_>>()
            .and_then(|registry| {
               registry
                  .composer_buttons
                  .borrow()
                  .iter()
                  .find(|entry| entry.plugin == "silent-typing")
                  .map(|entry| entry.id)
            })
            .expect("registered composer button")
      });

      invoke_composer_button(&engine, button_id, 0);
      let after_off: serde_json::Value = serde_json::from_slice(
         &std::fs::read(&choices_path).expect("plugins.json after first composer press"),
      )
      .expect("valid plugins.json after first press");
      assert_eq!(after_off["settings"]["silent-typing"]["enabled"], false);

      invoke_composer_button(&engine, button_id, 0);
      let after_on: serde_json::Value = serde_json::from_slice(
         &std::fs::read(&choices_path).expect("plugins.json after second composer press"),
      )
      .expect("valid plugins.json after second press");
      assert_eq!(after_on["settings"]["silent-typing"]["enabled"], true);

      drop_desktop_engine(engine);
      let _ = std::fs::remove_dir_all(&dir);
   }
}
