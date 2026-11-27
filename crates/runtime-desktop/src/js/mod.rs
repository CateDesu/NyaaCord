//! The desktop share of the `phoenix` object: `phoenix.ui` and message
//! accessories, and `phoenix.native` for the console.

pub mod native;
pub mod ui;

use std::{cell::RefCell, collections::HashMap};

use rquickjs::{Ctx, Function, JsLifetime};

use crate::{
   accessories, composer, context_menu, emoji,
   js::ui::{DesktopAccessory, DesktopAction, DesktopComposerButton},
   typing,
};

/// What plugins registered with the desktop UI, kept in the runtime's userdata
/// next to the core's registry.
#[derive(Default)]
pub struct Desktop<'js> {
   /// `phoenix.ui.addMessageAccessory` callbacks.
   pub accessories: RefCell<Vec<DesktopAccessory<'js>>>,
   /// `phoenix.ui.addMessageAction` callbacks.
   pub actions: RefCell<Vec<DesktopAction<'js>>>,
   /// `phoenix.ui.addComposerButton` buttons.
   pub composer_buttons: RefCell<Vec<DesktopComposerButton<'js>>>,
   /// Active `phoenix.ui.openPage` button callbacks, keyed by button ID.
   pub page_buttons: RefCell<HashMap<u64, (u64, String, Function<'js>)>>,
   /// `phoenix.messages.onEmojiPick` handlers by id and plugin. Desktop has
   /// no picker presses to give them, only the unlock they turn on.
   pub emoji_pickers: RefCell<Vec<(u64, String)>>,
   /// `phoenix.messages.suppressTyping` registrations by id and plugin.
   pub typing_suppressors: RefCell<Vec<(u64, String)>>,
}

// SAFETY: every field is bound to 'js, and Changed swaps exactly that lifetime.
unsafe impl<'js> JsLifetime<'js> for Desktop<'js> {
   type Changed<'to> = Desktop<'to>;
}

/// Drops the accessories, context menu actions, composer buttons, open page
/// callbacks, emoji pick handlers, and typing suppressors `name` registered
/// and redraws Discord without them.
pub fn unload(ctx: &Ctx<'_>, name: &str) {
   let Some(registry) = ctx.userdata::<Desktop<'_>>() else {
      return;
   };
   let mut list = registry.accessories.borrow_mut();
   let before = list.len();
   list.retain(|entry| entry.plugin != name);
   if list.len() != before {
      accessories::set_active_count(list.len());
      accessories::clear_cache();
      accessories::invalidate_discord_windows();
   }
   drop(list);

   let mut actions = registry.actions.borrow_mut();
   let before_actions = actions.len();
   actions.retain(|entry| entry.plugin != name);
   if actions.len() != before_actions {
      let meta = actions.iter().map(DesktopAction::meta).collect();
      context_menu::sync_actions(meta);
   }
   drop(actions);

   let mut buttons = registry.composer_buttons.borrow_mut();
   let before_buttons = buttons.len();
   buttons.retain(|entry| entry.plugin != name);
   if buttons.len() != before_buttons {
      let meta = buttons.iter().map(DesktopComposerButton::meta).collect();
      composer::sync_buttons(meta);
   }
   drop(buttons);

   registry
      .page_buttons
      .borrow_mut()
      .retain(|_, (_, plugin, _)| plugin != name);

   let mut pickers = registry.emoji_pickers.borrow_mut();
   pickers.retain(|(_, plugin)| plugin != name);
   emoji::unlock(!pickers.is_empty());
   drop(pickers);

   let mut suppressors = registry.typing_suppressors.borrow_mut();
   suppressors.retain(|(_, plugin)| plugin != name);
   typing::suppress(!suppressors.is_empty());
}
