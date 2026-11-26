#!/usr/bin/env bash
# Builds a self-contained NyaaCord Linux x86-64 AppImage from a base Discord Phoenix AppImage.
set -euo pipefail

CALLER_PWD=$PWD
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

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
Usage: scripts/build-appimage.sh [options]

Options:
  --version <ver>       Version to embed (default: NYAACORD_VERSION or Cargo.toml workspace version)
  --client <path>       Base Discord Phoenix AppImage (default: DISCORD_PHOENIX_APPIMAGE or auto-detect)
  --out <path>          Output AppImage path (default: dist/NyaaCord-x86_64.AppImage)
  --comp <gzip|zstd>    SquashFS compression algorithm (default: gzip)
  --skip-cargo          Reuse existing target/release/libphoenix.so and target/release/nyaacord-updater
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

discover_client() {
   python3 - "$HOME/Applications" "$HOME/Downloads" "$CALLER_PWD" <<'PY'
import os, sys, glob

candidates = []
patterns = ("discord-phoenix*.AppImage", "*[Dd]iscord*.AppImage")
for base in sys.argv[1:]:
    if not base or not os.path.isdir(base):
        continue
    for pat in patterns:
        for path in glob.glob(os.path.join(base, pat)):
            name = os.path.basename(path)
            if name.lower().startswith("nyaacord"):
                continue
            if os.path.isfile(path):
                candidates.append((os.path.getmtime(path), os.path.realpath(path)))

if candidates:
    candidates.sort(key=lambda item: item[0], reverse=True)
    print(candidates[0][1])
PY
}

