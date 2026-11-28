//! Native GPUI message context menu (`phoenix.ui.addMessageAction`) for
//! Desktop Phoenix.
//!
//! Hooks Discord's `MessageContextMenu` item builder (called by both the
//! right-click context menu and the `...` message action popover), reads
//! `message_id` and `channel_id` from the input before Discord consumes it,
//! and appends a native `MenuItem::Separator` plus each visible plugin
//! `MenuItem::Action` with a native `Rc<dyn Fn>` click callback.

use std::{
   collections::{
      HashMap,
      VecDeque,
   },
   ffi::c_void,
   ptr,
   sync::{
      LazyLock,
      Mutex,
      OnceLock,
      PoisonError,
      RwLock,
      atomic::{
         AtomicU64,
         AtomicUsize,
         Ordering,
      },
      mpsc,
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::desktop::{
   self,
   MenuLayout,
   layout,
};

use crate::gpui::{
   self,
   RcFnVtbl,
   Scratch,
   SharedString,
};

/// `Vec<MenuItem>` in Discord's Rust ABI (`[cap, ptr, len]`).
#[repr(C)]
pub struct RawMenuVec {
   cap: usize,
   ptr: *mut c_void,
   len: usize,
}

type RawMenuItem = Scratch<{ layout::SCRATCH_CAP }>;

type BuildMenuFn = unsafe extern "C" fn(
   sret: *mut RawMenuVec,
   input: *const c_void,
   a2: usize,
   a3: usize,
   s0: u64,
   s1: u64,
   s2: u64,
   s3: u64,
) -> *mut RawMenuVec;

type PropsNewFn = unsafe extern "C" fn(*mut u8, *const SharedString, u8, u8) -> *mut u8;
type WithIdFn =
   unsafe extern "C" fn(*mut RawMenuItem, *mut RawMenuItem, *const u8, usize) -> *mut RawMenuItem;
type WithIconFn = unsafe extern "C" fn(*mut RawMenuItem, *mut RawMenuItem, u16) -> *mut RawMenuItem;
type WithColorFn = unsafe extern "C" fn(*mut RawMenuItem, *mut RawMenuItem, u8) -> *mut RawMenuItem;
type VecPushFn = unsafe extern "C" fn(*mut RawMenuVec, *mut RawMenuItem);

struct MenuSymbols {
   orig_build:        BuildMenuFn,
   msg_ptr_offset:    usize,
   msg_id_offset:     usize,
   channel_id_offset: usize,
   props_new:         PropsNewFn,
   with_id:           WithIdFn,
   with_icon:         WithIconFn,
   with_color:        WithColorFn,
   vec_push:          VecPushFn,
   layout:            MenuLayout,
}

// SAFETY: function pointers and plain struct offsets into `Discord.exe`.
unsafe impl Send for MenuSymbols {}
unsafe impl Sync for MenuSymbols {}

static MENU_SYMS: OnceLock<MenuSymbols> = OnceLock::new();

/// Snapshot of one registered `phoenix.ui.addMessageAction` item, read on the
/// GPUI thread when a message context menu opens.
#[derive(Clone, Debug)]
pub struct ActionMeta {
   pub id:          u64,
   pub label:       String,
   pub icon:        Option<String>,
   pub danger:      bool,
   pub priority:    i32,
   pub has_visible: bool,
}

static ACTIVE_COUNT: AtomicUsize = AtomicUsize::new(0);
static ACTIONS: LazyLock<RwLock<Vec<ActionMeta>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Maps an opaque click token stored inside `RcInner::action_id` to the
/// `(action_id, message_id, channel_id)` to invoke when the user clicks it.
#[derive(Clone, Copy)]
struct PendingClick {
   action:  u64,
   message: u64,
   channel: u64,
}

const MAX_PENDING_CLICKS: usize = 512;

struct ClickTable {
   order: VecDeque<u64>,
   map:   HashMap<u64, PendingClick>,
}

static NEXT_CLICK_TOKEN: AtomicU64 = AtomicU64::new(1);
static CLICKS: LazyLock<Mutex<ClickTable>> = LazyLock::new(|| {
   Mutex::new(ClickTable {
      order: VecDeque::new(),
      map:   HashMap::new(),
   })
});
static WORKER_TX: OnceLock<mpsc::Sender<PendingClick>> = OnceLock::new();

static MENU_ACTION_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     menu_action_callback as *const c_void,
   call_mut:      menu_action_callback as *const c_void,
   call:          menu_action_callback as *const c_void,
   extra:         ptr::null(),
};

/// Updates the menu-thread snapshot of registered `addMessageAction` entries.
pub fn sync_actions(mut items: Vec<ActionMeta>) {
   items.sort_by_key(|item| (item.priority, item.id));
   let len = items.len();
   if let Ok(mut guard) = ACTIONS.write() {
      *guard = items;
   }
   ACTIVE_COUNT.store(len, Ordering::Release);
   if len > 0 {
      ensure_worker();
   }
}

fn ensure_worker() -> &'static mpsc::Sender<PendingClick> {
   WORKER_TX.get_or_init(|| {
      let (tx, rx) = mpsc::channel::<PendingClick>();
      let _ = std::thread::Builder::new()
         .name("sunbird-msg-act".to_owned())
         .spawn(move || {
            while let Ok(click) = rx.recv() {
               if let Some(engine) = phoenix_runtime::engine() {
                  crate::js::ui::invoke_message_action(
                     &engine,
                     click.action,
                     click.message,
                     click.channel,
                  );
               }
            }
         });
      tx
   })
}

