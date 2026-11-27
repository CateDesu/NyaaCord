//! Desktop Phoenix chat composer buttons (`phoenix.ui.addComposerButton`).
//!
//! Hooks `ChatComposer::render` and injects plugin toolbar buttons directly
//! into the right-side composer toolbar `Div`, immediately to the left of the
//! native emoji button (`"emoji-tooltip"`). Each button renders a native GPUI
//! `Svg` icon resolved from Discord's own Mana `Icon` table (falling back to a
//! compact label pill when an icon name is unknown or SVG builders are absent)
//! and dispatches clicks through a dedicated worker thread so `QuickJS` never
//! runs on GPUI's UI thread.

use std::{
   ffi::c_void,
   mem::MaybeUninit,
   ptr,
   sync::{
      LazyLock, OnceLock, RwLock,
      atomic::{AtomicUsize, Ordering},
      mpsc,
   },
};

use phoenix_hook::{image::Image, x86::Abi};
use phoenix_sigs::desktop::{self, Dismiss, layout};

use crate::{
   gpui::{self, RawAnyElement, RawDiv, RcFnVtbl, Scratch, gpui_px},
   modal,
};

/// Metadata for one registered composer button, kept sorted by `(priority, id)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerButtonMeta {
   pub id: u64,
   pub icon: String,
   pub label: String,
   /// Optional 32-bit ARGB icon tint (`0xAARRGGBB`).
   pub tint: Option<u32>,
   pub priority: i32,
}

type RawSvg = Scratch<{ layout::SCRATCH_CAP }>;

/// `ChatComposer::render(sret, self, channel_id, arg3, window, cx) -> sret`.
type ComposerRenderFn = unsafe extern "C" fn(
   sret: *mut RawAnyElement,
   self_ptr: *const c_void,
   channel_id: u64,
   arg3: *const c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement;

type DivIdFn = unsafe extern "C" fn(*mut RawDiv, *mut RawDiv, *const u8, usize) -> *mut RawDiv;
type ListenerPushFn = unsafe extern "C" fn(*mut c_void, *mut u8, *const RcFnVtbl);
type IsHoveredFn = unsafe extern "C" fn(*const c_void, *const c_void) -> bool;

type SvgNewFn = unsafe extern "C" fn(*mut RawSvg, *const c_void) -> *mut RawSvg;
type SvgPathFn = unsafe extern "C" fn(*mut RawSvg, *mut RawSvg, *const u8, usize) -> *mut RawSvg;
type SvgSizeFn = unsafe extern "C" fn(*mut RawSvg, *mut RawSvg, f32) -> *mut RawSvg;
type SvgTextColorFn = unsafe extern "C" fn(*mut RawSvg, *mut RawSvg, u32) -> *mut RawSvg;
type SvgIntoAnyFn = unsafe extern "C" fn(*mut RawAnyElement, *mut RawSvg) -> *mut RawAnyElement;
type IconFromNameFn = unsafe extern "C" fn(*const u8, usize) -> u16;

struct SvgSymbols {
   caller_loc: usize,
   svg_new: SvgNewFn,
   svg_path: SvgPathFn,
   svg_size: SvgSizeFn,
   svg_text_color: SvgTextColorFn,
   svg_into_any: SvgIntoAnyFn,
   icon_from_name: IconFromNameFn,
   icon_table: usize,
}

struct ComposerSymbols {
   div_id: DivIdFn,
   element_id_offset: usize,
   div_vtbl: usize,
   drawable_div_off: usize,
   stateful_div_vtbl: usize,
   stateful_div_off: usize,
   shaker_vtbl: usize,
   shaker_any_off: usize,
   transform_vtbl: usize,
   transform_any_off: usize,
   interactivity_off: usize,
   children_off: usize,
   listener_push: ListenerPushFn,
   hitbox_is_hovered: IsHoveredFn,
   mouse_down_listeners: usize,
   propagate_event: usize,
   svg: Option<SvgSymbols>,
}

static ORIG_COMPOSER_RENDER: OnceLock<ComposerRenderFn> = OnceLock::new();
static COMPOSER_SYM: OnceLock<ComposerSymbols> = OnceLock::new();
static BUTTONS: LazyLock<RwLock<Vec<ComposerButtonMeta>>> =
   LazyLock::new(|| RwLock::new(Vec::new()));
static ACTIVE_COUNT: AtomicUsize = AtomicUsize::new(0);
static CLICK_TX: OnceLock<mpsc::Sender<(u64, u64)>> = OnceLock::new();

/// Payload stored inside each frame's `Box<dyn Fn>` mouse-down listener,
/// allocated on Discord's own heap so GPUI's `Box::drop` frees it cleanly.
#[repr(C)]
struct ComposerPressData {
   button_id: u64,
   channel_id: u64,
}

const PHASE_BUBBLE: u8 = 0;
const ALIGN_CENTER: u8 = 4;
const DEFAULT_ICON_RGBA: u32 = 0xB5BA_C1FF;
const ICON_SIZE_PX: f32 = 20.0;
const BUTTON_SIZE_PX: f32 = 30.0;
const ICON_TABLE_STRIDE: usize = 0x50;
const MAX_WALK_DEPTH: usize = 16;
const MAX_WALK_NODES: usize = 256;

const unsafe extern "C" fn noop_drop(_data: *mut u8) {}

static COMPOSER_PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size: size_of::<ComposerPressData>(),
   align: align_of::<ComposerPressData>(),
   call_once: composer_press as *const c_void,
   call_mut: composer_press as *const c_void,
   call: composer_press as *const c_void,
   extra: ptr::null(),
};

