//! HTTP and HTTPS client backed by `libcurl.so.4` on Unix and `winhttp.dll` on Windows.

use std::{fs::File, path::Path};

use crate::Error;

pub const USER_AGENT: &str = "NyaaCord-Updater/0.1";

const MAX_REDIRS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlScheme {
   Https,
   LoopbackHttp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadResult {
   pub bytes: u64,
   pub sha256: String,
}

pub(crate) struct DownloadOptions<'a> {
   pub url: &'a str,
   pub headers: &'a [(&'a str, &'a str)],
   pub max_bytes: u64,
   pub connect_timeout_ms: i64,
   pub stall_timeout_secs: i64,
}

/// Validates that a URL uses HTTPS or loopback HTTP for local tests.
pub fn validate_url(url: &str) -> Result<UrlScheme, Error> {
   if url.is_empty() || url.bytes().any(|b| b <= 0x20 || b == 0x7F) {
      return Err(Error::InvalidUrl {
         url: url.to_owned(),
         reason: "URL is empty or contains whitespace or control characters".to_owned(),
      });
   }

   if url.len() > 8 && url[..8].eq_ignore_ascii_case("https://") {
      let rest = &url[8..];
      let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
      if authority.is_empty() || authority.contains('@') || authority.starts_with(':') {
         return Err(Error::InvalidUrl {
            url: url.to_owned(),
            reason: "invalid or missing host".to_owned(),
         });
      }
      if !authority.starts_with('[')
         && let Some((host, port)) = authority.split_once(':')
         && (host.is_empty()
            || port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || !port.parse::<u16>().is_ok_and(|p| p > 0))
      {
         return Err(Error::InvalidUrl {
            url: url.to_owned(),
            reason: "invalid port in URL".to_owned(),
         });
      }
      return Ok(UrlScheme::Https);
   }

   if url.len() > 7 && url[..7].eq_ignore_ascii_case("http://") {
      let rest = &url[7..];
      let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
      if authority.is_empty() || authority.contains('@') {
         return Err(Error::InvalidUrl {
            url: url.to_owned(),
            reason: "plain HTTP is only allowed for 127.0.0.1 or localhost".to_owned(),
         });
      }
      let (host, port_opt) = match authority.split_once(':') {
         Some((host, port)) => (host, Some(port)),
         None => (authority, None),
      };
      let is_loopback = host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost");
      let port_ok = port_opt.is_none_or(|port| {
         !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|p| p > 0)
      });
      if is_loopback && port_ok {
         return Ok(UrlScheme::LoopbackHttp);
      }
      return Err(Error::InvalidUrl {
         url: url.to_owned(),
         reason: "plain HTTP is only allowed for 127.0.0.1 or localhost".to_owned(),
      });
   }

   Err(Error::InvalidUrl {
      url: url.to_owned(),
      reason: "scheme must be https".to_owned(),
   })
}

const fn is_redirect_status(status: i32) -> bool {
   matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[cfg_attr(
   not(any(windows, test)),
   allow(
      dead_code,
      reason = "used by the Windows WinHTTP redirect loop and tests"
   )
)]
fn normalize_path(path: &str) -> String {
   let mut segments: Vec<&str> = Vec::new();
   let trailing_slash = path.ends_with('/');
   for seg in path.split('/') {
      match seg {
         "" | "." => {},
         ".." => {
            let _ = segments.pop();
         },
         other => segments.push(other),
      }
   }
   let mut out = String::from("/");
   out.push_str(&segments.join("/"));
   if trailing_slash && !out.ends_with('/') {
      out.push('/');
   }
   out
}

/// Resolves an HTTP `Location` header value relative to `base`.
#[cfg_attr(
   not(any(windows, test)),
   allow(
      dead_code,
      reason = "used by the Windows WinHTTP redirect loop and tests"
   )
)]
fn resolve_redirect_url(base: &str, location: &str) -> Result<String, Error> {
   let loc = location.trim();
   let loc = loc.split('#').next().unwrap_or(loc);
   if loc.is_empty() {
      return Err(Error::InvalidUrl {
         url: base.to_owned(),
         reason: "empty redirect location".to_owned(),
      });
   }

   if let Some(scheme_sep) = loc.find("://") {
      let scheme = &loc[..scheme_sep];
      if !scheme.is_empty()
         && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
      {
         return Ok(loc.to_owned());
      }
   }
   if let Some(colon) = loc.find(':') {
      let before = &loc[..colon];
      if !before.is_empty()
         && !before.contains(['/', '?', '#'])
         && before.bytes().all(|b| b.is_ascii_alphabetic())
      {
         return Ok(loc.to_owned());
      }
   }

   let Some(scheme_end) = base.find("://") else {
      return Err(Error::InvalidUrl {
         url: base.to_owned(),
         reason: "base URL missing scheme".to_owned(),
      });
   };
   let base_scheme = &base[..scheme_end];
   let after_scheme = &base[scheme_end + 3..];
   let origin_len = after_scheme
      .find(['/', '?', '#'])
      .unwrap_or(after_scheme.len());
   let base_origin = &base[..scheme_end + 3 + origin_len];
   let base_rest = &after_scheme[origin_len..];

   if loc.starts_with("//") {
      return Ok(format!("{base_scheme}:{loc}"));
   }
   if loc.starts_with('/') {
      return Ok(format!("{base_origin}{loc}"));
   }
   if loc.starts_with('?') {
      let path_part = base_rest.split(['?', '#']).next().unwrap_or("");
      let path_part = if path_part.is_empty() { "/" } else { path_part };
      return Ok(format!("{base_origin}{path_part}{loc}"));
   }

   let (loc_path, loc_query) = match loc.split_once('?') {
      Some((path, query)) => (path, Some(query)),
      None => (loc, None),
   };
   let base_path = base_rest.split(['?', '#']).next().unwrap_or("/");
   let base_dir = base_path.rfind('/').map_or("/", |idx| &base_path[..=idx]);
   let joined = normalize_path(&format!("{base_dir}{loc_path}"));
   Ok(loc_query.map_or_else(
      || format!("{base_origin}{joined}"),
      |query| format!("{base_origin}{joined}?{query}"),
   ))
}

/// Fetches a bounded response body from an HTTPS or loopback HTTP URL.
pub fn get_bytes(
   url: &str,
   headers: &[(&str, &str)],
   connect_timeout_ms: i64,
   stall_timeout_secs: i64,
   max_bytes: u64,
) -> Result<Vec<u8>, Error> {
   #[cfg(unix)]
   {
      unix::get_bytes(
         url,
         headers,
         connect_timeout_ms,
         stall_timeout_secs,
         max_bytes,
      )
   }
   #[cfg(windows)]
   {
      windows::get_bytes(
         url,
         headers,
         connect_timeout_ms,
         stall_timeout_secs,
         max_bytes,
      )
   }
}