fn register_click(click: PendingClick) -> u64 {
   let token = NEXT_CLICK_TOKEN.fetch_add(1, Ordering::Relaxed);
   let mut table = CLICKS.lock().unwrap_or_else(PoisonError::into_inner);
   while table.order.len() >= MAX_PENDING_CLICKS {
      if let Some(old) = table.order.pop_front() {
         table.map.remove(&old);
      }
   }
   table.order.push_back(token);
   table.map.insert(token, click);
   token
}

unsafe extern "C" fn menu_action_callback(
   data: *const u64,
   _event: *const c_void,
   _window: *mut c_void,
   _app: *mut c_void,
) {
   if data.is_null() {
      return;
   }
   // SAFETY: `data` points to `RcInner::action_id` allocated by `alloc_rc_u64`.
   let token = unsafe { ptr::read_unaligned(data) };
   let click = CLICKS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .map
      .get(&token)
      .copied();
   if let Some(click) = click {
      let _ = ensure_worker().send(click);
   }
}

/// Maps well-known mobile/plugin icon names to Desktop Phoenix's verified
/// Mana icon enum values (observed in `MessageContextMenu`).
fn map_icon(icon: Option<&str>) -> Option<u16> {
   let icon = icon?;
   if icon.contains("delete") || icon.contains("trash") {
      Some(0x280)
   } else if icon.contains("copy_id") || icon.contains("id") {
      Some(0x15B)
   } else if icon.contains("link") {
      Some(0x177)
   } else if icon.contains("copy") || icon.contains("angle_brackets") || icon.contains("code") {
      Some(0x099)
   } else {
      None
   }
}

