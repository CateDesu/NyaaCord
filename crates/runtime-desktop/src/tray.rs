//! Native Desktop Phoenix system tray rebranding.
//!
//! Keeps `StatusNotifierItem::id` and `desktop-entry` as
//! `com.discord.libdiscord` while updating visible tray labels, titles, and
//! tooltips to `Nyaacord`.

use phoenix_hook::image::Image;
use phoenix_sigs::desktop;

const BRAND: &str = "Nyaacord";

/// Rebrands Desktop Phoenix's native tray strings and hooks before its own
/// threads start.
pub fn install(image: &Image<'static>) {
   let app = match desktop::tray_app_name(image) {
      Ok(app) => {
         let words = [
            (app.slot, BRAND.as_ptr().addr()),
            (app.slot + 8, BRAND.len()),
         ];
         // SAFETY: `app.slot` is a static `{ ptr, len }` descriptor in a
         // non-executable segment and `BRAND` has `'static` lifetime.
         match unsafe { image.swap_words(&words) } {
            Ok(_) => log::info!("tray: rebranded app name to {BRAND}"),
            Err(err) => log::warn!("tray: app name swap failed: {err}"),
         }
         Some(app)
      },
      Err(err) => {
         log::warn!("tray: app name: {err}");
         None
      },
   };

   #[cfg(target_os = "linux")]
   if let Some(app) = app {
      linux::install_sni_title(image, app);
   }

   #[cfg(windows)]
   windows::install(image, app.map(|found| found.menu_fn));
}

#[cfg(target_os = "linux")]
mod linux {
   use std::{ffi::c_void, sync::OnceLock};

   use phoenix_hook::image::Image;
   use phoenix_sigs::desktop;

   use super::BRAND;

   type CloneStrFn = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> *mut c_void;

   static SNI_CLONE: OnceLock<CloneStrFn> = OnceLock::new();

   /// Replaces Discord's `Tray::title` leaf by allocating `BRAND` through
   /// Discord's own `String::from(&str)` so dropping it uses the host allocator.
   unsafe extern "C" fn sni_title_thunk(out: *mut c_void, _self: *const c_void) -> *mut c_void {
      let Some(&clone_fn) = SNI_CLONE.get() else {
         return out;
      };
      // SAFETY: `clone_fn` is Discord's own `String::from(&str)` helper,
      // writing an owned host `String` into `out` and returning `out`.
      unsafe { clone_fn(out, BRAND.as_ptr(), BRAND.len()) }
   }

   pub fn install_sni_title(image: &Image<'static>, app: desktop::TrayAppName) {
      let sni = match desktop::sni_title(image, app) {
         Ok(sni) => sni,
         Err(err) => {
            log::warn!("tray: sni title: {err}");
            return;
         },
      };
      // SAFETY: `sni.clone_fn` was decoded from the `jmp` target of the leaf
      // `Tray::title` implementation with SysV `(out, ptr, len) -> out` ABI.
      let clone_fn = unsafe { std::mem::transmute::<usize, CloneStrFn>(sni.clone_fn) };
      let _ = SNI_CLONE.set(clone_fn);
      // SAFETY: `sni.got_slot` is the writable GOT entry holding `sni.func`,
      // and `sni_title_thunk` matches its SysV sret ABI.
      match unsafe { image.swap_words(&[(sni.got_slot, sni_title_thunk as *const () as usize)]) } {
         Ok(_) => log::info!("tray: rebranded SNI title to {BRAND}"),
         Err(err) => log::warn!("tray: sni title swap failed: {err}"),
      }
   }
}

#[cfg(windows)]
mod windows {
   use std::{
      ffi::c_void,
      mem::MaybeUninit,
      panic::{self, AssertUnwindSafe},
      sync::OnceLock,
   };

   use phoenix_hook::image::Image;
   use phoenix_sigs::desktop;

   use super::BRAND;
   use crate::{
      discord::HostDeallocFn,
      os::{DesktopOs as _, Os},
   };

   type TooltipSetterFn =
      unsafe extern "C" fn(*mut c_void, *mut c_void, *const u8, usize) -> *mut c_void;
   type StringFromFn = unsafe extern "C" fn(*mut RawString, *const u8, usize) -> *mut RawString;
   type MenuPushFn = unsafe extern "C" fn(*mut c_void, *mut RawTrayEntry);

   #[repr(C)]
   #[derive(Clone, Copy)]
   struct RawString {
      cap: usize,
      ptr: *mut u8,
      len: usize,
   }

   #[repr(C)]
   #[derive(Clone, Copy)]
   struct RawTrayEntry {
      tag: usize,
      data: *mut RawHeaderItem,
      extra: usize,
   }

   #[repr(C)]
   #[derive(Clone, Copy)]
   struct RawHeaderItem {
      head: [usize; 2],
      label: RawString,
      flags: u16,
   }

   const HEADER_DISABLED_FLAGS: u16 = 0x0200;

   struct HeaderHook {
      orig: MenuPushFn,
      string_from: StringFromFn,
      host_dealloc: Option<HostDeallocFn>,
   }

   static TOOLTIP_ORIG: OnceLock<TooltipSetterFn> = OnceLock::new();
   static HEADER_HOOK: OnceLock<HeaderHook> = OnceLock::new();

   fn is_discord_brand(ptr: *const u8, len: usize) -> bool {
      if ptr.is_null() || (len != 7 && len != 15) {
         return false;
      }
      let Some(bytes) = Os::read_memory(ptr.addr(), len) else {
         return false;
      };
      bytes.as_slice() == b"Discord" || bytes.as_slice() == b"Discord Phoenix"
   }