/// Streams a download into an open file while computing its SHA-256 digest.
pub(crate) fn download_into_file(
   opts: &DownloadOptions<'_>,
   file: &mut File,
   dest_for_errors: &Path,
   on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<DownloadResult, Error> {
   #[cfg(unix)]
   {
      unix::download_into_file(opts, file, dest_for_errors, on_progress)
   }
   #[cfg(windows)]
   {
      windows::download_into_file(opts, file, dest_for_errors, on_progress)
   }
}

/// Streams a download to `dest`, hashing SHA-256 incrementally and syncing to disk.
pub fn download_to_file(
   url: &str,
   headers: &[(&str, &str)],
   dest: &Path,
   max_bytes: u64,
   connect_timeout_ms: i64,
   stall_timeout_secs: i64,
   on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<DownloadResult, Error> {
   #[cfg(unix)]
   {
      unix::download_to_file(
         url,
         headers,
         dest,
         max_bytes,
         connect_timeout_ms,
         stall_timeout_secs,
         on_progress,
      )
   }
   #[cfg(windows)]
   {
      windows::download_to_file(
         url,
         headers,
         dest,
         max_bytes,
         connect_timeout_ms,
         stall_timeout_secs,
         on_progress,
      )
   }
}

#[cfg(unix)]
mod unix {
   use std::{
      ffi::{CStr, CString, c_char, c_int, c_long, c_void},
      fs::{File, OpenOptions},
      io::{self, BufWriter, Write},
      os::unix::fs::OpenOptionsExt,
      path::Path,
      ptr,
      sync::OnceLock,
   };

   use super::{
      DownloadOptions, DownloadResult, MAX_REDIRS, UrlScheme, is_redirect_status, validate_url,
   };
   use crate::{Error, sha256};

   const CURL_GLOBAL_DEFAULT: c_long = 3;
   const CURLPROTO_HTTP: c_long = 1;
   const CURLPROTO_HTTPS: c_long = 2;

   const CURLOPT_WRITEDATA: c_int = 10_001;
   const CURLOPT_URL: c_int = 10_002;
   const CURLOPT_USERAGENT: c_int = 10_018;
   const CURLOPT_LOW_SPEED_LIMIT: c_int = 19;
   const CURLOPT_LOW_SPEED_TIME: c_int = 20;
   const CURLOPT_HTTPHEADER: c_int = 10_023;
   const CURLOPT_FOLLOWLOCATION: c_int = 52;
   const CURLOPT_SSL_VERIFYPEER: c_int = 64;
   const CURLOPT_SSL_VERIFYHOST: c_int = 81;
   const CURLOPT_NOSIGNAL: c_int = 99;
   const CURLOPT_CONNECTTIMEOUT_MS: c_int = 156;
   const CURLOPT_PROTOCOLS: c_int = 181;
   const CURLOPT_REDIR_PROTOCOLS: c_int = 182;
   const CURLOPT_PROTOCOLS_STR: c_int = 10_318;
   const CURLOPT_REDIR_PROTOCOLS_STR: c_int = 10_319;
   const CURLOPT_WRITEFUNCTION: c_int = 20_011;

   const CURLINFO_EFFECTIVE_URL: c_int = 0x0010_0001;
   const CURLINFO_REDIRECT_URL: c_int = 0x0010_001F;
   const CURLINFO_RESPONSE_CODE: c_int = 0x0020_0002;
   const CURLINFO_CONTENT_LENGTH_DOWNLOAD_T: c_int = 0x0060_000F;

   const CURLE_UNSUPPORTED_PROTOCOL: c_int = 1;
   const CURLE_URL_MALFORMAT: c_int = 3;
   const CURLE_WRITE_ERROR: c_int = 23;
   const CURLE_UNKNOWN_OPTION: c_int = 48;

   const MAX_REDIRECT_BODY: u64 = 64 * 1024;

   type Easy = *mut c_void;
   type Slist = *mut c_void;
   type WriteFn = extern "C" fn(*const c_char, usize, usize, *mut c_void) -> usize;

   struct Curl {
      easy_init: unsafe extern "C" fn() -> Easy,
      easy_setopt: unsafe extern "C" fn(Easy, c_int, ...) -> c_int,
      easy_perform: unsafe extern "C" fn(Easy) -> c_int,
      easy_getinfo: unsafe extern "C" fn(Easy, c_int, ...) -> c_int,
      easy_cleanup: unsafe extern "C" fn(Easy),
      easy_strerror: unsafe extern "C" fn(c_int) -> *const c_char,
      slist_append: unsafe extern "C" fn(Slist, *const c_char) -> Slist,
      slist_free_all: unsafe extern "C" fn(Slist),
   }

   /// # Safety
   ///
   /// `T` must match the C function pointer type of symbol `name`.
   unsafe fn sym<T>(lib: *mut c_void, name: &CStr) -> Option<T> {
      const { assert!(size_of::<T>() == size_of::<*mut c_void>()) };
      // SAFETY: `lib` is an open handle from `dlopen` and `name` is NUL terminated.
      let found = unsafe { libc::dlsym(lib, name.as_ptr()) };
      // SAFETY: caller ensures `T` matches the symbol type and size matches a pointer.
      (!found.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&found) })
   }

   fn curl() -> Option<&'static Curl> {
      static CURL: OnceLock<Option<Curl>> = OnceLock::new();
      CURL
         .get_or_init(|| {
            // SAFETY: symbol signatures match `curl/curl.h`.
            let loaded = unsafe {
               let lib = libc::dlopen(c"libcurl.so.4".as_ptr(), libc::RTLD_NOW);
               if lib.is_null() {
                  log::warn!("updater: libcurl.so.4 is not installed");
                  return None;
               }
               let global_init: unsafe extern "C" fn(c_long) -> c_int =
                  sym(lib, c"curl_global_init")?;
               if global_init(CURL_GLOBAL_DEFAULT) != 0 {
                  return None;
               }
               Curl {
                  easy_init: sym(lib, c"curl_easy_init")?,
                  easy_setopt: sym(lib, c"curl_easy_setopt")?,
                  easy_perform: sym(lib, c"curl_easy_perform")?,
                  easy_getinfo: sym(lib, c"curl_easy_getinfo")?,
                  easy_cleanup: sym(lib, c"curl_easy_cleanup")?,
                  easy_strerror: sym(lib, c"curl_easy_strerror")?,
                  slist_append: sym(lib, c"curl_slist_append")?,
                  slist_free_all: sym(lib, c"curl_slist_free_all")?,
               }
            };
            Some(loaded)
         })
         .as_ref()
   }

   struct Handle<'a>(&'a Curl, Easy);

   impl Drop for Handle<'_> {
      fn drop(&mut self) {
         // SAFETY: `self.1` is a valid easy handle owned by this guard.
         unsafe { (self.0.easy_cleanup)(self.1) };
      }
   }

   struct Headers<'a>(&'a Curl, Slist);

   impl Drop for Headers<'_> {
      fn drop(&mut self) {
         // SAFETY: `self.1` is null or a list returned by `curl_slist_append`.
         unsafe { (self.0.slist_free_all)(self.1) };
      }
   }

   fn query_redirect_url(curl: &Curl, easy: Easy, url: &str) -> Result<Option<String>, Error> {
      let mut ptr: *const c_char = ptr::null();
      // SAFETY: `CURLINFO_REDIRECT_URL` writes one `*const c_char`.
      let code = unsafe { (curl.easy_getinfo)(easy, CURLINFO_REDIRECT_URL, &raw mut ptr) };
      if code != 0 {
         return Err(curl_err(curl, url, code));
      }
      if ptr.is_null() {
         return Ok(None);
      }
      // SAFETY: libcurl owns this NUL terminated string until easy handle cleanup.
      let target = unsafe { CStr::from_ptr(ptr) }
         .to_str()
         .map_err(|_| Error::InvalidUrl {
            url: url.to_owned(),
            reason: "redirect target is not UTF-8".to_owned(),
         })?;
      if target.is_empty() {
         return Ok(None);
      }
      Ok(Some(target.to_owned()))
   }

   fn curl_err(curl: &Curl, url: &str, code: c_int) -> Error {
      // SAFETY: `curl_easy_strerror` returns a static NUL terminated string.
      let text = unsafe { CStr::from_ptr((curl.easy_strerror)(code)) };
      Error::Http {
         url: url.to_owned(),
         message: format!("curl: {}", text.to_string_lossy()),
      }
   }

   fn query_status(curl: &Curl, easy: Easy) -> Result<i32, c_int> {
      let mut status: c_long = 0;
      // SAFETY: `CURLINFO_RESPONSE_CODE` writes one `c_long`.
      let code = unsafe { (curl.easy_getinfo)(easy, CURLINFO_RESPONSE_CODE, &raw mut status) };
      if code != 0 {
         return Err(code);
      }
      Ok(i32::try_from(status).unwrap_or(0))
   }

   fn query_content_length(curl: &Curl, easy: Easy) -> Option<u64> {
      let mut len: i64 = -1;
      // SAFETY: `CURLINFO_CONTENT_LENGTH_DOWNLOAD_T` writes one `curl_off_t` which is `i64`.
      let code =
         unsafe { (curl.easy_getinfo)(easy, CURLINFO_CONTENT_LENGTH_DOWNLOAD_T, &raw mut len) };
      if code != 0 || len < 0 {
         return None;
      }
      u64::try_from(len).ok()
   }

   fn check_effective_url(
      curl: &Curl,
      easy: Easy,
      initial_url: &str,
      initial_scheme: UrlScheme,
   ) -> Result<String, Error> {
      let mut ptr: *const c_char = ptr::null();
      // SAFETY: `CURLINFO_EFFECTIVE_URL` writes one `*const c_char`.
      let code = unsafe { (curl.easy_getinfo)(easy, CURLINFO_EFFECTIVE_URL, &raw mut ptr) };
      if code != 0 {
         return Err(curl_err(curl, initial_url, code));
      }
      if ptr.is_null() {
         return Err(Error::Http {
            url: initial_url.to_owned(),
            message: "missing effective URL".to_owned(),
         });
      }
      // SAFETY: libcurl owns this NUL terminated string until easy handle cleanup.
      let effective = unsafe { CStr::from_ptr(ptr) }
         .to_str()
         .map_err(|_| Error::InvalidUrl {
            url: initial_url.to_owned(),
            reason: "redirect target is not UTF-8".to_owned(),
         })?
         .to_owned();

      let effective_scheme = validate_url(&effective)?;
      if initial_scheme == UrlScheme::Https && effective_scheme != UrlScheme::Https {
         return Err(Error::InvalidUrl {
            url: effective,
            reason: "redirect downgraded from HTTPS".to_owned(),
         });
      }
      Ok(effective)
   }

   fn build_headers<'a>(
      curl: &'a Curl,
      url: &str,
      headers: &[(&str, &str)],
   ) -> Result<Headers<'a>, Error> {
      let mut list = Headers(curl, ptr::null_mut());
      let mut lines = Vec::with_capacity(headers.len() + 1);
      for &(name, value) in headers {
         if name.is_empty()
            || name
               .bytes()
               .any(|b| matches!(b, b'\r' | b'\n' | b'\0' | b':'))
            || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0'))
         {
            return Err(Error::Http {
               url: url.to_owned(),
               message: "invalid HTTP header".to_owned(),
            });
         }
         lines.push(if value.is_empty() {
            format!("{name};")
         } else {
            format!("{name}: {value}")
         });
      }
      lines.push("Expect:".to_owned());

      for line in lines {
         let c_line = CString::new(line).map_err(|_| Error::Http {
            url: url.to_owned(),
            message: "header contains NUL byte".to_owned(),
         })?;
         // SAFETY: libcurl copies `c_line` and `list.1` is null or its own slist.
         let next = unsafe { (curl.slist_append)(list.1, c_line.as_ptr()) };
         if next.is_null() {
            return Err(Error::Http {
               url: url.to_owned(),
               message: "curl_slist_append failed".to_owned(),
            });
         }
         list.1 = next;
      }
      Ok(list)
   }

   const USER_AGENT_CSTR: &CStr = c"NyaaCord-Updater/0.1";
   const PROTO_HTTPS_CSTR: &CStr = c"https";
   const PROTO_HTTP_HTTPS_CSTR: &CStr = c"http,https";

   struct PreparedEasy<'a> {
      // Declared first so `curl_easy_cleanup` runs before headers and URL drop.
      easy: Handle<'a>,
      curl: &'a Curl,
      _headers: Headers<'a>,
      _url_cstr: CString,
   }

   fn prepare_easy(
      url: &str,
      scheme: UrlScheme,
      headers: &[(&str, &str)],
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
   ) -> Result<PreparedEasy<'static>, Error> {
      let curl = curl().ok_or_else(|| Error::Http {
         url: url.to_owned(),
         message: "libcurl.so.4 is not available".to_owned(),
      })?;
      let url_cstr = CString::new(url).map_err(|_| Error::InvalidUrl {
         url: url.to_owned(),
         reason: "URL contains NUL byte".to_owned(),
      })?;
      let slist = build_headers(curl, url, headers)?;

      // SAFETY: `curl_easy_init` is safe after `curl_global_init`.
      let easy = Handle(curl, unsafe { (curl.easy_init)() });
      if easy.1.is_null() {
         return Err(Error::Http {
            url: url.to_owned(),
            message: "curl_easy_init failed".to_owned(),
         });
      }

      let set_ptr = |option: c_int, value: *const c_void| {
         // SAFETY: `value` outlives `curl_easy_perform` inside `PreparedEasy`.
         unsafe { (curl.easy_setopt)(easy.1, option, value) }
      };
      let set_long = |option: c_int, value: c_long| {
         // SAFETY: `option` takes a `c_long`.
         unsafe { (curl.easy_setopt)(easy.1, option, value) }
      };

      let (proto_str, proto_bits) = match scheme {
         UrlScheme::Https => (PROTO_HTTPS_CSTR, CURLPROTO_HTTPS),
         UrlScheme::LoopbackHttp => (PROTO_HTTP_HTTPS_CSTR, CURLPROTO_HTTP | CURLPROTO_HTTPS),
      };
      let proto_code = match set_ptr(CURLOPT_PROTOCOLS_STR, proto_str.as_ptr().cast()) {
         CURLE_UNKNOWN_OPTION => set_long(CURLOPT_PROTOCOLS, proto_bits),
         other => other,
      };
      let redir_proto_code = match set_ptr(CURLOPT_REDIR_PROTOCOLS_STR, proto_str.as_ptr().cast()) {
         CURLE_UNKNOWN_OPTION => set_long(CURLOPT_REDIR_PROTOCOLS, proto_bits),
         other => other,
      };

      let connect_ms = c_long::try_from(connect_timeout_ms.max(1)).unwrap_or(c_long::MAX);
      let stall_secs = c_long::try_from(stall_timeout_secs.max(1)).unwrap_or(c_long::MAX);

      let codes = [
         set_ptr(CURLOPT_URL, url_cstr.as_ptr().cast()),
         proto_code,
         redir_proto_code,
         // Redirects are followed manually so every hop is validated before connecting.
         set_long(CURLOPT_FOLLOWLOCATION, 0),
         set_long(CURLOPT_SSL_VERIFYPEER, 1),
         set_long(CURLOPT_SSL_VERIFYHOST, 2),
         set_long(CURLOPT_NOSIGNAL, 1),
         set_long(CURLOPT_CONNECTTIMEOUT_MS, connect_ms),
         set_long(CURLOPT_LOW_SPEED_LIMIT, 1),
         set_long(CURLOPT_LOW_SPEED_TIME, stall_secs),
         set_ptr(CURLOPT_USERAGENT, USER_AGENT_CSTR.as_ptr().cast()),
         set_ptr(CURLOPT_HTTPHEADER, slist.1.cast_const()),
      ];
      if let Some(&code) = codes.iter().find(|&&code| code != 0) {
         return Err(curl_err(curl, url, code));
      }

      Ok(PreparedEasy {
         easy,
         curl,
         _headers: slist,
         _url_cstr: url_cstr,
      })
   }

   enum StepOutcome {
      Complete,
      Redirect(String),
   }

   fn finish_hop(
      prepared: &PreparedEasy<'_>,
      url: &str,
      initial_scheme: UrlScheme,
      code: c_int,
      aborted_non_2xx: bool,
   ) -> Result<StepOutcome, Error> {
      match code {
         0 => {},
         CURLE_WRITE_ERROR if aborted_non_2xx => {},
         CURLE_UNSUPPORTED_PROTOCOL | CURLE_URL_MALFORMAT => {
            return Err(Error::InvalidUrl {
               url: url.to_owned(),
               reason: "unsupported or malformed URL".to_owned(),
            });
         },
         _ => return Err(curl_err(prepared.curl, url, code)),
      }

      let status = query_status(prepared.curl, prepared.easy.1)
         .map_err(|err_code| curl_err(prepared.curl, url, err_code))?;
      if is_redirect_status(status) {
         let Some(next_url) = query_redirect_url(prepared.curl, prepared.easy.1, url)? else {
            return Err(Error::HttpStatus {
               url: url.to_owned(),
               status,
            });
         };
         let next_scheme = validate_url(&next_url)?;
         if initial_scheme == UrlScheme::Https && next_scheme != UrlScheme::Https {
            return Err(Error::InvalidUrl {
               url: next_url,
               reason: "redirect downgraded from HTTPS".to_owned(),
            });
         }
         return Ok(StepOutcome::Redirect(next_url));
      }

      check_effective_url(prepared.curl, prepared.easy.1, url, initial_scheme)?;
      if !(200..300).contains(&status) {
         return Err(Error::HttpStatus {
            url: url.to_owned(),
            status,
         });
      }
      Ok(StepOutcome::Complete)
   }

   struct MemSink<'a> {
      curl: &'a Curl,
      easy: Easy,
      initial_url: &'a str,
      initial_scheme: UrlScheme,
      body: Vec<u8>,
      max_bytes: u64,
      discarded: u64,
      too_large: bool,
      aborted_non_2xx: bool,
      bad_redirect: Option<Error>,
   }

   extern "C" fn write_mem(
      data: *const c_char,
      size: usize,
      count: usize,
      sink_ptr: *mut c_void,
   ) -> usize {
      let len = size.saturating_mul(count);
      // SAFETY: `sink_ptr` is the `MemSink` set in `get_bytes`.
      let sink = unsafe { &mut *sink_ptr.cast::<MemSink<'_>>() };

      if let Ok(status) = query_status(sink.curl, sink.easy)
         && status != 0
         && !(200..300).contains(&status)
      {
         if is_redirect_status(status) {
            let add = u64::try_from(len).unwrap_or(u64::MAX);
            if sink.discarded.saturating_add(add) <= MAX_REDIRECT_BODY {
               sink.discarded = sink.discarded.saturating_add(add);
               return len;
            }
            sink.too_large = true;
            return 0;
         }
         sink.aborted_non_2xx = true;
         return 0;
      }

      if let Err(err) =
         check_effective_url(sink.curl, sink.easy, sink.initial_url, sink.initial_scheme)
      {
         sink.bad_redirect = Some(err);
         return 0;
      }

      if sink.body.is_empty()
         && query_content_length(sink.curl, sink.easy).is_some_and(|total| total > sink.max_bytes)
      {
         sink.too_large = true;
         return 0;
      }

      let cur = u64::try_from(sink.body.len()).unwrap_or(u64::MAX);
      let add = u64::try_from(len).unwrap_or(u64::MAX);
      if cur.saturating_add(add) > sink.max_bytes {
         sink.too_large = true;
         return 0;
      }

      if len > 0 {
         // SAFETY: libcurl provides `len` readable bytes at `data`.
         let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
         sink.body.extend_from_slice(bytes);
      }
      len
   }

   pub fn get_bytes(
      url: &str,
      headers: &[(&str, &str)],
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
      max_bytes: u64,
   ) -> Result<Vec<u8>, Error> {
      let initial_scheme = validate_url(url)?;
      let mut current_url = url.to_owned();

      for redirects in 0..=MAX_REDIRS {
         let hop_scheme = validate_url(&current_url)?;
         if initial_scheme == UrlScheme::Https && hop_scheme != UrlScheme::Https {
            return Err(Error::InvalidUrl {
               url: current_url,
               reason: "redirect downgraded from HTTPS".to_owned(),
            });
         }

         let prepared = prepare_easy(
            &current_url,
            hop_scheme,
            headers,
            connect_timeout_ms,
            stall_timeout_secs,
         )?;
         let mut sink = MemSink {
            curl: prepared.curl,
            easy: prepared.easy.1,
            initial_url: &current_url,
            initial_scheme,
            body: Vec::new(),
            max_bytes,
            discarded: 0,
            too_large: false,
            aborted_non_2xx: false,
            bad_redirect: None,
         };

         // SAFETY: `write_mem` and `sink` outlive `curl_easy_perform`.
         let c1 = unsafe {
            (prepared.curl.easy_setopt)(
               prepared.easy.1,
               CURLOPT_WRITEFUNCTION,
               (write_mem as WriteFn) as *const c_void,
            )
         };
         // SAFETY: `sink` outlives `curl_easy_perform`.
         let c2 = unsafe {
            (prepared.curl.easy_setopt)(
               prepared.easy.1,
               CURLOPT_WRITEDATA,
               (&raw mut sink).cast_const(),
            )
         };
         for code in [c1, c2] {
            if code != 0 {
               return Err(curl_err(prepared.curl, &current_url, code));
            }
         }

         // SAFETY: all pointers configured on `prepared.easy.1` remain alive for this call.
         let code = unsafe { (prepared.curl.easy_perform)(prepared.easy.1) };
         if let Some(err) = sink.bad_redirect {
            return Err(err);
         }
         if sink.too_large {
            return Err(Error::TooLarge {
               url: current_url,
               limit: max_bytes,
            });
         }
         match finish_hop(
            &prepared,
            &current_url,
            initial_scheme,
            code,
            sink.aborted_non_2xx,
         )? {
            StepOutcome::Complete => return Ok(sink.body),
            StepOutcome::Redirect(next_url) => {
               if redirects == MAX_REDIRS {
                  return Err(Error::Http {
                     url: current_url,
                     message: "too many redirects".to_owned(),
                  });
               }
               current_url = next_url;
            },
         }
      }

      Err(Error::Http {
         url: url.to_owned(),
         message: "too many redirects".to_owned(),
      })
   }

   struct FileSink<'a, 'u> {
      curl: &'a Curl,
      easy: Easy,
      initial_url: &'u str,
      initial_scheme: UrlScheme,
      writer: BufWriter<&'a mut File>,
      hasher: sha256::Hasher,
      downloaded: u64,
      discarded: u64,
      total: Option<u64>,
      max_bytes: u64,
      on_progress: &'a mut dyn FnMut(u64, Option<u64>),
      too_large: bool,
      aborted_non_2xx: bool,
      bad_redirect: Option<Error>,
      io_error: Option<io::Error>,
   }

   extern "C" fn write_file(
      data: *const c_char,
      size: usize,
      count: usize,
      sink_ptr: *mut c_void,
   ) -> usize {
      let len = size.saturating_mul(count);
      // SAFETY: `sink_ptr` is the `FileSink` set in `download_into_file`.
      let sink = unsafe { &mut *sink_ptr.cast::<FileSink<'_, '_>>() };

      if let Ok(status) = query_status(sink.curl, sink.easy)
         && status != 0
         && !(200..300).contains(&status)
      {
         if is_redirect_status(status) {
            let add = u64::try_from(len).unwrap_or(u64::MAX);
            if sink.discarded.saturating_add(add) <= MAX_REDIRECT_BODY {
               sink.discarded = sink.discarded.saturating_add(add);
               return len;
            }
            sink.too_large = true;
            return 0;
         }
         sink.aborted_non_2xx = true;
         return 0;
      }

      if let Err(err) =
         check_effective_url(sink.curl, sink.easy, sink.initial_url, sink.initial_scheme)
      {
         sink.bad_redirect = Some(err);
         return 0;
      }

      if sink.total.is_none() {
         sink.total = query_content_length(sink.curl, sink.easy);
         if sink.total.is_some_and(|total| total > sink.max_bytes) {
            sink.too_large = true;
            return 0;
         }
      }

      let add = u64::try_from(len).unwrap_or(u64::MAX);
      if sink.downloaded.saturating_add(add) > sink.max_bytes {
         sink.too_large = true;
         return 0;
      }

      if len > 0 {
         // SAFETY: libcurl provides `len` readable bytes at `data`.
         let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
         if let Err(err) = sink.writer.write_all(bytes) {
            sink.io_error = Some(err);
            return 0;
         }
         sink.hasher.update(bytes);
         sink.downloaded += add;
         (sink.on_progress)(sink.downloaded, sink.total);
      }
      len
   }

   enum FileHopOutcome {
      Complete(DownloadResult),
      Redirect(String),
   }

   fn download_file_hop(
      current_url: &str,
      initial_scheme: UrlScheme,
      opts: &DownloadOptions<'_>,
      file: &mut File,
      dest_for_errors: &Path,
      on_progress: &mut dyn FnMut(u64, Option<u64>),
   ) -> Result<FileHopOutcome, Error> {
      let hop_scheme = validate_url(current_url)?;
      if initial_scheme == UrlScheme::Https && hop_scheme != UrlScheme::Https {
         return Err(Error::InvalidUrl {
            url: current_url.to_owned(),
            reason: "redirect downgraded from HTTPS".to_owned(),
         });
      }

      let prepared = prepare_easy(
         current_url,
         hop_scheme,
         opts.headers,
         opts.connect_timeout_ms,
         opts.stall_timeout_secs,
      )?;

      let mut sink = FileSink {
         curl: prepared.curl,
         easy: prepared.easy.1,
         initial_url: current_url,
         initial_scheme,
         writer: BufWriter::with_capacity(64 * 1024, file),
         hasher: sha256::Hasher::new(),
         downloaded: 0,
         discarded: 0,
         total: None,
         max_bytes: opts.max_bytes,
         on_progress,
         too_large: false,
         aborted_non_2xx: false,
         bad_redirect: None,
         io_error: None,
      };

      // SAFETY: `write_file` and `sink` outlive `curl_easy_perform`.
      let c1 = unsafe {
         (prepared.curl.easy_setopt)(
            prepared.easy.1,
            CURLOPT_WRITEFUNCTION,
            (write_file as WriteFn) as *const c_void,
         )
      };
      // SAFETY: `sink` outlives `curl_easy_perform`.
      let c2 = unsafe {
         (prepared.curl.easy_setopt)(
            prepared.easy.1,
            CURLOPT_WRITEDATA,
            (&raw mut sink).cast_const(),
         )
      };
      for code in [c1, c2] {
         if code != 0 {
            return Err(curl_err(prepared.curl, current_url, code));
         }
      }

      // SAFETY: all pointers configured on `prepared.easy.1` remain alive for this call.
      let code = unsafe { (prepared.curl.easy_perform)(prepared.easy.1) };
      let FileSink {
         writer,
         hasher,
         downloaded,
         too_large,
         aborted_non_2xx,
         bad_redirect,
         io_error,
         ..
      } = sink;
      if let Some(err) = bad_redirect {
         return Err(err);
      }
      if let Some(source) = io_error {
         return Err(Error::Io {
            path: dest_for_errors.to_path_buf(),
            source,
         });
      }
      if too_large {
         return Err(Error::TooLarge {
            url: current_url.to_owned(),
            limit: opts.max_bytes,
         });
      }

      match finish_hop(
         &prepared,
         current_url,
         initial_scheme,
         code,
         aborted_non_2xx,
      )? {
         StepOutcome::Complete => {
            let digest = hasher.finalize_hex();
            let out_file = writer.into_inner().map_err(|err| Error::Io {
               path: dest_for_errors.to_path_buf(),
               source: err.into_error(),
            })?;
            out_file.sync_all().map_err(|source| Error::Io {
               path: dest_for_errors.to_path_buf(),
               source,
            })?;
            Ok(FileHopOutcome::Complete(DownloadResult {
               bytes: downloaded,
               sha256: digest,
            }))
         },
         StepOutcome::Redirect(next_url) => Ok(FileHopOutcome::Redirect(next_url)),
      }
   }

   pub fn download_into_file(
      opts: &DownloadOptions<'_>,
      file: &mut File,
      dest_for_errors: &Path,
      on_progress: &mut dyn FnMut(u64, Option<u64>),
   ) -> Result<DownloadResult, Error> {
      let initial_scheme = validate_url(opts.url)?;
      let mut current_url = opts.url.to_owned();

      for redirects in 0..=MAX_REDIRS {
         match download_file_hop(
            &current_url,
            initial_scheme,
            opts,
            file,
            dest_for_errors,
            on_progress,
         )? {
            FileHopOutcome::Complete(res) => return Ok(res),
            FileHopOutcome::Redirect(next_url) => {
               if redirects == MAX_REDIRS {
                  return Err(Error::Http {
                     url: current_url,
                     message: "too many redirects".to_owned(),
                  });
               }
               current_url = next_url;
            },
         }
      }

      Err(Error::Http {
         url: opts.url.to_owned(),
         message: "too many redirects".to_owned(),
      })
   }

   pub fn download_to_file(
      url: &str,
      headers: &[(&str, &str)],
      dest: &Path,
      max_bytes: u64,
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
      on_progress: &mut dyn FnMut(u64, Option<u64>),
   ) -> Result<DownloadResult, Error> {
      let _ = validate_url(url)?;
      let mut file = OpenOptions::new()
         .write(true)
         .create(true)
         .truncate(true)
         .custom_flags(libc::O_NOFOLLOW)
         .open(dest)
         .map_err(|source| Error::Io {
            path: dest.to_path_buf(),
            source,
         })?;
      let opts = DownloadOptions {
         url,
         headers,
         max_bytes,
         connect_timeout_ms,
         stall_timeout_secs,
      };
      let res = download_into_file(&opts, &mut file, dest, on_progress);
      drop(file);
      if res.is_err() {
         let _ = std::fs::remove_file(dest);
      }
      res
   }
}

