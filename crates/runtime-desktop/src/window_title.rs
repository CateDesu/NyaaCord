//! The desktop window caption at creation and on later GPUI updates.

use std::{borrow::Cow, ffi::c_void, panic, ptr, slice, str, sync::OnceLock};

use phoenix_hook::image::Image;
use phoenix_sigs::desktop;

const BRAND: &str = "NyaaCord";

fn rewrite_title(title: &str) -> Option<Cow<'_, str>> {
   for name in ["Discord Phoenix", "Discord"] {
      let Some(prefix) = title.strip_suffix(name) else {
         continue;
      };
      if prefix.is_empty() {
         return Some(Cow::Borrowed(BRAND));
      }
      if prefix.ends_with(" - ") || prefix.ends_with(" | ") {
         let mut result = String::new();
         result
            .try_reserve_exact(prefix.len().checked_add(BRAND.len())?)
            .ok()?;
         result.push_str(prefix);
         result.push_str(BRAND);
         return Some(Cow::Owned(result));
      }
   }
   None
}

type OptionsFn = unsafe extern "C" fn(*mut u8, usize, usize, usize) -> *mut u8;
type SetterFn = unsafe extern "C" fn(*mut c_void, *const u8, usize);
static OPTIONS: OnceLock<(OptionsFn, usize)> = OnceLock::new();
static SETTER: OnceLock<SetterFn> = OnceLock::new();

unsafe extern "C" fn options(out: *mut u8, bounds: usize, a: usize, b: usize) -> *mut u8 {
   let Some(&(original, offset)) = OPTIONS.get() else {
      return out;
   };
   // SAFETY: the constructor writes its WindowOptions to the caller's return slot.
   let result = unsafe { original(out, bounds, a, b) };
   if result == out
      && !result.is_null()
      // SAFETY: the locator follows the inline SharedString into the returned titlebar.
      && panic::catch_unwind(|| unsafe { rewrite_initial(result.add(offset)) }).is_err()
   {
      log::error!("initial window title rewrite panicked");
   }
   result
}

unsafe fn rewrite_initial(field: *mut u8) {
   // Only the inline variant carries its byte length in its first byte.
   let len = usize::from(unsafe { field.read() });
   if !matches!(len, 7 | 15) {
      return;
   }
   // SAFETY: the inline variant holds these initialized bytes and has room for 23.
   let text = unsafe { slice::from_raw_parts(field.add(1), len) };
   if text == b"Discord" || text == b"Discord Phoenix" {
      let Ok(brand_len) = u8::try_from(BRAND.len()) else {
         return;
      };
      unsafe {
         ptr::copy_nonoverlapping(BRAND.as_ptr(), field.add(1), BRAND.len());
         if len > BRAND.len() {
            field.add(1 + BRAND.len()).write_bytes(0, len - BRAND.len());
         }
         field.write(brand_len);
      }
   }
}

unsafe extern "C" fn set_title(window: *mut c_void, data: *const u8, len: usize) {
   let Some(&original) = SETTER.get() else {
      return;
   };
   let rewritten = panic::catch_unwind(|| {
      if data.is_null() || isize::try_from(len).is_err() {
         return None;
      }
      // SAFETY: GPUI's setter takes a borrowed str which lives through this call.
      str::from_utf8(unsafe { slice::from_raw_parts(data, len) })
         .ok()
         .and_then(rewrite_title)
   })
   .unwrap_or_else(|_| {
      log::error!("window title update panicked");
      None
   });
   let (ptr, len) = rewritten
      .as_deref()
      .map_or((data, len), |s| (s.as_ptr(), s.len()));
   // SAFETY: GPUI borrows the argument and makes its own copy for the window.
   unsafe { original(window, ptr, len) };
}

pub fn install(image: &Image<'static>) {
   let loc = match desktop::window_title(image) {
      Ok(loc) => loc,
      Err(err) => {
         log::warn!("window title: {err}");
         return;
      },
   };
   // SAFETY: prepare runs before either entry can be called by the host.
   unsafe {
      match image.hook_entry(
         loc.options,
         loc.options_head.pattern,
         loc.options_head.len,
         options as *const () as usize,
      ) {
         Ok(addr) => {
            let _ = OPTIONS.set((
               std::mem::transmute::<usize, OptionsFn>(addr),
               loc.inline_title,
            ));
         },
         Err(err) => log::warn!("initial window title: {err}"),
      }
      match image.hook_entry(
         loc.setter,
         loc.setter_head.pattern,
         loc.setter_head.len,
         set_title as *const () as usize,
      ) {
         Ok(addr) => {
            let _ = SETTER.set(std::mem::transmute::<usize, SetterFn>(addr));
         },
         Err(err) => log::warn!("window title updates: {err}"),
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn replaces_only_complete_title_brands() {
      for brand in ["Discord", "Discord Phoenix"] {
         assert_eq!(rewrite_title(brand).as_deref(), Some(BRAND));
         for separator in [" - ", " | "] {
            for prefix in ["", "Discord", "#猫 😸", "a | b - c", "line\nbreak"] {
               let title = format!("{prefix}{separator}{brand}");
               let expected = format!("{prefix}{separator}{BRAND}");
               assert_eq!(rewrite_title(&title).as_deref(), Some(expected.as_str()));
               assert_eq!(rewrite_title(&expected), None);
            }
         }
      }
      for text in [
         "",
         BRAND,
         "discord",
         "Discord ",
         "Discord Settings",
         "My Discord",
         "x-Discord",
         "x |Discord",
         "x — Discord",
         "x - Discord!",
         "Discord | x",
      ] {
         assert_eq!(rewrite_title(text), None);
      }
   }

   #[test]
   fn rewrites_only_initialized_inline_titles() {
      for text in ["Discord", "Discord Phoenix", "Ignored", "Discord Channel"] {
         let mut field = [0xCC; 25];
         field[0] = u8::try_from(text.len()).unwrap();
         field[1..=text.len()].copy_from_slice(text.as_bytes());
         let before = field;
         unsafe { rewrite_initial(field.as_mut_ptr()) };
         if matches!(text, "Discord" | "Discord Phoenix") {
            assert_eq!(&field[..9], b"\x08NyaaCord");
            let end = (1 + text.len()).max(9);
            assert!(field[9..end].iter().all(|&byte| byte == 0));
            assert_eq!(field[end..], before[end..]);
         } else {
            assert_eq!(field, before);
         }
      }
      for tag in [0, 1, 0x18, 0x19, 0xFF] {
         let mut field = [tag; 24];
         unsafe { rewrite_initial(field.as_mut_ptr()) };
         assert_eq!(field, [tag; 24]);
      }
   }
}