/// GPUI mouse-down callback for injected composer buttons.
///
/// Uses Rust ABI matching GPUI's `Fn::call` vtable slot (on Linux `extern "C"`
/// would expect widened `phase`, which Rust callers do not pass).
unsafe fn composer_press(
   data: *const ComposerPressData,
   _event: *const c_void,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase != PHASE_BUBBLE
         || data.is_null()
         || hitbox.is_null()
         || window.is_null()
         || app.is_null()
      {
         return;
      }
      let Some(sym) = COMPOSER_SYM.get() else {
         return;
      };
      // SAFETY: GPUI invokes this listener with live `&Hitbox` and `&Window`.
      if !unsafe { (sym.hitbox_is_hovered)(hitbox, window) } {
         return;
      }
      // Swallow the press so the composer underneath does not handle it too.
      // SAFETY: `propagate_event` is a `bool` in `App`, decoded by `sigs`.
      unsafe { app.add(sym.propagate_event).write(0) };
      // SAFETY: `data` points to our 16-byte `ComposerPressData` allocation.
      let (button_id, channel_id) = unsafe { ((*data).button_id, (*data).channel_id) };
      dispatch_click(button_id, channel_id);
   });
}

fn dispatch_click(button_id: u64, channel_id: u64) {
   let tx = CLICK_TX.get_or_init(|| {
      let (tx, rx) = mpsc::channel::<(u64, u64)>();
      let _ = std::thread::Builder::new()
         .name("nyaacord-composer-btn".to_owned())
         .spawn(move || {
            for (btn_id, ch_id) in rx {
               if let Some(engine) = phoenix_runtime::engine() {
                  crate::js::ui::invoke_composer_button(&engine, btn_id, ch_id);
               }
            }
         });
      tx
   });
   let _ = tx.send((button_id, channel_id));
}