find_mksquashfs() {
   if [ -n "${MKSQUASHFS:-}" ] && [ -x "$MKSQUASHFS" ]; then
      printf '%s\n' "$MKSQUASHFS"
      return 0
   fi
   if command -v mksquashfs >/dev/null 2>&1; then
      command -v mksquashfs
      return 0
   fi
   local fallback
   for fallback in \
      "$HOME/.cache/electron-builder/appimage/appimage-12.0.1/linux-x64/mksquashfs" \
      "$HOME"/.cache/electron-builder/appimage/*/linux-x64/mksquashfs; do
      if [ -x "$fallback" ]; then
         printf '%s\n' "$fallback"
         return 0
      fi
   done
   return 1
}

check_mksquashfs_comp() {
   local bin=$1
   local algo=$2
   local probe_dir probe_err
   probe_dir=$(mktemp -d)
   if ! probe_err=$("$bin" "$probe_dir" "$probe_dir/probe.sqfs" -noappend -comp "$algo" 2>&1 >/dev/null); then
      rm -rf "$probe_dir"
      echo "mksquashfs at $bin does not support '$algo' compression." >&2
      if [ -n "$probe_err" ]; then
         printf '%s\n' "$probe_err" | head -n 3 >&2
      fi
      echo "Use --comp gzip or install squashfs-tools with $algo support." >&2
      return 1
   fi
   rm -rf "$probe_dir"
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
client=${DISCORD_PHOENIX_APPIMAGE:-}
out="$ROOT/dist/NyaaCord-x86_64.AppImage"
comp="gzip"
skip_cargo=0

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
         shift 2
         ;;
      --out)
         [ "$#" -ge 2 ] || { echo "--out requires a value" >&2; exit 1; }
         out=$(resolve_path "$2")
         shift 2
         ;;
      --comp)
         [ "$#" -ge 2 ] || { echo "--comp requires gzip or zstd" >&2; exit 1; }
         comp=$2
         shift 2
         ;;
      --skip-cargo)
         skip_cargo=1
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

case "$comp" in
   gzip|zstd) ;;
   xz)
      echo "Unsupported compression 'xz': base type2-runtime only supports gzip (zlib) and zstd" >&2
      exit 1
      ;;
   *)
      echo "Unsupported compression '$comp': expected gzip or zstd" >&2
      exit 1
      ;;
esac

if ! command -v python3 >/dev/null 2>&1; then
   echo "python3 is required" >&2
   exit 1
fi

if [ -z "$version" ]; then
   version=$(parse_workspace_version)
fi
version=$(normalize_version "$version")

if [ -z "$client" ]; then
   client=$(discover_client)
fi
if [ -z "$client" ] || [ ! -f "$client" ]; then
   echo "Base Discord Phoenix AppImage not found. Pass --client <path> or set DISCORD_PHOENIX_APPIMAGE." >&2
   exit 1
fi
client=$(realpath "$client")

mksquashfs=$(find_mksquashfs) || {
   echo "mksquashfs not found. Install squashfs-tools or set MKSQUASHFS." >&2
   exit 1
}
check_mksquashfs_comp "$mksquashfs" "$comp"

esbuild=$(find_esbuild) || {
   echo "Failed to locate or install esbuild." >&2
   exit 1
}

if [ "$skip_cargo" = 0 ]; then
   echo "Building phoenix-loader and phoenix-updater (version $version)..."
   (cd "$ROOT" && NYAACORD_VERSION="$version" cargo build --release -p phoenix-loader -p phoenix-updater)
fi

loader_bin="$ROOT/target/release/libphoenix.so"
updater_bin="$ROOT/target/release/nyaacord-updater"
icon_src="$ROOT/web/plugins/nyaacord.png"
gif_icon_src="$ROOT/web/plugins/nyaacord.gif"

[ -f "$loader_bin" ] || { echo "Missing $loader_bin" >&2; exit 1; }
[ -f "$updater_bin" ] || { echo "Missing $updater_bin" >&2; exit 1; }
[ -f "$icon_src" ] || { echo "Missing $icon_src" >&2; exit 1; }
[ -f "$gif_icon_src" ] || { echo "Missing $gif_icon_src" >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

client_exec=$client
if [ ! -x "$client_exec" ]; then
   cp "$client" "$WORK/client.AppImage"
   chmod +x "$WORK/client.AppImage"
   client_exec="$WORK/client.AppImage"
fi

echo "Extracting base client from $client..."
offset=$("$client_exec" --appimage-offset)
case "$offset" in
   ''|*[!0-9]*)
      echo "Failed to read AppImage offset from $client" >&2
      exit 1
      ;;
esac

head -c "$offset" "$client_exec" > "$WORK/runtime"
(cd "$WORK" && "$client_exec" --appimage-extract >/dev/null)

APPDIR="$WORK/squashfs-root"
if [ ! -x "$APPDIR/usr/bin/discord-phoenix" ]; then
   echo "Extracted AppImage does not contain usr/bin/discord-phoenix" >&2
   exit 1
fi

mkdir -p "$APPDIR/usr/lib/nyaacord" "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/nyaacord" "$WORK/plugins"

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

python3 - "$WORK/plugins" "$APPDIR/usr/lib/nyaacord/libplm.plugins.so" "${DEFAULT_PLUGINS[@]}" <<'PY'
import json, os, sys
src_dir, out_path = sys.argv[1], sys.argv[2]
bundle = {}
for name in sorted(sys.argv[3:]):
    with open(os.path.join(src_dir, f"{name}.js"), encoding="utf-8") as fh:
        bundle[name] = fh.read()
with open(out_path, "w", encoding="utf-8") as fh:
    json.dump(bundle, fh, ensure_ascii=False)
PY
chmod 0644 "$APPDIR/usr/lib/nyaacord/libplm.plugins.so"

install -m 0755 "$loader_bin" "$APPDIR/usr/lib/nyaacord/libphoenix.so"
install -m 0755 "$updater_bin" "$APPDIR/usr/bin/nyaacord-updater"
printf '%s\n' "$version" > "$APPDIR/usr/lib/nyaacord/version"

echo "Generating metadata-free icons..."
python3 - "$icon_src" "$APPDIR" <<'PY'
import os, struct, sys, zlib

src_path, appdir = sys.argv[1], sys.argv[2]
SIG = b"\x89PNG\r\n\x1a\n"
SIZES = (16, 24, 32, 48, 64, 128, 256, 512)

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
            rgba[i*4:i*4+3] = out[i*3:i*3+3]
            rgba[i*4+3] = 255
    elif ctype == 3:
        plte = next(d for t, d in chunks if t == b"PLTE")
        trns = next((d for t, d in chunks if t == b"tRNS"), b"")
        for i, pi in enumerate(out):
            rgba[i*4:i*4+3] = plte[pi*3:pi*3+3]
            rgba[i*4+3] = trns[pi] if pi < len(trns) else 255
    elif ctype == 4:
        for i in range(w * h):
            g, a = out[i*2], out[i*2+1]
            rgba[i*4:i*4+4] = bytes((g, g, g, a))
    elif ctype == 0:
        for i, g in enumerate(out):
            rgba[i*4:i*4+4] = bytes((g, g, g, 255))
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

def encode_rgba(w: int, h: int, rgba: bytes) -> bytes:
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)
        raw.extend(rgba[y * stride:(y + 1) * stride])
    idat = zlib.compress(bytes(raw), level=9)
    return SIG + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")

src_w, src_h, src_rgba = decode_rgba(open(src_path, "rb").read())
for size in SIZES:
    png_bytes = encode_rgba(size, size, scale_nearest(src_w, src_h, src_rgba, size))
    apps_dir = os.path.join(appdir, "usr", "share", "icons", "hicolor", f"{size}x{size}", "apps")
    os.makedirs(apps_dir, exist_ok=True)
    for filename in ("nyaacord.png", "com.discord.libdiscord.png"):
        with open(os.path.join(apps_dir, filename), "wb") as fh:
            fh.write(png_bytes)
    if size == 256:
        with open(os.path.join(appdir, "nyaacord.png"), "wb") as fh:
            fh.write(png_bytes)
PY

install -m 0644 "$gif_icon_src" "$APPDIR/usr/share/nyaacord/nyaacord.gif"
ln -sf nyaacord.png "$APPDIR/.DirIcon"

rm -f \
   "$APPDIR/com.discord.libdiscord.png" \
   "$APPDIR/com.discord.libdiscord.desktop" \
   "$APPDIR/usr/share/applications/com.discord.libdiscord.desktop"

cat > "$APPDIR/nyaacord.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=NyaaCord
GenericName=Internet Messenger
Comment=NyaaCord for Discord Phoenix
Exec=nyaacord %U
Icon=nyaacord
StartupWMClass=com.discord.libdiscord
Categories=Network;InstantMessaging;
MimeType=x-scheme-handler/discord;
StartupNotify=true
Terminal=false
X-AppImage-Version=$version
EOF
cp "$APPDIR/nyaacord.desktop" "$APPDIR/usr/share/applications/nyaacord.desktop"

if command -v desktop-file-validate >/dev/null 2>&1; then
   desktop-file-validate "$APPDIR/nyaacord.desktop"
fi

cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
set -eu

HERE=$(dirname "$(readlink -f "$0")")
version=$(tr -d '\r\n' < "$HERE/usr/lib/nyaacord/version")

export NYAACORD_APPDIR="$HERE"
export NYAACORD_VERSION="$version"
export NYAACORD_ICON="$HERE/nyaacord.png"
export NYAACORD_REPO="${NYAACORD_REPO:-CateDesu/NyaaCord}"

usage() {
   cat <<'USAGE'
Usage: NyaaCord-x86_64.AppImage [options] [-- Discord arguments]

Options:
  --check-update        Check GitHub Releases for a newer NyaaCord AppImage
  --update              Download and replace this AppImage with the latest release
  --rollback            Restore the previous AppImage backup
  --no-runtime          Launch with only the access gate opened
  --version             Print NyaaCord version
  --help                Show this help message
USAGE
}

case "${1:-}" in
   --version)
      printf 'NyaaCord %s\n' "$version"
      exit 0
      ;;
   --help|-h)
      usage
      exit 0
      ;;
   --check-update)
      shift
      exec "$HERE/usr/bin/nyaacord-updater" check --current "$version" "$@"
      ;;
   --update)
      shift
      target=${APPIMAGE:-${NYAACORD_MANAGED_APPIMAGE:-}}
      if [ -z "$target" ]; then
         echo "--update requires APPIMAGE or NYAACORD_MANAGED_APPIMAGE to be set" >&2
         exit 1
      fi
      exec "$HERE/usr/bin/nyaacord-updater" update --current "$version" --target "$target" "$@"
      ;;
   --rollback)
      shift
      target=${APPIMAGE:-${NYAACORD_MANAGED_APPIMAGE:-}}
      if [ -z "$target" ]; then
         echo "--rollback requires APPIMAGE or NYAACORD_MANAGED_APPIMAGE to be set" >&2
         exit 1
      fi
      exec "$HERE/usr/bin/nyaacord-updater" rollback --target "$target" "$@"
      ;;
esac

while [ "${1:-}" = "--no-runtime" ] || [ "${1:-}" = "--gate-only" ]; do
   export SUNBIRD_GATE_ONLY=1
   shift
done
if [ "${1:-}" = "--" ]; then
   shift
fi

if [ "${NYAACORD_AUTO_UPDATE:-0}" = "1" ] && [ -n "${APPIMAGE:-}" ]; then
   appimage=$APPIMAGE
   target_real=$(readlink -f "$appimage" 2>/dev/null || printf '%s' "$appimage")
   target_dir=$(dirname -- "$target_real")
   if [ -w "$target_dir" ]; then
      "$HERE/usr/bin/nyaacord-updater" update --current "$version" --target "$appimage" --timeout 5 --quiet || true
      new_v=$(env -u APPIMAGE -u APPDIR -u OWD -u ARGV0 -u NYAACORD_APPDIR -u NYAACORD_VERSION -u NYAACORD_ICON \
         NYAACORD_AUTO_UPDATE=0 "$appimage" --version 2>/dev/null | sed -n 's/^NyaaCord //p' | head -n 1 | tr -d '\r' | sed 's/^[vV]//' || true)
      if [ -n "$new_v" ] && [ "$new_v" != "$version" ]; then
         exec env -u APPIMAGE -u APPDIR -u OWD -u ARGV0 -u NYAACORD_APPDIR -u NYAACORD_VERSION -u NYAACORD_ICON \
            NYAACORD_AUTO_UPDATE=0 "$appimage" "$@"
      fi
   fi
fi

norm_dir() {
   d=$1
   resolved=$(readlink -m "$d" 2>/dev/null || printf '%s' "$d")
   while [ "${resolved%/}" != "$resolved" ] && [ "$resolved" != "/" ]; do
      resolved=${resolved%/}
   done
   printf '%s' "$resolved"
}

resolve_data_dir() {
   if [ -n "$1" ]; then
      norm_dir "$1"
   elif [ -n "$2" ]; then
      norm_dir "$2/discord-phoenix"
   elif [ -n "$3" ]; then
      norm_dir "$3/.local/share/discord-phoenix"
   else
      printf ''
   fi
}

# Only restart an unpatched Discord Phoenix process sharing this launch's data directory.
my_uid=$(id -u)
cur_data_dir=$(resolve_data_dir "${LIBDISCORD_DATA_DIR:-}" "${XDG_DATA_HOME:-}" "${HOME:-}")
pids=
patched=
if [ -n "$cur_data_dir" ]; then
   for p in /proc/[0-9]*; do
      [ -d "$p" ] || continue
      pid=${p#/proc/}
      [ "$pid" != "$$" ] || continue
      exe=$(readlink "$p/exe" 2>/dev/null || true)
      case "$exe" in
         */discord-phoenix|*/discord-phoenix' (deleted)') ;;
         *) continue ;;
      esac
      p_uid=$(stat -c '%u' "$p" 2>/dev/null || true)
      [ "$p_uid" = "$my_uid" ] || continue
      p_env=$(tr '\0' '\n' < "$p/environ" 2>/dev/null || true)
      [ -n "$p_env" ] || continue
      p_lib=$(printf '%s\n' "$p_env" | sed -n 's/^LIBDISCORD_DATA_DIR=//p' | head -n 1)
      p_xdg=$(printf '%s\n' "$p_env" | sed -n 's/^XDG_DATA_HOME=//p' | head -n 1)
      p_home=$(printf '%s\n' "$p_env" | sed -n 's/^HOME=//p' | head -n 1)
      p_data_dir=$(resolve_data_dir "$p_lib" "$p_xdg" "$p_home")
      [ "$p_data_dir" = "$cur_data_dir" ] || continue
      pids="$pids $pid"
      if grep -q libphoenix "$p/maps" 2>/dev/null; then
         patched=1
      fi
   done