#[cfg(windows)]
mod windows {
   use std::{
      ffi::c_void,
      fs::{self, File, OpenOptions},
      io::{self, BufWriter, Write},
      path::Path,
      ptr,
   };

   use super::{
      DownloadOptions, DownloadResult, MAX_REDIRS, USER_AGENT, UrlScheme, is_redirect_status,
      resolve_redirect_url, validate_url,
   };
   use crate::{Error, sha256};

   const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
   const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
   const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
   const WINHTTP_OPTION_DISABLE_FEATURE: u32 = 63;
   const WINHTTP_DISABLE_REDIRECTS: u32 = 0x0000_0002;
   const WINHTTP_ADDREQ_FLAG_ADD: u32 = 0x2000_0000;
   const WINHTTP_ADDREQ_FLAG_REPLACE: u32 = 0x8000_0000;
   const WINHTTP_QUERY_CONTENT_LENGTH: u32 = 5;
   const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
   const WINHTTP_QUERY_LOCATION: u32 = 33;
   const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
   const INTERNET_SCHEME_HTTP: i32 = 1;
   const INTERNET_SCHEME_HTTPS: i32 = 2;
   const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

   #[repr(C)]
   struct UrlComponents {
      struct_size: u32,
      scheme: *mut u16,
      scheme_len: u32,
      scheme_kind: i32,
      host_name: *mut u16,
      host_name_len: u32,
      port: u16,
      user_name: *mut u16,
      user_name_len: u32,
      password: *mut u16,
      password_len: u32,
      url_path: *mut u16,
      url_path_len: u32,
      extra_info: *mut u16,
      extra_info_len: u32,
   }