/// Installs the inline detour on `ChatComposer::render` during `prepare()`.
/// Logs a warning and leaves composer buttons inactive if locators fail.
pub fn install(image: &Image<'static>) {
   if ORIG_COMPOSER_RENDER.get().is_some() {
      return;
   }
   let Ok(gpui_addrs) = desktop::gpui(image) else {
      log::warn!("desktop composer: gpui builders unavailable, leaving composer buttons off");
      return;
   };
   let Some(gpui_layout) = gpui::layout() else {
      log::warn!("desktop composer: gpui layouts unavailable, leaving composer buttons off");
      return;
   };
   if !gpui::supports_push_any() {
      log::warn!("desktop composer: div_push_any unavailable, leaving composer buttons off");
      return;
   }
   let found = match desktop::composer(image, &gpui_addrs, &gpui_layout.div) {
      Ok(found) => found,
      Err(err) => {
         log::warn!("desktop composer: {err}, leaving composer buttons off");
         return;
      },
   };

   let Dismiss {
      mouse_down_listeners,
      listener_push,
      hitbox_is_hovered,
      propagate_event,
   } = found.dismiss;

   // SAFETY: every function pointer was resolved from the running Discord
   // binary by `desktop::composer` with verified signatures on both ABIs.
   let symbols = unsafe {
      ComposerSymbols {
         div_id: std::mem::transmute::<usize, DivIdFn>(found.div_id),
         element_id_offset: found.element_id_offset,
         div_vtbl: found.div_vtbl,
         drawable_div_off: found.drawable_div_off,
         stateful_div_vtbl: found.stateful_div_vtbl,
         stateful_div_off: found.stateful_div_off,
         shaker_vtbl: found.shaker_vtbl,
         shaker_any_off: found.shaker_any_off,
         transform_vtbl: found.transform_vtbl,
         transform_any_off: found.transform_any_off,
         interactivity_off: gpui_layout.div.interactivity,
         children_off: gpui_layout.div.children,
         listener_push: std::mem::transmute::<usize, ListenerPushFn>(listener_push),
         hitbox_is_hovered: std::mem::transmute::<usize, IsHoveredFn>(hitbox_is_hovered),
         mouse_down_listeners,
         propagate_event,
         svg: found.svg.map(|s| SvgSymbols {
            caller_loc: s.caller_loc,
            svg_new: std::mem::transmute::<usize, SvgNewFn>(s.svg_new),
            svg_path: std::mem::transmute::<usize, SvgPathFn>(s.svg_path),
            svg_size: std::mem::transmute::<usize, SvgSizeFn>(s.svg_size),
            svg_text_color: std::mem::transmute::<usize, SvgTextColorFn>(s.svg_text_color),
            svg_into_any: std::mem::transmute::<usize, SvgIntoAnyFn>(s.svg_into_any),
            icon_from_name: std::mem::transmute::<usize, IconFromNameFn>(s.icon_from_name),
            icon_table: s.icon_table,
         }),
      }
   };

   let _ = COMPOSER_SYM.set(symbols);
   let prologue = desktop::Prologues::of(Abi::of(image)).row;
   // SAFETY: `desktop::composer` verified the entry prologue for `hook_entry`.
   let orig = match unsafe {
      image.hook_entry(
         found.render,
         prologue.pattern,
         prologue.len,
         composer_render_detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("desktop composer: hook failed: {err}, leaving composer buttons off");
         return;
      },
   };
   // SAFETY: `orig` is the relocated trampoline for `ChatComposer::render`.
   let _ =
      ORIG_COMPOSER_RENDER.set(unsafe { std::mem::transmute::<usize, ComposerRenderFn>(orig) });
   log::info!(
      "desktop composer: hooked render at {:#x} (svg={})",
      found.render,
      found.svg.is_some()
   );
}

/// Replaces the active composer buttons in deterministic `(priority, id)` order
/// and requests a GPUI redraw.
pub fn sync_buttons(mut buttons: Vec<ComposerButtonMeta>) {
   sort_buttons(&mut buttons);
   let count = buttons.len();
   if let Ok(mut guard) = BUTTONS.write() {
      *guard = buttons;
   }
   ACTIVE_COUNT.store(count, Ordering::Release);
   modal::force_redraw();
}

/// Sorts composer buttons deterministically by `(priority, id)`.
fn sort_buttons(buttons: &mut [ComposerButtonMeta]) {
   buttons.sort_by_key(|btn| (btn.priority, btn.id));
}

