//! Desktop Discord HTTP transport and request/response interception.
//!
//! Desktop Phoenix routes all Discord REST traffic through
//! `libdiscord/crates/http`'s `dyn Transport` (`ReqwestTransport::send(&self,
//! req) -> Pin<Box<dyn Future>>`). [`install`] locates `Transport::send` and
//! the `SendFuture::poll` vtable slots (build-independently via
//! [`DesktopHttpTransport::locate`]). The API client calls `send` directly, so
//! `send` gets an inline jump to its detour with a trampoline back, and the
//! poll slots are swapped with [`Image::swap_words`].
//!
//! Captured session headers stay in memory only and `authorization` is never
//! logged or persisted.

use std::{
   borrow::Cow,
   ffi::c_void,
   ops::Range,
   panic::{self, AssertUnwindSafe},
   ptr::{self, NonNull},
   slice, str,
   sync::{
      Arc, OnceLock,
      atomic::{AtomicPtr, Ordering},
   },
};

use phoenix_hook::image::Image;
use phoenix_runtime::{
   discord::{
      Authorization, DiscordError, RequestAction, api_path, observe_request, run_response_hooks,
      session_request, take_response, track_response, watch_response,
   },
   fetch::{self, Request, Response},
};
use phoenix_sigs::{
   desktop,
   http::{self, DesktopHttpTransport, RequestLayout},
};

/// Standard HTTP/1.x `http::header::StandardHeader` names in discriminant
/// order (`0..=80`), matching `Discord.exe`'s compiled `http` crate.
const STANDARD_HEADERS: [&str; 81] = [
   "accept",
   "accept-charset",
   "accept-encoding",
   "accept-language",
   "accept-ranges",
   "access-control-allow-credentials",
   "access-control-allow-headers",
   "access-control-allow-methods",
   "access-control-allow-origin",
   "access-control-expose-headers",
   "access-control-max-age",
   "access-control-request-headers",
   "access-control-request-method",
   "age",
   "allow",
   "alt-svc",
   "authorization",
   "cache-control",
   "cache-status",
   "cdn-cache-control",
   "connection",
   "content-disposition",
   "content-encoding",
   "content-language",
   "content-length",
   "content-location",
   "content-range",
   "content-security-policy",
   "content-security-policy-report-only",
   "content-type",
   "cookie",
   "dnt",
   "date",
   "etag",
   "expect",
   "expires",
   "forwarded",
   "from",
   "host",
   "if-match",
   "if-modified-since",
   "if-none-match",
   "if-range",
   "if-unmodified-since",
   "last-modified",
   "link",
   "location",
   "max-forwards",
   "origin",
   "pragma",
   "proxy-authenticate",
   "proxy-authorization",
   "public-key-pins",
   "public-key-pins-report-only",
   "range",
   "referer",
   "referrer-policy",
   "refresh",
   "retry-after",
   "sec-websocket-accept",
   "sec-websocket-extensions",
   "sec-websocket-key",
   "sec-websocket-protocol",
   "sec-websocket-version",
   "server",
   "set-cookie",
   "strict-transport-security",
   "te",
   "trailer",
   "transfer-encoding",
   "user-agent",
   "upgrade",
   "upgrade-insecure-requests",
   "vary",
   "via",
   "warning",
   "www-authenticate",
   "x-content-type-options",
   "x-dns-prefetch-control",
   "x-frame-options",
   "x-xss-protection",
];

const STANDARD_METHODS: [&str; 10] = [
   "OPTIONS", "GET", "POST", "PUT", "DELETE", "HEAD", "TRACE", "CONNECT", "PATCH", "QUERY",
];

// ---------------------------------------------------------------------------
// ABI mirror of Desktop Phoenix's `libdiscord/crates/http` transport types
// (verified against Windows x86_64 `Discord.exe`). These are Rust layouts, so
// the same rustc and crates lay them out alike on Linux x86_64.
// ---------------------------------------------------------------------------

#[repr(C)]
struct RawVec {
   cap: usize,
   ptr: *mut u8,
   len: usize,
}

#[repr(C)]
struct RawBytesVtable {
   clone: unsafe fn(&AtomicPtr<()>, *const u8, usize) -> RawBytes,
   // bytes 1.12 consumes the handle and passes the loaded data pointer.
   to_vec: unsafe fn(*mut (), *const u8, usize) -> RawVec,
   to_mut: unsafe fn(*mut (), *const u8, usize) -> RawBytesMut,
   is_unique: unsafe fn(&AtomicPtr<()>) -> bool,
   drop: unsafe fn(*mut (), *const u8, usize),
}

#[repr(C)]
struct RawBytesMut {
   ptr: NonNull<u8>,
   len: usize,
   cap: usize,
   data: *mut (),
}
const _: () = assert!(size_of::<RawBytesMut>() == 0x20);

#[repr(C)]
struct RawBytes {
   vtable: *const RawBytesVtable,
   ptr: *const u8,
   len: usize,
   data: AtomicPtr<()>,
}

#[repr(C)]
struct RawHeaderBucket {
   tag: u64,
   next_extra: usize,
   tail_extra: usize,
   value: RawBytes,
   _sens: u64,
   name: RawBytes,
   _pad1: u64,
}
const _: () = assert!(size_of::<RawHeaderBucket>() == 0x68);

#[repr(C)]
struct RawExtraValue {
   prev_tag: u64,
   prev_idx: usize,
   next_tag: u64,
   next_idx: usize,
   value: RawBytes,
   _sens: u64,
}
const _: () = assert!(size_of::<RawExtraValue>() == 0x48);

#[repr(C)]
struct RawHeaderMap {
   _head: [u64; 3],
   entries_cap: usize,
   entries_ptr: *mut RawHeaderBucket,
   entries_len: usize,
   extra_cap: usize,
   extra_ptr: *mut RawExtraValue,
   extra_len: usize,
   _tail: [u64; 3],
}
const _: () = assert!(size_of::<RawHeaderMap>() == 0x60);

#[repr(C)]
struct RawBody {
   tag: u64,
   bytes: RawBytes,
   _rest: [u64; 2],
}
const _: () = assert!(size_of::<RawBody>() == 0x38);

#[repr(C)]
struct RawMethod {
   bytes: [u8; 24],
}
const _: () = assert!(size_of::<RawMethod>() == 0x18);

