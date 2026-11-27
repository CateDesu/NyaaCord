//! Desktop Phoenix local typing-start suppression.
//!
//! Hooks `libdiscord`'s typing-start operation (`ops/typing.rs`) so that while
//! silent typing is enabled, typing in the chat box neither starts the local
//! typing indicator (member list dots) nor spawns `POST /channels/*/typing`.
//! Clearing the draft (`text_len == 0 && flag == 0`) still forwards to the
//! original function so any stale typing state clears cleanly without ever
//! triggering a typing-start event.

use std::{
   ffi::c_void,
   sync::{
      OnceLock,
      atomic::{AtomicBool, Ordering},
   },
};

use phoenix_hook::{image::Image, x86::Abi};
use phoenix_sigs::desktop;

/// `Client::typing_start(&Client, ChannelId, text: &str, flag: bool)`.
///
/// Verified on both System V (Linux build 2233) and Win64 (Windows build 2208):
/// all five arguments are borrowed or `Copy`, the return type is `()`, and the
/// typing state mutex is acquired and released internally.
type TypingStartFn = unsafe extern "C" fn(
   client: *const c_void,
   channel_id: u64,
   text_ptr: *const u8,
   text_len: usize,
   flag: u8,
);

static SUPPRESSED: AtomicBool = AtomicBool::new(false);
static ORIGINAL: OnceLock<TypingStartFn> = OnceLock::new();

/// Installs the inline detour on Discord's typing-start operation from
/// `prepare()`. Logs a warning and stays disabled if the locator or hook fails.
pub fn install(image: &Image<'static>) {
   let target = match desktop::typing_start(image) {
      Ok(target) => target,
      Err(err) => {
         log::warn!("desktop typing: {err}, leaving local typing suppression off");
         return;
      },
   };
   let prologue = desktop::Prologues::of(Abi::of(image)).frame;
   // SAFETY: `typing_start` verified the entry prologue for `hook_entry`, and
   // `typing_start_detour` matches the exact 5-argument C/Rust ABI verified on
   // both System V and Win64.
   let orig = match unsafe {
      image.hook_entry(
         target,
         prologue.pattern,
         prologue.len,
         typing_start_detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("desktop typing: hook failed: {err}, leaving local typing suppression off");
         return;
      },
   };
   // SAFETY: `orig` is the relocated trampoline for `target`.
   let _ = ORIGINAL.set(unsafe { std::mem::transmute::<usize, TypingStartFn>(orig) });
   log::info!("desktop typing: hooked typing-start at {target:#x}");
}

/// Enables or disables local typing-start suppression.
pub fn suppress(on: bool) {
   SUPPRESSED.store(on, Ordering::Release);
}

/// Detour for `libdiscord`'s typing-start operation.
///
/// Never panics across the FFI boundary: performs only an atomic read and a
/// direct trampoline call.
unsafe extern "C" fn typing_start_detour(
   client: *const c_void,
   channel_id: u64,
   text_ptr: *const u8,
   text_len: usize,
   flag: u8,
) {
   if SUPPRESSED.load(Ordering::Acquire) && !(text_len == 0 && flag == 0) {
      return;
   }
   let Some(&orig) = ORIGINAL.get() else {
      return;
   };
   // SAFETY: forwards the untouched borrowed arguments to Discord's typing
   // handler trampoline.
   unsafe { orig(client, channel_id, text_ptr, text_len, flag) };
}