/// Converts a plugin icon URI (`res://ic_mana_chat_eye`, `icons/ChatEye.svg`,
/// or `ChatEye`) into the `PascalCase` Mana icon name expected by
/// `Icon::from_name`.
fn mana_icon_name(icon: &str) -> Option<String> {
   let trimmed = icon.trim();
   if trimmed.is_empty() {
      return None;
   }
   let stripped = trimmed
      .strip_prefix("res://")
      .unwrap_or(trimmed)
      .strip_prefix("icons/")
      .unwrap_or_else(|| trimmed.strip_prefix("res://").unwrap_or(trimmed));
   let stripped = stripped.strip_suffix(".svg").unwrap_or(stripped);
   let body = stripped
      .strip_prefix("ic_mana_")
      .or_else(|| stripped.strip_prefix("ic_"))
      .unwrap_or(stripped);
   if body.is_empty() {
      return None;
   }
   if !body.contains('_') && !body.contains('-') {
      let mut chars = body.chars();
      let first = chars.next()?;
      let mut out = String::with_capacity(body.len());
      out.extend(first.to_uppercase());
      out.push_str(chars.as_str());
      return Some(out);
   }
   let mut out = String::with_capacity(body.len());
   for part in body.split(['_', '-']).filter(|p| !p.is_empty()) {
      let mut chars = part.chars();
      if let Some(first) = chars.next() {
         out.extend(first.to_uppercase());
         out.push_str(chars.as_str());
      }
   }
   (!out.is_empty()).then_some(out)
}

/// Converts 32-bit ARGB (`0xAARRGGBB`) into GPUI's `Rgba` `u32` (`0xRRGGBBAA`).
/// If alpha is `0` on a non-zero RGB value, assumes opaque (`0xFF`).
const fn argb_to_rgba(argb: u32) -> u32 {
   let alpha = (argb >> 24) & 0xFF;
   let effective = if alpha == 0 && argb != 0 {
      argb | 0xFF00_0000
   } else {
      argb
   };
   effective.rotate_left(8)
}

/// Looks up Discord's static `"icons/<Name>.svg"` slice via `Icon::from_name`
/// and the static Mana icon table.
unsafe fn resolve_svg_path(svg: &SvgSymbols, icon: &str) -> Option<(*const u8, usize)> {
   let name = mana_icon_name(icon)?;
   // SAFETY: `icon_from_name` takes a borrowed UTF-8 slice and returns `u16`
   // (`0xFFFF` on miss).
   let id = unsafe { (svg.icon_from_name)(name.as_ptr(), name.len()) };
   if id == u16::MAX {
      return None;
   }
   // SAFETY: `id` is a valid index returned by Discord's own `Icon::from_name`
   // into the static icon table (`stride 0x50`, `info` pointer at `+0x10`,
   // static path `(ptr, len)` at `+0x18` / `+0x20`).
   unsafe {
      let entry = (svg.icon_table as *const u8).add(usize::from(id) * ICON_TABLE_STRIDE);
      let info = entry.add(0x10).cast::<*const u8>().read_unaligned();
      if info.is_null() {
         return None;
      }
      let ptr = info.add(0x18).cast::<*const u8>().read_unaligned();
      let len = info.add(0x20).cast::<usize>().read_unaligned();
      if ptr.is_null() || len == 0 || len > 128 {
         return None;
      }
      Some((ptr, len))
   }
}

/// Builds a native GPUI `Svg` `AnyElement` for `btn` when its icon resolves in
/// Discord's Mana icon table.
fn build_svg_icon(svg: &SvgSymbols, btn: &ComposerButtonMeta) -> Option<RawAnyElement> {
   // SAFETY: calls Discord's resolved `Icon::from_name` and `gpui::Svg`
   // builders into `SCRATCH_CAP`-sized buffers verified by `sigs`.
   unsafe {
      let (path_ptr, path_len) = resolve_svg_path(svg, &btn.icon)?;
      let mut s0 = RawSvg::zeroed();
      (svg.svg_new)(&raw mut s0, svg.caller_loc as *const c_void);
      let mut s1 = RawSvg::zeroed();
      (svg.svg_path)(&raw mut s1, &raw mut s0, path_ptr, path_len);
      let mut s2 = RawSvg::zeroed();
      (svg.svg_size)(&raw mut s2, &raw mut s1, ICON_SIZE_PX);
      let rgba = btn.tint.map_or(DEFAULT_ICON_RGBA, argb_to_rgba);
      let mut s3 = RawSvg::zeroed();
      (svg.svg_text_color)(&raw mut s3, &raw mut s2, rgba);
      let mut out = MaybeUninit::<RawAnyElement>::uninit();
      (svg.svg_into_any)(out.as_mut_ptr(), &raw mut s3);
      Some(out.assume_init())
   }
}

