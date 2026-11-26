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

version=${NYAACORD_VERSION:-}
out="$ROOT/dist/NyaaCord-x86_64.exe"
skip_plugins=0

while [ "$#" -gt 0 ]; do
   case "$1" in
      --version)
         [ "$#" -ge 2 ] || { echo "--version requires a value" >&2; exit 1; }
         version=$2
         shift 2
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

if [ -z "$version" ]; then
   version=$(parse_workspace_version)
fi
version=$(normalize_version "$version")

icon_src="$ROOT/web/plugins/nyaacord.png"
[ -f "$icon_src" ] || { echo "Missing $icon_src" >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

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

echo "Building phoenix-inject (NyaaCord-x86_64.exe, version $version)..."
(
   cd "$ROOT"
   XWIN_SDK_VERSION="$xwin_sdk_ver" \
   CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="$crt_flags" \
   NYAACORD_VERSION="$version" \
   NYAACORD_EMBED_DLL="$dll_bin" \
   NYAACORD_EMBED_PLUGINS="$plugins_bundle" \
   NYAACORD_ICON_ICO="$ico_path" \
      cargo xwin build --release --target "$TARGET" -p phoenix-inject
)

exe_bin="$ROOT/target/$TARGET/release/phoenix-inject.exe"
[ -f "$exe_bin" ] || { echo "Missing built executable: $exe_bin" >&2; exit 1; }

echo "Verifying PE32+ headers, icon resources, and static CRT imports..."
python3 - "$dll_bin" "$exe_bin" <<'PY'
import struct, sys

FORBIDDEN_IMPORTS = ("vcruntime140", "msvcp140", "api-ms-win-crt", "ucrtbase")

def inspect_pe(path: str, expect_dll: bool, expect_icon: bool):
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
    if expect_icon:
        assert any(s[0] == ".rsrc" for s in sections), f"{path}: missing .rsrc section"
        assert rsrc_rva and rsrc_size, f"{path}: missing resource data directory"
        rsrc_off = rva_to_off(rsrc_rva)
        assert rsrc_off is not None, f"{path}: could not map .rsrc RVA"
        _, _, _, _, named_entries, id_entries = struct.unpack_from("<IIHHHH", data, rsrc_off)
        ent_off = rsrc_off + 16
        for _ in range(named_entries + id_entries):
            rid, _ = struct.unpack_from("<II", data, ent_off)
            resource_types.append(rid)
            ent_off += 8
        assert 3 in resource_types and 14 in resource_types, (
            f"{path}: missing RT_ICON (3) or RT_GROUP_ICON (14) in .rsrc types {resource_types}"
        )

    print(f"Verified PE32+ x86-64: {path}")
    print(f"  Machine: {machine:#06x}  OptMagic: {opt_magic:#06x}  Sections: {[s[0] for s in sections]}")
    print(f"  Imports ({len(imports)}): {', '.join(imports)}")
    if expect_icon:
        print(f"  Resource types: {resource_types} (RT_ICON=3, RT_GROUP_ICON=14)")

inspect_pe(sys.argv[1], expect_dll=True, expect_icon=False)
inspect_pe(sys.argv[2], expect_dll=False, expect_icon=True)
PY

out_dir=$(dirname "$out")
out_base=$(basename "$out")
mkdir -p "$out_dir"
cp -f "$exe_bin" "$out.tmp"
mv -f "$out.tmp" "$out"

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
