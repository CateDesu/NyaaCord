#![cfg(unix)]

use std::{
   collections::HashMap,
   fs::{self, Permissions},
   io::{BufRead, BufReader, Write},
   net::TcpListener,
   os::unix::fs::PermissionsExt,
   path::{Path, PathBuf},
   process::Command,
   sync::{
      Arc,
      atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
   },
   thread,
   time::Duration,
};

use phoenix_updater::{
   CheckResult, Config, Error, ReleaseInfo, apply_update, check_latest, http, previous_path,
   rollback, sha256, verify_appimage,
};

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
   path: PathBuf,
}

impl TempDir {
   fn new(label: &str) -> Self {
      let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
      let path = std::env::temp_dir().join(format!(
         "phoenix-updater-test-{label}-{}-{seq}",
         std::process::id()
      ));
      let _ = fs::remove_dir_all(&path);
      fs::create_dir_all(&path).expect("create temp test dir");
      Self { path }
   }

   fn path(&self) -> &Path {
      &self.path
   }
}

impl Drop for TempDir {
   fn drop(&mut self) {
      let _ = fs::set_permissions(&self.path, Permissions::from_mode(0o755));
      let _ = fs::remove_dir_all(&self.path);
   }
}

/// Builds a minimal static Linux x86 64 ELF executable with `AppImage` type 2
/// magic and a `SquashFS` `hsqs` superblock at offset 256.
fn build_fake_appimage(payload: &[u8], exit_code: u8) -> Vec<u8> {
   const OFFSET: usize = 256;
   let mut buf = vec![0_u8; OFFSET];

   // ELF64 header with AppImage type 2 signature at e_ident[8..11].
   buf[0..4].copy_from_slice(b"\x7fELF");
   buf[4] = 2; // 64 bit
   buf[5] = 1; // little endian
   buf[6] = 1; // ELF version
   buf[7] = 0; // System V ABI
   buf[8..11].copy_from_slice(b"AI\x02");
   buf[16..18].copy_from_slice(&2_u16.to_le_bytes()); // ET_EXEC
   buf[18..20].copy_from_slice(&62_u16.to_le_bytes()); // EM_X86_64
   buf[20..24].copy_from_slice(&1_u32.to_le_bytes()); // EV_CURRENT
   buf[24..32].copy_from_slice(&0x0040_0078_u64.to_le_bytes()); // e_entry
   buf[32..40].copy_from_slice(&64_u64.to_le_bytes()); // e_phoff
   buf[52..54].copy_from_slice(&64_u16.to_le_bytes()); // e_ehsize
   buf[54..56].copy_from_slice(&56_u16.to_le_bytes()); // e_phentsize
   buf[56..58].copy_from_slice(&1_u16.to_le_bytes()); // e_phnum

   // Single PT_LOAD segment mapping bytes 0..256 at 0x400000.
   let ph = 64;
   buf[ph..ph + 4].copy_from_slice(&1_u32.to_le_bytes()); // PT_LOAD
   buf[ph + 4..ph + 8].copy_from_slice(&5_u32.to_le_bytes()); // PF_R | PF_X
   buf[ph + 8..ph + 16].copy_from_slice(&0_u64.to_le_bytes()); // p_offset
   buf[ph + 16..ph + 24].copy_from_slice(&0x0040_0000_u64.to_le_bytes()); // p_vaddr
   buf[ph + 24..ph + 32].copy_from_slice(&0x0040_0000_u64.to_le_bytes()); // p_paddr
   buf[ph + 32..ph + 40].copy_from_slice(&(OFFSET as u64).to_le_bytes()); // p_filesz
   buf[ph + 40..ph + 48].copy_from_slice(&(OFFSET as u64).to_le_bytes()); // p_memsz
   buf[ph + 48..ph + 56].copy_from_slice(&0x1000_u64.to_le_bytes()); // p_align

   // Machine code at 0x400078: write(1, "256\n", 4); exit(exit_code);
   let code: [u8; 40] = [
      0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
      0xBF, 0x01, 0x00, 0x00, 0x00, // mov edi, 1
      0x48, 0x8D, 0x35, 0x17, 0x00, 0x00, 0x00, // lea rsi, [rip + 23] -> 0x4000a0
      0xBA, 0x04, 0x00, 0x00, 0x00, // mov edx, 4
      0x0F, 0x05, // syscall
      0xB8, 0x3C, 0x00, 0x00, 0x00, // mov eax, 60
      0xBF, exit_code, 0x00, 0x00, 0x00, // mov edi, exit_code
      0x0F, 0x05, // syscall
      0x90, 0x90, 0x90, 0x90, // pad to 0x4000a0
   ];
   buf[0x78..0x78 + code.len()].copy_from_slice(&code);
   buf[0xA0..0xA4].copy_from_slice(b"256\n");

   // SquashFS superblock magic followed by caller payload.
   buf.extend_from_slice(b"hsqs");
   buf.extend_from_slice(payload);
   buf
}