/// Attaches a per-frame `Box<dyn Fn>` mouse-down listener carrying
/// `(button_id, channel_id)` allocated on Discord's own heap.
fn attach_click_listener(
   sym: &ComposerSymbols,
   div: &mut RawDiv,
   button_id: u64,
   channel_id: u64,
) -> bool {
   // SAFETY: `interactivity_off` is decoded from `Div`'s layout by `sigs`.
   let interactivity = unsafe {
      div.as_mut_ptr()
         .add(sym.interactivity_off)
         .cast::<*mut u8>()
         .read_unaligned()
   };
   if interactivity.is_null() {
      return false;
   }
   let raw = gpui::alloc_discord(
      align_of::<ComposerPressData>(),
      size_of::<ComposerPressData>(),
   );
   if raw.is_null() {
      return false;
   }
   // SAFETY: `raw` was allocated with `ComposerPressData`'s size and alignment
   // on Discord's heap; GPUI's `Box<dyn Fn>::drop` calls `noop_drop` and frees
   // it with Discord's matching deallocator.
   unsafe {
      #[expect(
         clippy::cast_ptr_alignment,
         reason = "alloc_discord requested align_of::<ComposerPressData>()"
      )]
      raw.cast::<ComposerPressData>().write(ComposerPressData {
         button_id,
         channel_id,
      });
      (sym.listener_push)(
         interactivity.add(sym.mouse_down_listeners).cast::<c_void>(),
         raw,
         &raw const COMPOSER_PRESS_VTBL,
      );
   }
   true
}

/// Compact fallback badge text when an icon cannot be resolved to a Mana SVG.
fn fallback_badge_text(label: &str) -> String {
   let trimmed = label.trim();
   if trimmed.is_empty() {
      return "?".to_owned();
   }
   let first_word = trimmed.split_whitespace().next().unwrap_or(trimmed);
   first_word.chars().take(6).collect()
}

/// Builds one clickable GPUI composer toolbar button element.
fn build_composer_button(
   sym: &ComposerSymbols,
   slot_index: usize,
   btn: &ComposerButtonMeta,
   channel_id: u64,
) -> Option<RawAnyElement> {
   let mut div = gpui::h_row(0.0, false)?;
   // Assign a stable, bounded unique GPUI `ElementId` per slot so siblings
   // never collide and interned strings remain O(active buttons).
   let id_str = gpui::intern_str(&format!("nyaacord-composer-btn-{slot_index}"));
   let mut with_id = RawDiv::zeroed();
   // SAFETY: `div_id` moves `div` into `with_id` with a static element ID.
   unsafe {
      (sym.div_id)(
         &raw mut with_id,
         &raw mut div,
         id_str.as_ptr(),
         id_str.len(),
      );
   }
   div = with_id;

   let svg_child = sym.svg.as_ref().and_then(|svg| build_svg_icon(svg, btn));
   let is_svg = svg_child.is_some();
   let mut content = match svg_child {
      Some(elem) => elem,
      None => gpui::str_to_any(&fallback_badge_text(&btn.label))?,
   };

   gpui::style_div(&mut div, |s| {
      s.set(s.at.align_items, ALIGN_CENTER);
      s.set(s.at.justify_content, ALIGN_CENTER);
      s.float(s.at.flex_shrink, 0.0);
      s.lengths(&s.at.corner_radii, gpui_px(6.0));
      s.length(s.at.size[1], gpui_px(BUTTON_SIZE_PX));
      if is_svg {
         s.length(s.at.size[0], gpui_px(BUTTON_SIZE_PX));
      } else {
         s.length(s.at.padding[1], gpui_px(8.0));
         s.length(s.at.padding[3], gpui_px(8.0));
         s.length(s.at.font_size, gpui_px(12.0));
         let text_hsla = btn.tint.map_or([0.60, 0.05, 0.78, 1.0], gpui::argb_to_hsla);
         s.hsla(s.at.text_color, text_hsla);
      }
      if let Some(argb) = btn.tint {
         let mut bg = gpui::argb_to_hsla(argb);
         bg[3] = 0.18;
         s.solid_background(bg);
      }
   });

   if !gpui::push_any(&mut div, &mut content) {
      return None;
   }
   if !attach_click_listener(sym, &mut div, btn.id, channel_id) {
      return None;
   }

   let mut out = MaybeUninit::<RawAnyElement>::uninit();
   gpui::write_any(div, out.as_mut_ptr());
   // SAFETY: `write_any` initialized `out` via `Div::into_any_element`.
   Some(unsafe { out.assume_init() })
}