   #[link(name = "winhttp")]
   unsafe extern "system" {
      fn WinHttpOpen(
         agent: *const u16,
         access_type: u32,
         proxy: *const u16,
         proxy_bypass: *const u16,
         flags: u32,
      ) -> *mut c_void;
      fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
      fn WinHttpSetTimeouts(
         handle: *mut c_void,
         resolve: i32,
         connect: i32,
         send: i32,
         receive: i32,
      ) -> i32;
      fn WinHttpSetOption(
         handle: *mut c_void,
         option: u32,
         buffer: *const c_void,
         buffer_len: u32,
      ) -> i32;
      fn WinHttpCrackUrl(
         url: *const u16,
         url_len: u32,
         flags: u32,
         components: *mut UrlComponents,
      ) -> i32;
      fn WinHttpConnect(
         session: *mut c_void,
         server_name: *const u16,
         port: u16,
         reserved: u32,
      ) -> *mut c_void;
      fn WinHttpOpenRequest(
         connect: *mut c_void,
         verb: *const u16,
         object_name: *const u16,
         version: *const u16,
         referrer: *const u16,
         accept_types: *const *const u16,
         flags: u32,
      ) -> *mut c_void;
      fn WinHttpAddRequestHeaders(
         request: *mut c_void,
         headers: *const u16,
         headers_len: u32,
         modifiers: u32,
      ) -> i32;
      fn WinHttpSendRequest(
         request: *mut c_void,
         headers: *const u16,
         headers_len: u32,
         optional: *const c_void,
         optional_len: u32,
         total_len: u32,
         context: usize,
      ) -> i32;
      fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
      fn WinHttpQueryHeaders(
         request: *mut c_void,
         info_level: u32,
         name: *const u16,
         buffer: *mut c_void,
         buffer_len: *mut u32,
         index: *mut u32,
      ) -> i32;
      fn WinHttpQueryDataAvailable(request: *mut c_void, available: *mut u32) -> i32;
      fn WinHttpReadData(
         request: *mut c_void,
         buffer: *mut c_void,
         to_read: u32,
         read: *mut u32,
      ) -> i32;
   }

