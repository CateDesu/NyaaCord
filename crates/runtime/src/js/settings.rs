//! `phoenix.settings`, the settings a plugin defines and what the user chose.

use std::sync::atomic::Ordering;

use rquickjs::{CatchResultExt, Ctx, Exception, Function, IntoJs, Object, Value, object::Accessor};

use crate::{
   Choice, Chosen, Control,
   js::{Engine, Registry, drain},
};

impl Engine {
   pub fn set_setting(&self, plugin: &str, key: &str, value: Chosen) {
      self.context.with(|ctx| {
         let js_value = match &value {
            Chosen::Switch(on) => Ok(Value::new_bool(ctx.clone(), *on)),
            Chosen::Number(number) => Ok(Value::new_float(ctx.clone(), *number)),
            Chosen::Text(text) => {
               rquickjs::String::from_str(ctx.clone(), text).map(rquickjs::String::into_value)
            },
         };
         let callback = ctx.userdata::<Registry<'_>>().and_then(|registry| {
            registry
               .chosen
               .borrow_mut()
               .entry(plugin.to_owned())
               .or_default()
               .insert(key.to_owned(), value);
            redact_secrets(&registry);
            registry
               .defined
               .borrow()
               .get(plugin)
               .and_then(|list| list.iter().find(|definition| definition.key == key))
               .and_then(|definition| definition.on_change.clone())
         });
         if let Some(callback) = callback
            && let Ok(js_value) = js_value
            && let Err(err) = callback.call::<_, Value<'_>>((js_value,)).catch(&ctx)
         {
            log::error!("[{plugin}] {key} onChange: {err}");
         }
      });
      drain(&self.runtime);
      let _ = self.take_choices_dirty();
      self.save_choices();
   }

   pub fn press(&self, plugin: &str, key: &str) {
      self.context.with(|ctx| {
         let handler = ctx.userdata::<Registry<'_>>().and_then(|registry| {
            registry
               .defined
               .borrow()
               .get(plugin)?
               .iter()
               .find(|definition| definition.key == key)
               .and_then(|definition| match &definition.kind {
                  Kind::Button(handler) => Some(handler.clone()),
                  _ => None,
               })
         });
         if let Some(handler) = handler
            && let Err(err) = handler.call::<_, ()>(()).catch(&ctx)
         {
            log::error!("[{plugin}] {key}: {err}");
         }
      });
      self.drain_and_flush();
   }
}

/// One setting a plugin defined with `phoenix.settings.define`.
pub struct Definition<'js> {
   pub key: String,
   pub title: String,
   pub description: Option<String>,
   pub kind: Kind<'js>,
   on_change: Option<Function<'js>>,
}

/// A setting's type with its default, or a button's handler.
pub enum Kind<'js> {
   Switch(bool),
   Text(String),
   Secret(String),
   Select {
      options: Vec<Choice>,
      default: String,
   },
   Number(f64),
   Button(Function<'js>),
}

impl Kind<'_> {
   /// The user's choice when it has this kind's type, else the default.
   pub fn current(&self, chosen: Option<&Chosen>) -> Control {
      match (self, chosen) {
         (Self::Switch(_), Some(Chosen::Switch(on))) => Control::Switch(*on),
         (Self::Switch(default), _) => Control::Switch(*default),
         (Self::Text(_), Some(Chosen::Text(text))) => Control::Text(text.clone()),
         (Self::Text(default), _) => Control::Text(default.clone()),
         (Self::Secret(_), Some(Chosen::Text(text))) => Control::Secret(text.clone()),
         (Self::Secret(default), _) => Control::Secret(default.clone()),
         (Self::Select { options, default }, chosen) => {
            let chosen = match chosen {
               Some(Chosen::Text(value)) if options.iter().any(|choice| choice.value == *value) => {
                  value.clone()
               },
               _ => default.clone(),
            };
            Control::Select {
               options: options.clone(),
               chosen,
            }
         },
         (Self::Number(_), Some(Chosen::Number(number))) => Control::Number(*number),
         (Self::Number(default), _) => Control::Number(*default),
         (Self::Button(_), _) => Control::Button,
      }
   }
}