/// A `TransportRequest`'s fields, where [`RequestLayout`] puts them.
struct RawTransportRequest<'a> {
   headers: &'a mut RawHeaderMap,
   body: &'a mut RawBody,
   url: &'a mut RawVec,
   method: &'a RawMethod,
}

impl RawTransportRequest<'_> {
   /// # Safety
   ///
   /// `req` is a live `TransportRequest` laid out as `layout` says.
   #[expect(
      clippy::cast_ptr_alignment,
      reason = "every offset is a multiple of 8 into an 8-aligned struct"
   )]
   unsafe fn at(req: *mut u8, layout: RequestLayout) -> Self {
      // SAFETY: the caller's layout puts each field at its offset.
      unsafe {
         Self {
            headers: &mut *req.add(layout.headers).cast::<RawHeaderMap>(),
            body: &mut *req.add(layout.body).cast::<RawBody>(),
            url: &mut *req.add(layout.url).cast::<RawVec>(),
            method: &*req.add(layout.method).cast::<RawMethod>(),
         }
      }
   }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawPinnedFuture {
   data: *mut c_void,
   vtable: *const c_void,
}

#[repr(C)]
struct BufferedResult {
   headers: [u64; 12],
   body: RawVec,
   status: u16,
   _pad: [u8; 6],
}
const _: () = assert!(size_of::<BufferedResult>() == http::BUFFERED_RESULT_SIZE);

impl BufferedResult {
   const PENDING_TAG: u64 = u64::MAX - 1;
   const ERR_TAG: u64 = u64::MAX;

   const fn is_pending(&self) -> bool {
      self.headers[0] == Self::PENDING_TAG
   }

   const fn is_ok(&self) -> bool {
      self.headers[0] != Self::PENDING_TAG && self.headers[0] != Self::ERR_TAG
   }
}

/// `poll`'s result on builds that stream the body: `u64::MAX` pending, 2 an
/// error, and the response otherwise, whose status sits at the offset
/// [`http::ResponseLayout::Streamed`] gives.
#[repr(C)]
struct StreamedResult {
   tag: u64,
   _response: [u64; 46],
   /// 1 for the boxed stream that follows.
   body_kind: u64,
   body: RawPinnedFuture,
   _tail: [u64; 7],
}
const _: () = assert!(
   size_of::<StreamedResult>() == http::STREAMED_RESULT_SIZE
      && std::mem::offset_of!(StreamedResult, body_kind) == http::STREAMED_BODY
);

// `send` and `poll` are Rust-ABI trait methods, declared Rust ABI here so the
// same lowering applies on Win64 and SysV: `send` returns its pointer pair
// (`Pin<Box<dyn Future>>`) in RAX:RDX, and `poll` returns its result through a
// hidden sret pointer in the first argument register.
type SendFn = unsafe fn(*const c_void, *mut u8) -> RawPinnedFuture;
type BufferedPollFn = unsafe fn(*mut c_void, *mut c_void) -> BufferedResult;
type StreamedPollFn = unsafe fn(*mut c_void, *mut c_void) -> StreamedResult;
type HostAllocFn = unsafe extern "C" fn(usize, usize) -> *mut u8;
pub type HostDeallocFn = unsafe extern "C" fn(*mut u8, usize, usize);

#[cfg(windows)]
pub fn host_dealloc() -> Option<HostDeallocFn> {
   INSTALLED.get().and_then(|inst| inst.host_dealloc)
}

/// The original `poll`, by how it returns the response.
#[derive(Clone, Copy)]
enum Poll {
   Buffered(BufferedPollFn),
   Streamed { poll: StreamedPollFn, status: usize },
}