fn sha256_bytes(data: &[u8]) -> String {
   let mut hasher = sha256::Hasher::new();
   hasher.update(data);
   hasher.finalize_hex()
}

#[derive(Clone)]
struct MockResponse {
   status: u16,
   reason: &'static str,
   content_type: &'static str,
   location: Option<String>,
   body: Vec<u8>,
}

impl MockResponse {
   const fn ok(content_type: &'static str, body: Vec<u8>) -> Self {
      Self {
         status: 200,
         reason: "OK",
         content_type,
         location: None,
         body,
      }
   }

   const fn redirect(location: String, body: Vec<u8>) -> Self {
      Self {
         status: 302,
         reason: "Found",
         content_type: "text/html",
         location: Some(location),
         body,
      }
   }
}

struct MockServer {
   base_url: String,
   hits: Arc<HashMap<String, AtomicUsize>>,
   stop: Arc<AtomicBool>,
   handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
   fn start(routes: HashMap<String, MockResponse>) -> Self {
      let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
      let addr = listener.local_addr().expect("local addr");
      listener
         .set_nonblocking(true)
         .expect("set nonblocking listener");

      let hits: Arc<HashMap<String, AtomicUsize>> = Arc::new(
         routes
            .keys()
            .map(|k| (k.clone(), AtomicUsize::new(0)))
            .collect(),
      );
      let hits_clone = Arc::clone(&hits);
      let stop = Arc::new(AtomicBool::new(false));
      let stop_clone = Arc::clone(&stop);

      let handle = thread::spawn(move || {
         while !stop_clone.load(Ordering::Acquire) {
            match listener.accept() {
               Ok((mut stream, _)) => {
                  let _ = stream.set_nonblocking(false);
                  let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                  let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

                  let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                  let mut request_line = String::new();
                  if reader.read_line(&mut request_line).is_err() {
                     continue;
                  }
                  loop {
                     let mut header = String::new();
                     match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" || header == "\n" => break,
                        Ok(_) => {},
                     }
                  }

                  let path = request_line
                     .split_whitespace()
                     .nth(1)
                     .unwrap_or("/")
                     .to_owned();
                  if let Some(counter) = hits_clone.get(&path) {
                     counter.fetch_add(1, Ordering::AcqRel);
                  }

                  let resp = routes.get(&path).cloned().unwrap_or_else(|| MockResponse {
                     status: 404,
                     reason: "Not Found",
                     content_type: "text/plain",
                     location: None,
                     body: b"not found".to_vec(),
                  });

                  let mut head = format!(
                     "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: \
                      close\r\n",
                     resp.status,
                     resp.reason,
                     resp.content_type,
                     resp.body.len()
                  );
                  if let Some(loc) = &resp.location {
                     head.push_str("Location: ");
                     head.push_str(loc);
                     head.push_str("\r\n");
                  }
                  head.push_str("\r\n");
                  let _ = stream.write_all(head.as_bytes());
                  let _ = stream.write_all(&resp.body);
                  let _ = stream.flush();
               },
               Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                  thread::sleep(Duration::from_millis(5));
               },
               Err(_) => break,
            }
         }
      });

      Self {
         base_url: format!("http://{addr}"),
         hits,
         stop,
         handle: Some(handle),
      }
   }

   fn url(&self, path: &str) -> String {
      format!("{}{path}", self.base_url)
   }

   fn hits(&self, path: &str) -> usize {
      self.hits.get(path).map_or(0, |c| c.load(Ordering::Acquire))
   }
}

impl Drop for MockServer {
   fn drop(&mut self) {
      self.stop.store(true, Ordering::Release);
      if let Some(handle) = self.handle.take() {
         let _ = handle.join();
      }
   }
}

