#!/bin/sh
# Installs Nyaacord for Linux desktop Phoenix from a Nyaacord AppImage, a cloned
# checkout, a local loader, or the latest GitHub release.
set -eu

data=${XDG_DATA_HOME:-$HOME/.local/share}
home=$data/sunbird
entry=$data/applications/com.discord.libdiscord.desktop
legacy_entry=$data/applications/sunbird.desktop
repo=${NYAACORD_REPO:-CateDesu/NyaaCord}
local_loader=${SUNBIRD_LOCAL_LOADER:-}
install_version=${NYAACORD_INSTALL_VERSION:-}

DEFAULT_PLUGINS="clear-urls dearrow fakenitro fix-embeds global-badges message-latency message-logger no-profile-themes no-reply-mention silent-typing spoiler-mode text-replace translate view-raw"

refresh_desktop_cache() {
   if [ -d "$1" ] && command -v update-desktop-database >/dev/null 2>&1; then
      update-desktop-database "$1" || true
   fi
   if command -v kbuildsycoca6 >/dev/null 2>&1; then
      kbuildsycoca6 >/dev/null 2>&1 || true
   elif command -v kbuildsycoca5 >/dev/null 2>&1; then
      kbuildsycoca5 >/dev/null 2>&1 || true
   fi
}

usage() {
   cat <<'EOF'
Usage: scripts/nyaacord-linux.sh [options] [AppImage-or-discord-phoenix-path]

Options:
  --uninstall           Remove Nyaacord launcher, data files, and desktop entry
  --no-runtime          Configure desktop entry to launch with gate only
  --gate-only           Alias for --no-runtime
  --version <ver>       Seed installed version metadata for local checkout installs
  -h, --help            Show this help message
EOF
}

no_runtime=
uninstall=0
arg_path=

while [ "$#" -gt 0 ]; do
   case "$1" in
      --uninstall)
         uninstall=1
         shift
         ;;
      --no-runtime|--gate-only)
         no_runtime=1
         shift
         ;;
      --version)
         [ "$#" -ge 2 ] || { echo "--version requires a value" >&2; exit 1; }
         install_version=$2
         shift 2
         ;;
      -h|--help)
         usage
         exit 0
         ;;
      --)
         shift
         [ "$#" -gt 0 ] && arg_path=$1
         break
         ;;
      -*)
         echo "Unknown option: $1" >&2
         usage >&2
         exit 1
         ;;
      *)
         if [ -z "$arg_path" ]; then
            arg_path=$1
         else
            echo "Unexpected argument: $1" >&2
            exit 1
         fi
         shift
         ;;
   esac
done

is_nyaacord_desktop_entry() {
   [ -f "$1" ] || return 1
   grep -Fq "X-Nyaacord-Managed=true" "$1" 2>/dev/null \
      || grep -Fq "X-NyaaCord-Managed=true" "$1" 2>/dev/null \
      || grep -Fq "$home/sunbird" "$1" 2>/dev/null \
      || grep -Eq '^Name=Nyaa[Cc]ord$' "$1" 2>/dev/null
}

if [ "$uninstall" = 1 ]; then
   rm -rf "$home" "$legacy_entry"
   if is_nyaacord_desktop_entry "$entry"; then
      rm -f "$entry"
   fi
   refresh_desktop_cache "$data/applications"
   echo "Nyaacord is uninstalled. Discord itself was not changed."
   exit 0
fi