struct Installed {
   send: SendFn,
   request: RequestLayout,
   /// `None` when `poll` is left alone, so responses never reach the plugins.
   poll: Option<Poll>,
   host_alloc: Option<HostAllocFn>,
   host_dealloc: Option<HostDeallocFn>,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

// ---------------------------------------------------------------------------
// Allocator-isolated `bytes::Bytes` vtable for replaced request bodies.
// ---------------------------------------------------------------------------

static OWNED_BYTES_VTABLE: RawBytesVtable = RawBytesVtable {
   clone: owned_bytes_clone,
   to_vec: owned_bytes_to_vec,
   to_mut: owned_bytes_to_mut,
   is_unique: owned_bytes_is_unique,
   drop: owned_bytes_drop,
};

fn make_owned_bytes(data: Vec<u8>) -> RawBytes {
   let arc = Arc::new(data);
   let ptr = arc.as_ptr();
   let len = arc.len();
   let raw = Arc::into_raw(arc).cast_mut().cast::<()>();
   RawBytes {
      vtable: &raw const OWNED_BYTES_VTABLE,
      ptr,
      len,
      data: AtomicPtr::new(raw),
   }
}

unsafe fn owned_bytes_clone(data: &AtomicPtr<()>, ptr: *const u8, len: usize) -> RawBytes {
   let raw = data.load(Ordering::Relaxed);
   if !raw.is_null() {
      // SAFETY: `raw` came from `Arc::into_raw` in `make_owned_bytes`.
      unsafe { Arc::increment_strong_count(raw.cast::<Vec<u8>>()) };
   }
   RawBytes {
      vtable: &raw const OWNED_BYTES_VTABLE,
      ptr,
      len,
      data: AtomicPtr::new(raw),
   }
}

unsafe fn owned_bytes_to_vec(data: *mut (), ptr: *const u8, len: usize) -> RawVec {
   let dst = if len == 0 {
      NonNull::<u8>::dangling()
   } else {
      let Some(alloc) = INSTALLED.get().and_then(|inst| inst.host_alloc) else {
         std::process::abort();
      };
      // SAFETY: the host allocator supplies a buffer it can later free.
      let Some(buffer) = NonNull::new(unsafe { alloc(len, 1) }) else {
         std::process::abort();
      };
      // SAFETY: the handle owns `len` initialized bytes until it is dropped.
      unsafe { ptr::copy_nonoverlapping(ptr, buffer.as_ptr(), len) };
      buffer
   };
   // SAFETY: `to_vec` consumes the `Bytes` instance.
   unsafe { owned_bytes_drop(data, ptr, len) };
   RawVec {
      cap: len,
      ptr: dst.as_ptr(),
      len,
   }
}

unsafe fn owned_bytes_to_mut(data: *mut (), ptr: *const u8, len: usize) -> RawBytesMut {
   // SAFETY: both conversions consume the same handle and take ownership of the copy.
   let vec = unsafe { owned_bytes_to_vec(data, ptr, len) };
   // BytesMut stores the original capacity hint and the owned Vec flag here.
   let capacity_hint = (vec.cap >> 10)
      .checked_ilog2()
      .map_or(0, |width| width + 1)
      .min(7);
   RawBytesMut {
      // SAFETY: even an empty host Vec carries a non-null dangling pointer.
      ptr: unsafe { NonNull::new_unchecked(vec.ptr) },
      len: vec.len,
      cap: vec.cap,
      data: ptr::without_provenance_mut(1 | ((capacity_hint as usize) << 2)),
   }
}

const unsafe fn owned_bytes_is_unique(_data: &AtomicPtr<()>) -> bool {
   false
}

unsafe fn owned_bytes_drop(data: *mut (), _ptr: *const u8, _len: usize) {
   if !data.is_null() {
      // SAFETY: matches the `Arc::into_raw` / `increment_strong_count` pair.
      drop(unsafe { Arc::from_raw(data.cast::<Vec<u8>>()) });
   }
}

unsafe fn replace_bytes(target: &mut RawBytes, replacement: RawBytes) {
   let old = std::mem::replace(target, replacement);
   // SAFETY: the old handle and its vtable are live and this consumes it.
   unsafe { ((*old.vtable).drop)(old.data.into_inner(), old.ptr, old.len) };
}

// ---------------------------------------------------------------------------
// Vtable installation & detours
// ---------------------------------------------------------------------------

/// Locates Desktop Phoenix's `ReqwestTransport` and `SendFuture` vtable slots
/// and installs request/response interception.
pub fn install(image: &Image<'static>) {
   let found = match DesktopHttpTransport::locate(image) {
      Ok(found) => found,
      Err(err) => {
         log::warn!("discord desktop: HTTP transport: {err}");
         return;
      },
   };
   let request = match found.request {
      Ok(request) => request,
      Err(err) => {
         log::warn!("discord desktop: HTTP send not hooked: {err}");
         return;
      },
   };
   // Discord's API client calls `send` directly rather than through the
   // vtable, so `send` is hooked in place and the detour calls the trampoline.
   // It opens with the same register saves and `sub rsp` as a render frame.
   // SAFETY: runs from `prepare` before Discord's own code, and `send_detour`
   // matches `send`'s Rust ABI.
   let pro = desktop::Prologues::of(phoenix_hook::x86::Abi::of(image)).frame;
   let trampoline = match unsafe {
      image.hook_entry(
         found.send_fn,
         pro.pattern,
         pro.len,
         send_detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("discord desktop: HTTP send not hooked: {err}");
         return;
      },
   };
   let poll = match found.response {
      // SAFETY: `poll_fn` returns the result `layout` describes.
      Ok(http::ResponseLayout::Buffered) => Some(Poll::Buffered(unsafe {
         std::mem::transmute::<usize, BufferedPollFn>(found.poll_fn)
      })),
      // SAFETY: as above; `body_stream` wraps chunks with `make_owned_bytes`
      // (whose `to_vec` requires `host_alloc`) and frees Discord's boxed
      // stream with `host_dealloc`.
      Ok(http::ResponseLayout::Streamed { status })
         if found.host_alloc.is_some() && found.host_dealloc.is_some() =>
      {
         Some(Poll::Streamed {
            poll: unsafe { std::mem::transmute::<usize, StreamedPollFn>(found.poll_fn) },
            status,
         })
      },
      Ok(http::ResponseLayout::Streamed { .. }) => {
         log::warn!("discord desktop: hooked HTTP send, response hooks off: host allocator missing");
         None
      },
      Err(ref err) => {
         log::warn!("discord desktop: hooked HTTP send, response hooks off: {err}");
         None
      },
   };
   // SAFETY: `DesktopHttpTransport::locate` verified every target against
   // `.pdata`, and the trampoline replays `send`'s prologue.
   let installed = Installed {
      send: unsafe { std::mem::transmute::<usize, SendFn>(trampoline) },
      request,
      poll,
      host_alloc: found
         .host_alloc
         .map(|addr| unsafe { std::mem::transmute::<usize, HostAllocFn>(addr) }),
      host_dealloc: found
         .host_dealloc
         .map(|addr| unsafe { std::mem::transmute::<usize, HostDeallocFn>(addr) }),
   };
   if INSTALLED.set(installed).is_err() {
      return;
   }
   let detour = match poll {
      None => return,
      Some(Poll::Buffered(_)) => buffered_poll_detour as *const () as usize,
      Some(Poll::Streamed { .. }) => streamed_poll_detour as *const () as usize,
   };

   let polls = found
      .poll_slots
      .iter()
      .map(|&slot| (slot, detour))
      .collect::<Vec<_>>();
   // SAFETY: every poll slot is an aligned `.rdata` vtable entry pointing to
   // `poll_fn`, and the detour matches its Rust ABI and result.
   match unsafe { image.swap_words(&polls) } {
      Ok(_) => {
         log::info!(
            "discord desktop: hooked HTTP send and {} poll vtable slots",
            found.poll_slots.len(),
         );
      },
      Err(err) => log::error!("discord desktop: failed to hook HTTP transport: {err}"),
   }
}

/// # Safety
///
/// Reads a live `RawVec` (Rust `String`) owned by the host.
unsafe fn read_str<'a>(raw: &RawVec) -> Option<&'a str> {
   if raw.len == 0 {
      return Some("");
   }
   if raw.ptr.is_null() {
      return None;
   }
   // SAFETY: host guarantees valid UTF-8 `String` bytes.
   str::from_utf8(unsafe { slice::from_raw_parts(raw.ptr, raw.len) }).ok()
}

/// # Safety
///
/// Reads a live `RawBytes` owned by the host.
const unsafe fn read_bytes<'a>(raw: &RawBytes) -> &'a [u8] {
   if raw.len == 0 || raw.ptr.is_null() {
      return &[];
   }
   // SAFETY: host guarantees `raw.ptr` is valid for `raw.len` bytes.
   unsafe { slice::from_raw_parts(raw.ptr, raw.len) }
}

/// # Safety
///
/// Reads a live `http::Method` owned by the host.
unsafe fn read_method(raw: &RawMethod) -> Cow<'static, str> {
   let tag = raw.bytes[0];
   if let Some(&name) = STANDARD_METHODS.get(usize::from(tag)) {
      return Cow::Borrowed(name);
   }
   if tag == 10 {
      let len = usize::from(raw.bytes[16]).min(15);
      return Cow::Owned(String::from_utf8_lossy(&raw.bytes[1..=len]).into_owned());
   }
   if tag == 11 {
      let ptr = usize::from_le_bytes(raw.bytes[8..16].try_into().unwrap_or([0; 8])) as *const u8;
      let len = usize::from_le_bytes(raw.bytes[16..24].try_into().unwrap_or([0; 8]));
      if !ptr.is_null() && len <= 64 {
         // SAFETY: `ExtensionAllocated` stores a valid UTF-8 `Box<str>`.
         let bytes = unsafe { slice::from_raw_parts(ptr, len) };
         return Cow::Owned(String::from_utf8_lossy(bytes).into_owned());
      }
   }
   Cow::Borrowed("GET")
}

