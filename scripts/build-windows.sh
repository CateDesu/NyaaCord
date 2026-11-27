#!/usr/bin/env bash
# Cross-compiles a self-contained NyaaCord Windows x86-64 executable with static MSVC CRT.
set -euo pipefail

CALLER_PWD=$PWD
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TARGET="x86_64-pc-windows-msvc"

DEFAULT_PLUGINS=(
   clear-urls
   dearrow
   fakenitro
   fix-embeds
   global-badges
   message-latency
   message-logger
   no-profile-themes
   no-reply-mention
   silent-typing
   spoiler-mode
   text-replace
   translate
   view-raw
)

usage() {
   cat <<'EOF'
Usage: scripts/build-windows.sh [options]

Options:
  --version <ver>       Version to embed (default: NYAACORD_VERSION or Cargo.toml workspace version)
  --client <path>       Discord Phoenix Windows setup exe or directory containing Discord.exe
                        (default: DISCORD_PHOENIX_WINDOWS_SETUP or auto-detect)
  --no-client           Build a thin launcher without bundling Discord Phoenix
  --out <path>          Output executable path (default: dist/NyaaCord-x86_64.exe)
  --skip-plugins        Skip esbuild plugin bundling (embed an empty plugin map)
  -h, --help            Show this help message
EOF
}

parse_workspace_version() {
   python3 - "$ROOT/Cargo.toml" <<'PY'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
m = re.search(r"(?ms)^\[workspace\.package\]\s*(.*?)(?:^\[|\Z)", text)
if m:
    vm = re.search(r'(?m)^\s*version\s*=\s*"([^"]+)"', m.group(1))
    if vm:
        print(vm.group(1))
        sys.exit(0)
print("0.1.0")
PY
}