fn test_config(api_url: String) -> Config {
   Config {
      repo: "CateDesu/NyaaCord".to_owned(),
      api_url: Some(api_url),
      connect_timeout_ms: 5_000,
      stall_timeout_secs: 10,
   }
}

#[test]
fn check_up_to_date_and_available_via_asset_digest() {
   let v2_bytes = build_fake_appimage(b"version-2-payload", 0);
   let v2_sha = sha256_bytes(&v2_bytes);

   let mut routes = HashMap::new();
   let listener_server = MockServer::start(HashMap::new());
   let dummy_url = listener_server.url("/dl/Nyaacord-x86_64.AppImage");
   let release_json = serde_json::json!({
      "tag_name": "v0.2.0",
      "draft": false,
      "html_url": "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0",
      "assets": [
         {
            "name": "Nyaacord-x86_64.AppImage",
            "browser_download_url": dummy_url,
            "size": v2_bytes.len(),
            "digest": format!("sha256:{v2_sha}")
         }
      ]
   });
   routes.insert(
      "/latest".to_owned(),
      MockResponse::ok("application/json", release_json.to_string().into_bytes()),
   );
   let server = MockServer::start(routes);
   let cfg = test_config(server.url("/latest"));

   let up_to_date = check_latest("0.2.0", &cfg).expect("check up to date");
   assert_eq!(
      up_to_date,
      CheckResult::UpToDate {
         current: "0.2.0".to_owned(),
         latest: "0.2.0".to_owned(),
      }
   );

   let available = check_latest("0.1.9", &cfg).expect("check available");
   match available {
      CheckResult::Available(info) => {
         assert_eq!(info.version, "0.2.0");
         assert_eq!(info.tag_name, "v0.2.0");
         assert_eq!(info.asset_name, "Nyaacord-x86_64.AppImage");
         assert_eq!(info.sha256, v2_sha);
         assert_eq!(info.size, v2_bytes.len() as u64);
      },
      CheckResult::UpToDate { .. } => panic!("expected available release"),
   }

   let legacy = check_latest("2026.11.26.0843-7f5c2ee", &cfg).expect("legacy replaced");
   assert!(matches!(legacy, CheckResult::Available(_)));
}

#[test]
fn check_available_via_sha256sums_fallback() {
   let v2_bytes = build_fake_appimage(b"sums-fallback", 0);
   let v2_sha = sha256_bytes(&v2_bytes);

   let sums_body = format!("{v2_sha}  Nyaacord-x86_64.AppImage\n");
   let mut sums_routes = HashMap::new();
   sums_routes.insert(
      "/SHA256SUMS.txt".to_owned(),
      MockResponse::ok("text/plain", sums_body.into_bytes()),
   );
   let sums_server = MockServer::start(sums_routes);

   let release_json = serde_json::json!({
      "tag_name": "v0.3.0",
      "draft": false,
      "html_url": "https://github.com/CateDesu/NyaaCord/releases/tag/v0.3.0",
      "assets": [
         {
            "name": "Nyaacord-x86_64.AppImage",
            "browser_download_url": sums_server.url("/asset.AppImage"),
            "size": v2_bytes.len()
         },
         {
            "name": "SHA256SUMS.txt",
            "browser_download_url": sums_server.url("/SHA256SUMS.txt"),
            "size": 100
         }
      ]
   });

   let mut api_routes = HashMap::new();
   api_routes.insert(
      "/latest".to_owned(),
      MockResponse::ok("application/json", release_json.to_string().into_bytes()),
   );
   let api_server = MockServer::start(api_routes);

   let res = check_latest("0.2.0", &test_config(api_server.url("/latest")))
      .expect("check via SHA256SUMS.txt");
   match res {
      CheckResult::Available(info) => {
         assert_eq!(info.sha256, v2_sha);
         assert_eq!(info.version, "0.3.0");
      },
      CheckResult::UpToDate { .. } => panic!("expected available release"),
   }
   assert_eq!(sums_server.hits("/SHA256SUMS.txt"), 1);
}