/// # Safety
///
/// Reads header `(name, value)` pairs from the host's `http::HeaderMap`.
unsafe fn read_headers(map: &RawHeaderMap) -> Vec<(String, String)> {
   if map.entries_ptr.is_null() || map.entries_len == 0 || map.entries_len > 256 {
      return Vec::new();
   }
   // SAFETY: `entries_ptr` holds `entries_len` initialized `RawHeaderBucket`s.
   let buckets = unsafe { slice::from_raw_parts(map.entries_ptr, map.entries_len) };
   let mut out = Vec::with_capacity(buckets.len());
   for bucket in buckets {
      // SAFETY: all buckets come from the host HeaderMap.
      let Some(name) = (unsafe { header_name(bucket) }) else {
         continue;
      };
      // SAFETY: `bucket.value` is a live `HeaderValue` starting with `Bytes`.
      let val_bytes = unsafe { read_bytes(&bucket.value) };
      if let Ok(value) = str::from_utf8(val_bytes) {
         out.push((name.to_owned(), value.to_owned()));
      }
   }
   out
}

unsafe fn header_name(bucket: &RawHeaderBucket) -> Option<&str> {
   if (bucket.tag & 0xFF) == 2 {
      return None;
   }
   if bucket.name.vtable.is_null() {
      let index = bucket.name.ptr.addr() & 0xFF;
      return STANDARD_HEADERS.get(index).copied();
   }
   // SAFETY: a non-null vtable means a custom header name backed by Bytes.
   str::from_utf8(unsafe { read_bytes(&bucket.name) }).ok()
}

unsafe fn replace_content_length(map: &mut RawHeaderMap, len: usize) {
   if map.entries_ptr.is_null() || map.entries_len == 0 || map.entries_len > 256 {
      return;
   }
   let formatted = len.to_string();
   // SAFETY: the detour has exclusive access to the request's owned buckets.
   let buckets = unsafe { slice::from_raw_parts_mut(map.entries_ptr, map.entries_len) };
   let mut extras = if !map.extra_ptr.is_null() && map.extra_len > 0 && map.extra_len <= 256 {
      // SAFETY: `extra_ptr` holds `extra_len` initialized extra header values.
      Some(unsafe { slice::from_raw_parts_mut(map.extra_ptr, map.extra_len) })
   } else {
      None
   };
   for bucket in buckets {
      // SAFETY: the host initialized the bucket, including the value of every live name.
      if unsafe { header_name(bucket) }
         .is_some_and(|name| name.eq_ignore_ascii_case("content-length"))
      {
         if !bucket.value.vtable.is_null() {
            let body = make_owned_bytes(formatted.as_bytes().to_vec());
            // SAFETY: `bucket.value` is a live `RawBytes` owned by this header map.
            unsafe { replace_bytes(&mut bucket.value, body) };
         }
         if (bucket.tag & 0xFF) == 1
            && let Some(extras) = extras.as_deref_mut()
         {
            let mut idx = bucket.next_extra;
            for _ in 0..extras.len() {
               let Some(extra) = extras.get_mut(idx) else {
                  break;
               };
               if !extra.value.vtable.is_null() {
                  let body = make_owned_bytes(formatted.as_bytes().to_vec());
                  // SAFETY: `extra.value` is a live `RawBytes` in the linked list.
                  unsafe { replace_bytes(&mut extra.value, body) };
               }
               if (extra.next_tag & 0xFF) != 1 {
                  break;
               }
               idx = extra.next_idx;
            }
         }
      }
   }
}

fn is_multipart_request(headers: &[(String, String)], body: &[u8]) -> bool {
   headers.iter().any(|(name, value)| {
      name.eq_ignore_ascii_case("content-type")
         && value
            .trim_start()
            .get(..10)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("multipart/"))
   }) || (body.starts_with(b"--") && multipart_payload(body).is_some())
}

unsafe fn send_detour(self_ptr: *const c_void, req: *mut u8) -> RawPinnedFuture {
   let installed = INSTALLED
      .get()
      .expect("send_detour only runs after INSTALLED is set");

   let mut watched = None;

   if !req.is_null()
      && panic::catch_unwind(AssertUnwindSafe(|| {
         // SAFETY: `req` is a live owned `TransportRequest` passed by
         // `libdiscord_http`, laid out as `locate` read from `send`.
         let req_ref = unsafe { RawTransportRequest::at(req, installed.request) };
         let tags = installed.request.body_tags;
         let is_bytes_body =
            req_ref.body.tag == tags.bytes || Some(req_ref.body.tag) == tags.chunked;
         // SAFETY: reading live fields of `req_ref`.
         let url = unsafe { read_str(req_ref.url) }
            .unwrap_or_default()
            .to_owned();
         let method = unsafe { read_method(req_ref.method) }.into_owned();
         let headers = unsafe { read_headers(req_ref.headers) };
         let body = if is_bytes_body {
            // SAFETY: both decoded byte-backed body variants start with a live Bytes.
            unsafe { read_bytes(&req_ref.body.bytes) }
         } else {
            &[]
         };

         let action = if is_bytes_body && is_multipart_request(&headers, body) {
            observe_multipart(&method, &url, &headers, body)
         } else {
            observe(&method, &url, &headers, body)
         };
         match action {
            RequestAction::Drop => {
               // Zeroing `url.len` keeps the host's `String` capacity/pointer
               // intact so `Url::parse("")` fails cleanly with zero network I/O
               // and the host drops every owned field with its own allocator.
               req_ref.url.len = 0;
               return;
            },
            RequestAction::Replace(new_body) if installed.host_alloc.is_some() => {
               let new_len = new_body.len();
               if is_bytes_body && !req_ref.body.bytes.vtable.is_null() {
                  let replacement = make_owned_bytes(new_body);
                  // SAFETY: these variants both own the leading Bytes.
                  unsafe { replace_bytes(&mut req_ref.body.bytes, replacement) };
               } else if req_ref.body.tag == tags.empty {
                  req_ref.body.bytes = make_owned_bytes(new_body);
                  req_ref.body.tag = tags.bytes;
               } else {
                  return;
               }
               // SAFETY: request headers are still exclusively owned by this call.
               unsafe { replace_content_length(req_ref.headers, new_len) };
            },
            RequestAction::Pass | RequestAction::Replace(_) => {},
         }

         if installed.poll.is_some() {
            watched = watch_response(&method, &url);
         }
      }))
      .is_err()
   {
      log::error!("discord desktop: send_detour panicked");
   }

   // SAFETY: forward directly to the original `ReqwestTransport::send`.
   let future = unsafe { (installed.send)(self_ptr, req) };

   if let Some(in_flight) = watched
      && !future.data.is_null()
   {
      track_response(future.data.addr() as u64, in_flight);
   }

   future
}