validate_repo() {
   r=$1
   owner=${r%%/*}
   name=${r#*/}
   if [ "$owner" = "$r" ] || [ -z "$owner" ] || [ -z "$name" ] || [ "$owner" = "." ] || [ "$owner" = ".." ] || [ "$name" = "." ] || [ "$name" = ".." ]; then
      return 1
   fi
   case "$name" in
      */*) return 1 ;;
   esac
   case "$owner$name" in
      *[!A-Za-z0-9._-]*) return 1 ;;
   esac
   return 0
}

if ! validate_repo "$repo"; then
   echo "Invalid NYAACORD_REPO '$repo': expected owner/name" >&2
   exit 1
fi

# LD_PRELOAD splits on spaces and colons, and desktop Exec needs quoting for the rest.
case $home in
   *[[:space:]:\"\$\`\\\']*)
      echo "Nyaacord can't be installed under $home: set XDG_DATA_HOME to a path without spaces, colons or quotes." >&2
      exit 1
      ;;
esac

script_dir=
repo_root=
if [ -f "$0" ]; then
   script_dir=$(cd "$(dirname -- "$0")" && pwd)
   if [ -f "$script_dir/../Cargo.toml" ]; then
      repo_root=$(cd "$script_dir/.." && pwd)
   fi
fi

parse_workspace_version() {
   if [ -n "$repo_root" ] && [ -f "$repo_root/Cargo.toml" ] && command -v python3 >/dev/null 2>&1; then
      python3 - "$repo_root/Cargo.toml" <<'PY'
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
   else
      printf '%s\n' "0.1.0"
   fi
}

normalize_version() {
   v=$(printf '%s' "$1" | tr -d '\r\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//;s/^[vV]//')
   printf '%s\n' "$v"
}

seed_version=$(normalize_version "$install_version")
if [ -z "$seed_version" ]; then
   seed_version=$(normalize_version "$(parse_workspace_version)")
fi
if [ -z "$seed_version" ]; then
   seed_version="0.1.0"
fi

if [ -n "$local_loader" ] && { [ ! -f "$local_loader" ] || [ ! -r "$local_loader" ]; }; then
   echo "Cannot read local loader at $local_loader" >&2
   exit 1
fi

if [ -n "$arg_path" ]; then
   if [ ! -f "$arg_path" ]; then
      echo "No file at $arg_path" >&2
      exit 1
   fi
   arg_path=$(realpath "$arg_path")
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Single-quotes a value for the generated launcher.
q() { printf "'%s'" "$(printf %s "$1" | sed "s/'/'\\\\''/g")"; }

is_type2_appimage() {
   [ -f "$1" ] && [ -r "$1" ] || return 1
   hex=$(od -An -tx1 -N11 "$1" 2>/dev/null | tr -d ' \t\r\n')
   case "$hex" in
      7f454c46????????414902) return 0 ;;
      *) return 1 ;;
   esac
}

extract_from_appimage() {
   src=$1
   rel=$2
   dst=$3
   is_type2_appimage "$src" || return 1
   rm -rf "$WORK/extract"
   mkdir -p "$WORK/extract"
   exec_src=$src
   if [ ! -x "$exec_src" ]; then
      cp "$src" "$WORK/extract/input.AppImage"
      chmod +x "$WORK/extract/input.AppImage"
      exec_src="$WORK/extract/input.AppImage"
   fi
   if (cd "$WORK/extract" && env -u LD_PRELOAD -u APPIMAGE -u APPDIR -u OWD -u ARGV0 "$exec_src" --appimage-extract "$rel" >/dev/null 2>&1) &&
      [ -f "$WORK/extract/squashfs-root/$rel" ]; then
      cp "$WORK/extract/squashfs-root/$rel" "$dst"
      return 0
   fi
   return 1
}

is_nyaacord_appimage() {
   extract_from_appimage "$1" "usr/lib/nyaacord/version" "$WORK/detected-version"
}

find_discord_phoenix() {
   found=
   found=$(command -v discord-phoenix 2>/dev/null || true)
   if [ -z "$found" ] && command -v python3 >/dev/null 2>&1; then
      found=$(python3 - "$PWD" "$HOME/Applications" "$HOME/Downloads" "$HOME/Desktop" <<'PY'
import glob, os, sys
candidates = []
for base in sys.argv[1:]:
    if not base or not os.path.isdir(base):
        continue
    for pat in ("discord-phoenix*.AppImage", "*[Dd]iscord*.AppImage"):
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
)
   fi
   if [ -z "$found" ]; then
      for dir in "$PWD" "$HOME/Applications" "$HOME/Downloads" "$HOME/Desktop"; do
         [ -d "$dir" ] || continue
         for candidate in "$dir"/discord-phoenix*.AppImage "$dir"/*[Dd]iscord*.AppImage; do
            [ -f "$candidate" ] || continue
            case "$(basename -- "$candidate")" in
               [Nn]yaa[Cc]ord*) continue ;;
            esac
            found=$candidate
            break 2
         done
      done
   fi
   if [ -n "$found" ]; then
      realpath "$found"
   fi
}

install_updater_binary() {
   for cand in "$home/Nyaacord-x86_64.AppImage" "$home/NyaaCord-x86_64.AppImage"; do
      if [ -f "$cand" ] &&
         extract_from_appimage "$cand" "usr/bin/nyaacord-updater" "$WORK/nyaacord-updater"; then
         tmp=$(mktemp "$home/nyaacord-updater.XXXXXX")
         if cp "$WORK/nyaacord-updater" "$tmp" && chmod +x "$tmp" && mv -f "$tmp" "$home/nyaacord-updater"; then
            return 0
         fi
         rm -f "$tmp"
      fi
   done
   if [ -n "$repo_root" ] && [ -x "$repo_root/target/release/nyaacord-updater" ]; then
      tmp=$(mktemp "$home/nyaacord-updater.XXXXXX")
      if cp "$repo_root/target/release/nyaacord-updater" "$tmp" && chmod +x "$tmp" && mv -f "$tmp" "$home/nyaacord-updater"; then
         return 0
      fi
      rm -f "$tmp"
   fi
   if [ -n "$repo_root" ] && [ -f "$repo_root/dist/Nyaacord-x86_64.AppImage" ] &&
      extract_from_appimage "$repo_root/dist/Nyaacord-x86_64.AppImage" "usr/bin/nyaacord-updater" "$WORK/nyaacord-updater"; then
      tmp=$(mktemp "$home/nyaacord-updater.XXXXXX")
      if cp "$WORK/nyaacord-updater" "$tmp" && chmod +x "$tmp" && mv -f "$tmp" "$home/nyaacord-updater"; then
         return 0
      fi
      rm -f "$tmp"
   fi
   if [ -n "$local_loader" ] && [ -x "$(dirname -- "$local_loader")/nyaacord-updater" ]; then
      tmp=$(mktemp "$home/nyaacord-updater.XXXXXX")
      if cp "$(dirname -- "$local_loader")/nyaacord-updater" "$tmp" && chmod +x "$tmp" && mv -f "$tmp" "$home/nyaacord-updater"; then
         return 0
      fi
      rm -f "$tmp"
   fi
   if [ -n "$repo_root" ] && command -v cargo >/dev/null 2>&1; then
      echo "Building nyaacord-updater from checkout..."
      (cd "$repo_root" && NYAACORD_VERSION="$seed_version" cargo build --release -p phoenix-updater)
      if [ -x "$repo_root/target/release/nyaacord-updater" ]; then
         tmp=$(mktemp "$home/nyaacord-updater.XXXXXX")
         if cp "$repo_root/target/release/nyaacord-updater" "$tmp" && chmod +x "$tmp" && mv -f "$tmp" "$home/nyaacord-updater"; then
            return 0
         fi
         rm -f "$tmp"
      fi
   fi
   return 1
}

install_plugin_bundle() {
   loader_path=$1
   beside="$(dirname -- "$loader_path")/libplm.plugins.so"
   if [ -r "$beside" ]; then
      tmp=$(mktemp "$home/libplm.plugins.so.XXXXXX")
      cp "$beside" "$tmp"
      chmod 0644 "$tmp"
      mv -f "$tmp" "$home/libplm.plugins.so"
      return 0
   fi
   [ -n "$repo_root" ] || return 0
   command -v python3 >/dev/null 2>&1 || return 0

   esbuild_bin=${ESBUILD:-}
   if [ -z "$esbuild_bin" ] || [ ! -x "$esbuild_bin" ]; then
      esbuild_bin=$(command -v esbuild 2>/dev/null || true)
   fi
   if [ -z "$esbuild_bin" ] && [ -x "$repo_root/.cache/esbuild/node_modules/.bin/esbuild" ]; then
      esbuild_bin="$repo_root/.cache/esbuild/node_modules/.bin/esbuild"
   fi

   mkdir -p "$WORK/plugins"
   bundled_count=0
   for name in $DEFAULT_PLUGINS; do
      entry_ts="$repo_root/plugins/$name/src/index.ts"
      pre_js="$repo_root/web/plugins/$name.js"
      if [ -n "$esbuild_bin" ] && [ -f "$entry_ts" ]; then
         header=$(python3 - "$entry_ts" <<'PY'
import sys
out = []
for line in open(sys.argv[1], encoding="utf-8").read().splitlines():
    if line.startswith("// @"):
        out.append(line)
    else:
        break
sys.stdout.write("\n".join(out))
PY
)
         "$esbuild_bin" "$entry_ts" --bundle --format=iife --target=es2023 \
            --log-level=warning "--banner:js=$header" "--outfile=$WORK/plugins/$name.js"
         bundled_count=$((bundled_count + 1))
      elif [ -r "$pre_js" ]; then
         cp "$pre_js" "$WORK/plugins/$name.js"
         bundled_count=$((bundled_count + 1))
      fi
   done

   if [ "$bundled_count" -gt 0 ]; then
      tmp=$(mktemp "$home/libplm.plugins.so.XXXXXX")
      if python3 - "$WORK/plugins" "$tmp" <<'PY'
import json, os, sys
src_dir, out_path = sys.argv[1], sys.argv[2]
bundle = {}
for fname in sorted(os.listdir(src_dir)):
    if fname.endswith(".js"):
        with open(os.path.join(src_dir, fname), encoding="utf-8") as fh:
            bundle[fname[:-3]] = fh.read()
with open(out_path, "w", encoding="utf-8") as fh:
    json.dump(bundle, fh, ensure_ascii=False)
PY
      then
         chmod 0644 "$tmp"
         mv -f "$tmp" "$home/libplm.plugins.so"
      else
         rm -f "$tmp"
         return 1
      fi
   fi
}

verify_sha256sums_file() {
   sums_file=$1
   target_file=$2
   asset_name=$3
   expected=$(awk -v target="$asset_name" '
      /^[[:space:]]*#/ || /^[[:space:]]*$/ { next }
      {
         h = tolower($1)
         if (h !~ /^[0-9a-f]{64}$/) next
         f = $2
         sub(/^\*/, "", f)
         sub(/^\.\//, "", f)
         if (f == target) {
            print h
            exit 0
         }
      }
   ' "$sums_file")
   if [ -z "$expected" ]; then
      echo "SHA256SUMS.txt does not contain a valid checksum for $asset_name" >&2
      return 1
   fi
   actual=
   if command -v sha256sum >/dev/null 2>&1; then
      actual=$(sha256sum "$target_file" | awk '{print tolower($1)}')
   elif command -v python3 >/dev/null 2>&1; then
      actual=$(python3 - "$target_file" <<'PY'
import hashlib, sys
h = hashlib.sha256()
with open(sys.argv[1], "rb") as fh:
    for chunk in iter(lambda: fh.read(65536), b""):
        h.update(chunk)
print(h.hexdigest())
PY
)
   else
      echo "sha256sum or python3 is required to verify download integrity" >&2
      return 1
   fi
   if [ "$actual" != "$expected" ]; then
      echo "Checksum mismatch for $asset_name: expected $expected, got $actual" >&2
      return 1
   fi
   return 0
}