   unsafe extern "C" fn tooltip_detour(
      sret: *mut c_void,
      builder: *mut c_void,
      ptr: *const u8,
      len: usize,
   ) -> *mut c_void {
      let Some(&orig) = TOOLTIP_ORIG.get() else {
         return sret;
      };
      let is_brand =
         panic::catch_unwind(AssertUnwindSafe(|| is_discord_brand(ptr, len))).unwrap_or(false);
      let (ptr, len) = if is_brand {
         (BRAND.as_ptr(), BRAND.len())
      } else {
         (ptr, len)
      };
      // SAFETY: forwards to the trampoline for Discord's tooltip setter.
      unsafe { orig(sret, builder, ptr, len) }
   }

   fn read_struct<T: Copy>(addr: usize) -> Option<T> {
      let bytes = Os::read_memory(addr, size_of::<T>())?;
      if bytes.len() < size_of::<T>() {
         return None;
      }
      // SAFETY: `bytes` has at least `size_of::<T>()` initialized bytes.
      Some(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
   }

   unsafe extern "C" fn menu_push_detour(vec: *mut c_void, item: *mut RawTrayEntry) {
      let Some(hook) = HEADER_HOOK.get() else {
         return;
      };
      if !item.is_null() {
         let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let Some(entry) = read_struct::<RawTrayEntry>(item.addr()) else {
               return;
            };
            if entry.tag != 1 || entry.extra != 1 || entry.data.is_null() {
               return;
            }
            let Some(header) = read_struct::<RawHeaderItem>(entry.data.addr()) else {
               return;
            };
            if header.flags != HEADER_DISABLED_FLAGS
               || !is_discord_brand(header.label.ptr.cast_const(), header.label.len)
            {
               return;
            }
            let mut replacement = MaybeUninit::<RawString>::uninit();
            // SAFETY: `string_from` is Discord's own `String::from(&str)`
            // helper, returning a host-allocated `RawString`.
            let replacement = unsafe {
               (hook.string_from)(replacement.as_mut_ptr(), BRAND.as_ptr(), BRAND.len());
               replacement.assume_init()
            };
            if replacement.ptr.is_null() || replacement.len != BRAND.len() {
               return;
            }
            let old = header.label;
            // SAFETY: `entry.data` is the freshly allocated tray header item
            // owned by `item` and about to be moved into `vec`.
            unsafe {
               (*entry.data).label = replacement;
            }
            if old.cap > 0
               && !old.ptr.is_null()
               && let Some(dealloc) = hook.host_dealloc
            {
               // SAFETY: frees the replaced host `String` buffer via Discord's
               // own `__rust_dealloc(ptr, cap, 1)`.
               unsafe { dealloc(old.ptr, old.cap, 1) };
            }
         }));
      }
      // SAFETY: forwards to the trampoline for `Vec::push`.
      unsafe { (hook.orig)(vec, item) };
   }

   pub fn install(image: &Image<'static>, menu_fn: Option<usize>) {
      let mut string_from_addr = None;

      match desktop::tray_tooltip(image) {
         Ok(tooltip) => {
            string_from_addr = tooltip.string_from;
            // SAFETY: runs from `prepare` before Discord's threads start, and
            // `tooltip_detour` matches the Win64 sret signature of `setter`.
            match unsafe {
               image.hook_entry(
                  tooltip.setter,
                  tooltip.prologue.pattern,
                  tooltip.prologue.len,
                  tooltip_detour as *const () as usize,
               )
            } {
               Ok(trampoline) => {
                  // SAFETY: `trampoline` replays `tooltip.setter`'s prologue.
                  let orig = unsafe { std::mem::transmute::<usize, TooltipSetterFn>(trampoline) };
                  let _ = TOOLTIP_ORIG.set(orig);
                  log::info!("tray: hooked tooltip setter @ {:#x}", tooltip.setter);
               },
               Err(err) => log::warn!("tray: tooltip hook failed: {err}"),
            }
         },
         Err(err) => log::warn!("tray: tooltip: {err}"),
      }

      let (Some(menu_fn), Some(string_from_addr)) = (menu_fn, string_from_addr) else {
         return;
      };
      let header = match desktop::tray_header(image, menu_fn) {
         Ok(header) => header,
         Err(err) => {
            log::info!("tray: header skip: {err}");
            return;
         },
      };
      // SAFETY: runs from `prepare` before Discord's threads start, and
      // `menu_push_detour` matches the Win64 `(vec, item)` signature.
      match unsafe {
         image.hook_entry(
            header.menu_push,
            header.prologue.pattern,
            header.prologue.len,
            menu_push_detour as *const () as usize,
         )
      } {
         Ok(trampoline) => {
            // SAFETY: `trampoline` replays `menu_push`'s prologue and
            // `string_from_addr` is Discord's `String::from(&str)`.
            let hook = unsafe {
               HeaderHook {
                  orig: std::mem::transmute::<usize, MenuPushFn>(trampoline),
                  string_from: std::mem::transmute::<usize, StringFromFn>(string_from_addr),
                  host_dealloc: crate::discord::host_dealloc(),
               }
            };
            let _ = HEADER_HOOK.set(hook);
            log::info!("tray: hooked menu header @ {:#x}", header.menu_push);
         },
         Err(err) => log::warn!("tray: header hook failed: {err}"),
      }
   }
}