unsafe fn buffered_poll_detour(future: *mut c_void, cx: *mut c_void) -> BufferedResult {
   let installed = INSTALLED
      .get()
      .expect("poll detours only run after INSTALLED is set");
   let Some(Poll::Buffered(poll)) = installed.poll else {
      unreachable!("the buffered detour is only swapped in for a buffered poll");
   };
   // SAFETY: forward to original `SendFuture::poll`.
   let mut result = unsafe { poll(future, cx) };

   if result.is_pending() {
      return result;
   }

   if let Some(in_flight) = take_response(future.addr() as u64)
      && result.is_ok()
      && panic::catch_unwind(AssertUnwindSafe(|| {
         let old_body = if result.body.len == 0 || result.body.ptr.is_null() {
            Vec::new()
         } else {
            // SAFETY: `Poll::Ready(Ok(resp)).body` is a live `Vec<u8>`.
            unsafe { slice::from_raw_parts(result.body.ptr, result.body.len) }.to_vec()
         };
         let orig_len = old_body.len();
         let (new_status, new_body) =
            run_response_hooks(in_flight, i32::from(result.status), old_body);
         if let Ok(status) = u16::try_from(new_status) {
            result.status = status;
         }
         // SAFETY: `result.body` is owned by the Ready response being returned.
         unsafe {
            replace_host_vec(&mut result.body, orig_len, &new_body, installed);
         }
      }))
      .is_err()
   {
      log::error!("discord desktop: poll_detour panicked");
   }

   result
}

unsafe fn streamed_poll_detour(future: *mut c_void, cx: *mut c_void) -> StreamedResult {
   const PENDING: u64 = u64::MAX;
   const ERR: u64 = 2;
   const STREAM_BODY: u64 = 1;

   let installed = INSTALLED
      .get()
      .expect("poll detours only run after INSTALLED is set");
   let Some(Poll::Streamed { poll, status }) = installed.poll else {
      unreachable!("the streamed detour is only swapped in for a streamed poll");
   };
   // SAFETY: forward to original `SendFuture::poll`.
   let mut result = unsafe { poll(future, cx) };
   if result.tag == PENDING {
      return result;
   }
   let Some(in_flight) = take_response(future.addr() as u64).filter(|_| result.tag != ERR) else {
      return result;
   };
   // SAFETY: `locate` saw `poll` store the `u16` status at `status`, inside the
   // result.
   let status = i32::from(unsafe {
      ptr::from_ref(&result)
         .byte_add(status)
         .cast::<u16>()
         .read_unaligned()
   });
   if result.body_kind == STREAM_BODY
      && !result.body.data.is_null()
      && !result.body.vtable.is_null()
   {
      result.body = body_stream::wrap(result.body, in_flight, status);
   } else if panic::catch_unwind(AssertUnwindSafe(|| {
      run_response_hooks(in_flight, status, Vec::new())
   }))
   .is_err()
   {
      log::error!("discord desktop: streamed_poll_detour panicked");
   }
   result
}

/// A response body stream that collects Discord's chunks, runs the response
/// hooks on the whole body when it ends, and yields that as one chunk. The
/// status is already Discord's by then, so hooks change only the body.
mod body_stream {
   use std::{
      ffi::c_void,
      mem,
      panic::{self, AssertUnwindSafe},
      ptr, slice,
      sync::atomic::AtomicPtr,
   };

   use phoenix_runtime::discord::{InFlight, run_response_hooks};

   use crate::discord::{INSTALLED, RawBytes, RawPinnedFuture, make_owned_bytes};

   /// `Poll<Option<Result<Bytes, Error>>>`.
   #[repr(C)]
   struct Item {
      tag: u64,
      bytes: RawBytes,
   }
   const CHUNK: u64 = 0;
   const END: u64 = 2;
   const PENDING: u64 = u64::MAX;

   /// A `dyn Stream` vtable, whose methods are Rust ABI like `poll`.
   #[repr(C)]
   struct Vtable {
      drop: Option<unsafe fn(*mut c_void)>,
      size: usize,
      align: usize,
      poll_next: unsafe fn(*mut c_void, *mut c_void) -> Item,
      size_hint: unsafe fn(*const c_void) -> (usize, Option<usize>),
   }

   struct Wrapped {
      inner: RawPinnedFuture,
      body: Vec<u8>,
      in_flight: Option<InFlight>,
      status: i32,
      done: bool,
   }

   // Size 0 keeps Discord from freeing the box with its allocator, `drop`
   // frees it with ours.
   static VTABLE: Vtable = Vtable {
      drop: Some(drop),
      size: 0,
      align: 1,
      poll_next,
      size_hint,
   };

   pub fn wrap(inner: RawPinnedFuture, in_flight: InFlight, status: i32) -> RawPinnedFuture {
      let wrapped = Box::new(Wrapped {
         inner,
         body: Vec::new(),
         in_flight: Some(in_flight),
         status,
         done: false,
      });
      RawPinnedFuture {
         data: Box::into_raw(wrapped).cast(),
         vtable: (&raw const VTABLE).cast(),
      }
   }

