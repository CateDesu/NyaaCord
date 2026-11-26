//! Minimal pure-Rust PNG and RFC 1951 deflate decoder for the bundled tray icon.

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_BITS: usize = 15;
const MAX_DECODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIMENSION: u32 = 512;

const LEN_BASE: [u16; 29] = [
   3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
   163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
   0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
   1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049,
   3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
   0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];
const CL_ORDER: [usize; 19] = [
   16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Decoded 8-bit straight RGBA icon image.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "used by the Windows tray icon and host tests")
)]
pub struct DecodedIcon {
   pub width: u32,
   pub height: u32,
   pub rgba: Vec<u8>,
}

impl DecodedIcon {
   /// Converts RGBA pixels into Win32 little-endian `0xAARRGGBB` words.
   #[cfg_attr(
      not(any(windows, test)),
      allow(dead_code, reason = "used by the Windows tray icon and host tests")
   )]
   pub fn to_bgra_u32(&self) -> Vec<u32> {
      self
         .rgba
         .as_chunks::<4>()
         .0
         .iter()
         .map(|&[r, g, b, a]| {
            if a == 0 {
               0
            } else {
               (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
            }
         })
         .collect()
   }
}

struct BitReader<'a> {
   data: &'a [u8],
   pos: usize,
   bit_buf: u64,
   bit_count: u32,
}

impl<'a> BitReader<'a> {
   const fn new(data: &'a [u8]) -> Self {
      Self {
         data,
         pos: 0,
         bit_buf: 0,
         bit_count: 0,
      }
   }

   fn read_bits(&mut self, count: u32) -> Option<u32> {
      while self.bit_count < count {
         let byte = *self.data.get(self.pos)?;
         self.pos += 1;
         self.bit_buf |= u64::from(byte) << self.bit_count;
         self.bit_count += 8;
      }
      let mask = if count == 32 {
         u64::from(u32::MAX)
      } else {
         (1_u64 << count) - 1
      };
      #[expect(clippy::cast_possible_truncation, reason = "count <= 32")]
      let value = (self.bit_buf & mask) as u32;
      self.bit_buf >>= count;
      self.bit_count -= count;
      Some(value)
   }

   const fn align_byte(&mut self) {
      let rem = self.bit_count & 7;
      self.bit_buf >>= rem;
      self.bit_count -= rem;
   }

   fn read_aligned_u16(&mut self) -> Option<u16> {
      let lo = u16::try_from(self.read_bits(8)?).ok()?;
      let hi = u16::try_from(self.read_bits(8)?).ok()?;
      Some(lo | (hi << 8))
   }
}

struct HuffmanTree {
   count: [u16; MAX_BITS + 1],
   symbols: Vec<u16>,
}

impl HuffmanTree {
   fn from_lengths(lengths: &[u8]) -> Option<Self> {
      let mut count = [0_u16; MAX_BITS + 1];
      for &len in lengths {
         let len = usize::from(len);
         if len > MAX_BITS {
            return None;
         }
         count[len] = count[len].checked_add(1)?;
      }

      let mut left: i32 = 1;
      for &c in &count[1..=MAX_BITS] {
         left = (left << 1) - i32::from(c);
         if left < 0 {
            return None;
         }
      }

      let mut offs = [0_usize; MAX_BITS + 1];
      for len in 1..MAX_BITS {
         offs[len + 1] = offs[len] + usize::from(count[len]);
      }

      let total = offs[MAX_BITS] + usize::from(count[MAX_BITS]);
      let mut symbols = vec![0_u16; total];
      for (sym, &len) in lengths.iter().enumerate() {
         let len = usize::from(len);
         if len != 0 {
            let slot = offs[len];
            symbols[slot] = u16::try_from(sym).ok()?;
            offs[len] += 1;
         }
      }
      Some(Self { count, symbols })
   }

   fn fixed_litlen() -> Option<Self> {
      let mut lengths = [0_u8; 288];
      lengths[..144].fill(8);
      lengths[144..256].fill(9);
      lengths[256..280].fill(7);
      lengths[280..288].fill(8);
      Self::from_lengths(&lengths)
   }

