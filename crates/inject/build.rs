//! Embeds the `Nyaacord` version, optional Windows DLL and plugin bundle payloads,
//! and Win32 version and icon resources into `phoenix-inject`.

use std::{
   env,
   fmt::Write as _,
   fs,
   path::{Path, PathBuf},
   process::Command,
};

fn emit_version() -> (String, bool) {
   println!("cargo:rerun-if-env-changed=NYAACORD_VERSION");
   let pkg_version = env!("CARGO_PKG_VERSION");
   let explicit = env::var("NYAACORD_VERSION").is_ok_and(|val| !val.trim().is_empty());
   let raw = env::var("NYAACORD_VERSION")
      .ok()
      .filter(|val| !val.trim().is_empty())
      .unwrap_or_else(|| pkg_version.to_owned());
   let trimmed = raw.trim();
   let stripped = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed).trim();
   let version = if stripped.is_empty() || stripped.contains(['\r', '\n']) {
      pkg_version
   } else {
      stripped
   };
   println!("cargo:rustc-env=NYAACORD_VERSION={version}");
   (version.to_owned(), explicit)
}

fn embedded_const(env_name: &str, const_name: &str) -> String {
   println!("cargo:rerun-if-env-changed={env_name}");
   let Ok(raw) = env::var(env_name) else {
      return format!("pub const {const_name}: &[u8] = &[];\n");
   };
   let trimmed = raw.trim();
   if trimmed.is_empty() {
      return format!("pub const {const_name}: &[u8] = &[];\n");
   }
   let path = PathBuf::from(trimmed);
   let canon = fs::canonicalize(&path)
      .unwrap_or_else(|err| panic!("{env_name} points to unreadable file {trimmed:?}: {err}"));
   assert!(
      canon.is_file(),
      "{env_name} must point to a regular file: {}",
      canon.display()
   );
   println!("cargo:rerun-if-changed={}", canon.display());
   format!(
      "pub const {const_name}: &[u8] = include_bytes!({:?});\n",
      canon.to_string_lossy()
   )
}

fn emit_embedded_assets(out_dir: &Path) {
   let mut src = String::new();
   src.push_str(&embedded_const("NYAACORD_EMBED_DLL", "EMBEDDED_DLL"));
   src.push_str(&embedded_const(
      "NYAACORD_EMBED_PLUGINS",
      "EMBEDDED_PLUGINS",
   ));
   let out_file = out_dir.join("embedded_assets.rs");
   fs::write(&out_file, src).expect("write embedded_assets.rs");
}

fn parse_win32_version(version: &str) -> [u16; 4] {
   let core = version.split(['-', '+']).next().unwrap_or("");
   let mut parts = [0_u16; 4];
   for (slot, seg) in parts.iter_mut().zip(core.split('.')) {
      if let Ok(num) = seg.parse::<u16>() {
         *slot = num;
      }
   }
   parts
}

fn escape_rc_string(value: &str) -> String {
   value.replace('\\', "\\\\").replace('"', "\"\"")
}

fn append_version_rc(rc: &mut String, version: &str) {
   let [major, minor, patch, build] = parse_win32_version(version);
   let ver = escape_rc_string(version);
   let _ = writeln!(
      rc,
      "1 VERSIONINFO\n\
       FILEVERSION {major},{minor},{patch},{build}\n\
       PRODUCTVERSION {major},{minor},{patch},{build}\n\
       FILEFLAGSMASK 0x3f\n\
       FILEFLAGS 0x0\n\
       FILEOS 0x40004\n\
       FILETYPE 0x1\n\
       FILESUBTYPE 0x0\n\
       BEGIN\n\
          BLOCK \"StringFileInfo\"\n\
          BEGIN\n\
             BLOCK \"040904B0\"\n\
             BEGIN\n\
                VALUE \"FileDescription\", \"Nyaacord\\0\"\n\
                VALUE \"ProductName\", \"Nyaacord\\0\"\n\
                VALUE \"InternalName\", \"Nyaacord\\0\"\n\
                VALUE \"OriginalFilename\", \"Nyaacord.exe\\0\"\n\
                VALUE \"FileVersion\", \"{ver}\\0\"\n\
                VALUE \"ProductVersion\", \"{ver}\\0\"\n\
             END\n\
          END\n\
          BLOCK \"VarFileInfo\"\n\
          BEGIN\n\
             VALUE \"Translation\", 0x0409, 1200\n\
          END\n\
       END"
   );
}