pub fn settings_object<'js>(ctx: &Ctx<'js>, plugin: &str) -> rquickjs::Result<Object<'js>> {
   let settings = Object::new(ctx.clone())?;
   let name = plugin.to_owned();
   settings.set(
      "define",
      Function::new(ctx.clone(), move |ctx: Ctx<'js>, schema: Object<'js>| {
         define(&ctx, &name, &schema)
      })?,
   )?;
   Ok(settings)
}

/// Hands every secret setting's current value to the logger to scrub.
fn redact_secrets(registry: &Registry<'_>) {
   let chosen = registry.chosen.borrow();
   let secrets = registry
      .defined
      .borrow()
      .iter()
      .flat_map(|(plugin, definitions)| {
         let picked = chosen.get(plugin);
         definitions.iter().filter_map(move |definition| {
            match definition
               .kind
               .current(picked.and_then(|values| values.get(&definition.key)))
            {
               Control::Secret(value) => Some(value),
               _ => None,
            }
         })
      })
      .collect::<Vec<String>>();
   crate::console::redact(secrets);
}

/// The kind of setting `key` that `spec` describes.
fn setting_kind<'js>(ctx: &Ctx<'js>, key: &str, spec: &Object<'js>) -> rquickjs::Result<Kind<'js>> {
   Ok(match spec.get::<_, String>("type")?.as_str() {
      "switch" => Kind::Switch(spec.get::<_, Option<bool>>("default")?.unwrap_or_default()),
      "text" => Kind::Text(
         spec
            .get::<_, Option<String>>("default")?
            .unwrap_or_default(),
      ),
      "secret" => Kind::Secret(
         spec
            .get::<_, Option<String>>("default")?
            .unwrap_or_default(),
      ),
      "select" => {
         let options = spec
            .get::<_, Vec<Object<'js>>>("options")?
            .into_iter()
            .map(|option| {
               let value: String = option.get("value")?;
               let label = option.get::<_, Option<String>>("label")?;
               Ok(Choice {
                  label: label.unwrap_or_else(|| value.clone()),
                  value,
               })
            })
            .collect::<rquickjs::Result<Vec<Choice>>>()?;
         let Some(first) = options.first() else {
            return Err(Exception::throw_type(
               ctx,
               &format!("setting {key}: a select needs at least one option"),
            ));
         };
         let default = spec
            .get::<_, Option<String>>("default")?
            .unwrap_or_else(|| first.value.clone());
         Kind::Select { options, default }
      },
      "number" => Kind::Number(spec.get::<_, Option<f64>>("default")?.unwrap_or_default()),
      "button" => Kind::Button(spec.get("onPress")?),
      other => {
         return Err(Exception::throw_type(
            ctx,
            &format!(
               "setting {key}: unknown type {other:?}, expected switch, text, secret, select, \
                number or button"
            ),
         ));
      },
   })
}

/// Validates and converts a JS value assigned to `settings.<key>`.
fn parse_setting_value<'js>(
   ctx: &Ctx<'js>,
   key: &str,
   kind: &Kind<'js>,
   value: &Value<'js>,
) -> rquickjs::Result<Option<Chosen>> {
   match kind {
      Kind::Switch(_) => {
         let on = value.as_bool().ok_or_else(|| {
            Exception::throw_type(ctx, &format!("setting {key}: boolean expected"))
         })?;
         Ok(Some(Chosen::Switch(on)))
      },
      Kind::Number(_) => {
         let num = value.as_number().ok_or_else(|| {
            Exception::throw_type(ctx, &format!("setting {key}: number expected"))
         })?;
         if !num.is_finite() {
            return Err(Exception::throw_range(
               ctx,
               &format!("setting {key}: finite number expected"),
            ));
         }
         Ok(Some(Chosen::Number(num)))
      },
      Kind::Text(_) | Kind::Secret(_) => {
         let text = value
            .as_string()
            .and_then(|s| s.to_string().ok())
            .ok_or_else(|| {
               Exception::throw_type(ctx, &format!("setting {key}: string expected"))
            })?;
         Ok(Some(Chosen::Text(text)))
      },
      Kind::Select { options, .. } => {
         let text = value
            .as_string()
            .and_then(|s| s.to_string().ok())
            .ok_or_else(|| {
               Exception::throw_type(ctx, &format!("setting {key}: string expected"))
            })?;
         if !options.iter().any(|choice| choice.value == text) {
            return Err(Exception::throw_range(
               ctx,
               &format!("setting {key}: unknown option {text:?}"),
            ));
         }
         Ok(Some(Chosen::Text(text)))
      },
      Kind::Button(_) => Ok(None),
   }
}