   const fn inner_vtable(wrapped: &Wrapped) -> &'static Vtable {
      // SAFETY: `inner` is Discord's own body stream, whose vtable has this
      // layout.
      unsafe { &*wrapped.inner.vtable.cast::<Vtable>() }
   }

   fn drop_chunk(item: Item) {
      let bytes = item.bytes;
      if !bytes.vtable.is_null() {
         // SAFETY: the chunk is ours to drop, once, through its own vtable.
         unsafe { ((*bytes.vtable).drop)(bytes.data.into_inner(), bytes.ptr, bytes.len) };
      }
   }

   const fn end() -> Item {
      Item {
         tag: END,
         bytes: RawBytes {
            vtable: ptr::null(),
            ptr: ptr::null(),
            len: 0,
            data: AtomicPtr::new(ptr::null_mut()),
         },
      }
   }

   unsafe fn poll_next(this: *mut c_void, cx: *mut c_void) -> Item {
      // SAFETY: Discord polls the box `wrap` made.
      let wrapped = unsafe { &mut *this.cast::<Wrapped>() };
      if wrapped.done {
         return end();
      }
      loop {
         // SAFETY: the inner stream is polled with Discord's own context.
         let item = unsafe { (inner_vtable(wrapped).poll_next)(wrapped.inner.data, cx) };
         match item.tag {
            CHUNK => {
               let bytes = &item.bytes;
               if bytes.len > 0 && !bytes.ptr.is_null() {
                  // SAFETY: a chunk's `Bytes` holds `len` bytes at `ptr`.
                  wrapped
                     .body
                     .extend_from_slice(unsafe { slice::from_raw_parts(bytes.ptr, bytes.len) });
               }
               drop_chunk(item);
            },
            END => {
               wrapped.done = true;
               let original = mem::take(&mut wrapped.body);
               let status = wrapped.status;
               let hooked = wrapped.in_flight.take().and_then(|in_flight| {
                  let body = original.clone();
                  panic::catch_unwind(AssertUnwindSafe(|| {
                     run_response_hooks(in_flight, status, body).1
                  }))
                  .ok()
               });
               let body = hooked.unwrap_or(original);
               if body.is_empty() {
                  return item;
               }
               return Item {
                  tag: CHUNK,
                  bytes: make_owned_bytes(body),
               };
            },
            PENDING => return item,
            _ => {
               wrapped.done = true;
               wrapped.in_flight = None;
               wrapped.body.clear();
               return item;
            },
         }
      }
   }

   const unsafe fn size_hint(_this: *const c_void) -> (usize, Option<usize>) {
      (0, None)
   }

   unsafe fn drop(this: *mut c_void) {
      // SAFETY: `this` is the box `wrap` leaked, dropped once.
      let mut wrapped = unsafe { Box::from_raw(this.cast::<Wrapped>()) };
      // Discord drops bodies it never reads, like a DELETE's 204, so the hooks
      // still see the response, with what arrived of its body.
      if let Some(in_flight) = wrapped.in_flight.take() {
         let (status, body) = (wrapped.status, mem::take(&mut wrapped.body));
         let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            run_response_hooks(in_flight, status, body)
         }));
      }
      let vtable = inner_vtable(&wrapped);
      if let Some(drop_inner) = vtable.drop {
         // SAFETY: drops Discord's stream in place, once.
         unsafe { drop_inner(wrapped.inner.data) };
      }
      if vtable.size > 0
         && vtable.align.is_power_of_two()
         && let Some(dealloc) = INSTALLED.get().and_then(|inst| inst.host_dealloc)
      {
         // SAFETY: Discord boxed the stream with its own allocator at this
         // size and alignment.
         unsafe { dealloc(wrapped.inner.data.cast(), vtable.size, vtable.align) };
      }
   }

   #[cfg(test)]
   mod tests {
      use std::collections::VecDeque;

      use super::*;
      use crate::discord::read_bytes;

      struct FakeStream {
         items: VecDeque<Item>,
         dropped: bool,
      }

      unsafe fn fake_poll(this: *mut c_void, _cx: *mut c_void) -> Item {
         let state = unsafe { &mut *this.cast::<FakeStream>() };
         state.items.pop_front().unwrap_or_else(end)
      }

      unsafe fn fake_drop(this: *mut c_void) {
         let state = unsafe { &mut *this.cast::<FakeStream>() };
         state.dropped = true;
      }

      static FAKE_VTBL: Vtable = Vtable {
         drop: Some(fake_drop),
         size: 0,
         align: 1,
         poll_next: fake_poll,
         size_hint,
      };

      static NO_DROP_VTBL: Vtable = Vtable {
         drop: None,
         size: 0,
         align: 1,
         poll_next: fake_poll,
         size_hint,
      };

      #[test]
      fn collects_chunks_and_yields_single_body_then_end() {
         let mut fake = FakeStream {
            items: VecDeque::from([
               Item {
                  tag: CHUNK,
                  bytes: make_owned_bytes(b"hello ".to_vec()),
               },
               Item {
                  tag: PENDING,
                  bytes: end().bytes,
               },
               Item {
                  tag: CHUNK,
                  bytes: make_owned_bytes(b"world".to_vec()),
               },
               end(),
            ]),
            dropped: false,
         };
         let wrapped = Box::into_raw(Box::new(Wrapped {
            inner: RawPinnedFuture {
               data: (&raw mut fake).cast(),
               vtable: (&raw const FAKE_VTBL).cast(),
            },
            body: Vec::new(),
            in_flight: None,
            status: 200,
            done: false,
         }))
         .cast::<c_void>();

         unsafe {
            assert_eq!(poll_next(wrapped, ptr::null_mut()).tag, PENDING);
            let merged = poll_next(wrapped, ptr::null_mut());
            assert_eq!(merged.tag, CHUNK);
            assert_eq!(read_bytes(&merged.bytes), b"hello world");
            drop_chunk(merged);
            assert_eq!(poll_next(wrapped, ptr::null_mut()).tag, END);
            drop(wrapped);
         }
         assert!(fake.dropped);
      }

      #[test]
      fn error_clears_partial_body_and_null_drop_vtable_is_safe() {
         let mut fake = FakeStream {
            items: VecDeque::from([
               Item {
                  tag: CHUNK,
                  bytes: make_owned_bytes(b"partial".to_vec()),
               },
               Item {
                  tag: 1,
                  bytes: end().bytes,
               },
            ]),
            dropped: false,
         };
         let wrapped = Box::into_raw(Box::new(Wrapped {
            inner: RawPinnedFuture {
               data: (&raw mut fake).cast(),
               vtable: (&raw const NO_DROP_VTBL).cast(),
            },
            body: Vec::new(),
            in_flight: None,
            status: 200,
            done: false,
         }))
         .cast::<c_void>();

         unsafe {
            let err = poll_next(wrapped, ptr::null_mut());
            assert_eq!(err.tag, 1);
            assert!(
               (*wrapped.cast::<Wrapped>()).body.is_empty(),
               "error must discard partial chunks"
            );
            assert!(
               (*wrapped.cast::<Wrapped>()).in_flight.is_none(),
               "error must suppress drop-time hooks"
            );
            assert_eq!(poll_next(wrapped, ptr::null_mut()).tag, END);
            drop(wrapped);
         }
         assert!(!fake.dropped);
      }
   }
}