enum RcStatus {
   Compiled,
   MissingCompiler,
   Failed(&'static str),
}

fn run_rc_compiler(rc_path: &Path, res_path: &Path) -> RcStatus {
   match Command::new("llvm-rc")
      .args(["/no-preprocess", "/FO"])
      .arg(res_path)
      .arg(rc_path)
      .status()
   {
      Ok(status) if status.success() => return RcStatus::Compiled,
      Ok(_) => return RcStatus::Failed("llvm-rc exited with an error"),
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
      Err(_) => return RcStatus::Failed("failed to invoke llvm-rc"),
   }

   match Command::new("rc.exe")
      .args(["/nologo", "/fo"])
      .arg(res_path)
      .arg(rc_path)
      .status()
   {
      Ok(status) if status.success() => RcStatus::Compiled,
      Ok(_) => RcStatus::Failed("rc.exe exited with an error"),
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => RcStatus::MissingCompiler,
      Err(_) => RcStatus::Failed("failed to invoke rc.exe"),
   }
}

fn cvtres_to_obj(obj_path: &Path, res_files: &[&Path]) -> bool {
   let mut cmd = Command::new("llvm-cvtres");
   cmd.arg("/machine:x64")
      .arg(format!("/out:{}", obj_path.display()));
   for res in res_files {
      cmd.arg(res);
   }
   cmd.status().is_ok_and(|s| s.success()) && obj_path.is_file()
}

fn link_artifact(path: &Path) {
   println!("cargo:rustc-link-arg-bin=phoenix-inject={}", path.display());
}

fn link_res_files(obj_path: &Path, res_files: &[&Path]) {
   if cvtres_to_obj(obj_path, res_files) {
      link_artifact(obj_path);
   } else {
      for res in res_files {
         link_artifact(res);
      }
   }
}

fn compile_version_res(out_dir: &Path, version: &str, required: bool) -> Option<PathBuf> {
   let mut version_rc = String::new();
   append_version_rc(&mut version_rc, version);
   let version_rc_path = out_dir.join("nyaacord_version.rc");
   let version_res_path = out_dir.join("nyaacord_version.res");
   fs::write(&version_rc_path, version_rc).expect("write nyaacord_version.rc");

   match run_rc_compiler(&version_rc_path, &version_res_path) {
      RcStatus::Compiled => Some(version_res_path),
      RcStatus::MissingCompiler if !required => {
         println!(
            "cargo:warning=skipping Windows VS_VERSION_INFO resource because neither llvm-rc nor \
             rc.exe is on PATH"
         );
         None
      },
      RcStatus::MissingCompiler => {
         panic!("failed to compile Windows version resource: neither llvm-rc nor rc.exe found")
      },
      RcStatus::Failed(reason) => panic!("failed to compile Windows version resource: {reason}"),
   }
}

fn compile_windows_resources(out_dir: &Path, version: &str, explicit_version: bool) {
   println!("cargo:rerun-if-env-changed=NYAACORD_ICON_RES");
   println!("cargo:rerun-if-env-changed=NYAACORD_ICON_ICO");

   let icon_res_env = env::var("NYAACORD_ICON_RES")
      .ok()
      .map(|raw| raw.trim().to_owned())
      .filter(|raw| !raw.is_empty());
   let icon_ico_env = env::var("NYAACORD_ICON_ICO")
      .ok()
      .map(|raw| raw.trim().to_owned())
      .filter(|raw| !raw.is_empty());

   if let Some(raw_res) = icon_res_env {
      let icon_res = fs::canonicalize(&raw_res)
         .unwrap_or_else(|err| panic!("NYAACORD_ICON_RES unreadable ({raw_res:?}): {err}"));
      println!("cargo:rerun-if-changed={}", icon_res.display());

      let Some(version_res) = compile_version_res(out_dir, version, explicit_version) else {
         link_artifact(&icon_res);
         return;
      };
      let is_res = icon_res
         .extension()
         .is_some_and(|ext| ext.eq_ignore_ascii_case("res"));
      if is_res {
         link_res_files(
            &out_dir.join("nyaacord_res.obj"),
            &[&icon_res, &version_res],
         );
      } else {
         link_artifact(&icon_res);
         link_res_files(&out_dir.join("nyaacord_version.obj"), &[&version_res]);
      }
      return;
   }

   if let Some(raw_ico) = icon_ico_env {
      let ico = fs::canonicalize(&raw_ico)
         .unwrap_or_else(|err| panic!("NYAACORD_ICON_ICO unreadable ({raw_ico:?}): {err}"));
      assert!(
         ico.is_file(),
         "NYAACORD_ICON_ICO is not a regular file: {}",
         ico.display()
      );
      println!("cargo:rerun-if-changed={}", ico.display());

      let ico_rc_path = ico
         .to_string_lossy()
         .replace('\\', "/")
         .replace('"', "\"\"");
      let icon_rc_path = out_dir.join("nyaacord_icon.rc");
      let icon_res_path = out_dir.join("nyaacord_icon.res");
      fs::write(&icon_rc_path, format!("1 ICON \"{ico_rc_path}\"\n"))
         .expect("write nyaacord_icon.rc");

      match run_rc_compiler(&icon_rc_path, &icon_res_path) {
         RcStatus::Compiled => {},
         RcStatus::MissingCompiler => {
            panic!("failed to compile Windows icon resource: neither llvm-rc nor rc.exe found")
         },
         RcStatus::Failed(reason) => panic!("failed to compile Windows icon resource: {reason}"),
      }
      let version_res = compile_version_res(out_dir, version, true).expect("version resource");
      link_res_files(
         &out_dir.join("nyaacord_res.obj"),
         &[&icon_res_path, &version_res],
      );
      return;
   }

   if let Some(version_res) = compile_version_res(out_dir, version, explicit_version) {
      link_res_files(&out_dir.join("nyaacord_version.obj"), &[&version_res]);
   }
}

fn emit_expect_client() {
   println!("cargo:rerun-if-env-changed=NYAACORD_EXPECT_CLIENT");
   if let Ok(raw) = env::var("NYAACORD_EXPECT_CLIENT") {
      let trimmed = raw.trim();
      if !trimmed.is_empty() {
         println!("cargo:rustc-env=NYAACORD_EXPECT_CLIENT={trimmed}");
      }
   }
}

fn main() {
   let (version, explicit_version) = emit_version();
   emit_expect_client();
   let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
   emit_embedded_assets(&out_dir);

   if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
      compile_windows_resources(&out_dir, &version, explicit_version);
   }
}