   fn fixed_dist() -> Option<Self> {
      Self::from_lengths(&[5_u8; 32])
   }

   fn decode(&self, bits: &mut BitReader<'_>) -> Option<u16> {
      let mut code: i32 = 0;
      let mut first: i32 = 0;
      let mut index: i32 = 0;
      for len in 1..=MAX_BITS {
         code |= i32::try_from(bits.read_bits(1)?).ok()?;
         let c = i32::from(self.count[len]);
         if code - c < first {
            let idx = usize::try_from(index + (code - first)).ok()?;
            return self.symbols.get(idx).copied();
         }
         index += c;
         first = (first + c) << 1;
         code <<= 1;
      }
      None
   }
}

fn decode_dynamic_trees(bits: &mut BitReader<'_>) -> Option<(HuffmanTree, HuffmanTree)> {
   let hlit = (bits.read_bits(5)? as usize) + 257;
   let hdist = (bits.read_bits(5)? as usize) + 1;
   let hclen = (bits.read_bits(4)? as usize) + 4;
   if hlit > 286 || hdist > 30 {
      return None;
   }

   let mut cl_lengths = [0_u8; 19];
   for &idx in CL_ORDER.iter().take(hclen) {
      cl_lengths[idx] = u8::try_from(bits.read_bits(3)?).ok()?;
   }
   let cl_tree = HuffmanTree::from_lengths(&cl_lengths)?;

   let total = hlit + hdist;
   let mut lengths = Vec::with_capacity(total);
   while lengths.len() < total {
      let sym = cl_tree.decode(bits)?;
      match sym {
         0..=15 => lengths.push(u8::try_from(sym).ok()?),
         16 => {
            let prev = *lengths.last()?;
            let repeat = (bits.read_bits(2)? as usize) + 3;
            if lengths.len() + repeat > total {
               return None;
            }
            lengths.resize(lengths.len() + repeat, prev);
         },
         17 => {
            let repeat = (bits.read_bits(3)? as usize) + 3;
            if lengths.len() + repeat > total {
               return None;
            }
            lengths.resize(lengths.len() + repeat, 0);
         },
         18 => {
            let repeat = (bits.read_bits(7)? as usize) + 11;
            if lengths.len() + repeat > total {
               return None;
            }
            lengths.resize(lengths.len() + repeat, 0);
         },
         _ => return None,
      }
   }

   let litlen = HuffmanTree::from_lengths(&lengths[..hlit])?;
   let dist = HuffmanTree::from_lengths(&lengths[hlit..])?;
   Some((litlen, dist))
}

fn inflate_huffman_block(
   bits: &mut BitReader<'_>,
   litlen: &HuffmanTree,
   dist: &HuffmanTree,
   out: &mut Vec<u8>,
) -> Option<()> {
   loop {
      let sym = litlen.decode(bits)?;
      match sym {
         0..=255 => {
            if out.len() >= MAX_DECODED_BYTES {
               return None;
            }
            out.push(u8::try_from(sym).ok()?);
         },
         256 => return Some(()),
         257..=285 => {
            let len_idx = usize::from(sym - 257);
            let length = usize::from(LEN_BASE[len_idx])
               + (bits.read_bits(u32::from(LEN_EXTRA[len_idx]))? as usize);
            let dist_sym = usize::from(dist.decode(bits)?);
            if dist_sym >= DIST_BASE.len() {
               return None;
            }
            let distance = usize::from(DIST_BASE[dist_sym])
               + (bits.read_bits(u32::from(DIST_EXTRA[dist_sym]))? as usize);
            if distance == 0 || distance > out.len() || out.len() + length > MAX_DECODED_BYTES {
               return None;
            }
            for _ in 0..length {
               let byte = out[out.len() - distance];
               out.push(byte);
            }
         },
         _ => return None,
      }
   }
}

