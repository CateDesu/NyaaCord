//! Start Menu and Desktop shortcuts via native Shell COM (`IShellLinkW`).

use std::{
   ffi::{OsStr, OsString, c_void},
   fs, io,
   mem::offset_of,
   os::windows::ffi::{OsStrExt as _, OsStringExt as _},
   path::{Path, PathBuf},
   ptr, slice,
};

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

impl Guid {
   #[expect(
      clippy::cast_possible_truncation,
      reason = "splits the u128 into its fields"
   )]
   const fn new(value: u128) -> Self {
      Self(
         (value >> 96) as u32,
         (value >> 80) as u16,
         (value >> 64) as u16,
         (value as u64).to_be_bytes(),
      )
   }
}

const FOLDER_PROGRAMS: Guid = Guid::new(0xA77F_5D77_2E2B_44C3_A6A2_ABA6_0105_4A51);
const FOLDER_DESKTOP: Guid = Guid::new(0xB4BF_CC3A_DB2C_424C_B029_7FE9_9A87_C641);
const SHELL_LINK: Guid = Guid::new(0x0002_1401_0000_0000_C000_0000_0000_0046);
const ISHELL_LINK_W: Guid = Guid::new(0x0002_14F9_0000_0000_C000_0000_0000_0046);
const IPERSIST_FILE: Guid = Guid::new(0x0000_010B_0000_0000_C000_0000_0000_0046);

const APARTMENT_THREADED: u32 = 2;
const INPROC_SERVER: u32 = 1;
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32.cast_signed();

type SetText = unsafe extern "system" fn(*mut c_void, *const u16) -> i32;