mkdir -p "$home" "$data/applications"
rm -f "$home"/sunbird.png "$home"/sunbird-*.png

managed_appimage="$home/Nyaacord-x86_64.AppImage"
discord=
mode=

if [ -n "$local_loader" ]; then
   mode=local_loader
   [ -z "$arg_path" ] || discord=$arg_path
elif [ -n "$arg_path" ]; then
   if is_nyaacord_appimage "$arg_path"; then
      mode=appimage_file
   else
      mode=local_loader
      discord=$arg_path
   fi
elif [ -n "$repo_root" ] && [ -f "$repo_root/dist/Nyaacord-x86_64.AppImage" ] &&
   is_nyaacord_appimage "$repo_root/dist/Nyaacord-x86_64.AppImage"; then
   arg_path="$repo_root/dist/Nyaacord-x86_64.AppImage"
   mode=appimage_file
elif [ -n "$repo_root" ] && [ -r "$repo_root/target/release/libphoenix.so" ]; then
   mode=local_loader
else
   mode=download_release
fi

case "$mode" in
   appimage_file)
      tmp=$(mktemp "$home/Nyaacord-x86_64.AppImage.XXXXXX")
      if ! cp "$arg_path" "$tmp" || ! chmod +x "$tmp" || ! mv -f "$tmp" "$managed_appimage"; then
         rm -f "$tmp"
         echo "Failed to install AppImage to $managed_appimage" >&2
         exit 1
      fi
      detected_ver=
      if extract_from_appimage "$managed_appimage" "usr/lib/nyaacord/version" "$WORK/app-ver"; then
         detected_ver=$(normalize_version "$(cat "$WORK/app-ver")")
      fi
      if [ -n "$install_version" ]; then
         printf '%s\n' "$(normalize_version "$install_version")" > "$home/version"
      elif [ -n "$detected_ver" ]; then
         printf '%s\n' "$detected_ver" > "$home/version"
      else
         printf '%s\n' "$seed_version" > "$home/version"
      fi
      rm -f "$home/libphoenix.so" "$home/libplm.plugins.so" \
         "$home/NyaaCord-x86_64.AppImage" "$home/NyaaCord-x86_64.AppImage.previous"
      install_updater_binary || true
      echo "Installed Nyaacord AppImage to $managed_appimage"
      ;;
   local_loader)
      loader_src=$local_loader
      if [ -z "$loader_src" ] && [ -n "$repo_root" ] && [ -r "$repo_root/target/release/libphoenix.so" ]; then
         loader_src="$repo_root/target/release/libphoenix.so"
      fi
      if [ -z "$loader_src" ]; then
         echo "No local libphoenix.so found. Build it first or pass a Nyaacord AppImage." >&2
         exit 1
      fi
      if [ -z "$discord" ]; then
         discord=$(find_discord_phoenix || true)
      fi
      if [ -z "$discord" ]; then
         cat >&2 <<EOF