/// Replaces `vec`'s contents with `new_body` while staying 100% compatible
/// with `Discord.exe`'s custom `#[global_allocator]`.
///
/// # Safety
///
/// `vec` is an owned host `Vec<u8>`.
unsafe fn replace_host_vec(
   vec: &mut RawVec,
   orig_len: usize,
   new_body: &[u8],
   installed: &Installed,
) {
   // Fast path: unchanged length and identical bytes.
   if new_body.len() == orig_len
      && (orig_len == 0
         || (!vec.ptr.is_null()
            // SAFETY: `vec.ptr` is valid for `orig_len` bytes.
            && unsafe { slice::from_raw_parts(vec.ptr, orig_len) } == new_body))
   {
      return;
   }
   // Fits inside existing host capacity: overwrite in place with zero
   // alloc/dealloc.
   if new_body.len() <= vec.cap && !vec.ptr.is_null() {
      // SAFETY: `vec.ptr` has capacity >= `new_body.len()`.
      unsafe { ptr::copy_nonoverlapping(new_body.as_ptr(), vec.ptr, new_body.len()) };
      vec.len = new_body.len();
      return;
   }
   // Needs growth: use the host's own `__rust_alloc` and `__rust_dealloc`.
   let (Some(alloc), Some(dealloc)) = (installed.host_alloc, installed.host_dealloc) else {
      log::warn!("discord onResponse: cannot grow response body without host allocator");
      return;
   };
   let new_len = new_body.len();
   // SAFETY: calls host's `__rust_alloc(new_len, 1)`.
   let new_ptr = unsafe { alloc(new_len, 1) };
   if new_ptr.is_null() {
      return;
   }
   // SAFETY: copies `new_len` bytes into freshly allocated host buffer.
   unsafe { ptr::copy_nonoverlapping(new_body.as_ptr(), new_ptr, new_len) };
   if vec.cap > 0 && !vec.ptr.is_null() {
      // SAFETY: frees old host buffer via host's `__rust_dealloc(ptr, cap, 1)`.
      unsafe { dealloc(vec.ptr, vec.cap, 1) };
   }
   vec.cap = new_len;
   vec.ptr = new_ptr;
   vec.len = new_len;
}

// ---------------------------------------------------------------------------
// High-level session capture, hook dispatch & `phoenix.discord.*` transport
// ---------------------------------------------------------------------------

/// Extracts the channel id from a send-message API path such as
/// `/channels/123456789/messages` (ignoring any query string).
#[must_use]
pub fn send_channel_id(path: &str) -> Option<&str> {
   let clean = path.split(['?', '#']).next().unwrap_or(path);
   let rest = clean.strip_prefix("/channels/")?;
   let id = rest.strip_suffix("/messages")?;
   (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then_some(id)
}

/// Desktop has no composer hook, so `phoenix.messages.onSend` runs on the
/// request that posts a message, before the shared request hooks.
fn observe(method: &str, url: &str, headers: &[(String, String)], body: &[u8]) -> RequestAction {
   let channel = api_path(url)
      .filter(|_| method.eq_ignore_ascii_case("POST"))
      .and_then(send_channel_id);
   let sent = channel.map_or(RequestAction::Pass, |channel| {
      phoenix_runtime::engine().map_or(RequestAction::Pass, |engine| {
         crate::js::ui::intercept_send_body(&engine, channel, body)
      })
   });
   match sent {
      RequestAction::Pass => observe_request(method, url, headers, body),
      RequestAction::Drop => RequestAction::Drop,
      RequestAction::Replace(sent) => match observe_request(method, url, headers, &sent) {
         RequestAction::Pass => RequestAction::Replace(sent),
         hooked => hooked,
      },
   }
}

fn multipart_payload(body: &[u8]) -> Option<Range<usize>> {
   let opening = body.get(..body.len().min(256))?;
   let boundary_end = opening.windows(2).position(|line| line == b"\r\n")?;
   let boundary = opening[..boundary_end].strip_prefix(b"--")?;
   if boundary.is_empty() || !boundary.iter().all(u8::is_ascii_graphic) {
      return None;
   }
   let mut marker = b"\r\n--".to_vec();
   marker.extend_from_slice(boundary);
   let mut start = boundary_end + 2;
   loop {
      let header_end = start
         + body
            .get(start..)?
            .get(..(body.len() - start).min(8192))?
            .windows(4)
            .position(|end| end == b"\r\n\r\n")?;
      let payload_start = header_end + 4;
      let tail = body.get(payload_start..)?;
      let delimiter = tail
         .windows(marker.len())
         .enumerate()
         .find_map(|(offset, window)| {
            (window == marker
               && matches!(
                  tail.get(offset + marker.len()..offset + marker.len() + 2),
                  Some(b"--" | b"\r\n")
               ))
            .then_some(offset)
         })?;
      let header = String::from_utf8_lossy(body.get(start..header_end)?);
      let is_payload = header.lines().any(|line| {
         line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("content-disposition")
               && value.split(';').skip(1).any(|field| {
                  field.split_once('=').is_some_and(|(key, val)| {
                     key.trim().eq_ignore_ascii_case("name")
                        && matches!(val.trim(), "\"payload_json\"" | "payload_json")
                  })
               })
         })
      });
      if is_payload {
         return Some(payload_start..payload_start + delimiter);
      }
      let after_marker = payload_start + delimiter + marker.len();
      if body.get(after_marker..after_marker + 2)? != b"\r\n" {
         return None;
      }
      start = after_marker + 2;
   }
}