/// Locates and hooks `MessageContextMenu` during [`crate::prepare`].
pub fn install(image: &Image<'static>) {
   let menu = match desktop::message_context_menu(image) {
      Ok(menu) => menu,
      Err(err) => {
         log::warn!("desktop context menu: {err}, skipping");
         return;
      },
   };

   let pro = desktop::Prologues::of(phoenix_hook::x86::Abi::of(image)).row;
   // SAFETY: runs from `prepare` before Discord threads execute;
   // `build_context_menu_detour` matches `MessageContextMenu`'s 8-arg sret ABI.
   let trampoline = match unsafe {
      image.hook_entry(
         menu.build_fn,
         pro.pattern,
         pro.len,
         build_context_menu_detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("desktop context menu: hook failed: {err}");
         return;
      },
   };

   // SAFETY: every address was validated by `desktop::message_context_menu`.
   let syms = unsafe {
      MenuSymbols {
         orig_build:        std::mem::transmute::<usize, BuildMenuFn>(trampoline),
         msg_ptr_offset:    menu.msg_ptr_offset,
         msg_id_offset:     menu.msg_id_offset,
         channel_id_offset: menu.channel_id_offset,
         props_new:         std::mem::transmute::<usize, PropsNewFn>(menu.props_new),
         with_id:           std::mem::transmute::<usize, WithIdFn>(menu.with_id),
         with_icon:         std::mem::transmute::<usize, WithIconFn>(menu.with_icon),
         with_color:        std::mem::transmute::<usize, WithColorFn>(menu.with_color),
         vec_push:          std::mem::transmute::<usize, VecPushFn>(menu.vec_push),
         layout:            menu.layout,
      }
   };
   let _ = MENU_SYMS.set(syms);
   log::info!(
      "desktop context menu: hooked MessageContextMenu @ {:#x} (msg +{:#x}, id +{:#x}, ch +{:#x}) \
       {:x?}",
      menu.build_fn,
      menu.msg_ptr_offset,
      menu.msg_id_offset,
      menu.channel_id_offset,
      menu.layout,
   );
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn build_context_menu_detour(
   sret: *mut RawMenuVec,
   input: *const c_void,
   a2: usize,
   a3: usize,
   s0: u64,
   s1: u64,
   s2: u64,
   s3: u64,
) -> *mut RawMenuVec {
   let Some(syms) = MENU_SYMS.get() else {
      return sret;
   };

   // Read `message_id` and `channel_id` BEFORE calling `orig_build`, because
   // `MessageContextMenu` takes `input` by value and drops it before returning.
   let (message_id, channel_id) = if ACTIVE_COUNT.load(Ordering::Acquire) > 0 && !input.is_null() {
      // SAFETY: `input` is a live `MessageContextMenuInput`; offsets were
      // decoded from `MessageContextMenu` and its permission check callee.
      unsafe {
         let bytes = input.cast::<u8>();
         let mid = bytes.add(syms.msg_id_offset).cast::<u64>().read_unaligned();
         let msg_ptr = bytes
            .add(syms.msg_ptr_offset)
            .cast::<*const u8>()
            .read_unaligned();
         let cid = if msg_ptr.is_null() {
            0
         } else {
            remember_message(msg_ptr, mid);
            msg_ptr
               .add(syms.channel_id_offset)
               .cast::<u64>()
               .read_unaligned()
         };
         (mid, cid)
      }
   } else {
      (0, 0)
   };

   // SAFETY: forwards all arguments verbatim to the original menu builder.
   let out = unsafe { (syms.orig_build)(sret, input, a2, a3, s0, s1, s2, s3) };

   if message_id == 0 || sret.is_null() || ACTIVE_COUNT.load(Ordering::Acquire) == 0 {
      return out;
   }

   let _ = std::panic::catch_unwind(|| {
      append_plugin_actions(syms, sret, message_id, channel_id);
   });
   out
}

/// The JSON of the message the last menu opened on, read from Discord's memory
/// while the menu's input still holds it.
static MENU_MESSAGE: Mutex<Option<(u64, std::time::Instant, String)>> = Mutex::new(None);
/// GPUI rebuilds an open menu every frame, so its message is serialized again
/// only past this age, which still catches an edit before the next menu.
const MENU_MESSAGE_FRESH: std::time::Duration = std::time::Duration::from_millis(500);

/// Serializes the message at `message` for the menu built for it.
///
/// # Safety
///
/// `message` points to the live `Message` the menu is built for.
unsafe fn remember_message(message: *const u8, id: u64) {
   if MENU_MESSAGE
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .as_ref()
      .is_some_and(|(held, at, _)| *held == id && at.elapsed() < MENU_MESSAGE_FRESH)
   {
      return;
   }
   // SAFETY: the caller's `message` is live for this call.
   if let Some(json) = unsafe { crate::message::json(message, id) } {
      *MENU_MESSAGE.lock().unwrap_or_else(PoisonError::into_inner) =
         Some((id, std::time::Instant::now(), json));
   }
}

/// The JSON of message `id`, when the last menu was opened on it.
pub fn menu_message(id: u64) -> Option<String> {
   let held = MENU_MESSAGE.lock().unwrap_or_else(PoisonError::into_inner);
   let json = held
      .as_ref()
      .filter(|(held, ..)| *held == id)
      .map(|(.., json)| json.clone());
   drop(held);
   json
}

fn append_plugin_actions(
   syms: &MenuSymbols,
   vec: *mut RawMenuVec,
   message_id: u64,
   channel_id: u64,
) {
   let actions: Vec<ActionMeta> = ACTIONS
      .read()
      .map_or_else(|_| Vec::new(), |guard| guard.clone());
   if actions.is_empty() {
      return;
   }

   let visible: Vec<ActionMeta> = actions
      .into_iter()
      .filter(|action| {
         if !action.has_visible {
            return true;
         }
         phoenix_runtime::engine().is_none_or(|engine| {
            crate::js::ui::check_action_visible(&engine, action.id, message_id, channel_id)
         })
      })
      .collect();
   if visible.is_empty() {
      return;
   }

   // Push a native separator first if the menu already has Discord's own items.
   // SAFETY: `vec` is the live `Vec<MenuItem>` just returned by `orig_build`.
   let has_existing = unsafe { (*vec).len > 0 };
   if has_existing {
      let mut sep = RawMenuItem::zeroed();
      sep.put(0, layout::MENU_ITEM_SEPARATOR_TAG);
      // SAFETY: `vec_push` copies one `MenuItem`, which fits `sep`.
      unsafe { (syms.vec_push)(vec, &raw mut sep) };
   }
   let at = &syms.layout;

   for action in visible {
      let token = register_click(PendingClick {
         action:  action.id,
         message: message_id,
         channel: channel_id,
      });
      let Some(rc_ptr) = gpui::alloc_rc_u64(token) else {
         continue;
      };
      let label_ss = SharedString::from_str(&action.label);
      let id_str = gpui::intern_str(&format!("sunbird-msg-action-{}", action.id));

      // A native `MenuItem::Action` with our `Rc<dyn Fn>` click handler, then
      // Discord's `MenuItemProps`, id, optional icon and optional danger color
      // (`2`), at the offsets `sigs` decoded.
      let mut item = RawMenuItem::zeroed();
      item.put(0, at.action_tag);
      item.put(at.on_click_tag, 0u64);
      item.put(at.on_click, rc_ptr);
      item.put(at.on_click + 8, &raw const MENU_ACTION_VTBL);
      // SAFETY: `props_new` fills `props_size` bytes at `props`, inside the
      // item; the builders take the item by value and return it through
      // buffers that fit it.
      unsafe {
         (syms.props_new)(item.as_mut_ptr().add(at.props), &raw const label_ss, 0, 0);

         let mut next = RawMenuItem::zeroed();
         (syms.with_id)(&raw mut next, &raw mut item, id_str.as_ptr(), id_str.len());
         let mut item = next;

         if let Some(icon_id) = map_icon(action.icon.as_deref()) {
            let mut next = RawMenuItem::zeroed();
            (syms.with_icon)(&raw mut next, &raw mut item, icon_id);
            item = next;
         }

         if action.danger {
            let mut next = RawMenuItem::zeroed();
            (syms.with_color)(&raw mut next, &raw mut item, 2);
            item = next;
         }

         (syms.vec_push)(vec, &raw mut item);
      }
   }
}