/// Reads a `Div`'s `ElementId::Name(SharedString)` string bytes if present.
///
/// Layout verified on both Linux 2233 and Windows 2208:
/// `Interactivity + element_id_offset` holds `ElementId`:
/// - `[+0x00]`: tag (`2` = `ElementId::Name(SharedString)`)
/// - `[+0x08]`: `SharedString { tag: u8 (0x18 Static | 0x19 Arc), ptr, len }`
const unsafe fn div_element_name<'a>(
   sym: &ComposerSymbols,
   div_ptr: *const u8,
) -> Option<&'a [u8]> {
   // SAFETY: `div_ptr` points to a live `Div` inside a `DrawableState::Render`.
   unsafe {
      let interactivity = div_ptr
         .add(sym.interactivity_off)
         .cast::<*const u8>()
         .read_unaligned();
      if interactivity.is_null() {
         return None;
      }
      let elem_id = interactivity.add(sym.element_id_offset);
      if elem_id.read() != 2 {
         return None;
      }
      let ss = elem_id.add(0x08);
      let tag = ss.read();
      let ptr = ss.add(0x08).cast::<*const u8>().read_unaligned();
      let len = ss.add(0x10).cast::<usize>().read_unaligned();
      if ptr.is_null() || len == 0 || len > 128 {
         return None;
      }
      let bytes_ptr = match tag {
         0x18 => ptr,
         0x19 => ptr.add(0x10),
         _ => return None,
      };
      Some(std::slice::from_raw_parts(bytes_ptr, len))
   }
}

/// Reads the live `(data_ptr, len)` slice of a `SmallVec<[RawAnyElement; 2]>`.
///
/// Layout verified on both Linux 2233 and Windows 2208 (`72` bytes):
/// - `cap = *(sv + 0x40)`: when `< 3`, inline array starts at `sv` with length
///   `cap`; when `>= 3`, spilled heap pointer is `*(sv + 0x00)` with length
///   `*(sv + 0x08)`.
#[expect(
   clippy::cast_ptr_alignment,
   reason = "SmallVec inline storage is checked to be 8-byte aligned before casting"
)]
unsafe fn smallvec_slice_mut<'a>(sv_ptr: *mut u8) -> Option<&'a mut [RawAnyElement]> {
   // SAFETY: `sv_ptr` points to a live `SmallVec<[RawAnyElement; 2]>` inside a `Div`.
   unsafe {
      let cap = sv_ptr.add(0x40).cast::<usize>().read_unaligned();
      if cap < 3 {
         if !sv_ptr.addr().is_multiple_of(align_of::<RawAnyElement>()) {
            return None;
         }
         let data = sv_ptr.cast::<RawAnyElement>();
         Some(std::slice::from_raw_parts_mut(data, cap))
      } else {
         let data = sv_ptr.cast::<*mut RawAnyElement>().read_unaligned();
         let len = sv_ptr.add(0x08).cast::<usize>().read_unaligned();
         if data.is_null()
            || !data.addr().is_multiple_of(align_of::<RawAnyElement>())
            || len > cap
            || len > 1024
         {
            return None;
         }
         Some(std::slice::from_raw_parts_mut(data, len))
      }
   }
}

/// Unwraps a `RawAnyElement` pointing to a `Drawable<Div>` or
/// `Drawable<Stateful<Div>>` (in `DrawableState::Render`) to its `*mut u8`
/// `Div` pointer.
const unsafe fn any_as_div(sym: &ComposerSymbols, any: &RawAnyElement) -> Option<*mut u8> {
   let base = (&raw const *any).cast::<u8>();
   // SAFETY: `RawAnyElement` is 32 bytes: `[arena_ref, data_ptr, vtable, arena]`.
   unsafe {
      let data = base.add(0x08).cast::<*mut u8>().read_unaligned();
      let vtbl = base.add(0x10).cast::<usize>().read_unaligned();
      if data.is_null() {
         return None;
      }
      let off = if vtbl == sym.div_vtbl {
         sym.drawable_div_off
      } else if vtbl == sym.stateful_div_vtbl {
         sym.stateful_div_off
      } else {
         return None;
      };
      if data.read() != 0 {
         return None;
      }
      Some(data.add(off))
   }
}

