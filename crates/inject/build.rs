//! Embeds the `NyaaCord` version, optional Windows DLL and plugin bundle payloads,
//! and optional Win32 icon resource into `phoenix-inject`.

use std::{
   env, fs,
   path::{Path, PathBuf},
   process::Command,
};

fn emit_version() {
   println!("cargo:rerun-if-env-changed=NYAACORD_VERSION");
   let pkg_version = env!("CARGO_PKG_VERSION");
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

fn compile_windows_icon(out_dir: &Path) {
   println!("cargo:rerun-if-env-changed=NYAACORD_ICON_RES");
   println!("cargo:rerun-if-env-changed=NYAACORD_ICON_ICO");

   if let Ok(raw) = env::var("NYAACORD_ICON_RES")
      && !raw.trim().is_empty()
   {
      let canon = fs::canonicalize(raw.trim())
         .unwrap_or_else(|err| panic!("NYAACORD_ICON_RES unreadable ({raw:?}): {err}"));
      println!("cargo:rerun-if-changed={}", canon.display());
      println!(
         "cargo:rustc-link-arg-bin=phoenix-inject={}",
         canon.display()
      );
      return;
   }

   let Ok(raw_ico) = env::var("NYAACORD_ICON_ICO") else {
      return;
   };
   let trimmed = raw_ico.trim();
   if trimmed.is_empty() {
      return;
   }
   let ico = fs::canonicalize(trimmed)
      .unwrap_or_else(|err| panic!("NYAACORD_ICON_ICO unreadable ({trimmed:?}): {err}"));
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
   let rc_path = out_dir.join("nyaacord.rc");
   let res_path = out_dir.join("nyaacord.res");
   fs::write(&rc_path, format!("1 ICON \"{ico_rc_path}\"\n")).expect("write nyaacord.rc");

   let rc_ok = Command::new("llvm-rc")
      .args(["/no-preprocess", "/FO"])
      .arg(&res_path)
      .arg(&rc_path)
      .status()
      .is_ok_and(|s| s.success());
   if !rc_ok {
      let fallback = Command::new("rc.exe")
         .args(["/nologo", "/fo"])
         .arg(&res_path)
         .arg(&rc_path)
         .status();
      assert!(
         fallback.is_ok_and(|s| s.success()),
         "failed to compile Windows icon resource with llvm-rc or rc.exe"
      );
   }

   let obj_path = out_dir.join("nyaacord_res.obj");
   let cvtres_ok = Command::new("llvm-cvtres")
      .arg("/machine:x64")
      .arg(format!("/out:{}", obj_path.display()))
      .arg(&res_path)
      .status()
      .is_ok_and(|s| s.success());

   let link_artifact = if cvtres_ok && obj_path.is_file() {
      obj_path
   } else {
      res_path
   };
   println!(
      "cargo:rustc-link-arg-bin=phoenix-inject={}",
      link_artifact.display()
   );
}

fn main() {
   emit_version();
   let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
   emit_embedded_assets(&out_dir);

   if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
      compile_windows_icon(&out_dir);
   }
}