fn decompress_zlib(data: &[u8]) -> Option<Vec<u8>> {
   if data.len() < 6 {
      return None;
   }
   let cmf = data[0];
   let flg = data[1];
   if (cmf & 0x0F) != 8 || (flg & 0x20) != 0 {
      return None;
   }
   if ((u16::from(cmf) << 8) | u16::from(flg)) % 31 != 0 {
      return None;
   }

   let mut bits = BitReader::new(&data[2..]);
   let mut out = Vec::new();
   loop {
      let bfinal = bits.read_bits(1)?;
      let btype = bits.read_bits(2)?;
      match btype {
         0 => {
            bits.align_byte();
            let len = bits.read_aligned_u16()?;
            let nlen = bits.read_aligned_u16()?;
            if len != !nlen || out.len() + usize::from(len) > MAX_DECODED_BYTES {
               return None;
            }
            for _ in 0..len {
               out.push(u8::try_from(bits.read_bits(8)?).ok()?);
            }
         },
         1 => {
            let litlen = HuffmanTree::fixed_litlen()?;
            let dist = HuffmanTree::fixed_dist()?;
            inflate_huffman_block(&mut bits, &litlen, &dist, &mut out)?;
         },
         2 => {
            let (litlen, dist) = decode_dynamic_trees(&mut bits)?;
            inflate_huffman_block(&mut bits, &litlen, &dist, &mut out)?;
         },
         _ => return None,
      }
      if bfinal == 1 {
         break;
      }
   }
   Some(out)
}

const fn paeth(a: u8, b: u8, c: u8) -> u8 {
   let a_i = a as i16;
   let b_i = b as i16;
   let c_i = c as i16;
   let p = a_i + b_i - c_i;
   let pa = (p - a_i).abs();
   let pb = (p - b_i).abs();
   let pc = (p - c_i).abs();
   if pa <= pb && pa <= pc {
      a
   } else if pb <= pc {
      b
   } else {
      c
   }
}

fn unfilter_scanlines(raw: &[u8], width: usize, height: usize, bpp: usize) -> Option<Vec<u8>> {
   let stride = width.checked_mul(bpp)?;
   let expected = height.checked_mul(stride.checked_add(1)?)?;
   if raw.len() < expected {
      return None;
   }

   let mut out = vec![0_u8; height.checked_mul(stride)?];
   for y in 0..height {
      let src_row = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
      let filter = src_row[0];
      let scan = &src_row[1..];
      let row_start = y * stride;

      for i in 0..stride {
         let left = if i >= bpp {
            out[row_start + i - bpp]
         } else {
            0
         };
         let up = if y > 0 { out[(y - 1) * stride + i] } else { 0 };
         let up_left = if y > 0 && i >= bpp {
            out[(y - 1) * stride + i - bpp]
         } else {
            0
         };
         let recon = match filter {
            0 => scan[i],
            1 => scan[i].wrapping_add(left),
            2 => scan[i].wrapping_add(up),
            3 => scan[i].wrapping_add(left.midpoint(up)),
            4 => scan[i].wrapping_add(paeth(left, up, up_left)),
            _ => return None,
         };
         out[row_start + i] = recon;
      }
   }
   Some(out)
}

fn expand_to_rgba(
   color_type: u8,
   count: usize,
   pixels: Vec<u8>,
   plte: Option<&[u8]>,
   trns: Option<&[u8]>,
) -> Option<Vec<u8>> {
   match color_type {
      6 => Some(pixels),
      2 => {
         let mut out = Vec::with_capacity(count * 4);
         for &[r, g, b] in pixels.as_chunks::<3>().0 {
            out.extend_from_slice(&[r, g, b, 255]);
         }
         Some(out)
      },
      4 => {
         let mut out = Vec::with_capacity(count * 4);
         for &[g, a] in pixels.as_chunks::<2>().0 {
            out.extend_from_slice(&[g, g, g, a]);
         }
         Some(out)
      },
      0 => {
         let mut out = Vec::with_capacity(count * 4);
         for &g in &pixels {
            out.extend_from_slice(&[g, g, g, 255]);
         }
         Some(out)
      },
      3 => {
         let palette = plte?;
         let alpha = trns.unwrap_or(&[]);
         let mut out = Vec::with_capacity(count * 4);
         for &idx in &pixels {
            let p = usize::from(idx) * 3;
            let rgb = palette.get(p..p + 3)?;
            let a = alpha.get(usize::from(idx)).copied().unwrap_or(255);
            out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
         }
         Some(out)
      },
      _ => None,
   }
}