/// Recursively searches the composer `AnyElement` tree for the toolbar `Div`
/// whose immediate child has `ElementId` `"emoji-tooltip"`, returning
/// `(toolbar_div_ptr, emoji_child_index)`.
#[expect(
   clippy::cast_ptr_alignment,
   reason = "wrapped AnyElement pointers are checked for alignment before dereferencing"
)]
unsafe fn find_emoji_toolbar(
   sym: &ComposerSymbols,
   any: &mut RawAnyElement,
   depth: usize,
   visited: &mut usize,
) -> Option<(*mut u8, usize)> {
   if depth >= MAX_WALK_DEPTH || *visited >= MAX_WALK_NODES {
      return None;
   }
   *visited += 1;

   let base = (&raw mut *any).cast::<u8>();
   // SAFETY: reads `data` (+0x08) and `vtable` (+0x10) of a live `AnyElement`.
   let (data, vtbl) = unsafe {
      (
         base.add(0x08).cast::<*mut u8>().read_unaligned(),
         base.add(0x10).cast::<usize>().read_unaligned(),
      )
   };
   if data.is_null() {
      return None;
   }

   if vtbl == sym.shaker_vtbl {
      // SAFETY: `Drawable<ShakerInner>` has tag `0` at `+0` and its inner
      // `AnyElement` at `shaker_any_off`.
      if unsafe { data.read() } != 0 {
         return None;
      }
      let inner_ptr = unsafe { data.add(sym.shaker_any_off) };
      if !inner_ptr.addr().is_multiple_of(align_of::<RawAnyElement>()) {
         return None;
      }
      let inner = unsafe { &mut *inner_ptr.cast::<RawAnyElement>() };
      return unsafe { find_emoji_toolbar(sym, inner, depth + 1, visited) };
   }

   if vtbl == sym.transform_vtbl {
      // SAFETY: `Drawable<Transform>` stores its wrapped `AnyElement` at
      // `transform_any_off` (`+0x08`).
      let inner_ptr = unsafe { data.add(sym.transform_any_off) };
      if !inner_ptr.addr().is_multiple_of(align_of::<RawAnyElement>()) {
         return None;
      }
      let inner = unsafe { &mut *inner_ptr.cast::<RawAnyElement>() };
      return unsafe { find_emoji_toolbar(sym, inner, depth + 1, visited) };
   }

   let div_ptr = unsafe { any_as_div(sym, any) }?;
   let sv_ptr = unsafe { div_ptr.add(sym.children_off) };
   let children = unsafe { smallvec_slice_mut(sv_ptr) }?;

   for (idx, child) in children.iter().enumerate() {
      if let Some(child_div) = unsafe { any_as_div(sym, child) }
         && unsafe { div_element_name(sym, child_div) } == Some(b"emoji-tooltip")
      {
         return Some((div_ptr, idx));
      }
   }

   for child in children.iter_mut() {
      if let Some(found) = unsafe { find_emoji_toolbar(sym, child, depth + 1, visited) } {
         return Some(found);
      }
   }
   None
}