Couldn't find Discord Phoenix. Looked for discord-phoenix on PATH and for a
Discord AppImage in $PWD, ~/Applications, ~/Downloads and ~/Desktop.

Pass the path to Discord Phoenix's AppImage:
  $0 /path/to/discord-phoenix.AppImage
EOF
         exit 1
      fi
      [ -x "$discord" ] || chmod +x "$discord"
      rm -f "$managed_appimage" "$home/NyaaCord-x86_64.AppImage" "$home/NyaaCord-x86_64.AppImage.previous"
      tmp=$(mktemp "$home/libphoenix.so.XXXXXX")
      if ! cp "$loader_src" "$tmp" || ! mv -f "$tmp" "$home/libphoenix.so"; then
         rm -f "$tmp"
         echo "Failed to install loader to $home/libphoenix.so" >&2
         exit 1
      fi
      install_plugin_bundle "$loader_src"
      printf '%s\n' "$seed_version" > "$home/version"
      install_updater_binary || true
      echo "Installed local loader $loader_src (version $seed_version) for $discord"
      ;;
   download_release)
      echo "Downloading latest Nyaacord release from $repo..."
      if install_updater_binary && [ -x "$home/nyaacord-updater" ]; then
         "$home/nyaacord-updater" update --current "0.0.0" --target "$managed_appimage" --repo "$repo"
      else
         url="https://github.com/$repo/releases/latest/download/Nyaacord-x86_64.AppImage"
         sums_url="https://github.com/$repo/releases/latest/download/SHA256SUMS.txt"
         sums_file="$WORK/SHA256SUMS.txt"
         if ! curl -fsSL --proto =https --proto-redir =https -o "$sums_file" "$sums_url" || [ ! -s "$sums_file" ]; then
            echo "Failed to download $sums_url" >&2
            exit 1
         fi
         tmp=$(mktemp "$home/Nyaacord-x86_64.AppImage.XXXXXX")
         if ! curl -fsSL --proto =https --proto-redir =https -o "$tmp" "$url" || [ ! -s "$tmp" ]; then
            rm -f "$tmp"
            echo "Failed to download $url" >&2
            exit 1
         fi
         if ! verify_sha256sums_file "$sums_file" "$tmp" "Nyaacord-x86_64.AppImage" || ! is_type2_appimage "$tmp"; then
            rm -f "$tmp"
            echo "Downloaded release failed verification" >&2
            exit 1
         fi
         chmod +x "$tmp"
         mv -f "$tmp" "$managed_appimage"
      fi
      if extract_from_appimage "$managed_appimage" "usr/lib/nyaacord/version" "$WORK/app-ver"; then
         detected_ver=$(normalize_version "$(cat "$WORK/app-ver")")
         printf '%s\n' "${detected_ver:-$seed_version}" > "$home/version"
      else
         printf '%s\n' "$seed_version" > "$home/version"
      fi
      rm -f "$home/libphoenix.so" "$home/libplm.plugins.so" \
         "$home/NyaaCord-x86_64.AppImage" "$home/NyaaCord-x86_64.AppImage.previous"
      install_updater_binary || true
      echo "Downloaded Nyaacord AppImage to $managed_appimage"
      ;;
