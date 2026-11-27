//! Gateway hooks: `phoenix.on`, `hook`, `dispatch` and `enqueue`, and the
//! `before` and `after` phases that run them.

use phoenix_sigs::registry::EventId;
use rquickjs::{CatchResultExt, Ctx, Exception, Function, Object, Value};

use crate::{
   Error, gateway,
   js::{Before, Engine, Registry, js_error},
};

#[derive(Clone)]
pub struct Hook<'js> {
   pub plugin: String,
   pub before: Option<Function<'js>>,
   pub after: Option<Function<'js>>,
}

impl Engine {
   /// Runs the `before` hooks for `event`. Each sees the previous hook's
   /// payload.
   pub fn before(&self, event: EventId, name: &str, json: &str) -> Result<Before, Error> {
      let result = self.context.with(|ctx| {
         let hooks = registered(&ctx, event, |hook| hook.before.as_ref());
         if hooks.is_empty() {
            return Ok(Before::Keep);
         }
         let mut payload = ctx.json_parse(json).catch(&ctx).map_err(js_error)?;
         for (plugin, before) in hooks {
            match before.call::<_, Value<'_>>((payload.clone(),)).catch(&ctx) {
               Ok(result) if result.as_bool() == Some(false) => return Ok(Before::Drop),
               Ok(result) if result.is_undefined() => {},
               Ok(result) => payload = result,
               Err(err) => log::error!("[{plugin}] before {name}: {err}"),
            }
         }
         let json = ctx
            .json_stringify(payload)
            .catch(&ctx)
            .map_err(js_error)?
            .ok_or_else(|| Error::Js(format!("a before {name} hook returned no JSON value")))?;
         Ok(Before::Replace(
            json.to_string().map_err(|err| Error::Js(err.to_string()))?,
         ))
      });
      self.drain_and_flush();
      result
   }

   pub fn after(&self, event: EventId, name: &str, json: &str, ok: bool) -> Result<(), Error> {
      let result = self.context.with(|ctx| {
         let hooks = registered(&ctx, event, |hook| hook.after.as_ref());
         if hooks.is_empty() {
            return Ok(());
         }
         let payload = ctx.json_parse(json).catch(&ctx).map_err(js_error)?;
         for (plugin, after) in hooks {
            if let Err(err) = after.call::<_, ()>((payload.clone(), ok)).catch(&ctx) {
               log::error!("[{plugin}] after {name}: {err}");
            }
         }
         Ok(())
      });
      self.drain_and_flush();
      result
   }
}

/// Snapshot of one phase's callbacks, so a callback that registers a hook does
/// not run into the borrow held while iterating.
fn registered<'js>(
   ctx: &Ctx<'js>,
   event: EventId,
   phase: impl for<'hook> Fn(&'hook Hook<'js>) -> Option<&'hook Function<'js>>,
) -> Vec<(String, Function<'js>)> {
   let Some(registry) = ctx.userdata::<Registry<'js>>() else {
      return Vec::new();
   };
   let hooks = registry.hooks.borrow();
   hooks
      .get(&event)
      .into_iter()
      .flatten()
      .filter_map(|hook| Some((hook.plugin.clone(), phase(hook)?.clone())))
      .collect()
}

fn event_of<'js>(ctx: &Ctx<'js>, sig: &Value<'js>) -> rquickjs::Result<EventId> {
   ctx.userdata::<Registry<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?
      .handles
      .iter()
      .find(|(_, handle)| handle.as_value() == sig)
      .map(|(event, _)| *event)
      .ok_or_else(|| Exception::throw_type(ctx, "expected a hook point from phoenix.gateway"))
}

fn register<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   sig: &Value<'js>,
   before: Option<Function<'js>>,
   after: Option<Function<'js>>,
) -> rquickjs::Result<()> {
   let event = event_of(ctx, sig)?;
   gateway::install(event);
   ctx.userdata::<Registry<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?
      .hooks
      .borrow_mut()
      .entry(event)
      .or_default()
      .push(Hook {
         plugin: plugin.to_owned(),
         before,
         after,
      });
   Ok(())
}

pub fn add_gateway_api<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   api: &Object<'js>,
) -> rquickjs::Result<()> {
   let name = plugin.to_owned();
   api.set(
      "on",
      Function::new(
         ctx.clone(),
         move |ctx: Ctx<'js>, sig: Value<'js>, listener: Function<'js>| {
            register(&ctx, &name, &sig, None, Some(listener))
         },
      )?,
   )?;
   let name = plugin.to_owned();
   api.set(
      "hook",
      Function::new(
         ctx.clone(),
         move |ctx: Ctx<'js>, sig: Value<'js>, hooks: Object<'js>| {
            register(&ctx, &name, &sig, hooks.get("before")?, hooks.get("after")?)
         },
      )?,
   )?;
   api.set(
      "dispatch",
      Function::new(
         ctx.clone(),
         |ctx: Ctx<'js>, sig: Value<'js>, payload: Value<'js>| {
            let event = event_of(&ctx, &sig)?;
            let json = ctx
               .json_stringify(payload)?
               .ok_or_else(|| Exception::throw_type(&ctx, "dispatch needs a JSON payload"))?
               .to_string()?;
            gateway::dispatch(event, &json).ok_or_else(|| {
               Exception::throw_message(&ctx, "dispatch runs only inside a gateway hook")
            })
         },
      )?,
   )?;

   api.set(
      "enqueue",
      Function::new(
         ctx.clone(),
         |ctx: Ctx<'js>, sig: Value<'js>, payload: Value<'js>| {
            let event = event_of(&ctx, &sig)?;
            let json = ctx
               .json_stringify(payload)?
               .ok_or_else(|| Exception::throw_type(&ctx, "enqueue needs a JSON payload"))?
               .to_string()?;
            gateway::enqueue(event, json);
            Ok::<_, rquickjs::Error>(())
         },
      )?,
   )?;
   Ok(())
}