/// Injects all active composer buttons into `sret` immediately before the
/// native emoji button (`"emoji-tooltip"`).
fn inject_buttons(sret: *mut RawAnyElement, channel_id: u64) {
   if sret.is_null() {
      return;
   }
   let Some(sym) = COMPOSER_SYM.get() else {
      return;
   };
   let snapshot = match BUTTONS.read() {
      Ok(guard) => guard.clone(),
      Err(poisoned) => poisoned.into_inner().clone(),
   };
   if snapshot.is_empty() {
      return;
   }

   let mut visited = 0;
   // SAFETY: `sret` was just initialized by `ChatComposer::render`.
   let Some((toolbar_div, emoji_idx)) =
      (unsafe { find_emoji_toolbar(sym, &mut *sret, 0, &mut visited) })
   else {
      return;
   };

   let mut built: Vec<RawAnyElement> = Vec::with_capacity(snapshot.len());
   for (slot_idx, btn) in snapshot.iter().enumerate() {
      if let Some(elem) = build_composer_button(sym, slot_idx, btn, channel_id) {
         built.push(elem);
      }
   }
   if built.is_empty() {
      return;
   }

   // SAFETY: `toolbar_div` is a live `Div` inside `sret`; we push each built
   // element using Discord's own `SmallVec::push` (which handles inline->heap
   // spilling) and then rotate the appended elements directly before `emoji_idx`.
   unsafe {
      let sv_ptr = toolbar_div.add(sym.children_off);
      let mut pushed = 0usize;
      for mut elem in built {
         if gpui::push_any_to_smallvec(sv_ptr.cast::<c_void>(), &mut elem) {
            pushed += 1;
         }
      }
      if pushed > 0
         && let Some(slice) = smallvec_slice_mut(sv_ptr)
         && emoji_idx + pushed <= slice.len()
      {
         slice[emoji_idx..].rotate_right(pushed);
      }
   }
}

unsafe extern "C" fn composer_render_detour(
   sret: *mut RawAnyElement,
   self_ptr: *const c_void,
   channel_id: u64,
   arg3: *const c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement {
   let Some(&orig) = ORIG_COMPOSER_RENDER.get() else {
      return sret;
   };
   // SAFETY: calls the original `ChatComposer::render` trampoline with the
   // exact arguments and calling convention.
   let out = unsafe { orig(sret, self_ptr, channel_id, arg3, window, cx) };
   if ACTIVE_COUNT.load(Ordering::Acquire) > 0 {
      let _ = std::panic::catch_unwind(|| inject_buttons(sret, channel_id));
   }
   out
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn maps_mana_resource_names_to_pascal_case() {
      assert_eq!(
         mana_icon_name("res://ic_mana_chat_eye").as_deref(),
         Some("ChatEye")
      );
      assert_eq!(
         mana_icon_name("res://ic_mana_image_spoiler").as_deref(),
         Some("ImageSpoiler")
      );
      assert_eq!(
         mana_icon_name("res://ic_mana_puzzle_piece_plus").as_deref(),
         Some("PuzzlePiecePlus")
      );
      assert_eq!(
         mana_icon_name("icons/ChatDots.svg").as_deref(),
         Some("ChatDots")
      );
      assert_eq!(mana_icon_name("ChatX").as_deref(), Some("ChatX"));
      assert_eq!(mana_icon_name(""), None);
      assert_eq!(mana_icon_name("res://ic_mana_"), None);
   }

   #[test]
   fn converts_argb_to_gpui_rgba() {
      assert_eq!(argb_to_rgba(0xFF58_65F2), 0x5865_F2FF);
      assert_eq!(argb_to_rgba(0x8011_2233), 0x1122_3380);
      assert_eq!(argb_to_rgba(0x0058_65F2), 0x5865_F2FF);
      assert_eq!(argb_to_rgba(0), 0);
   }

   #[test]
   fn sorts_buttons_deterministically_by_priority_then_id() {
      let mut buttons = vec![
         ComposerButtonMeta {
            id: 3,
            icon: "res://ic_mana_image_spoiler".to_owned(),
            label: "Spoiler".to_owned(),
            tint: None,
            priority: 10,
         },
         ComposerButtonMeta {
            id: 2,
            icon: "res://ic_mana_chat_eye".to_owned(),
            label: "Silent Typing".to_owned(),
            tint: Some(0xFF58_65F2),
            priority: 0,
         },
         ComposerButtonMeta {
            id: 1,
            icon: "res://ic_mana_chat_dots".to_owned(),
            label: "First".to_owned(),
            tint: None,
            priority: 0,
         },
      ];
      sort_buttons(&mut buttons);
      let ids: Vec<u64> = buttons.iter().map(|b| b.id).collect();
      assert_eq!(ids, vec![1, 2, 3]);
   }
}