#[test]
fn check_rejects_draft_and_missing_checksum() {
   let draft_json = serde_json::json!({
      "tag_name": "v9.0.0",
      "draft": true,
      "html_url": "https://github.com/CateDesu/NyaaCord/releases/tag/v9.0.0",
      "assets": []
   });
   let no_sum_json = serde_json::json!({
      "tag_name": "v9.0.0",
      "draft": false,
      "html_url": "https://github.com/CateDesu/NyaaCord/releases/tag/v9.0.0",
      "assets": [
         {
            "name": "Nyaacord-x86_64.AppImage",
            "browser_download_url": "https://github.com/CateDesu/NyaaCord/releases/download/v9.0.0/Nyaacord-x86_64.AppImage",
            "size": 1024
         }
      ]
   });

   let mut routes = HashMap::new();
   routes.insert(
      "/draft".to_owned(),
      MockResponse::ok("application/json", draft_json.to_string().into_bytes()),
   );
   routes.insert(
      "/nosum".to_owned(),
      MockResponse::ok("application/json", no_sum_json.to_string().into_bytes()),
   );
   let server = MockServer::start(routes);

   let err =
      check_latest("0.1.0", &test_config(server.url("/draft"))).expect_err("draft must fail");
   assert!(matches!(err, Error::DraftRelease { .. }));

   let err = check_latest("0.1.0", &test_config(server.url("/nosum")))
      .expect_err("missing checksum must fail");
   assert!(matches!(err, Error::MissingChecksum { .. }));
}

#[test]
fn verifies_fake_appimage_and_rejects_bad_images() {
   let dir = TempDir::new("verify");
   let good = build_fake_appimage(b"valid-squashfs-data", 0);
   let good_sha = sha256_bytes(&good);
   let good_path = dir.path().join("good.AppImage");
   fs::write(&good_path, &good).expect("write good appimage");

   verify_appimage(&good_path, Some(good.len() as u64), Some(&good_sha))
      .expect("valid appimage should pass");

   assert!(matches!(
      verify_appimage(&good_path, Some(123), None),
      Err(Error::SizeMismatch { .. })
   ));
   assert!(matches!(
      verify_appimage(&good_path, None, Some(&"0".repeat(64))),
      Err(Error::ChecksumMismatch { .. })
   ));

   let mut bad_magic = good.clone();
   bad_magic[8..11].copy_from_slice(b"XXX");
   let bad_magic_path = dir.path().join("bad_magic.AppImage");
   fs::write(&bad_magic_path, &bad_magic).expect("write bad magic");
   assert!(matches!(
      verify_appimage(&bad_magic_path, None, None),
      Err(Error::InvalidAppImage { .. })
   ));

   let mut bad_hsqs = good;
   bad_hsqs[256..260].copy_from_slice(b"NOPE");
   let bad_hsqs_path = dir.path().join("bad_hsqs.AppImage");
   fs::write(&bad_hsqs_path, &bad_hsqs).expect("write bad hsqs");
   assert!(matches!(
      verify_appimage(&bad_hsqs_path, None, None),
      Err(Error::InvalidAppImage { .. })
   ));

   let failing_exec = build_fake_appimage(b"fail-exit", 1);
   let fail_path = dir.path().join("fail_exec.AppImage");
   fs::write(&fail_path, &failing_exec).expect("write failing exec");
   assert!(matches!(
      verify_appimage(&fail_path, None, None),
      Err(Error::InvalidAppImage { .. })
   ));
}