esac

icon_tmp=$(mktemp "$home/nyaacord.png.XXXXXX")
if [ -f "$managed_appimage" ] && extract_from_appimage "$managed_appimage" "nyaacord.png" "$icon_tmp" && [ -s "$icon_tmp" ]; then
   chmod 0644 "$icon_tmp" && mv -f "$icon_tmp" "$home/nyaacord.png"
elif [ -n "$repo_root" ] && [ -r "$repo_root/web/plugins/nyaacord.png" ]; then
   cp "$repo_root/web/plugins/nyaacord.png" "$icon_tmp" && chmod 0644 "$icon_tmp" && mv -f "$icon_tmp" "$home/nyaacord.png"
fi
rm -f "$icon_tmp"

gif_tmp=$(mktemp "$home/nyaacord.gif.XXXXXX")
if [ -n "$repo_root" ] && [ -r "$repo_root/web/plugins/nyaacord.gif" ]; then
   cp "$repo_root/web/plugins/nyaacord.gif" "$gif_tmp" && chmod 0644 "$gif_tmp" && mv -f "$gif_tmp" "$home/nyaacord.gif"
elif [ -f "$managed_appimage" ] && extract_from_appimage "$managed_appimage" "usr/share/nyaacord/nyaacord.gif" "$gif_tmp" && [ -s "$gif_tmp" ]; then
   chmod 0644 "$gif_tmp" && mv -f "$gif_tmp" "$home/nyaacord.gif"