/// Decodes a non-interlaced 8-bit PNG into straight RGBA pixels.
#[cfg_attr(
   not(any(windows, test)),
   allow(dead_code, reason = "used by the Windows tray icon and host tests")
)]
pub fn decode_png_rgba(data: &[u8]) -> Option<DecodedIcon> {
   if data.len() < 8 || &data[..8] != PNG_SIGNATURE {
      return None;
   }

   let mut pos = 8;
   let mut width: u32 = 0;
   let mut height: u32 = 0;
   let mut color_type: u8 = 0;
   let mut saw_ihdr = false;
   let mut idat = Vec::new();
   let mut plte: Option<&[u8]> = None;
   let mut trns: Option<&[u8]> = None;

   while pos + 12 <= data.len() {
      let len =
         u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
      let chunk_type = &data[pos + 4..pos + 8];
      let chunk_start = pos + 8;
      let chunk_end = chunk_start.checked_add(len)?;
      let next_pos = chunk_end.checked_add(4)?;
      if next_pos > data.len() {
         return None;
      }
      let chunk_data = &data[chunk_start..chunk_end];
      match chunk_type {
         b"IHDR" => {
            if chunk_data.len() != 13 {
               return None;
            }
            width =
               u32::from_be_bytes([chunk_data[0], chunk_data[1], chunk_data[2], chunk_data[3]]);
            height =
               u32::from_be_bytes([chunk_data[4], chunk_data[5], chunk_data[6], chunk_data[7]]);
            let bit_depth = chunk_data[8];
            color_type = chunk_data[9];
            if width == 0
               || height == 0
               || width > MAX_DIMENSION
               || height > MAX_DIMENSION
               || bit_depth != 8
               || chunk_data[10] != 0
               || chunk_data[11] != 0
               || chunk_data[12] != 0
            {
               return None;
            }
            saw_ihdr = true;
         },
         b"IDAT" => idat.extend_from_slice(chunk_data),
         b"PLTE" => plte = Some(chunk_data),
         b"tRNS" => trns = Some(chunk_data),
         b"IEND" => break,
         _ => {},
      }
      pos = next_pos;
   }

   if !saw_ihdr || idat.is_empty() {
      return None;
   }

   let bpp = match color_type {
      0 | 3 => 1,
      2 => 3,
      4 => 2,
      6 => 4,
      _ => return None,
   };
   let raw = decompress_zlib(&idat)?;
   let pixels = unfilter_scanlines(&raw, width as usize, height as usize, bpp)?;
   let count = (width as usize).checked_mul(height as usize)?;
   let rgba = expand_to_rgba(color_type, count, pixels, plte, trns)?;

   Some(DecodedIcon {
      width,
      height,
      rgba,
   })
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn decodes_bundled_nyaacord_png_icon() {
      let decoded = decode_png_rgba(crate::ICON_PNG).expect("valid bundled PNG icon");
      assert_eq!(decoded.width, 64);
      assert_eq!(decoded.height, 64);
      assert_eq!(decoded.rgba.len(), 64 * 64 * 4);

      let bgra = decoded.to_bgra_u32();
      assert_eq!(bgra.len(), 64 * 64);
      // Icon has both transparent margin pixels and opaque fox body pixels.
      assert!(bgra.contains(&0));
      assert!(bgra.iter().any(|&px| (px >> 24) == 0xFF));
   }

   #[test]
   fn rejects_invalid_png_input() {
      assert!(decode_png_rgba(b"not a png").is_none());
      assert!(decode_png_rgba(PNG_SIGNATURE).is_none());
   }
}