#[test]
fn atomic_upgrade_through_redirect_creates_previous_and_rolls_back() {
   let dir = TempDir::new("upgrade");
   let target = dir.path().join("Nyaacord-x86_64.AppImage");
   let v1 = build_fake_appimage(b"version-1-image", 0);
   let v2 = build_fake_appimage(b"version-2-image-longer-payload", 0);
   let v2_sha = sha256_bytes(&v2);

   fs::write(&target, &v1).expect("write v1 target");

   let mut final_routes = HashMap::new();
   final_routes.insert(
      "/final/v2.AppImage".to_owned(),
      MockResponse::ok("application/octet-stream", v2.clone()),
   );
   let final_server = MockServer::start(final_routes);

   let mut redirect_routes = HashMap::new();
   redirect_routes.insert(
      "/download/v2.AppImage".to_owned(),
      MockResponse::redirect(
         final_server.url("/final/v2.AppImage"),
         b"<html><body>You are being redirected</body></html>".to_vec(),
      ),
   );
   let redirect_server = MockServer::start(redirect_routes);

   let release = ReleaseInfo {
      version: "0.2.0".to_owned(),
      tag_name: "v0.2.0".to_owned(),
      asset_name: "Nyaacord-x86_64.AppImage".to_owned(),
      download_url: redirect_server.url("/download/v2.AppImage"),
      size: v2.len() as u64,
      sha256: v2_sha,
      release_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
   };
   let cfg = test_config(redirect_server.url("/unused"));

   let mut progress_seen = Vec::new();
   let outcome = apply_update(&release, &target, &cfg, &mut |done, total| {
      progress_seen.push((done, total));
   })
   .expect("apply update");

   assert_eq!(outcome.target, target);
   assert_eq!(outcome.previous, Some(previous_path(&target)));
   assert_eq!(outcome.version, "0.2.0");
   assert!(
      progress_seen
         .last()
         .is_some_and(|&(done, total)| done == v2.len() as u64 && total == Some(v2.len() as u64))
   );

   assert_eq!(fs::read(&target).expect("read upgraded target"), v2);
   let prev = previous_path(&target);
   assert_eq!(fs::read(&prev).expect("read previous backup"), v1);

   let entries: Vec<String> = fs::read_dir(dir.path())
      .expect("read dir")
      .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
      .collect();
   assert_eq!(entries.len(), 2, "unexpected temp files left: {entries:?}");

   let restored_from = rollback(&target).expect("rollback to v1");
   assert_eq!(restored_from, prev);
   assert_eq!(fs::read(&target).expect("read rolled back target"), v1);
   assert!(prev.exists(), "backup file should remain intact");
}

#[test]
fn corrupt_download_fails_without_touching_target() {
   let dir = TempDir::new("corrupt");
   let target = dir.path().join("Nyaacord-x86_64.AppImage");
   let v1 = build_fake_appimage(b"original-v1", 0);
   fs::write(&target, &v1).expect("write v1");

   let v2 = build_fake_appimage(b"downloaded-v2", 0);
   let junk = b"not an appimage at all".to_vec();
   let junk_sha = sha256_bytes(&junk);

   let mut routes = HashMap::new();
   routes.insert(
      "/v2".to_owned(),
      MockResponse::ok("application/octet-stream", v2.clone()),
   );
   routes.insert(
      "/junk".to_owned(),
      MockResponse::ok("application/octet-stream", junk.clone()),
   );
   let server = MockServer::start(routes);
   let cfg = test_config(server.url("/unused"));

   let bad_hash_release = ReleaseInfo {
      version: "0.2.0".to_owned(),
      tag_name: "v0.2.0".to_owned(),
      asset_name: "Nyaacord-x86_64.AppImage".to_owned(),
      download_url: server.url("/v2"),
      size: v2.len() as u64,
      sha256: "f".repeat(64),
      release_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
   };
   let err = apply_update(&bad_hash_release, &target, &cfg, &mut |_, _| {})
      .expect_err("hash mismatch must fail");
   assert!(matches!(err, Error::ChecksumMismatch { .. }));
   assert_eq!(fs::read(&target).expect("target intact"), v1);
   assert!(!previous_path(&target).exists());

   let bad_image_release = ReleaseInfo {
      version: "0.2.0".to_owned(),
      tag_name: "v0.2.0".to_owned(),
      asset_name: "Nyaacord-x86_64.AppImage".to_owned(),
      download_url: server.url("/junk"),
      size: junk.len() as u64,
      sha256: junk_sha,
      release_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
   };
   let err = apply_update(&bad_image_release, &target, &cfg, &mut |_, _| {})
      .expect_err("non appimage must fail");
   assert!(matches!(err, Error::InvalidAppImage { .. }));
   assert_eq!(fs::read(&target).expect("target still intact"), v1);
   assert!(!previous_path(&target).exists());

   let entries: Vec<String> = fs::read_dir(dir.path())
      .expect("read dir")
      .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
      .collect();
   assert_eq!(entries, vec!["Nyaacord-x86_64.AppImage".to_owned()]);
}