fi
rm -f "$gif_tmp"

icon=com.discord.libdiscord
[ ! -f "$home/nyaacord.png" ] || icon=$home/nyaacord.png

cat > "$home/sunbird" <<EOF
#!/bin/sh
set -eu

home=$(q "$home")
discord=$(q "$discord")
default_repo=$(q "$repo")
managed="\$home/Nyaacord-x86_64.AppImage"
updater="\$home/nyaacord-updater"

export NYAACORD_MANAGED_APPIMAGE="\$managed"
export NYAACORD_LAUNCHER="\$home/sunbird"
export NYAACORD_ICON="\$home/nyaacord.png"
export NYAACORD_REPO="\${NYAACORD_REPO:-\$default_repo}"

managed_version() {
   env -u LD_PRELOAD -u APPIMAGE -u APPDIR -u OWD -u ARGV0 NYAACORD_AUTO_UPDATE=0 "\$managed" --version 2>/dev/null \
      | sed -n 's/^Nyaa[Cc]ord //p' | head -n 1 | tr -d '\r' | sed 's/^[vV]//' || true
}

current_version() {
   if [ -x "\$managed" ]; then
      v=\$(managed_version)
      if [ -n "\$v" ]; then
         printf '%s\n' "\$v"
         return 0
      fi
   fi
   if [ -f "\$home/version" ]; then
      v=\$(tr -d '\r\n' < "\$home/version" | sed 's/^[vV]//')
      if [ -n "\$v" ]; then
         printf '%s\n' "\$v"
         return 0
      fi
   fi
   printf '%s\n' "0.0.0"
}

is_type2_appimage() {
   [ -f "\$1" ] && [ -r "\$1" ] || return 1
   hex=\$(od -An -tx1 -N11 "\$1" 2>/dev/null | tr -d ' \t\r\n')
   case "\$hex" in
      7f454c46????????414902) return 0 ;;
      *) return 1 ;;
   esac
}

sync_from_managed() {
   [ -x "\$managed" ] || return 0
   v=\$(managed_version)
   if [ -n "\$v" ]; then
      printf '%s\n' "\$v" > "\$home/version"
   fi
   is_type2_appimage "\$managed" || return 0
   tmpd=\$(mktemp -d)
   if (cd "\$tmpd" && env -u LD_PRELOAD -u APPIMAGE -u APPDIR -u OWD -u ARGV0 "\$managed" --appimage-extract usr/bin/nyaacord-updater >/dev/null 2>&1) &&
      [ -f "\$tmpd/squashfs-root/usr/bin/nyaacord-updater" ]; then
      utmp=\$(mktemp "\$home/nyaacord-updater.XXXXXX")
      if cp "\$tmpd/squashfs-root/usr/bin/nyaacord-updater" "\$utmp" && chmod +x "\$utmp" && mv -f "\$utmp" "\$updater"; then
         :
      else
         rm -f "\$utmp"
      fi
   fi
   if (cd "\$tmpd" && env -u LD_PRELOAD -u APPIMAGE -u APPDIR -u OWD -u ARGV0 "\$managed" --appimage-extract nyaacord.png >/dev/null 2>&1) &&
      [ -s "\$tmpd/squashfs-root/nyaacord.png" ]; then
      itmp=\$(mktemp "\$home/nyaacord.png.XXXXXX")
      if cp "\$tmpd/squashfs-root/nyaacord.png" "\$itmp" && chmod 0644 "\$itmp" && mv -f "\$itmp" "\$home/nyaacord.png"; then
         :
      else
         rm -f "\$itmp"
      fi
   fi
   if (cd "\$tmpd" && env -u LD_PRELOAD -u APPIMAGE -u APPDIR -u OWD -u ARGV0 "\$managed" --appimage-extract usr/share/nyaacord/nyaacord.gif >/dev/null 2>&1) &&
      [ -s "\$tmpd/squashfs-root/usr/share/nyaacord/nyaacord.gif" ]; then
      gtmp=\$(mktemp "\$home/nyaacord.gif.XXXXXX")
      if cp "\$tmpd/squashfs-root/usr/share/nyaacord/nyaacord.gif" "\$gtmp" && chmod 0644 "\$gtmp" && mv -f "\$gtmp" "\$home/nyaacord.gif"; then
         :
      else
         rm -f "\$gtmp"
      fi
   fi
   rm -f "\$home/NyaaCord-x86_64.AppImage" "\$home/NyaaCord-x86_64.AppImage.previous"
   rm -rf "\$tmpd"
}

