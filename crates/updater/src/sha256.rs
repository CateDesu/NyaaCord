//! Streaming FIPS 180 4 SHA 256 implementation.

use std::{fmt::Write, fs::File, io::Read, path::Path};

use crate::Error;

const INIT: [u32; 8] = [
   0x6A09_E667,
   0xBB67_AE85,
   0x3C6E_F372,
   0xA54F_F53A,
   0x510E_527F,
   0x9B05_688C,
   0x1F83_D9AB,
   0x5BE0_CD19,
];

const K: [u32; 64] = [
   0x428A_2F98,
   0x7137_4491,
   0xB5C0_FBCF,
   0xE9B5_DBA5,
   0x3956_C25B,
   0x59F1_11F1,
   0x923F_82A4,
   0xAB1C_5ED5,
   0xD807_AA98,
   0x1283_5B01,
   0x2431_85BE,
   0x550C_7DC3,
   0x72BE_5D74,
   0x80DE_B1FE,
   0x9BDC_06A7,
   0xC19B_F174,
   0xE49B_69C1,
   0xEFBE_4786,
   0x0FC1_9DC6,
   0x240C_A1CC,
   0x2DE9_2C6F,
   0x4A74_84AA,
   0x5CB0_A9DC,
   0x76F9_88DA,
   0x983E_5152,
   0xA831_C66D,
   0xB003_27C8,
   0xBF59_7FC7,
   0xC6E0_0BF3,
   0xD5A7_9147,
   0x06CA_6351,
   0x1429_2967,
   0x27B7_0A85,
   0x2E1B_2138,
   0x4D2C_6DFC,
   0x5338_0D13,
   0x650A_7354,
   0x766A_0ABB,
   0x81C2_C92E,
   0x9272_2C85,
   0xA2BF_E8A1,
   0xA81A_664B,
   0xC24B_8B70,
   0xC76C_51A3,
   0xD192_E819,
   0xD699_0624,
   0xF40E_3585,
   0x106A_A070,
   0x19A4_C116,
   0x1E37_6C08,
   0x2748_774C,
   0x34B0_BCB5,
   0x391C_0CB3,
   0x4ED8_AA4A,
   0x5B9C_CA4F,
   0x682E_6FF3,
   0x748F_82EE,
   0x78A5_636F,
   0x84C8_7814,
   0x8CC7_0208,
   0x90BE_FFFA,
   0xA450_6CEB,
   0xBEF9_A3F7,
   0xC671_78F2,
];

#[derive(Clone)]
pub struct Hasher {
   state: [u32; 8],
   block: [u8; 64],
   block_len: usize,
   total_bytes: u64,
}

impl Default for Hasher {
   fn default() -> Self {
      Self::new()
   }
}

impl Hasher {
   #[must_use]
   pub const fn new() -> Self {
      Self {
         state: INIT,
         block: [0; 64],
         block_len: 0,
         total_bytes: 0,
      }
   }

   pub fn update(&mut self, mut data: &[u8]) {
      self.total_bytes = self.total_bytes.wrapping_add(data.len() as u64);

      if self.block_len > 0 {
         let need = 64 - self.block_len;
         let take = need.min(data.len());
         self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
         self.block_len += take;
         data = &data[take..];
         if self.block_len == 64 {
            let block = self.block;
            compress(&mut self.state, &block);
            self.block_len = 0;
         }
      }

      let (chunks, rem) = data.as_chunks::<64>();
      for block in chunks {
         compress(&mut self.state, block);
      }

      if !rem.is_empty() {
         self.block[..rem.len()].copy_from_slice(rem);
         self.block_len = rem.len();
      }
   }

   #[must_use]
   pub fn finalize_bytes(mut self) -> [u8; 32] {
      let bit_len = self.total_bytes.wrapping_mul(8);

      self.block[self.block_len] = 0x80;
      self.block_len += 1;

      if self.block_len > 56 {
         self.block[self.block_len..64].fill(0);
         let block = self.block;
         compress(&mut self.state, &block);
         self.block_len = 0;
      }

      self.block[self.block_len..56].fill(0);
      self.block[56..64].copy_from_slice(&bit_len.to_be_bytes());
      let block = self.block;
      compress(&mut self.state, &block);

      let mut out = [0_u8; 32];
      for (i, word) in self.state.iter().enumerate() {
         out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
      }
      out
   }

   #[must_use]
   pub fn finalize_hex(self) -> String {
      let digest = self.finalize_bytes();
      let mut hex = String::with_capacity(64);
      for byte in digest {
         let _ = write!(hex, "{byte:02x}");
      }
      hex
   }
}

fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
   let mut schedule = [0_u32; 64];
   for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
      schedule[i] = u32::from_be_bytes(*chunk);
   }
   for i in 16..64 {
      let s0 = schedule[i - 15].rotate_right(7)
         ^ schedule[i - 15].rotate_right(18)
         ^ (schedule[i - 15] >> 3);
      let s1 = schedule[i - 2].rotate_right(17)
         ^ schedule[i - 2].rotate_right(19)
         ^ (schedule[i - 2] >> 10);
      schedule[i] = schedule[i - 16]
         .wrapping_add(s0)
         .wrapping_add(schedule[i - 7])
         .wrapping_add(s1);
   }

   let [
      mut h0,
      mut h1,
      mut h2,
      mut h3,
      mut h4,
      mut h5,
      mut h6,
      mut h7,
   ] = *state;

   for i in 0..64 {
      let s1 = h4.rotate_right(6) ^ h4.rotate_right(11) ^ h4.rotate_right(25);
      let ch = (h4 & h5) ^ ((!h4) & h6);
      let temp1 = h7
         .wrapping_add(s1)
         .wrapping_add(ch)
         .wrapping_add(K[i])
         .wrapping_add(schedule[i]);
      let s0 = h0.rotate_right(2) ^ h0.rotate_right(13) ^ h0.rotate_right(22);
      let maj = (h0 & h1) ^ (h0 & h2) ^ (h1 & h2);
      let temp2 = s0.wrapping_add(maj);

      h7 = h6;
      h6 = h5;
      h5 = h4;
      h4 = h3.wrapping_add(temp1);
      h3 = h2;
      h2 = h1;
      h1 = h0;
      h0 = temp1.wrapping_add(temp2);
   }

   state[0] = state[0].wrapping_add(h0);
   state[1] = state[1].wrapping_add(h1);
   state[2] = state[2].wrapping_add(h2);
   state[3] = state[3].wrapping_add(h3);
   state[4] = state[4].wrapping_add(h4);
   state[5] = state[5].wrapping_add(h5);
   state[6] = state[6].wrapping_add(h6);
   state[7] = state[7].wrapping_add(h7);
}

/// Validates and lowercases a 64 character hexadecimal SHA 256 digest.
#[must_use]
pub fn normalize_hex(raw: &str) -> Option<String> {
   let trimmed = raw.trim();
   if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
      return None;
   }
   Some(trimmed.to_ascii_lowercase())
}

/// Computes the lowercase hexadecimal SHA 256 digest of a file.
pub fn sha256_file(path: &Path) -> Result<String, Error> {
   let mut file = File::open(path).map_err(|source| Error::Io {
      path: path.to_path_buf(),
      source,
   })?;
   let mut hasher = Hasher::new();
   let mut buf = vec![0_u8; 64 * 1024];
   loop {
      let read = file.read(&mut buf).map_err(|source| Error::Io {
         path: path.to_path_buf(),
         source,
      })?;
      if read == 0 {
         break;
      }
      hasher.update(&buf[..read]);
   }
   Ok(hasher.finalize_hex())
}

#[cfg(test)]
mod tests {
   use super::*;

   fn hash_hex(input: &[u8]) -> String {
      let mut hasher = Hasher::new();
      hasher.update(input);
      hasher.finalize_hex()
   }

   #[test]
   fn nist_vectors() {
      assert_eq!(
         hash_hex(b""),
         "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
      );
      assert_eq!(
         hash_hex(b"abc"),
         "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
      );
      assert_eq!(
         hash_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
         "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
      );
      let million_a = vec![b'a'; 1_000_000];
      assert_eq!(
         hash_hex(&million_a),
         "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
      );
   }

   #[test]
   fn streaming_matches_single_shot() {
      let data: Vec<u8> = (0..10_000)
         .map(|i| u8::try_from(i % 251).unwrap_or(0))
         .collect();
      let expected = hash_hex(&data);
      for step in [1, 7, 55, 56, 63, 64, 65, 137] {
         let mut hasher = Hasher::new();
         for chunk in data.chunks(step) {
            hasher.update(chunk);
         }
         assert_eq!(hasher.finalize_hex(), expected, "chunk size {step}");
      }
   }

   #[test]
   fn validates_hex_digests() {
      assert_eq!(
         normalize_hex("BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"),
         Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned())
      );
      assert!(normalize_hex("short").is_none());
      assert!(
         normalize_hex("zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
            .is_none()
      );
   }
}