#[test]
fn non_writable_and_mount_targets_fail_before_download() {
   let dir = TempDir::new("readonly");
   let v2 = build_fake_appimage(b"never-downloaded", 0);
   let v2_sha = sha256_bytes(&v2);

   let mut routes = HashMap::new();
   routes.insert(
      "/asset".to_owned(),
      MockResponse::ok("application/octet-stream", v2.clone()),
   );
   let server = MockServer::start(routes);
   let cfg = test_config(server.url("/unused"));

   let release = ReleaseInfo {
      version: "0.2.0".to_owned(),
      tag_name: "v0.2.0".to_owned(),
      asset_name: "Nyaacord-x86_64.AppImage".to_owned(),
      download_url: server.url("/asset"),
      size: v2.len() as u64,
      sha256: v2_sha,
      release_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
   };

   let mount_target = Path::new("/tmp/.mount_NyaacordTest/Nyaacord-x86_64.AppImage");
   let err = apply_update(&release, mount_target, &cfg, &mut |_, _| {})
      .expect_err("mount path must be refused");
   assert!(matches!(err, Error::MountPath { .. }));
   assert_eq!(server.hits("/asset"), 0);

   let custom_tmpdir_mount = Path::new("/home/cate/.t3/tmp/.mount_NyaacordTest/Nyaacord.AppImage");
   let err = apply_update(&release, custom_tmpdir_mount, &cfg, &mut |_, _| {})
      .expect_err("custom TMPDIR mount path must be refused");
   assert!(matches!(err, Error::MountPath { .. }));
   assert_eq!(server.hits("/asset"), 0);

   fs::set_permissions(dir.path(), Permissions::from_mode(0o555)).expect("chmod 0555");
   let ro_target = dir.path().join("Nyaacord-x86_64.AppImage");
   let err = apply_update(&release, &ro_target, &cfg, &mut |_, _| {})
      .expect_err("read only dir must be refused");
   let _ = fs::set_permissions(dir.path(), Permissions::from_mode(0o755));

   assert!(matches!(err, Error::NotWritable { .. }));
   assert_eq!(
      server.hits("/asset"),
      0,
      "download must not start for non writable target"
   );
}

#[test]
fn rejects_redirect_from_loopback_to_non_loopback_http() {
   let bad_listener = TcpListener::bind("127.0.0.2:0").expect("bind 127.0.0.2");
   let bad_addr = bad_listener.local_addr().expect("bad addr");
   bad_listener
      .set_nonblocking(true)
      .expect("set nonblocking bad listener");
   let bad_hits = Arc::new(AtomicUsize::new(0));
   let bad_hits_clone = Arc::clone(&bad_hits);
   let stop = Arc::new(AtomicBool::new(false));
   let stop_clone = Arc::clone(&stop);
   let bad_thread = thread::spawn(move || {
      while !stop_clone.load(Ordering::Acquire) {
         match bad_listener.accept() {
            Ok((mut stream, _)) => {
               bad_hits_clone.fetch_add(1, Ordering::AcqRel);
               let _ = stream.write_all(
                  b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nevil",
               );
            },
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
               thread::sleep(Duration::from_millis(5));
            },
            Err(_) => break,
         }
      }
   });

   let mut routes = HashMap::new();
   routes.insert(
      "/bounce".to_owned(),
      MockResponse::redirect(format!("http://{bad_addr}/evil"), Vec::new()),
   );
   let server = MockServer::start(routes);

   let err = http::get_bytes(&server.url("/bounce"), &[], 2_000, 2, 1024)
      .expect_err("redirect to non allowed http host must fail");
   stop.store(true, Ordering::Release);
   let _ = bad_thread.join();
   assert!(matches!(err, Error::InvalidUrl { .. }), "got {err:?}");
   assert_eq!(
      bad_hits.load(Ordering::Acquire),
      0,
      "client must not connect to disallowed redirect target"
   );
}