usage() {
   cat <<'USAGE'
Usage: sunbird [options] [-- Discord arguments]

Options:
  --check-update        Check GitHub Releases for a newer Nyaacord version
  --update              Download and install the latest Nyaacord AppImage
  --rollback            Restore the previous Nyaacord AppImage backup
  --no-runtime          Launch with only the access gate opened
  --version             Print installed Nyaacord version
  --help                Show this help message
USAGE
}

case "\${1:-}" in
   --version|--help|-h|--rollback) ;;
   *)
      if [ -x "\$managed" ]; then
         if [ ! -x "\$updater" ] || [ -n "\$(find "\$managed" -prune -newer "\$updater" 2>/dev/null || true)" ]; then
            sync_from_managed
         fi
      fi
      ;;
esac

cur_ver=\$(current_version)

case "\${1:-}" in
   --version)
      printf 'Nyaacord %s\n' "\$cur_ver"
      exit 0
      ;;
   --help|-h)
      usage
      exit 0
      ;;
   --check-update)
      shift
      if [ ! -x "\$updater" ]; then
         echo "nyaacord-updater is missing at \$updater" >&2
         exit 1
      fi
      exec "\$updater" check --current "\$cur_ver" "\$@"
      ;;
   --update)
      shift
      if [ ! -x "\$updater" ]; then
         echo "nyaacord-updater is missing at \$updater" >&2
         exit 1
      fi
      set +e
      "\$updater" update --current "\$cur_ver" --target "\$managed" "\$@"
      rc=\$?
      set -e
      if [ "\$rc" -eq 0 ]; then
         sync_from_managed
      fi
      exit "\$rc"
      ;;
   --rollback)
      shift
      if [ ! -x "\$updater" ]; then
         echo "nyaacord-updater is missing at \$updater" >&2
         exit 1
      fi
      set +e
      "\$updater" rollback --target "\$managed" "\$@"
      rc=\$?
      set -e
      if [ "\$rc" -eq 0 ]; then
         sync_from_managed
      fi
      exit "\$rc"
      ;;
esac

while [ "\${1:-}" = "--no-runtime" ] || [ "\${1:-}" = "--gate-only" ]; do
   export SUNBIRD_GATE_ONLY=1
   shift
done
if [ "\${1:-}" = "--" ]; then
   shift
fi

if [ "\${NYAACORD_NO_AUTO_UPDATE:-0}" != "1" ] && [ -x "\$updater" ]; then
   before_ver=\$cur_ver
   "\$updater" update --current "\$before_ver" --target "\$managed" --timeout 5 --quiet || true
   if [ -x "\$managed" ]; then
      new_v=\$(managed_version)
      if [ -n "\$new_v" ] && [ "\$new_v" != "\$before_ver" ]; then
         sync_from_managed
      fi
   fi
fi

if [ -x "\$managed" ]; then
   if [ -f "\$managed.previous" ]; then
      m_id=\$(stat -c '%d:%i' "\$managed" 2>/dev/null || true)
      p_id=\$(stat -c '%d:%i' "\$managed.previous" 2>/dev/null || true)
      if [ -n "\$m_id" ] && [ "\$m_id" != "\$p_id" ]; then
         before_sig=\$(stat -c '%d:%i:%Y' "\$managed" 2>/dev/null || true)
         start_ts=\$(date +%s)
         set +e
         NYAACORD_AUTO_UPDATE=0 "\$managed" "\$@"
         rc=\$?
         set -e
         end_ts=\$(date +%s)
         elapsed=\$((end_ts - start_ts))
         after_sig=\$(stat -c '%d:%i:%Y' "\$managed" 2>/dev/null || true)
         if [ "\$rc" -ne 0 ] && [ "\$elapsed" -le 4 ] && [ "\$after_sig" = "\$before_sig" ] && [ -x "\$updater" ]; then
            if "\$updater" rollback --target "\$managed"; then
               sync_from_managed
               if command -v notify-send >/dev/null 2>&1; then
                  if [ -f "\$home/nyaacord.png" ]; then
                     notify-send --app-name=Nyaacord --hint=string:desktop-entry:com.discord.libdiscord -i "\$home/nyaacord.png" "Nyaacord" "Update failed to start. Rolled back to previous version." || true
                  else
                     notify-send --app-name=Nyaacord --hint=string:desktop-entry:com.discord.libdiscord "Nyaacord" "Update failed to start. Rolled back to previous version." || true
                  fi
               fi
               exec env NYAACORD_AUTO_UPDATE=0 "\$managed" "\$@"
            fi
         fi
         exit "\$rc"
      fi
   fi
   exec env NYAACORD_AUTO_UPDATE=0 "\$managed" "\$@"