fi

if [ -n "$pids" ] && [ -z "$patched" ]; then
   kill $pids 2>/dev/null || true
   i=0
   while [ "$i" -lt 10 ]; do
      alive=
      for pid in $pids; do
         if kill -0 "$pid" 2>/dev/null; then
            alive=1
         fi
      done
      if [ -z "$alive" ]; then
         break
      fi
      sleep 0.5
      i=$((i + 1))
   done
fi

# LD_PRELOAD splits on spaces and colons, so link the directory into a clean temp dir when needed.
preload_lib="$HERE/usr/lib/nyaacord/libphoenix.so"
case "$HERE" in
   *[[:space:]:]*)
      preload_base=/tmp
      for cand in "${XDG_RUNTIME_DIR:-}" "${TMPDIR:-}" /tmp; do
         [ -n "$cand" ] && [ -d "$cand" ] && [ -w "$cand" ] || continue
         cand=${cand%/}
         [ -n "$cand" ] || cand=/
         case "$cand" in
            *[[:space:]:]*) continue ;;
         esac
         preload_base=$cand
         break
      done
      for old in "$preload_base"/nyaacord-preload.*; do
         [ -d "$old" ] || continue
         old_uid=$(stat -c '%u' "$old" 2>/dev/null || true)
         if [ "$old_uid" = "$my_uid" ] && [ -L "$old/lib" ] && [ ! -e "$old/lib" ]; then
            rm -rf "$old" 2>/dev/null || true
         fi
      done
      preload_dir=$(mktemp -d "$preload_base/nyaacord-preload.XXXXXX")
      ln -s "$HERE/usr/lib/nyaacord" "$preload_dir/lib"
      preload_lib="$preload_dir/lib/libphoenix.so"
      ;;