resolve_path() {
   local p=$1
   case "$p" in
      /*) printf '%s\n' "$p" ;;
      *) printf '%s/%s\n' "$CALLER_PWD" "$p" ;;
   esac
}

normalize_version() {
   local raw=$1
   case "$raw" in
      *$'\n'*|*$'\r'*)
         echo "Invalid --version: must be a single line" >&2
         return 1
         ;;
   esac
   raw=$(printf '%s' "$raw" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//;s/^[vV]//')
   if [ -z "$raw" ]; then
      echo "Invalid --version: empty version" >&2
      return 1
   fi
   printf '%s\n' "$raw"
}

find_esbuild() {
   if [ -n "${ESBUILD:-}" ] && [ -x "$ESBUILD" ]; then
      printf '%s\n' "$ESBUILD"
      return 0
   fi
   if command -v esbuild >/dev/null 2>&1; then
      command -v esbuild
      return 0
   fi
   local cached="$ROOT/.cache/esbuild/node_modules/.bin/esbuild"
   if [ -x "$cached" ]; then
      printf '%s\n' "$cached"
      return 0
   fi
   if ! command -v npm >/dev/null 2>&1; then
      echo "esbuild not found and npm is unavailable to install it" >&2
      return 1
   fi
   mkdir -p "$ROOT/.cache/esbuild"
   npm install --no-save --no-package-lock --no-audit --no-fund \
      --prefix "$ROOT/.cache/esbuild" esbuild@0.25.12 >&2
   if [ -x "$cached" ]; then
      printf '%s\n' "$cached"
      return 0
   fi
   return 1
}

discover_client() {
   local cand
   for cand in \
      "$ROOT/DiscordPhoenixSetup.exe" \
      "$HOME/Downloads/DiscordPhoenixSetup.exe" \
      "$HOME/Applications/DiscordPhoenixSetup.exe"; do
      if [ -f "$cand" ]; then
         printf '%s\n' "$cand"
         return 0
      fi
   done
}

version=${NYAACORD_VERSION:-}
client=${DISCORD_PHOENIX_WINDOWS_SETUP:-}
client_explicit=0
no_client=0
out="$ROOT/dist/NyaaCord-x86_64.exe"
skip_plugins=0

while [ "$#" -gt 0 ]; do
   case "$1" in
      --version)
         [ "$#" -ge 2 ] || { echo "--version requires a value" >&2; exit 1; }
         version=$2
         shift 2
         ;;
      --client)
         [ "$#" -ge 2 ] || { echo "--client requires a value" >&2; exit 1; }
         client=$(resolve_path "$2")
         client_explicit=1
         shift 2
         ;;
      --no-client)
         no_client=1
         shift
         ;;
      --out)
         [ "$#" -ge 2 ] || { echo "--out requires a value" >&2; exit 1; }
         out=$(resolve_path "$2")
         shift 2
         ;;
      --skip-plugins)
         skip_plugins=1
         shift
         ;;
      -h|--help)
         usage
         exit 0
         ;;
      *)
         echo "Unknown option: $1" >&2
         usage >&2
         exit 1
         ;;
   esac
done

if [ "$client_explicit" = 1 ] && [ "$no_client" = 1 ]; then
   echo "Cannot combine --client and --no-client" >&2
   exit 1
fi
if [ "$no_client" = 1 ]; then
   client=""
fi

for tool in python3 cargo llvm-rc; do
   if ! command -v "$tool" >/dev/null 2>&1; then
      echo "$tool is required" >&2
      exit 1
   fi
done

if ! cargo xwin --version >/dev/null 2>&1; then
   echo "cargo-xwin is required" >&2
   exit 1
fi

if [ "$no_client" = 0 ]; then
   if [ -z "$client" ]; then
      client=$(discover_client || true)
   fi
   if [ -z "$client" ] || { [ ! -f "$client" ] && [ ! -d "$client" ]; }; then
      echo "Discord Phoenix Windows client not found. Pass --client <path>, set DISCORD_PHOENIX_WINDOWS_SETUP, or pass --no-client." >&2
      exit 1
   fi
   client=$(realpath "$client")
   if [ -f "$client" ] && ! command -v 7z >/dev/null 2>&1; then
      echo "7z is required to extract $client" >&2
      exit 1
   fi
fi

if [ -z "$version" ]; then
   version=$(parse_workspace_version)
fi
version=$(normalize_version "$version")

icon_src="$ROOT/web/plugins/nyaacord.png"
[ -f "$icon_src" ] || { echo "Missing $icon_src" >&2; exit 1; }

WORK=$(mktemp -d)
out_tmp=""
cleanup() {
   rm -rf "$WORK"
   if [ -n "$out_tmp" ]; then
      rm -f "$out_tmp"
   fi
}
trap cleanup EXIT

plugins_bundle="$WORK/libplm.plugins.so"
if [ "$skip_plugins" = 1 ]; then
   echo "Skipping plugin bundling (--skip-plugins)..."
   printf '{}' > "$plugins_bundle"
else
   esbuild=$(find_esbuild) || {
      echo "Failed to locate or install esbuild." >&2
      exit 1
   }
   mkdir -p "$WORK/plugins"
   echo "Bundling ${#DEFAULT_PLUGINS[@]} default plugins..."
   for name in "${DEFAULT_PLUGINS[@]}"; do
      entry="$ROOT/plugins/$name/src/index.ts"
      if [ ! -f "$entry" ]; then
         echo "Missing plugin entry point: $entry" >&2
         exit 1
      fi
      header=$(python3 - "$entry" <<'PY'
import sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
out = []
for line in lines:
    if line.startswith("// @"):
        out.append(line)
    else:
        break
sys.stdout.write("\n".join(out))
PY
)
      "$esbuild" "$entry" \
         --bundle \
         --format=iife \
         --target=es2023 \
         --log-level=warning \
         "--banner:js=$header" \
         "--outfile=$WORK/plugins/$name.js"
   done

   python3 - "$WORK/plugins" "$plugins_bundle" "${DEFAULT_PLUGINS[@]}" <<'PY'
import json, os, sys
src_dir, out_path = sys.argv[1], sys.argv[2]
bundle = {}
for name in sorted(sys.argv[3:]):
    with open(os.path.join(src_dir, f"{name}.js"), encoding="utf-8") as fh:
        bundle[name] = fh.read()
with open(out_path, "w", encoding="utf-8") as fh:
    json.dump(bundle, fh, ensure_ascii=False)
PY
fi

ico_path="$WORK/nyaacord.ico"
echo "Generating metadata-free multi-size Windows icon..."
python3 - "$icon_src" "$ico_path" <<'PY'
import struct, sys, zlib

src_path, out_path = sys.argv[1], sys.argv[2]
SIG = b"\x89PNG\r\n\x1a\n"
BMP_SIZES = (16, 24, 32, 48, 64)
PNG_SIZES = (128, 256)

def read_chunks(data: bytes):
    assert data[:8] == SIG, "Source icon is not a PNG"
    pos = 8
    chunks = []
    while pos < len(data):
        length, ctype = struct.unpack(">I4s", data[pos:pos + 8])
        cdata = data[pos + 8:pos + 8 + length]
        crc, = struct.unpack(">I", data[pos + 8 + length:pos + 12 + length])
        assert (zlib.crc32(ctype + cdata) & 0xFFFFFFFF) == crc
        chunks.append((ctype, cdata))
        pos += 12 + length
    return chunks

def decode_rgba(data: bytes):
    chunks = read_chunks(data)
    ihdr = next(d for t, d in chunks if t == b"IHDR")
    w, h, depth, ctype, comp, filt, interlace = struct.unpack(">IIBBBBB", ihdr)
    assert depth == 8 and comp == 0 and filt == 0 and interlace == 0
    raw = zlib.decompress(b"".join(d for t, d in chunks if t == b"IDAT"))
    bpp = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
    stride = w * bpp
    out = bytearray(h * stride)
    idx = 0
    for y in range(h):
        f = raw[idx]
        idx += 1
        row = bytearray(raw[idx:idx + stride])
        idx += stride
        prev = out[(y - 1) * stride:y * stride] if y > 0 else bytes(stride)
        if f == 1:
            for i in range(bpp, stride):
                row[i] = (row[i] + row[i - bpp]) & 0xFF
        elif f == 2:
            for i in range(stride):
                row[i] = (row[i] + prev[i]) & 0xFF
        elif f == 3:
            for i in range(stride):
                left = row[i - bpp] if i >= bpp else 0
                row[i] = (row[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif f == 4:
            for i in range(stride):
                a = row[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                row[i] = (row[i] + pr) & 0xFF
        elif f != 0:
            raise ValueError(f"Unsupported PNG filter {f}")
        out[y * stride:(y + 1) * stride] = row
    if ctype == 6:
        return w, h, bytes(out)
    rgba = bytearray(w * h * 4)
    if ctype == 2:
        for i in range(w * h):
            rgba[i * 4:i * 4 + 3] = out[i * 3:i * 3 + 3]
            rgba[i * 4 + 3] = 255
    elif ctype == 3:
        plte = next(d for t, d in chunks if t == b"PLTE")
        trns = next((d for t, d in chunks if t == b"tRNS"), b"")
        for i, pi in enumerate(out):
            rgba[i * 4:i * 4 + 3] = plte[pi * 3:pi * 3 + 3]
            rgba[i * 4 + 3] = trns[pi] if pi < len(trns) else 255
    elif ctype == 4:
        for i in range(w * h):
            g, a = out[i * 2], out[i * 2 + 1]
            rgba[i * 4:i * 4 + 4] = bytes((g, g, g, a))
    elif ctype == 0:
        for i, g in enumerate(out):
            rgba[i * 4:i * 4 + 4] = bytes((g, g, g, 255))
    return w, h, bytes(rgba)

def scale_nearest(src_w, src_h, rgba, target):
    out = bytearray(target * target * 4)
    for dy in range(target):
        sy = (dy * src_h) // target
        src_row = sy * src_w * 4
        dst_row = dy * target * 4
        for dx in range(target):
            sx = (dx * src_w) // target
            si = src_row + sx * 4
            di = dst_row + dx * 4
            out[di:di + 4] = rgba[si:si + 4]
    return bytes(out)

def chunk(ctype: bytes, cdata: bytes) -> bytes:
    crc = zlib.crc32(ctype + cdata) & 0xFFFFFFFF
    return struct.pack(">I4s", len(cdata), ctype) + cdata + struct.pack(">I", crc)

def encode_png(size: int, rgba: bytes) -> bytes:
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)
        raw.extend(rgba[y * stride:(y + 1) * stride])
    idat = zlib.compress(bytes(raw), level=9)
    return SIG + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")

def encode_ico_dib(size: int, rgba: bytes) -> bytes:
    # Bottom-up 32-bit BGRA DIB + 32-bit-aligned 1bpp AND mask.
    xor_bytes = bytearray()
    mask_stride = ((size + 31) // 32) * 4
    and_bytes = bytearray(mask_stride * size)
    for row_idx, y in enumerate(range(size - 1, -1, -1)):
        src_row = y * size * 4
        for x in range(size):
            r, g, b, a = rgba[src_row + x * 4:src_row + x * 4 + 4]
            if a == 0:
                xor_bytes.extend((0, 0, 0, 0))
                and_bytes[row_idx * mask_stride + (x // 8)] |= 0x80 >> (x % 8)
            else:
                xor_bytes.extend((b, g, r, a))
    header = struct.pack(
        "<IiiHHIIiiII",
        40,
        size,
        size * 2,
        1,
        32,
        0,
        len(xor_bytes) + len(and_bytes),
        0,
        0,
        0,
        0,
    )
    return header + bytes(xor_bytes) + bytes(and_bytes)

src_w, src_h, src_rgba = decode_rgba(open(src_path, "rb").read())
entries = []
for size in BMP_SIZES:
    scaled = scale_nearest(src_w, src_h, src_rgba, size)
    entries.append((size, encode_ico_dib(size, scaled)))
for size in PNG_SIZES:
    scaled = scale_nearest(src_w, src_h, src_rgba, size)
    entries.append((size, encode_png(size, scaled)))

ico = bytearray(struct.pack("<HHH", 0, 1, len(entries)))
offset = 6 + 16 * len(entries)
for size, payload in entries:
    dim = 0 if size == 256 else size
    ico.extend(struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(payload), offset))
    offset += len(payload)
for _, payload in entries:
    ico.extend(payload)

with open(out_path, "wb") as fh:
    fh.write(ico)
PY

crt_flags="-C target-feature=+crt-static"
if [ -n "${CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS:-}" ]; then
   crt_flags="$CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS $crt_flags"
fi
xwin_sdk_ver=${XWIN_SDK_VERSION:-10.0.22621}

echo "Building phoenix-loader (phoenix.dll, version $version)..."
(
   cd "$ROOT"
   XWIN_SDK_VERSION="$xwin_sdk_ver" \
   CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="$crt_flags" \
   NYAACORD_VERSION="$version" \
      cargo xwin build --release --target "$TARGET" -p phoenix-loader
)

dll_bin="$ROOT/target/$TARGET/release/phoenix.dll"
[ -f "$dll_bin" ] || { echo "Missing built DLL: $dll_bin" >&2; exit 1; }

expect_client=1
if [ "$no_client" = 1 ]; then
   expect_client=0
fi

echo "Building phoenix-inject (NyaaCord-x86_64.exe, version $version)..."
(
   cd "$ROOT"
   XWIN_SDK_VERSION="$xwin_sdk_ver" \
   CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="$crt_flags" \
   NYAACORD_VERSION="$version" \
   NYAACORD_EMBED_DLL="$dll_bin" \
   NYAACORD_EMBED_PLUGINS="$plugins_bundle" \
   NYAACORD_ICON_ICO="$ico_path" \
   NYAACORD_EXPECT_CLIENT="$expect_client" \
      cargo xwin build --release --target "$TARGET" -p phoenix-inject
)

exe_bin="$ROOT/target/$TARGET/release/phoenix-inject.exe"
[ -f "$exe_bin" ] || { echo "Missing built executable: $exe_bin" >&2; exit 1; }

echo "Verifying PE32+ headers, icon and version resources, and static CRT imports..."
python3 - "$dll_bin" "$exe_bin" "$version" <<'PY'
import struct, sys

FORBIDDEN_IMPORTS = ("vcruntime140", "msvcp140", "api-ms-win-crt", "ucrtbase")

def parse_win32_version(ver: str):
    core = ver.split("-", 1)[0].split("+", 1)[0]
    parts = [0, 0, 0, 0]
    for idx, seg in enumerate(core.split(".")[:4]):
        try:
            num = int(seg)
        except ValueError:
            num = 0
        if 0 <= num <= 0xFFFF:
            parts[idx] = num
    return tuple(parts)

def read_utf16z(buf: bytes, off: int, limit: int):
    end = off
    while end + 2 <= limit and buf[end:end + 2] != b"\x00\x00":
        end += 2
    return buf[off:end].decode("utf-16le"), end

def parse_version_blob(blob: bytes):
    wlen, vlen, _ = struct.unpack_from("<HHH", blob, 0)
    assert wlen <= len(blob), f"truncated VS_VERSION_INFO ({wlen} > {len(blob)})"
    key, key_end = read_utf16z(blob, 6, wlen)
    assert key == "VS_VERSION_INFO", f"unexpected root version key {key!r}"
    val_off = (key_end + 2 + 3) & ~3
    assert vlen >= 52 and val_off + vlen <= wlen, "missing VS_FIXEDFILEINFO"
    sig, _, fv_ms, fv_ls, pv_ms, pv_ls = struct.unpack_from("<IIIIII", blob, val_off)
    assert sig == 0xFEEF04BD, f"unexpected VS_FIXEDFILEINFO signature {sig:#x}"
    fixed = (
        (fv_ms >> 16, fv_ms & 0xFFFF, fv_ls >> 16, fv_ls & 0xFFFF),
        (pv_ms >> 16, pv_ms & 0xFFFF, pv_ls >> 16, pv_ls & 0xFFFF),
    )

    strings = {}
    pos = (val_off + vlen + 3) & ~3
    while pos + 6 <= wlen:
        c_len, _, _ = struct.unpack_from("<HHH", blob, pos)
        if c_len == 0:
            break
        c_end = pos + c_len
        c_key, c_key_end = read_utf16z(blob, pos + 6, c_end)
        if c_key == "StringFileInfo":
            t_pos = (c_key_end + 2 + 3) & ~3
            while t_pos + 6 <= c_end:
                t_len, _, _ = struct.unpack_from("<HHH", blob, t_pos)
                if t_len == 0:
                    break
                t_end = t_pos + t_len
                _, t_key_end = read_utf16z(blob, t_pos + 6, t_end)
                s_pos = (t_key_end + 2 + 3) & ~3
                while s_pos + 6 <= t_end:
                    s_len, s_vlen, s_type = struct.unpack_from("<HHH", blob, s_pos)
                    if s_len == 0:
                        break
                    s_end = s_pos + s_len
                    s_key, s_key_end = read_utf16z(blob, s_pos + 6, s_end)
                    s_val_off = (s_key_end + 2 + 3) & ~3
                    if s_type == 1 and s_val_off <= s_end:
                        s_val_len = min(s_vlen * 2, s_end - s_val_off)
                        raw_val = blob[s_val_off:s_val_off + s_val_len]
                        strings[s_key] = raw_val.decode("utf-16le").split("\x00", 1)[0]
                    s_pos = (s_end + 3) & ~3
                t_pos = (t_end + 3) & ~3
        pos = (c_end + 3) & ~3
    return fixed, strings

def inspect_pe(path: str, expect_dll: bool, expect_resources: bool, expected_version: str):
    data = open(path, "rb").read()
    assert data[:2] == b"MZ", f"{path}: missing MZ header"
    e_lfanew, = struct.unpack_from("<I", data, 60)
    assert data[e_lfanew:e_lfanew + 4] == b"PE\0\0", f"{path}: missing PE signature"
    machine, num_sections, _, _, _, opt_size, characteristics = struct.unpack_from(
        "<HHIIIHH", data, e_lfanew + 4
    )
    assert machine == 0x8664, f"{path}: unexpected machine {machine:#06x}"
    is_dll = bool(characteristics & 0x2000)
    assert is_dll == expect_dll, f"{path}: DLL flag mismatch (got {is_dll}, expected {expect_dll})"

    opt_off = e_lfanew + 24
    opt_magic, = struct.unpack_from("<H", data, opt_off)
    assert opt_magic == 0x020B, f"{path}: unexpected optional header magic {opt_magic:#06x}"

    num_rva_and_sizes, = struct.unpack_from("<I", data, opt_off + 108)
    dd_off = opt_off + 112
    import_rva, import_size = (0, 0)
    rsrc_rva, rsrc_size = (0, 0)
    if num_rva_and_sizes > 1:
        import_rva, import_size = struct.unpack_from("<II", data, dd_off + 8)
    if num_rva_and_sizes > 2:
        rsrc_rva, rsrc_size = struct.unpack_from("<II", data, dd_off + 16)

    sec_off = opt_off + opt_size
    sections = []
    for i in range(num_sections):
        off = sec_off + i * 40
        raw_name = data[off:off + 8].split(b"\0", 1)[0].decode("ascii", "replace")
        vsize, vaddr, raw_size, raw_ptr = struct.unpack_from("<IIII", data, off + 8)
        sections.append((raw_name, vaddr, max(vsize, raw_size), raw_ptr, raw_size))

    def rva_to_off(rva: int):
        for _, vaddr, span, raw_ptr, raw_size in sections:
            if vaddr <= rva < vaddr + span:
                delta = rva - vaddr
                if delta < raw_size:
                    return raw_ptr + delta
        return None

    imports = []
    if import_rva and import_size:
        pos = rva_to_off(import_rva)
        assert pos is not None, f"{path}: could not map import directory RVA"
        while True:
            orig_first, ts, fwd, name_rva, first_thunk = struct.unpack_from("<IIIII", data, pos)
            if orig_first == 0 and ts == 0 and fwd == 0 and name_rva == 0 and first_thunk == 0:
                break
            name_off = rva_to_off(name_rva)
            assert name_off is not None, f"{path}: invalid import DLL name RVA"
            end = data.index(b"\0", name_off)
            imports.append(data[name_off:end].decode("ascii", "replace"))
            pos += 20

    bad = [dll for dll in imports if any(f in dll.lower() for f in FORBIDDEN_IMPORTS)]
    assert not bad, f"{path}: forbidden dynamic CRT imports found: {bad} (all: {imports})"

    resource_types = []
    ver_summary = None
    if expect_resources:
        assert any(s[0] == ".rsrc" for s in sections), f"{path}: missing .rsrc section"
        assert rsrc_rva and rsrc_size, f"{path}: missing resource data directory"
        rsrc_off = rva_to_off(rsrc_rva)
        assert rsrc_off is not None, f"{path}: could not map .rsrc RVA"
        _, _, _, _, named_entries, id_entries = struct.unpack_from("<IIHHHH", data, rsrc_off)
        ent_off = rsrc_off + 16
        version_blobs = []
        for _ in range(named_entries + id_entries):
            rid, data_or_sub = struct.unpack_from("<II", data, ent_off)
            resource_types.append(rid)
            if rid == 16 and (data_or_sub & 0x80000000):
                def collect_leaves(dir_off):
                    _, _, _, _, nn, ni = struct.unpack_from("<IIHHHH", data, dir_off)
                    eo = dir_off + 16
                    for _ in range(nn + ni):
                        _, target = struct.unpack_from("<II", data, eo)
                        eo += 8
                        if target & 0x80000000:
                            collect_leaves(rsrc_off + (target & 0x7FFFFFFF))
                        else:
                            drva, dsz, _, _ = struct.unpack_from("<IIII", data, rsrc_off + target)
                            doff = rva_to_off(drva)
                            assert doff is not None, f"{path}: unmapped RT_VERSION RVA"
                            version_blobs.append(data[doff:doff + dsz])
                collect_leaves(rsrc_off + (data_or_sub & 0x7FFFFFFF))
            ent_off += 8
        assert 3 in resource_types and 14 in resource_types, (
            f"{path}: missing RT_ICON (3) or RT_GROUP_ICON (14) in .rsrc types {resource_types}"
        )
        assert 16 in resource_types and version_blobs, (
            f"{path}: missing RT_VERSION (16) in .rsrc types {resource_types}"
        )
        (fv, pv), strings = parse_version_blob(version_blobs[0])
        want_tuple = parse_win32_version(expected_version)
        assert fv == want_tuple and pv == want_tuple, (
            f"{path}: unexpected FILEVERSION/PRODUCTVERSION {fv}/{pv}, expected {want_tuple}"
        )
        expected_strings = {
            "FileDescription": "NyaaCord",
            "ProductName": "NyaaCord",
            "InternalName": "NyaaCord",
            "OriginalFilename": "NyaaCord.exe",
            "FileVersion": expected_version,
            "ProductVersion": expected_version,
        }
        for key, want in expected_strings.items():
            got = strings.get(key)
            assert got == want, f"{path}: VS_VERSION_INFO {key}={got!r}, expected {want!r}"
        ver_summary = f"FILEVERSION={fv} FileDescription={strings['FileDescription']!r} ProductName={strings['ProductName']!r}"

    print(f"Verified PE32+ x86-64: {path}")
    print(f"  Machine: {machine:#06x}  OptMagic: {opt_magic:#06x}  Sections: {[s[0] for s in sections]}")
    print(f"  Imports ({len(imports)}): {', '.join(imports)}")
    if expect_resources:
        print(f"  Resource types: {resource_types} (RT_ICON=3, RT_GROUP_ICON=14, RT_VERSION=16)")
        print(f"  VersionInfo: {ver_summary}")

inspect_pe(sys.argv[1], expect_dll=True, expect_resources=False, expected_version=sys.argv[3])
inspect_pe(sys.argv[2], expect_dll=False, expect_resources=True, expected_version=sys.argv[3])
PY

client_zip=""
if [ "$no_client" = 0 ]; then
   mkdir -p "$WORK/client"
   if [ -d "$client" ]; then
      client_dir="$client"
      if [ ! -f "$client_dir/Discord.exe" ]; then
         echo "Discord.exe not found in $client_dir" >&2
         exit 1
      fi
      cp -f "$client_dir/Discord.exe" "$WORK/client/Discord.exe"
   else
      client_dir="$WORK/client"
      echo "Extracting Windows client from $client..."
      7z x -y "-o$client_dir" "$client" >/dev/null
      if [ ! -f "$client_dir/Discord.exe" ]; then
         echo "Discord.exe not found in $client_dir" >&2
         exit 1
      fi
   fi

   bundled_discord_exe="$WORK/client/Discord.exe"
   echo "Updating bundled Discord.exe Task Manager metadata..."
   python3 - "$bundled_discord_exe" <<'PY'
import array, struct, sys

NEW_BRAND = "NyaaCord"
TARGET_KEYS = ("FileDescription", "ProductName")
ALLOWED_ORIG = {"Discord Phoenix", "Discord", NEW_BRAND}

def compute_pe_checksum(buf: bytes, checksum_off: int) -> int:
    work = bytearray(buf)
    struct.pack_into("<I", work, checksum_off, 0)
    file_len = len(work)
    if file_len & 1:
        work.append(0)
    words = array.array("H")
    words.frombytes(work)
    if sys.byteorder != "little":
        words.byteswap()
    total = sum(words)
    while total > 0xFFFF:
        total = (total & 0xFFFF) + (total >> 16)
    return (total + file_len) & 0xFFFFFFFF

def parse_pe_layout(data: bytes):
    assert data[:2] == b"MZ", "missing MZ header"
    e_lfanew, = struct.unpack_from("<I", data, 60)
    assert data[e_lfanew:e_lfanew + 4] == b"PE\0\0", "missing PE signature"
    machine, num_sections, _, _, _, opt_size, characteristics = struct.unpack_from(
        "<HHIIIHH", data, e_lfanew + 4
    )
    assert machine == 0x8664, f"unexpected machine {machine:#06x}"
    assert (characteristics & 0x2000) == 0, "expected executable, got DLL"
    opt_off = e_lfanew + 24
    opt_magic, = struct.unpack_from("<H", data, opt_off)
    assert opt_magic == 0x020B, f"unexpected optional header magic {opt_magic:#06x}"
    checksum_off = opt_off + 64
    num_dd, = struct.unpack_from("<I", data, opt_off + 108)
    dd_off = opt_off + 112
    sec_off = opt_off + opt_size
    sections = []
    max_sec_end = 0
    for i in range(num_sections):
        off = sec_off + i * 40
        vsize, vaddr, raw_size, raw_ptr = struct.unpack_from("<IIII", data, off + 8)
        sections.append((vaddr, max(vsize, raw_size), raw_ptr, raw_size))
        if raw_ptr + raw_size > max_sec_end:
            max_sec_end = raw_ptr + raw_size
    return checksum_off, dd_off, num_dd, sections, max_sec_end

def rva_to_off(sections, rva: int):
    for vaddr, span, raw_ptr, raw_size in sections:
        if vaddr <= rva < vaddr + span:
            delta = rva - vaddr
            if delta < raw_size:
                return raw_ptr + delta
    return None

def collect_rt_version(data: bytes, sections, dd_off: int, num_dd: int):
    assert num_dd > 2, "missing resource data directory"
    rsrc_rva, rsrc_size = struct.unpack_from("<II", data, dd_off + 16)
    assert rsrc_rva and rsrc_size, "empty resource data directory"
    rsrc_off = rva_to_off(sections, rsrc_rva)
    assert rsrc_off is not None, "could not map .rsrc RVA"

    leaves = []
    def walk_dir(dir_off: int, depth: int):
        assert depth < 8, "resource directory nesting too deep"
        assert dir_off + 16 <= len(data), f"resource directory header out of bounds ({dir_off})"
        _, _, _, _, nn, ni = struct.unpack_from("<IIHHHH", data, dir_off)
        eo = dir_off + 16
        assert eo + (nn + ni) * 8 <= len(data), f"resource directory entries out of bounds ({eo})"
        for _ in range(nn + ni):
            rid, target = struct.unpack_from("<II", data, eo)
            eo += 8
            if depth == 0 and rid != 16:
                continue
            if target & 0x80000000:
                walk_dir(rsrc_off + (target & 0x7FFFFFFF), depth + 1)
            else:
                leaf_off = rsrc_off + target
                assert leaf_off + 16 <= len(data), f"resource data entry out of bounds ({leaf_off})"
                drva, dsz, _, _ = struct.unpack_from("<IIII", data, leaf_off)
                doff = rva_to_off(sections, drva)
                assert doff is not None, "could not map RT_VERSION RVA"
                assert doff + dsz <= len(data), (
                    f"RT_VERSION resource out of bounds ({doff} + {dsz} > {len(data)})"
                )
                leaves.append((doff, dsz))
    walk_dir(rsrc_off, 0)
    assert leaves, "RT_VERSION resource not found"
    return leaves

def read_utf16z(buf, off: int, limit: int):
    end = off
    while end + 2 <= limit and buf[end:end + 2] != b"\x00\x00":
        end += 2
    return bytes(buf[off:end]).decode("utf-16le"), end

def iter_version_string_slots(buf, base_off: int, size: int):
    assert base_off >= 0 and size >= 6 and base_off + size <= len(buf), (
        f"RT_VERSION bounds invalid ({base_off} + {size} > {len(buf)})"
    )
    wlen, vlen, _ = struct.unpack_from("<HHH", buf, base_off)
    assert 6 <= wlen <= size, f"invalid VS_VERSION_INFO length ({wlen}, resource size {size})"
    root_end = base_off + wlen
    root_key, root_key_end = read_utf16z(buf, base_off + 6, root_end)
    assert root_key == "VS_VERSION_INFO", f"unexpected root version key {root_key!r}"
    val_off = base_off + (((root_key_end - base_off) + 2 + 3) & ~3)
    pos = base_off + (((val_off - base_off) + vlen + 3) & ~3)
    while pos + 6 <= root_end:
        c_len, _, _ = struct.unpack_from("<HHH", buf, pos)
        if c_len < 6:
            break
        c_end = min(pos + c_len, root_end)
        c_key, c_key_end = read_utf16z(buf, pos + 6, c_end)
        if c_key == "StringFileInfo":
            t_pos = base_off + (((c_key_end - base_off) + 2 + 3) & ~3)
            while t_pos + 6 <= c_end:
                t_len, _, _ = struct.unpack_from("<HHH", buf, t_pos)
                if t_len < 6:
                    break
                t_end = min(t_pos + t_len, c_end)
                _, t_key_end = read_utf16z(buf, t_pos + 6, t_end)
                s_pos = base_off + (((t_key_end - base_off) + 2 + 3) & ~3)
                while s_pos + 6 <= t_end:
                    s_len, s_vlen, s_type = struct.unpack_from("<HHH", buf, s_pos)
                    if s_len < 6:
                        break
                    s_end = min(s_pos + s_len, t_end)
                    s_key, s_key_end = read_utf16z(buf, s_pos + 6, s_end)
                    s_val_off = base_off + (((s_key_end - base_off) + 2 + 3) & ~3)
                    if s_type == 1 and s_val_off < s_end:
                        slot_bytes = s_end - s_val_off
                        yield s_pos, s_key, s_val_off, slot_bytes, s_vlen
                    s_pos = base_off + (((s_end - base_off) + 3) & ~3)
                t_pos = base_off + (((t_end - base_off) + 3) & ~3)
        pos = base_off + (((c_end - base_off) + 3) & ~3)

path = sys.argv[1]
data = bytearray(open(path, "rb").read())
checksum_off, dd_off, num_dd, sections, max_sec_end = parse_pe_layout(data)
leaves = collect_rt_version(data, sections, dd_off, num_dd)

replacement = (NEW_BRAND + "\x00").encode("utf-16le")
new_vlen = len(NEW_BRAND) + 1
updated = set()
for doff, dsz in leaves:
    for s_pos, key, val_off, slot_bytes, _ in iter_version_string_slots(data, doff, dsz):
        if key not in TARGET_KEYS:
            continue
        existing = bytes(data[val_off:val_off + slot_bytes]).decode("utf-16le").split("\x00", 1)[0]
        assert existing in ALLOWED_ORIG, f"{key}: unexpected value {existing!r}"
        assert len(replacement) <= slot_bytes, (
            f"{key}: slot too small ({slot_bytes} bytes) for {NEW_BRAND!r}"
        )
        padded = replacement + b"\x00" * (slot_bytes - len(replacement))
        data[val_off:val_off + slot_bytes] = padded
        struct.pack_into("<H", data, s_pos + 2, new_vlen)
        updated.add(key)

assert updated == set(TARGET_KEYS), f"missing VS_VERSION_INFO keys: {set(TARGET_KEYS) - updated}"

trimmed_cert = 0
if num_dd > 4:
    cert_off, cert_size = struct.unpack_from("<II", data, dd_off + 32)
    struct.pack_into("<II", data, dd_off + 32, 0, 0)
    if cert_off > 0 and cert_size > 0 and cert_off + cert_size == len(data) and cert_off >= max_sec_end:
        del data[cert_off:]
        trimmed_cert = cert_size

new_checksum = compute_pe_checksum(data, checksum_off)
struct.pack_into("<I", data, checksum_off, new_checksum)

with open(path, "wb") as fh:
    fh.write(data)

# Read back from disk and verify metadata, certificate clearing, and checksum.
verify = open(path, "rb").read()
v_cs_off, v_dd_off, v_num_dd, v_sections, _ = parse_pe_layout(verify)
if v_num_dd > 4:
    assert struct.unpack_from("<II", verify, v_dd_off + 32) == (0, 0), "certificate directory not zeroed"
stored_cs, = struct.unpack_from("<I", verify, v_cs_off)
recomputed_cs = compute_pe_checksum(verify, v_cs_off)
assert stored_cs == recomputed_cs, f"checksum mismatch: stored={stored_cs:#010x} computed={recomputed_cs:#010x}"

readback = {}
readback_vlen = {}
for doff, dsz in collect_rt_version(verify, v_sections, v_dd_off, v_num_dd):
    for _, key, val_off, slot_bytes, s_vlen in iter_version_string_slots(verify, doff, dsz):
        if key in TARGET_KEYS:
            assert s_vlen == new_vlen, f"{key}: wValueLength mismatch ({s_vlen} != {new_vlen})"
            raw = verify[val_off:val_off + slot_bytes]
            assert raw.startswith(replacement) and all(b == 0 for b in raw[len(replacement):]), (
                f"{key}: invalid NUL-padded UTF-16LE slot"
            )
            readback[key] = raw.decode("utf-16le").split("\x00", 1)[0]
            readback_vlen[key] = s_vlen

for key in TARGET_KEYS:
    assert readback.get(key) == NEW_BRAND, f"{key} read back as {readback.get(key)!r}"
    assert readback_vlen.get(key) == new_vlen, f"{key} wValueLength read back as {readback_vlen.get(key)!r}"

print(
    f"Verified bundled Discord.exe metadata: FileDescription={readback['FileDescription']!r} "
    f"ProductName={readback['ProductName']!r} wValueLength={readback_vlen['FileDescription']} "
    f"trimmed_cert={trimmed_cert} checksum={stored_cs:#010x}"
)
PY

   client_zip="$WORK/client.zip"
   echo "Packing Windows client overlay archive..."
   python3 - "$client_dir" "$client_zip" "$bundled_discord_exe" <<'PY'
import hashlib, os, sys, zipfile

src_dir, zip_path, discord_exe_override = sys.argv[1], sys.argv[2], sys.argv[3]
CHUNK_BYTES = 1024 * 1024
FIXED_TIME = (2026, 1, 1, 0, 0, 0)
SKIP_TOP = {"discordphoenixsetup.exe", "nyaacord-client.stamp", "client-stamp.txt"}

files = []
for root, dirnames, filenames in os.walk(src_dir, followlinks=False):
    dirnames.sort()
    filenames.sort()
    for name in list(dirnames) + list(filenames):
        full = os.path.join(root, name)
        if os.path.islink(full):
            raise SystemExit(f"Unexpected symlink in Windows client directory: {full}")
    for name in filenames:
        full = os.path.join(root, name)
        rel = os.path.relpath(full, src_dir).replace(os.sep, "/")
        if "/" not in rel and rel.lower() in SKIP_TOP:
            continue
        if rel == "Discord.exe":
            full = discord_exe_override
        files.append((rel, full))

files.sort(key=lambda item: item[0])
assert any(rel == "Discord.exe" for rel, _ in files), "Discord.exe missing from client payload"

manifest = []
stamp_hasher = hashlib.sha256()
for rel, full in files:
    size = os.path.getsize(full)
    fh_hasher = hashlib.sha256()
    with open(full, "rb") as fh:
        while True:
            chunk = fh.read(CHUNK_BYTES)
            if not chunk:
                break
            fh_hasher.update(chunk)
    digest = fh_hasher.hexdigest()
    manifest.append((rel, full, size, digest))
    stamp_hasher.update(f"{rel}\0{size}\0{digest}\n".encode("utf-8"))

stamp = f"v1:{stamp_hasher.hexdigest()}\n"

with zipfile.ZipFile(
    zip_path,
    "w",
    compression=zipfile.ZIP_DEFLATED,
    compresslevel=6,
    allowZip64=True,
) as zf:
    for rel, full, size, _ in manifest:
        zi = zipfile.ZipInfo(filename=rel, date_time=FIXED_TIME)
        zi.compress_type = zipfile.ZIP_DEFLATED
        zi.external_attr = 0o100644 << 16
        with open(full, "rb") as src, zf.open(zi, "w", force_zip64=(size >= (1 << 31))) as dst:
            while True:
                chunk = src.read(CHUNK_BYTES)
                if not chunk:
                    break
                dst.write(chunk)
    stamp_info = zipfile.ZipInfo(filename="nyaacord-client.stamp", date_time=FIXED_TIME)
    stamp_info.compress_type = zipfile.ZIP_DEFLATED
    stamp_info.external_attr = 0o100644 << 16
    with zf.open(stamp_info, "w") as dst:
        dst.write(stamp.encode("utf-8"))

print(f"Packed {len(manifest)} client files ({os.path.getsize(zip_path)} bytes)")
print(f"Client stamp: {stamp.strip()}")
PY
fi

staged_out="$WORK/out.exe"
if [ -n "$client_zip" ]; then
   cat "$exe_bin" "$client_zip" > "$staged_out"
   echo "Verifying bundled Windows executable and ZIP overlay..."
   python3 - "$staged_out" <<'PY'
import os, struct, sys, zipfile

path = sys.argv[1]
size = os.path.getsize(path)
with open(path, "rb") as fh:
    dos = fh.read(64)
    assert len(dos) == 64 and dos[:2] == b"MZ", f"{path}: missing MZ header"
    e_lfanew, = struct.unpack_from("<I", dos, 60)
    assert 64 <= e_lfanew and e_lfanew + 26 <= size, f"{path}: invalid PE offset {e_lfanew}"
    fh.seek(e_lfanew)
    pe = fh.read(26)
    assert len(pe) == 26 and pe[:4] == b"PE\0\0", f"{path}: missing PE signature"
    machine, _, _, _, _, opt_size, characteristics, opt_magic = struct.unpack("<HHIIIHHH", pe[4:26])
    assert machine == 0x8664, f"{path}: unexpected machine {machine:#06x}"
    assert opt_size >= 2 and e_lfanew + 24 + opt_size <= size, f"{path}: invalid optional header size {opt_size}"
    assert (characteristics & 0x2000) == 0, f"{path}: expected executable, got DLL"
    assert opt_magic == 0x020B, f"{path}: unexpected optional header magic {opt_magic:#06x}"

with zipfile.ZipFile(path, "r") as zf:
    names = zf.namelist()
    assert "Discord.exe" in names, f"{path}: Discord.exe missing from overlay archive"
    assert "nyaacord-client.stamp" in names, f"{path}: nyaacord-client.stamp missing from overlay archive"
    assert not any(n.lower() == "discordphoenixsetup.exe" for n in names), (
        f"{path}: DiscordPhoenixSetup.exe should be excluded from overlay archive"
    )
    bad = zf.testzip()
    assert bad is None, f"{path}: corrupt ZIP entry {bad}"
    stamp = zf.read("nyaacord-client.stamp").decode("utf-8").strip()
    assert stamp.startswith("v1:") and len(stamp) == 67, f"{path}: invalid client stamp {stamp!r}"
    print(f"Verified bundled PE32+ overlay: {len(names)} archive entries, stamp {stamp}")
PY
else
   cp -f "$exe_bin" "$staged_out"
fi

out_dir=$(dirname "$out")
out_base=$(basename "$out")
mkdir -p "$out_dir"
out_tmp="$out.tmp"
cp -f "$staged_out" "$out_tmp"
mv -f "$out_tmp" "$out"
out_tmp=""

(cd "$out_dir" && sha256sum "$out_base") > "$out.sha256"

if [ "$(realpath "$out_dir")" = "$(realpath "$ROOT/dist")" ]; then
   python3 - "$out_dir/SHA256SUMS.txt" "$out.sha256" "$out_base" <<'PY'
import os, sys
sums_path, new_sum_path, target_name = sys.argv[1], sys.argv[2], sys.argv[3]
new_line = open(new_sum_path, encoding="utf-8").read().strip()
entries = {}
if os.path.isfile(sums_path):
    for line in open(sums_path, encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) == 2:
            entries[parts[1].lstrip("*")] = line
entries[target_name] = new_line
with open(sums_path, "w", encoding="utf-8") as fh:
    for name in sorted(entries):
        fh.write(entries[name] + "\n")
PY
fi

size_bytes=$(stat -c %s "$out")
size_human=$(du -h "$out" | cut -f1)
sha256=$(cut -d' ' -f1 "$out.sha256")
echo "Built $out"
echo "Size: $size_human ($size_bytes bytes)"
echo "SHA-256: $sha256"
echo "Checksum file: $out.sha256"
if [ "$(realpath "$out_dir")" = "$(realpath "$ROOT/dist")" ]; then
   echo "Checksums: $out_dir/SHA256SUMS.txt"
fi