/// `IUnknown`'s vtable prefix shared by every COM interface.
#[repr(C)]
struct Unknown {
   _query_interface: usize,
   _add_ref: usize,
   release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IShellLinkW`'s vtable, with unused slots padded out.
#[repr(C)]
struct ShellLink {
   query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
   _add_ref: usize,
   _release: usize,
   _ids: [usize; 4],
   set_description: SetText,
   _working_dir: [usize; 3],
   set_arguments: SetText,
   _hotkey_show: [usize; 5],
   set_icon: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32,
   _relative: [usize; 2],
   set_path: SetText,
}

/// `IPersistFile`'s vtable up to `Save`.
#[repr(C)]
struct PersistFile {
   _unknown: [usize; 6],
   save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32,
}

const _: () = {
   let ptr = size_of::<usize>();
   assert!(size_of::<Guid>() == 16);
   assert!(offset_of!(Unknown, release) == 2 * ptr);
   assert!(offset_of!(ShellLink, query_interface) == 0);
   assert!(offset_of!(ShellLink, set_description) == 7 * ptr);
   assert!(offset_of!(ShellLink, set_arguments) == 11 * ptr);
   assert!(offset_of!(ShellLink, set_icon) == 17 * ptr);
   assert!(offset_of!(ShellLink, set_path) == 20 * ptr);
   assert!(offset_of!(PersistFile, save) == 6 * ptr);
};

#[link(name = "ole32")]
unsafe extern "system" {
   fn CoInitializeEx(reserved: *mut c_void, model: u32) -> i32;
   fn CoUninitialize();
   fn CoCreateInstance(
      class: *const Guid,
      outer: *mut c_void,
      context: u32,
      interface: *const Guid,
      out: *mut *mut c_void,
   ) -> i32;
   fn CoTaskMemFree(ptr: *mut c_void);
}

#[link(name = "shell32")]
unsafe extern "system" {
   fn SHGetKnownFolderPath(
      folder: *const Guid,
      flags: u32,
      token: *mut c_void,
      path: *mut *mut u16,
   ) -> i32;
}

/// Balances a successful `CoInitializeEx` on this thread with `CoUninitialize`.
struct ComInit {
   uninit: bool,
}

impl ComInit {
   fn new() -> misstep::Result<Self> {
      // SAFETY: initializes COM on the current thread with no memory preconditions.
      let hr = unsafe { CoInitializeEx(ptr::null_mut(), APARTMENT_THREADED) };
      if hr == RPC_E_CHANGED_MODE {
         return Ok(Self { uninit: false });
      }
      check(hr, "CoInitializeEx")?;
      Ok(Self { uninit: true })
   }
}

impl Drop for ComInit {
   fn drop(&mut self) {
      if self.uninit {
         // SAFETY: matches a successful `CoInitializeEx` on this thread.
         unsafe { CoUninitialize() };
      }
   }
}

/// Releases a COM object on drop via `IUnknown::Release`.
struct Com(*mut c_void);

impl Com {
   fn new(raw: *mut c_void, call: &str) -> misstep::Result<Self> {
      if raw.is_null() {
         misstep::bail!("{call} returned null interface pointer");
      }
      Ok(Self(raw))
   }

   fn vtable<T>(&self) -> &T {
      // SAFETY: a COM object starts with a pointer to its vtable.
      unsafe { &**self.0.cast::<*const T>() }
   }
}

impl Drop for Com {
   fn drop(&mut self) {
      // SAFETY: the object is live and this guard owns one reference.
      unsafe { (self.vtable::<Unknown>().release)(self.0) };
   }
}

/// Frees a COM task-memory buffer on drop.
struct CoTaskMem(*mut u16);

impl Drop for CoTaskMem {
   fn drop(&mut self) {
      if !self.0.is_null() {
         // SAFETY: allocated by the Shell COM API and freed once here.
         unsafe { CoTaskMemFree(self.0.cast()) };
      }
   }
}

fn check(result: i32, call: &str) -> misstep::Result<()> {
   if result < 0 {
      misstep::bail!("{call} failed ({result:#x})");
   }
   Ok(())
}

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
   text.as_ref().encode_wide().chain([0]).collect()
}

/// Resolves a known folder path, following redirected Desktop folders such as `OneDrive`.
fn known_folder(id: &Guid) -> misstep::Result<PathBuf> {
   let mut raw = ptr::null_mut::<u16>();
   // SAFETY: `raw` receives a CoTaskMem string, freed unconditionally by `guard`.
   let hr = unsafe { SHGetKnownFolderPath(id, 0, ptr::null_mut(), &raw mut raw) };
   let guard = CoTaskMem(raw);
   check(hr, "SHGetKnownFolderPath")?;
   if guard.0.is_null() {
      return Ok(PathBuf::new());
   }
   // SAFETY: the call succeeded with a non-null NUL-terminated UTF-16 string.
   let path = unsafe {
      let mut len = 0;
      while *guard.0.add(len) != 0 {
         len += 1;
      }
      OsString::from_wide(slice::from_raw_parts(guard.0, len))
   };
   Ok(path.into())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
   match fs::remove_file(path) {
      Ok(()) => Ok(()),
      Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(err) => Err(err),
   }
}

fn create(
   shortcut: &Path,
   target: &Path,
   args: &str,
   description: &str,
   icon: &Path,
) -> misstep::Result<()> {
   let mut raw = ptr::null_mut();
   // SAFETY: COM is initialized on this thread.
   check(
      unsafe {
         CoCreateInstance(
            &SHELL_LINK,
            ptr::null_mut(),
            INPROC_SERVER,
            &ISHELL_LINK_W,
            &raw mut raw,
         )
      },
      "CoCreateInstance",
   )?;
   let link = Com::new(raw, "CoCreateInstance")?;
   let shell = link.vtable::<ShellLink>();
   let (target, args, description, icon) =
      (wide(target), wide(args), wide(description), wide(icon));
   // SAFETY: `link.0` is a live `IShellLinkW` and all strings are NUL terminated.
   unsafe {
      check((shell.set_path)(link.0, target.as_ptr()), "SetPath")?;
      check((shell.set_arguments)(link.0, args.as_ptr()), "SetArguments")?;
      check(
         (shell.set_description)(link.0, description.as_ptr()),
         "SetDescription",
      )?;
      check(
         (shell.set_icon)(link.0, icon.as_ptr(), 0),
         "SetIconLocation",
      )?;
   }
   let mut raw = ptr::null_mut();
   // SAFETY: `link.0` is a live `IShellLinkW`.
   check(
      unsafe { (shell.query_interface)(link.0, &IPERSIST_FILE, &raw mut raw) },
      "QueryInterface",
   )?;
   let file = Com::new(raw, "QueryInterface")?;
   let shortcut = wide(shortcut);
   // SAFETY: `file.0` is a live `IPersistFile` and `shortcut` is NUL terminated.
   check(
      unsafe { (file.vtable::<PersistFile>().save)(file.0, shortcut.as_ptr(), 1) },
      "Save",
   )
}

fn sync_dir(dir: &Path, launcher: &Path, remove: bool) -> misstep::Result<()> {
   if dir.as_os_str().is_empty() {
      return Ok(());
   }
   let _ = fs::remove_file(dir.join("Sunbird.lnk"));
   let lnk = dir.join("NyaaCord.lnk");
   remove_if_exists(&lnk)?;
   if !remove {
      create(&lnk, launcher, "", "NyaaCord for Discord Phoenix", launcher)?;
   }
   Ok(())
}

/// Refreshes or removes Start Menu and Desktop `NyaaCord.lnk` shortcuts and cleans legacy `Sunbird.lnk`.
pub fn sync(launcher: &Path, remove: bool) -> misstep::Result<()> {
   let _com = ComInit::new()?;
   let mut first_err: Option<misstep::Report> = None;
   for folder in [&FOLDER_PROGRAMS, &FOLDER_DESKTOP] {
      let res = known_folder(folder).and_then(|dir| sync_dir(&dir, launcher, remove));
      if let Err(err) = res
         && first_err.is_none()
      {
         first_err = Some(err);
      }
   }
   first_err.map_or(Ok(()), Err)
}