   struct Handle(*mut c_void);

   impl Drop for Handle {
      fn drop(&mut self) {
         if !self.0.is_null() {
            // SAFETY: `self.0` is a valid WinHTTP handle owned by this guard.
            unsafe { WinHttpCloseHandle(self.0) };
         }
      }
   }

   fn wide(text: &str) -> Vec<u16> {
      text.encode_utf16().chain(std::iter::once(0)).collect()
   }

   fn os_err(url: &str, context: &str) -> Error {
      Error::Http {
         url: url.to_owned(),
         message: format!("{context}: {}", io::Error::last_os_error()),
      }
   }

   fn query_header_string(req: *mut c_void, info_level: u32) -> Option<String> {
      let mut byte_len: u32 = 0;
      // SAFETY: size query with null output buffer.
      let ok = unsafe {
         WinHttpQueryHeaders(
            req,
            info_level,
            ptr::null(),
            ptr::null_mut(),
            &raw mut byte_len,
            ptr::null_mut(),
         )
      };
      if ok != 0
         || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER)
         || byte_len == 0
      {
         return None;
      }
      let units = (byte_len as usize).div_ceil(2).saturating_add(1);
      let mut buf = vec![0_u16; units];
      // SAFETY: `buf` holds at least `byte_len` bytes.
      if unsafe {
         WinHttpQueryHeaders(
            req,
            info_level,
            ptr::null(),
            buf.as_mut_ptr().cast(),
            &raw mut byte_len,
            ptr::null_mut(),
         )
      } == 0
      {
         return None;
      }
      let count = ((byte_len as usize) / 2).min(buf.len());
      let slice = &buf[..count];
      let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
      String::from_utf16(&slice[..end]).ok()
   }

   fn query_status(req: *mut c_void, url: &str) -> Result<i32, Error> {
      let mut status: u32 = 0;
      let mut len = u32::try_from(size_of::<u32>()).unwrap_or(4);
      // SAFETY: `status` is 4 bytes matching `len`.
      if unsafe {
         WinHttpQueryHeaders(
            req,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            ptr::null(),
            (&raw mut status).cast(),
            &raw mut len,
            ptr::null_mut(),
         )
      } == 0
      {
         return Err(os_err(url, "WinHttpQueryHeaders(status)"));
      }
      Ok(i32::try_from(status).unwrap_or(0))
   }

   fn query_content_length(req: *mut c_void) -> Option<u64> {
      query_header_string(req, WINHTTP_QUERY_CONTENT_LENGTH)?
         .trim()
         .parse::<u64>()
         .ok()
   }

   struct OpenRequest {
      // Dropped in declaration order: request, connection, then session.
      req: Handle,
      _conn: Handle,
      _session: Handle,
   }

   enum HopState {
      Ready {
         open: OpenRequest,
         content_length: Option<u64>,
      },
      Redirect(String),
   }

   struct CrackedUrl {
      host: Vec<u16>,
      full_path: Vec<u16>,
      port: u16,
      secure: bool,
   }

   fn crack_url(url: &str, hop_scheme: UrlScheme) -> Result<CrackedUrl, Error> {
      let wide_url = wide(url);
      let cap = wide_url.len().max(256);
      let mut host_buf = vec![0_u16; cap];
      let mut path_buf = vec![0_u16; cap];
      let mut extra_buf = vec![0_u16; cap];

      let mut parts = UrlComponents {
         struct_size: u32::try_from(size_of::<UrlComponents>()).unwrap_or(0),
         scheme: ptr::null_mut(),
         scheme_len: 0,
         scheme_kind: 0,
         host_name: host_buf.as_mut_ptr(),
         host_name_len: u32::try_from(host_buf.len()).unwrap_or(0),
         port: 0,
         user_name: ptr::null_mut(),
         user_name_len: 0,
         password: ptr::null_mut(),
         password_len: 0,
         url_path: path_buf.as_mut_ptr(),
         url_path_len: u32::try_from(path_buf.len()).unwrap_or(0),
         extra_info: extra_buf.as_mut_ptr(),
         extra_info_len: u32::try_from(extra_buf.len()).unwrap_or(0),
      };

      // SAFETY: `wide_url` is NUL terminated and output buffers match their lengths.
      if unsafe { WinHttpCrackUrl(wide_url.as_ptr(), 0, 0, &raw mut parts) } == 0 {
         return Err(Error::InvalidUrl {
            url: url.to_owned(),
            reason: "malformed URL".to_owned(),
         });
      }
      let secure = match parts.scheme_kind {
         INTERNET_SCHEME_HTTPS => true,
         INTERNET_SCHEME_HTTP if hop_scheme == UrlScheme::LoopbackHttp => false,
         _ => {
            return Err(Error::InvalidUrl {
               url: url.to_owned(),
               reason: "unsupported URL scheme".to_owned(),
            });
         },
      };

      host_buf.truncate(parts.host_name_len as usize);
      host_buf.push(0);

      let mut full_path =
         Vec::with_capacity(parts.url_path_len as usize + parts.extra_info_len as usize + 2);
      if parts.url_path_len == 0 {
         full_path.push(u16::from(b'/'));
      } else {
         full_path.extend_from_slice(&path_buf[..parts.url_path_len as usize]);
      }
      full_path.extend_from_slice(&extra_buf[..parts.extra_info_len as usize]);
      full_path.push(0);

      Ok(CrackedUrl {
         host: host_buf,
         full_path,
         port: parts.port,
         secure,
      })
   }

   fn open_session(
      url: &str,
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
   ) -> Result<Handle, Error> {
      let agent = wide(USER_AGENT);
      // SAFETY: WinHTTP handles are owned by RAII `Handle`.
      let raw_session = unsafe {
         let auto = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            ptr::null(),
            ptr::null(),
            0,
         );
         if auto.is_null() {
            WinHttpOpen(
               agent.as_ptr(),
               WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
               ptr::null(),
               ptr::null(),
               0,
            )
         } else {
            auto
         }
      };
      let session = Handle(raw_session);
      if session.0.is_null() {
         return Err(os_err(url, "WinHttpOpen"));
      }
      let connect_ms = i32::try_from(connect_timeout_ms.max(1)).unwrap_or(i32::MAX);
      let stall_ms =
         i32::try_from(stall_timeout_secs.max(1).saturating_mul(1000)).unwrap_or(i32::MAX);
      // SAFETY: `session.0` is a live session handle.
      unsafe {
         WinHttpSetTimeouts(session.0, connect_ms, connect_ms, stall_ms, stall_ms);
      }
      Ok(session)
   }

   fn validate_and_add_headers(
      req: *mut c_void,
      url: &str,
      headers: &[(&str, &str)],
   ) -> Result<(), Error> {
      for &(name, value) in headers {
         if name.is_empty()
            || name
               .bytes()
               .any(|b| matches!(b, b'\r' | b'\n' | b'\0' | b':'))
            || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0'))
         {
            return Err(Error::Http {
               url: url.to_owned(),
               message: "invalid HTTP header".to_owned(),
            });
         }
         let line = wide(&format!("{name}: {value}\r\n"));
         let len = u32::try_from(line.len().saturating_sub(1)).unwrap_or(0);
         // SAFETY: `req` is valid and `line` has `len` UTF-16 code units.
         if unsafe {
            WinHttpAddRequestHeaders(
               req,
               line.as_ptr(),
               len,
               WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
            )
         } == 0
         {
            return Err(os_err(url, "WinHttpAddRequestHeaders"));
         }
      }
      Ok(())
   }

   fn open_hop(
      url: &str,
      initial_scheme: UrlScheme,
      headers: &[(&str, &str)],
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
   ) -> Result<HopState, Error> {
      let hop_scheme = validate_url(url)?;
      if initial_scheme == UrlScheme::Https && hop_scheme != UrlScheme::Https {
         return Err(Error::InvalidUrl {
            url: url.to_owned(),
            reason: "redirect downgraded from HTTPS".to_owned(),
         });
      }

      let cracked = crack_url(url, hop_scheme)?;
      let session = open_session(url, connect_timeout_ms, stall_timeout_secs)?;

      // SAFETY: `cracked.host` is NUL terminated UTF-16.
      let conn =
         Handle(unsafe { WinHttpConnect(session.0, cracked.host.as_ptr(), cracked.port, 0) });
      if conn.0.is_null() {
         return Err(os_err(url, "WinHttpConnect"));
      }

      let verb = wide("GET");
      let flags = if cracked.secure {
         WINHTTP_FLAG_SECURE
      } else {
         0
      };
      // SAFETY: `verb` and `cracked.full_path` are NUL terminated UTF-16.
      let req = Handle(unsafe {
         WinHttpOpenRequest(
            conn.0,
            verb.as_ptr(),
            cracked.full_path.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            flags,
         )
      });
      if req.0.is_null() {
         return Err(os_err(url, "WinHttpOpenRequest"));
      }

      // Disable automatic redirects so every hop is validated before connecting.
      let disable = WINHTTP_DISABLE_REDIRECTS;
      // SAFETY: `disable` is a 4-byte `DWORD` option value for `WINHTTP_OPTION_DISABLE_FEATURE`.
      if unsafe {
         WinHttpSetOption(
            req.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            (&raw const disable).cast(),
            u32::try_from(size_of::<u32>()).unwrap_or(4),
         )
      } == 0
      {
         return Err(os_err(url, "WinHttpSetOption"));
      }

      validate_and_add_headers(req.0, url, headers)?;

      // SAFETY: empty body GET request on live `req.0`.
      if unsafe { WinHttpSendRequest(req.0, ptr::null(), 0, ptr::null(), 0, 0, 0) } == 0 {
         return Err(os_err(url, "WinHttpSendRequest"));
      }
      // SAFETY: `req.0` has a sent request.
      if unsafe { WinHttpReceiveResponse(req.0, ptr::null_mut()) } == 0 {
         return Err(os_err(url, "WinHttpReceiveResponse"));
      }

      let status = query_status(req.0, url)?;
      if is_redirect_status(status) {
         let Some(raw_loc) = query_header_string(req.0, WINHTTP_QUERY_LOCATION) else {
            return Err(Error::HttpStatus {
               url: url.to_owned(),
               status,
            });
         };
         let next_url = resolve_redirect_url(url, &raw_loc)?;
         let next_scheme = validate_url(&next_url)?;
         if initial_scheme == UrlScheme::Https && next_scheme != UrlScheme::Https {
            return Err(Error::InvalidUrl {
               url: next_url,
               reason: "redirect downgraded from HTTPS".to_owned(),
            });
         }
         return Ok(HopState::Redirect(next_url));
      }

      if !(200..300).contains(&status) {
         return Err(Error::HttpStatus {
            url: url.to_owned(),
            status,
         });
      }

      let content_length = query_content_length(req.0);
      Ok(HopState::Ready {
         open: OpenRequest {
            req,
            _conn: conn,
            _session: session,
         },
         content_length,
      })
   }

   fn follow_redirects(
      url: &str,
      headers: &[(&str, &str)],
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
   ) -> Result<(String, OpenRequest, Option<u64>), Error> {
      let initial_scheme = validate_url(url)?;
      let mut current_url = url.to_owned();

      for redirects in 0..=MAX_REDIRS {
         match open_hop(
            &current_url,
            initial_scheme,
            headers,
            connect_timeout_ms,
            stall_timeout_secs,
         )? {
            HopState::Ready {
               open,
               content_length,
            } => return Ok((current_url, open, content_length)),
            HopState::Redirect(next_url) => {
               if redirects == MAX_REDIRS {
                  return Err(Error::Http {
                     url: current_url,
                     message: "too many redirects".to_owned(),
                  });
               }
               current_url = next_url;
            },
         }
      }

      Err(Error::Http {
         url: url.to_owned(),
         message: "too many redirects".to_owned(),
      })
   }

   pub fn get_bytes(
      url: &str,
      headers: &[(&str, &str)],
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
      max_bytes: u64,
   ) -> Result<Vec<u8>, Error> {
      let (final_url, open, content_length) =
         follow_redirects(url, headers, connect_timeout_ms, stall_timeout_secs)?;
      if content_length.is_some_and(|total| total > max_bytes) {
         return Err(Error::TooLarge {
            url: final_url,
            limit: max_bytes,
         });
      }

      let mut body = Vec::new();
      let mut chunk = vec![0_u8; 64 * 1024];
      loop {
         let mut available: u32 = 0;
         // SAFETY: `open.req.0` has a received response.
         if unsafe { WinHttpQueryDataAvailable(open.req.0, &raw mut available) } == 0 {
            return Err(os_err(&final_url, "WinHttpQueryDataAvailable"));
         }
         if available == 0 {
            break;
         }
         let to_read = (available as usize).min(chunk.len());
         let cur = u64::try_from(body.len()).unwrap_or(u64::MAX);
         let add = u64::try_from(to_read).unwrap_or(u64::MAX);
         if cur.saturating_add(add) > max_bytes {
            return Err(Error::TooLarge {
               url: final_url,
               limit: max_bytes,
            });
         }
         let mut read: u32 = 0;
         // SAFETY: `chunk` has `to_read` writable bytes.
         if unsafe {
            WinHttpReadData(
               open.req.0,
               chunk.as_mut_ptr().cast(),
               u32::try_from(to_read).unwrap_or(64 * 1024),
               &raw mut read,
            )
         } == 0
         {
            return Err(os_err(&final_url, "WinHttpReadData"));
         }
         if read == 0 {
            break;
         }
         body.extend_from_slice(&chunk[..read as usize]);
      }

      Ok(body)
   }

   pub fn download_into_file(
      opts: &DownloadOptions<'_>,
      file: &mut File,
      dest_for_errors: &Path,
      on_progress: &mut dyn FnMut(u64, Option<u64>),
   ) -> Result<DownloadResult, Error> {
      let (final_url, open, content_length) = follow_redirects(
         opts.url,
         opts.headers,
         opts.connect_timeout_ms,
         opts.stall_timeout_secs,
      )?;
      if content_length.is_some_and(|total| total > opts.max_bytes) {
         return Err(Error::TooLarge {
            url: final_url,
            limit: opts.max_bytes,
         });
      }

      let mut writer = BufWriter::with_capacity(64 * 1024, file);
      let mut hasher = sha256::Hasher::new();
      let mut downloaded: u64 = 0;
      let mut chunk = vec![0_u8; 64 * 1024];

      loop {
         let mut available: u32 = 0;
         // SAFETY: `open.req.0` has a received response.
         if unsafe { WinHttpQueryDataAvailable(open.req.0, &raw mut available) } == 0 {
            return Err(os_err(&final_url, "WinHttpQueryDataAvailable"));
         }
         if available == 0 {
            break;
         }
         let to_read = (available as usize).min(chunk.len());
         let mut read: u32 = 0;
         // SAFETY: `chunk` has `to_read` writable bytes.
         if unsafe {
            WinHttpReadData(
               open.req.0,
               chunk.as_mut_ptr().cast(),
               u32::try_from(to_read).unwrap_or(64 * 1024),
               &raw mut read,
            )
         } == 0
         {
            return Err(os_err(&final_url, "WinHttpReadData"));
         }
         if read == 0 {
            break;
         }
         let add = u64::from(read);
         if downloaded.saturating_add(add) > opts.max_bytes {
            return Err(Error::TooLarge {
               url: final_url,
               limit: opts.max_bytes,
            });
         }
         let slice = &chunk[..read as usize];
         writer.write_all(slice).map_err(|source| Error::Io {
            path: dest_for_errors.to_path_buf(),
            source,
         })?;
         hasher.update(slice);
         downloaded += add;
         on_progress(downloaded, content_length);
      }

      let digest = hasher.finalize_hex();
      let out_file = writer.into_inner().map_err(|err| Error::Io {
         path: dest_for_errors.to_path_buf(),
         source: err.into_error(),
      })?;
      out_file.sync_all().map_err(|source| Error::Io {
         path: dest_for_errors.to_path_buf(),
         source,
      })?;

      Ok(DownloadResult {
         bytes: downloaded,
         sha256: digest,
      })
   }

   pub fn download_to_file(
      url: &str,
      headers: &[(&str, &str)],
      dest: &Path,
      max_bytes: u64,
      connect_timeout_ms: i64,
      stall_timeout_secs: i64,
      on_progress: &mut dyn FnMut(u64, Option<u64>),
   ) -> Result<DownloadResult, Error> {
      let _ = validate_url(url)?;
      if let Ok(meta) = fs::symlink_metadata(dest)
         && meta.file_type().is_symlink()
      {
         return Err(Error::Io {
            path: dest.to_path_buf(),
            source: io::Error::new(
               io::ErrorKind::InvalidInput,
               "refusing to overwrite symlink destination",
            ),
         });
      }
      let mut file = OpenOptions::new()
         .write(true)
         .create(true)
         .truncate(true)
         .open(dest)
         .map_err(|source| Error::Io {
            path: dest.to_path_buf(),
            source,
         })?;
      let opts = DownloadOptions {
         url,
         headers,
         max_bytes,
         connect_timeout_ms,
         stall_timeout_secs,
      };
      let res = download_into_file(&opts, &mut file, dest, on_progress);
      drop(file);
      if res.is_err() {
         let _ = fs::remove_file(dest);
      }
      res
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn validates_allowed_and_blocked_urls() {
      assert_eq!(
         validate_url("https://api.github.com/repos/CateDesu/NyaaCord/releases/latest").ok(),
         Some(UrlScheme::Https)
      );
      assert_eq!(
         validate_url("https://example.com:8443/@scope/file").ok(),
         Some(UrlScheme::Https)
      );
      assert_eq!(
         validate_url("http://127.0.0.1:8080/release").ok(),
         Some(UrlScheme::LoopbackHttp)
      );
      assert_eq!(
         validate_url("http://127.0.0.1/release").ok(),
         Some(UrlScheme::LoopbackHttp)
      );
      assert_eq!(
         validate_url("http://localhost:9000/a").ok(),
         Some(UrlScheme::LoopbackHttp)
      );
      assert_eq!(
         validate_url("http://localhost").ok(),
         Some(UrlScheme::LoopbackHttp)
      );

      for bad in [
         "",
         "http://example.com/file",
         "http://127.0.0.1.evil.com/file",
         "http://localhost.evil.com/file",
         "http://127.0.0.1@evil.com/file",
         "http://127.0.0.1:0/file",
         "http://127.0.0.1:abc/file",
         "file:///etc/passwd",
         "ftp://example.com/file",
         "javascript:alert(1)",
         "https://example.com/a b",
         "https://",
         "https://:443/file",
         "https://github.com@evil.com/file",
         "https://example.com:0/file",
         "https://example.com:abc/file",
      ] {
         assert!(
            validate_url(bad).is_err(),
            "expected {bad:?} to be rejected"
         );
      }
   }

   #[test]
   fn resolves_redirect_urls() {
      let base = "https://github.com/CateDesu/NyaaCord/releases/download/v0.2.0/NyaaCord.exe";
      assert_eq!(
         resolve_redirect_url(base, "https://objects.githubusercontent.com/blob?sig=1").ok(),
         Some("https://objects.githubusercontent.com/blob?sig=1".to_owned())
      );
      assert_eq!(
         resolve_redirect_url(base, "/final/file.exe").ok(),
         Some("https://github.com/final/file.exe".to_owned())
      );
      assert_eq!(
         resolve_redirect_url(base, "//cdn.example.com/file.exe").ok(),
         Some("https://cdn.example.com/file.exe".to_owned())
      );
      assert_eq!(
         resolve_redirect_url(base, "../v0.3.0/NyaaCord.exe?raw=1").ok(),
         Some(
            "https://github.com/CateDesu/NyaaCord/releases/download/v0.3.0/NyaaCord.exe?raw=1"
               .to_owned()
         )
      );
      assert_eq!(
         resolve_redirect_url("http://127.0.0.1:8080/bounce", "http://127.0.0.2:9000/evil").ok(),
         Some("http://127.0.0.2:9000/evil".to_owned())
      );
      assert!(validate_url("http://127.0.0.2:9000/evil").is_err());
   }
}