esac

export LD_PRELOAD="$preload_lib${LD_PRELOAD:+:$LD_PRELOAD}"
exec "$HERE/usr/bin/discord-phoenix" "$@"
EOF
chmod +x "$APPDIR/AppRun"

echo "Packing SquashFS ($comp)..."
"$mksquashfs" "$APPDIR" "$WORK/fs.squashfs" -root-owned -noappend -comp "$comp" >/dev/null

out_dir=$(dirname "$out")
out_base=$(basename "$out")
mkdir -p "$out_dir"
cat "$WORK/runtime" "$WORK/fs.squashfs" > "$out.tmp"
chmod +x "$out.tmp"
mv -f "$out.tmp" "$out"

(cd "$out_dir" && sha256sum "$out_base") > "$out_dir/SHA256SUMS.txt"

"$updater_bin" verify --file "$out"
built_version=$("$out" --version)
if [ "$built_version" != "NyaaCord $version" ]; then
   echo "Built AppImage version check failed: got '$built_version', expected 'NyaaCord $version'" >&2
   exit 1
fi

size_bytes=$(stat -c %s "$out")
size_human=$(du -h "$out" | cut -f1)
sha256=$(cut -d' ' -f1 "$out_dir/SHA256SUMS.txt")
echo "Built $out"
echo "Size: $size_human ($size_bytes bytes)"
echo "SHA-256: $sha256"
echo "Checksums: $out_dir/SHA256SUMS.txt"