#[test]
fn http_handles_relative_redirects_limits_errors_and_cleanup() {
   let mut routes = HashMap::new();
   routes.insert(
      "/rel".to_owned(),
      MockResponse::redirect("/final".to_owned(), b"redirect body".to_vec()),
   );
   routes.insert(
      "/final".to_owned(),
      MockResponse::ok("text/plain", b"hello".to_vec()),
   );
   routes.insert(
      "/loop".to_owned(),
      MockResponse::redirect("/loop".to_owned(), Vec::new()),
   );
   routes.insert(
      "/big".to_owned(),
      MockResponse::ok("application/octet-stream", vec![b'A'; 4096]),
   );
   routes.insert(
      "/err".to_owned(),
      MockResponse {
         status: 500,
         reason: "Internal Server Error",
         content_type: "text/plain",
         location: None,
         body: b"boom".to_vec(),
      },
   );
   let server = MockServer::start(routes);

   let body = http::get_bytes(&server.url("/rel"), &[], 2_000, 2, 1024).expect("relative redirect");
   assert_eq!(body, b"hello");

   let err = http::get_bytes(&server.url("/loop"), &[], 2_000, 2, 1024)
      .expect_err("redirect loop must fail");
   assert!(matches!(err, Error::Http { .. }), "got {err:?}");

   let err = http::get_bytes(&server.url("/big"), &[], 2_000, 2, 128)
      .expect_err("response over max_bytes must fail");
   assert!(matches!(err, Error::TooLarge { .. }), "got {err:?}");

   let err =
      http::get_bytes(&server.url("/err"), &[], 2_000, 2, 1024).expect_err("HTTP 500 must fail");
   assert!(
      matches!(err, Error::HttpStatus { status: 500, .. }),
      "got {err:?}"
   );

   let dir = TempDir::new("download-cleanup");
   let dest = dir.path().join("partial.bin");
   let err = http::download_to_file(
      &server.url("/big"),
      &[],
      &dest,
      128,
      2_000,
      2,
      &mut |_, _| {},
   )
   .expect_err("oversized download must fail");
   assert!(matches!(err, Error::TooLarge { .. }), "got {err:?}");
   assert!(
      !dest.exists(),
      "failed download must not leave partial file"
   );
}

#[test]
fn apply_update_handles_symlinks_and_stale_pid_temp_files() {
   let dir = TempDir::new("symlink-stale");
   let real_dir = dir.path().join("real");
   let link_dir = dir.path().join("bin");
   fs::create_dir_all(&real_dir).expect("create real dir");
   fs::create_dir_all(&link_dir).expect("create link dir");

   let real_target = real_dir.join("Nyaacord-x86_64.AppImage");
   let symlink_target = link_dir.join("nyaacord");
   let v1 = build_fake_appimage(b"symlink-v1", 0);
   let v2 = build_fake_appimage(b"symlink-v2", 0);
   let v2_sha = sha256_bytes(&v2);
   fs::write(&real_target, &v1).expect("write v1");
   std::os::unix::fs::symlink(&real_target, &symlink_target).expect("create symlink");

   // Pre-populate stale PID-named files to verify unique naming ignores them.
   let pid = std::process::id();
   let stale_new = real_dir.join(format!(".Nyaacord-x86_64.AppImage.new.{pid}"));
   let stale_prev = real_dir.join(format!(".Nyaacord-x86_64.AppImage.prev.{pid}"));
   fs::write(&stale_new, b"stale").expect("write stale new");
   fs::write(&stale_prev, b"stale").expect("write stale prev");

   let mut routes = HashMap::new();
   routes.insert(
      "/v2".to_owned(),
      MockResponse::ok("application/octet-stream", v2.clone()),
   );
   let server = MockServer::start(routes);
   let release = ReleaseInfo {
      version: "0.2.0".to_owned(),
      tag_name: "v0.2.0".to_owned(),
      asset_name: "Nyaacord-x86_64.AppImage".to_owned(),
      download_url: server.url("/v2"),
      size: v2.len() as u64,
      sha256: v2_sha,
      release_url: "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0".to_owned(),
   };
   let cfg = test_config(server.url("/unused"));

   let outcome = apply_update(&release, &symlink_target, &cfg, &mut |_, _| {})
      .expect("symlink update succeeds");
   assert_eq!(
      outcome.target,
      fs::canonicalize(&real_target).expect("canon real target")
   );
   assert!(
      fs::symlink_metadata(&symlink_target)
         .expect("symlink metadata")
         .file_type()
         .is_symlink(),
      "symlink must be preserved"
   );
   assert_eq!(
      fs::read(&real_target).expect("read upgraded real target"),
      v2
   );
   assert_eq!(fs::read(&stale_new).expect("stale untouched"), b"stale");
   assert_eq!(fs::read(&stale_prev).expect("stale untouched"), b"stale");

   rollback(&symlink_target).expect("rollback via symlink");
   assert_eq!(
      fs::read(&real_target).expect("read restored real target"),
      v1
   );
}