/// Persists a programmatic write to `settings.<key>` without firing `onChange`.
fn write_setting<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   key: &str,
   value: &Value<'js>,
) -> rquickjs::Result<()> {
   let registry = ctx
      .userdata::<Registry<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?;
   let next = {
      let defined = registry.defined.borrow();
      let Some(definition) = defined
         .get(plugin)
         .and_then(|list| list.iter().find(|definition| definition.key == key))
      else {
         return Ok(());
      };
      let Some(chosen) = parse_setting_value(ctx, key, &definition.kind, value)? else {
         return Ok(());
      };
      chosen
   };
   registry
      .chosen
      .borrow_mut()
      .entry(plugin.to_owned())
      .or_default()
      .insert(key.to_owned(), next);
   redact_secrets(&registry);
   registry.choices_dirty.store(true, Ordering::Release);
   if let Some(platform) = crate::platform() {
      platform.refresh();
   }
   Ok(())
}

/// Records `schema` as the plugin's settings and returns an object with one
/// getter and setter per non-button key backed by the user's current choice.
fn define<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   schema: &Object<'js>,
) -> rquickjs::Result<Object<'js>> {
   let mut definitions = Vec::new();
   for entry in schema.props::<String, Object<'js>>() {
      let (key, spec) = entry?;
      let kind = setting_kind(ctx, &key, &spec)?;
      definitions.push(Definition {
         title: spec.get("title")?,
         description: spec.get("description")?,
         on_change: spec.get("onChange")?,
         kind,
         key,
      });
   }

   let values = Object::new(ctx.clone())?;
   for definition in &definitions {
      if matches!(definition.kind, Kind::Button(_)) {
         continue;
      }
      let (read_plugin, read_key) = (plugin.to_owned(), definition.key.clone());
      let read = move |ctx: Ctx<'js>| -> rquickjs::Result<Value<'js>> {
         let registry = ctx
            .userdata::<Registry<'js>>()
            .ok_or_else(|| Exception::throw_internal(&ctx, "no registry"))?;
         let defined = registry.defined.borrow();
         let chosen = registry.chosen.borrow();
         let Some(definition) = defined
            .get(&read_plugin)
            .and_then(|list| list.iter().find(|definition| definition.key == read_key))
         else {
            return Ok(Value::new_undefined(ctx.clone()));
         };
         let picked = chosen
            .get(&read_plugin)
            .and_then(|values| values.get(&read_key));
         match definition.kind.current(picked) {
            Control::Switch(on) => on.into_js(&ctx),
            Control::Text(text) | Control::Secret(text) => text.into_js(&ctx),
            Control::Select { chosen, .. } => chosen.into_js(&ctx),
            Control::Number(number) => number.into_js(&ctx),
            Control::Button => Ok(Value::new_undefined(ctx.clone())),
         }
      };
      let (write_plugin, write_key) = (plugin.to_owned(), definition.key.clone());
      let write = move |ctx: Ctx<'js>, value: Value<'js>| -> rquickjs::Result<()> {
         write_setting(&ctx, &write_plugin, &write_key, &value)
      };
      values.prop(
         definition.key.as_str(),
         Accessor::new(read, write).enumerable(),
      )?;
   }
   ctx.userdata::<Registry<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?
      .defined
      .borrow_mut()
      .insert(plugin.to_owned(), definitions);
   if let Some(registry) = ctx.userdata::<Registry<'js>>() {
      redact_secrets(&registry);
   }
   Ok(values)
}