fi

if [ -z "\$discord" ] || [ ! -x "\$discord" ]; then
   echo "No managed Nyaacord AppImage and no Discord Phoenix client found." >&2
   exit 1
fi
if [ ! -r "\$home/libphoenix.so" ]; then
   echo "Cannot read the Nyaacord loader at \$home/libphoenix.so" >&2
   exit 1
fi

norm_dir() {
   d=\$1
   resolved=\$(readlink -m "\$d" 2>/dev/null || printf '%s' "\$d")
   while [ "\${resolved%/}" != "\$resolved" ] && [ "\$resolved" != "/" ]; do
      resolved=\${resolved%/}
   done
   printf '%s' "\$resolved"
}

resolve_data_dir() {
   if [ -n "\$1" ]; then
      norm_dir "\$1"
   elif [ -n "\$2" ]; then
      norm_dir "\$2/discord-phoenix"
   elif [ -n "\$3" ]; then
      norm_dir "\$3/.local/share/discord-phoenix"
   else
      printf ''
   fi
}

my_uid=\$(id -u)
cur_data_dir=\$(resolve_data_dir "\${LIBDISCORD_DATA_DIR:-}" "\${XDG_DATA_HOME:-}" "\${HOME:-}")
pids=
patched=
if [ -n "\$cur_data_dir" ]; then
   for p in /proc/[0-9]*; do
      [ -d "\$p" ] || continue
      pid=\${p#/proc/}
      [ "\$pid" != "\$\$" ] || continue
      exe=\$(readlink "\$p/exe" 2>/dev/null || true)
      case "\$exe" in
         */discord-phoenix|*/discord-phoenix' (deleted)') ;;
         *) continue ;;
      esac
      p_uid=\$(stat -c '%u' "\$p" 2>/dev/null || true)
      [ "\$p_uid" = "\$my_uid" ] || continue
      p_env=\$(tr '\0' '\n' < "\$p/environ" 2>/dev/null || true)
      [ -n "\$p_env" ] || continue
      p_lib=\$(printf '%s\n' "\$p_env" | sed -n 's/^LIBDISCORD_DATA_DIR=//p' | head -n 1)
      p_xdg=\$(printf '%s\n' "\$p_env" | sed -n 's/^XDG_DATA_HOME=//p' | head -n 1)
      p_home=\$(printf '%s\n' "\$p_env" | sed -n 's/^HOME=//p' | head -n 1)
      p_data_dir=\$(resolve_data_dir "\$p_lib" "\$p_xdg" "\$p_home")
      [ "\$p_data_dir" = "\$cur_data_dir" ] || continue
      pids="\$pids \$pid"
      if grep -q libphoenix "\$p/maps" 2>/dev/null; then
         patched=1
      fi
   done
fi

if [ -n "\$pids" ] && [ -z "\$patched" ]; then
   kill \$pids 2>/dev/null || true
   i=0
   while [ "\$i" -lt 10 ]; do
      alive=
      for pid in \$pids; do
         if kill -0 "\$pid" 2>/dev/null; then
            alive=1
         fi
      done
      if [ -z "\$alive" ]; then
         break
      fi
      sleep 0.5
      i=\$((i + 1))
   done
fi

export LD_PRELOAD="\$home/libphoenix.so\${LD_PRELOAD:+:\$LD_PRELOAD}"
exec "\$discord" "\$@"
EOF
chmod +x "$home/sunbird"

exec_args=
[ -n "$no_runtime" ] && exec_args=" --no-runtime"
# Omit X-Discord-AppImage=true so Phoenix preserves this launcher entry.
entry_tmp=$(mktemp "$data/applications/com.discord.libdiscord.desktop.XXXXXX")
cat > "$entry_tmp" <<EOF
[Desktop Entry]
Type=Application
Name=Nyaacord
GenericName=Internet Messenger
Comment=Nyaacord for Discord Phoenix
Exec="$home/sunbird"$exec_args %U
Icon=$icon
StartupWMClass=com.discord.libdiscord
StartupNotify=true
Terminal=false
Categories=Network;InstantMessaging;
MimeType=x-scheme-handler/discord;
X-Nyaacord-Managed=true
EOF
chmod 0644 "$entry_tmp"
mv -f "$entry_tmp" "$entry"
rm -f "$legacy_entry"
refresh_desktop_cache "$data/applications"

echo "Nyaacord is installed. Open \"Nyaacord\" from your app menu or run $home/sunbird."