#[test]
fn relaunch_watchdog_rolls_back_crashing_target() {
   let dir = TempDir::new("watchdog");
   let target = dir.path().join("Nyaacord-x86_64.AppImage");
   let prev = previous_path(&target);
   let v1_good = build_fake_appimage(b"watchdog-good-v1", 0);
   let v2_crash = build_fake_appimage(b"watchdog-crash-v2", 1);

   fs::write(&prev, &v1_good).expect("write good prev");
   fs::set_permissions(&prev, Permissions::from_mode(0o755)).expect("chmod prev");
   fs::write(&target, &v2_crash).expect("write crashing target");
   fs::set_permissions(&target, Permissions::from_mode(0o755)).expect("chmod target");

   let mut waiter = Command::new("/bin/sh")
      .args(["-c", "sleep 0.1"])
      .spawn()
      .expect("spawn dummy waiter");
   let wait_pid = waiter.id();

   phoenix_updater::spawn_relaunch_watchdog(&target, Some(&prev), wait_pid)
      .expect("spawn watchdog");
   let _ = waiter.wait();

   let mut rolled_back = false;
   for _ in 0..50 {
      if fs::read(&target).is_ok_and(|bytes| bytes == v1_good) {
         rolled_back = true;
         break;
      }
      thread::sleep(Duration::from_millis(100));
   }
   assert!(
      rolled_back,
      "watchdog did not restore previous AppImage after crash"
   );
}

#[test]
fn cli_check_update_verify_and_rollback_end_to_end() {
   let bin = env!("CARGO_BIN_EXE_nyaacord-updater");
   let dir = TempDir::new("cli");
   let target = dir.path().join("Nyaacord-x86_64.AppImage");
   let v1 = build_fake_appimage(b"cli-v1", 0);
   let v2 = build_fake_appimage(b"cli-v2", 0);
   let v2_sha = sha256_bytes(&v2);
   fs::write(&target, &v1).expect("write v1");

   let mut asset_routes = HashMap::new();
   asset_routes.insert(
      "/v2.AppImage".to_owned(),
      MockResponse::ok("application/octet-stream", v2.clone()),
   );
   let asset_server = MockServer::start(asset_routes);

   let release_json = serde_json::json!({
      "tag_name": "v0.2.0",
      "draft": false,
      "html_url": "https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.0",
      "assets": [
         {
            "name": "Nyaacord-x86_64.AppImage",
            "browser_download_url": asset_server.url("/v2.AppImage"),
            "size": v2.len(),
            "digest": format!("sha256:{v2_sha}")
         }
      ]
   });
   let mut api_routes = HashMap::new();
   api_routes.insert(
      "/latest".to_owned(),
      MockResponse::ok("application/json", release_json.to_string().into_bytes()),
   );
   let api_server = MockServer::start(api_routes);

   let check_out = Command::new(bin)
      .args([
         "check",
         "--current",
         "0.1.0",
         "--api",
         &api_server.url("/latest"),
         "--json",
      ])
      .output()
      .expect("run check");
   assert!(check_out.status.success(), "{check_out:?}");
   let check_json: serde_json::Value =
      serde_json::from_slice(&check_out.stdout).expect("parse check json");
   assert_eq!(check_json["status"], "available");
   assert_eq!(check_json["version"], "0.2.0");

   let update_out = Command::new(bin)
      .args([
         "update",
         "--current",
         "0.1.0",
         "--target",
         target.to_str().expect("utf8 path"),
         "--api",
         &api_server.url("/latest"),
         "--quiet",
      ])
      .output()
      .expect("run update");
   assert!(update_out.status.success(), "{update_out:?}");
   assert_eq!(fs::read(&target).expect("read v2"), v2);

   let verify_out = Command::new(bin)
      .args([
         "verify",
         "--file",
         target.to_str().expect("utf8 path"),
         "--expected-size",
         &v2.len().to_string(),
         "--expected-sha256",
         &v2_sha,
      ])
      .output()
      .expect("run verify");
   assert!(verify_out.status.success(), "{verify_out:?}");

   let rollback_out = Command::new(bin)
      .args(["rollback", "--target", target.to_str().expect("utf8 path")])
      .output()
      .expect("run rollback");
   assert!(rollback_out.status.success(), "{rollback_out:?}");
   assert_eq!(fs::read(&target).expect("read restored v1"), v1);

   // Verify bare relative filename works without searching PATH.
   let rel_verify = Command::new(bin)
      .current_dir(dir.path())
      .args(["verify", "--file", "Nyaacord-x86_64.AppImage"])
      .output()
      .expect("run relative verify");
   assert!(rel_verify.status.success(), "{rel_verify:?}");
}