fn observe_multipart(
   method: &str,
   url: &str,
   headers: &[(String, String)],
   body: &[u8],
) -> RequestAction {
   let Some(payload) = (method.eq_ignore_ascii_case("POST")
      || method.eq_ignore_ascii_case("PATCH"))
   .then(|| multipart_payload(body))
   .flatten() else {
      return match observe_request(method, url, headers, &[]) {
         RequestAction::Drop => RequestAction::Drop,
         _ => RequestAction::Pass,
      };
   };
   match observe(method, url, headers, &body[payload.clone()]) {
      RequestAction::Replace(json) => {
         let mut rewritten = Vec::with_capacity(body.len() - payload.len() + json.len());
         rewritten.extend_from_slice(&body[..payload.start]);
         rewritten.extend_from_slice(&json);
         rewritten.extend_from_slice(&body[payload.end..]);
         RequestAction::Replace(rewritten)
      },
      action => action,
   }
}

pub fn send(
   method: &str,
   path: &str,
   headers: Vec<(String, String)>,
   body: Vec<u8>,
) -> Result<Response, DiscordError> {
   let (url, merged) = session_request(path, headers)?;
   let request = Request {
      url,
      method: method.to_owned(),
      headers: merged,
      body: (!body.is_empty()).then_some(body),
   };
   fetch::send(&request).map_err(|err| DiscordError::Network {
      path: path.to_owned(),
      message: err.to_string(),
   })
}

pub const fn authorize(_request: &Authorization) -> Result<String, DiscordError> {
   Err(DiscordError::NoBridge)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn extracts_send_channel_id() {
      assert_eq!(
         send_channel_id("/channels/123456789012345678/messages"),
         Some("123456789012345678")
      );
      assert_eq!(send_channel_id("/channels/42/messages?nonce=1"), Some("42"));
      assert_eq!(send_channel_id("/channels/abc/messages"), None);
      assert_eq!(send_channel_id("/channels/42/typing"), None);
      assert_eq!(send_channel_id("/users/@me"), None);
   }

   #[test]
   fn rejects_relative_path_without_leading_slash() {
      let err = send("GET", "users/@me", Vec::new(), Vec::new());
      assert!(matches!(err, Err(DiscordError::Path { .. })));
   }

   #[test]
   fn standard_header_indices_match_http_crate() {
      assert_eq!(STANDARD_HEADERS[0], "accept");
      assert_eq!(STANDARD_HEADERS[14], "allow");
      assert_eq!(STANDARD_HEADERS[16], "authorization");
      assert_eq!(STANDARD_HEADERS[29], "content-type");
      assert_eq!(STANDARD_HEADERS[30], "cookie");
      assert_eq!(STANDARD_HEADERS[46], "location");
      assert_eq!(STANDARD_HEADERS[70], "user-agent");
      assert_eq!(STANDARD_HEADERS[80], "x-xss-protection");
   }

   #[test]
   fn owned_bytes_clone_and_drop_roundtrip() {
      let raw = make_owned_bytes(b"hello sunbird".to_vec());
      assert_eq!(raw.len, 13);
      // SAFETY: testing our own vtable clone + double drop.
      unsafe {
         let cloned = ((*raw.vtable).clone)(&raw.data, raw.ptr, raw.len);
         assert_eq!(read_bytes(&cloned), b"hello sunbird");
         ((*raw.vtable).drop)(raw.data.load(Ordering::Relaxed), raw.ptr, raw.len);
         ((*cloned.vtable).drop)(cloned.data.load(Ordering::Relaxed), cloned.ptr, cloned.len);
      }
   }

   #[test]
   fn multipart_payload_ignores_files_and_partial_boundary_matches() {
      let bytes = b"--test\r\nContent-Disposition: form-data; name=\"files[0]\"; filename=\"\xFFpayload_json\"\r\n\r\n\xFF\r\n--test-extra\r\n--test\r\ncontent-disposition: form-data; NAME = \"payload_json\"\r\n\r\n{\"content\":\"x\"}\r\n--test--\r\n";
      assert_eq!(
         &bytes[multipart_payload(bytes).unwrap()],
         br#"{"content":"x"}"#
      );
      for body in [
         b"{}".as_slice(),
         b"--test\r\n",
         b"--test\r\nContent-Disposition: form-data; name=payload_json\r\n\r\n{}",
      ] {
         assert_eq!(multipart_payload(body), None);
      }
   }

   #[test]
   fn replace_content_length_updates_bucket_and_extra_values() {
      let mut extra = [RawExtraValue {
         prev_tag: 0,
         prev_idx: 0,
         next_tag: 0,
         next_idx: 0,
         value: make_owned_bytes(b"10".to_vec()),
         _sens: 0,
      }];
      let mut buckets = [RawHeaderBucket {
         tag: 1,
         next_extra: 0,
         tail_extra: 0,
         value: make_owned_bytes(b"10".to_vec()),
         _sens: 0,
         name: RawBytes {
            vtable: ptr::null(),
            ptr: ptr::without_provenance(24),
            len: 0,
            data: AtomicPtr::new(ptr::null_mut()),
         },
         _pad1: 0,
      }];
      let mut map = RawHeaderMap {
         _head: [0; 3],
         entries_cap: 1,
         entries_ptr: buckets.as_mut_ptr(),
         entries_len: 1,
         extra_cap: 1,
         extra_ptr: extra.as_mut_ptr(),
         extra_len: 1,
         _tail: [0; 3],
      };
      unsafe {
         replace_content_length(&mut map, 42);
         assert_eq!(read_bytes(&buckets[0].value), b"42");
         assert_eq!(read_bytes(&extra[0].value), b"42");
         ((*buckets[0].value.vtable).drop)(
            buckets[0].value.data.load(Ordering::Relaxed),
            buckets[0].value.ptr,
            buckets[0].value.len,
         );
         ((*extra[0].value.vtable).drop)(
            extra[0].value.data.load(Ordering::Relaxed),
            extra[0].value.ptr,
            extra[0].value.len,
         );
      }
   }
}
