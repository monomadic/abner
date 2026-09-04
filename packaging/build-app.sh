#!/usr/bin/env bash
# Build a self-contained Abner.app macOS bundle. Same recipe as
# switchblade's packaging/build-app.sh, minus what abner doesn't have
# (document icons, random-pick asset folders).
#
# The release binary links against Homebrew's ffmpeg dylibs
# (/opt/homebrew/opt/ffmpeg/lib/*.dylib) with absolute paths — fine for
# `cargo run` on a dev machine, fatal for anything handed to someone else:
# Gatekeeper-quarantined apps don't inherit a shell PATH, homebrew may live
# at /usr/local instead of /opt/homebrew (Intel), or may not be installed
# at all. This script:
#
#   1. cargo builds the release binary
#   2. assembles the standard Contents/{MacOS,Resources,Frameworks} layout
#   3. renders assets/app-icon.png to Resources/AppIcon.icns
#   4. writes Info.plist (version + git hash baked in)
#   5. copies every non-system dylib the binary (transitively) links
#      against into Contents/Frameworks and rewrites LC_LOAD_DYLIB entries
#      to @rpath, so the bundle carries its own ffmpeg instead of pointing
#      at wherever it happened to be built
#   6. ad-hoc codesigns the whole bundle (required on Apple Silicon —
#      install_name_tool invalidates any prior signature)
#
# The app still shells out to the `ffprobe` CLI at startup (probe.rs) —
# that isn't linked, so it isn't bundled here. The launcher script
# prepends the common Homebrew bin dirs to PATH before exec'ing the real
# binary so a Finder-launched app (which gets a minimal PATH, no ~/.zshrc)
# can still find it if Homebrew is installed. Ship it alongside the app
# (see --with-cli-tools) for an install that doesn't depend on Homebrew.
#
# Flags:
#   --debug           build the debug profile instead of release
#   --sign <identity>  codesign with a real identity instead of ad-hoc ("-")
#   --with-cli-tools  copy ffmpeg/ffprobe into the bundle too
#   --install         copy the built app to /Applications (replacing any
#                      existing Abner.app there)
#   --open            launch the app once it's built (or installed)
#
# Either way it re-registers the bundle with LaunchServices afterwards —
# an in-place replacement otherwise keeps the OLD icon in Finder and the
# Dock (see the note by the lsregister call).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE=release
CARGO_PROFILE_FLAG=--release
APP_NAME="Abner"
BIN_NAME="abner"
BUNDLE_ID="com.abner.app"
SIGN_IDENTITY="-"   # "-" = ad-hoc; pass --sign "Developer ID Application: ..." for a real one
WITH_CLI_TOOLS=0
OPEN_AFTER=0
INSTALL_AFTER=0

while [ $# -gt 0 ]; do
  case "$1" in
    --debug) PROFILE=debug; CARGO_PROFILE_FLAG=;;
    --sign) SIGN_IDENTITY="$2"; shift ;;
    --with-cli-tools) WITH_CLI_TOOLS=1 ;;
    --open) OPEN_AFTER=1 ;;
    --install) INSTALL_AFTER=1 ;;
    -h|--help)
      sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
  shift
done

if [ "$(uname)" != "Darwin" ]; then
  echo "error: macOS app bundles can only be built on macOS" >&2
  exit 1
fi

VERSION="$(awk -F'"' '/^version/{print $2; exit}' Cargo.toml)"
GIT_HASH="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
if ! git diff --quiet --ignore-submodules 2>/dev/null || ! git diff --quiet --cached --ignore-submodules 2>/dev/null; then
  GIT_HASH="${GIT_HASH}-dirty"
fi

echo "==> building $BIN_NAME ($PROFILE, v$VERSION-$GIT_HASH)"
cargo build $CARGO_PROFILE_FLAG
BIN="$ROOT/target/$PROFILE/$BIN_NAME"
[ -x "$BIN" ] || { echo "error: build produced no binary at $BIN" >&2; exit 1; }

OUT_DIR="$ROOT/target/$PROFILE/bundle"
APP="$OUT_DIR/$APP_NAME.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
FRAMEWORKS="$CONTENTS/Frameworks"

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES" "$FRAMEWORKS"

# --- binary + launcher -------------------------------------------------
# CFBundleExecutable is a thin launcher, not the real binary: Finder gives
# a launched app almost no PATH (no /opt/homebrew/bin), which is exactly
# where the ffprobe CLI the startup probe shells out to usually lives.
cp "$BIN" "$MACOS/$BIN_NAME-bin"
chmod +w "$MACOS/$BIN_NAME-bin"

cat > "$MACOS/$BIN_NAME" <<'LAUNCHER'
#!/bin/bash
# Thin launcher: a GUI-launched app inherits almost no PATH, so the
# ffprobe lookup (probe.rs) would otherwise fail even though the exact
# same binary works fine from a terminal.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PATH="$DIR:/opt/homebrew/bin:/usr/local/bin:$PATH"
exec "$DIR/abner-bin" "$@"
LAUNCHER
chmod +x "$MACOS/$BIN_NAME"

if [ "$WITH_CLI_TOOLS" = 1 ]; then
  echo "==> bundling ffmpeg/ffprobe CLIs"
  for tool in ffmpeg ffprobe; do
    src="$(command -v "$tool" || true)"
    if [ -z "$src" ]; then
      echo "    warning: $tool not found on PATH, skipping (--with-cli-tools)" >&2
      continue
    fi
    cp "$src" "$MACOS/$tool"
    chmod +w "$MACOS/$tool"
  done
fi

# --- icon ------------------------------------------------------------------
# 1024px base PNG → .icns (all the standard sizes via sips + iconutil).
make_icns() {
  local src="$1" out="$2"
  local iconset size d
  iconset="$(mktemp -d)/icon.iconset"
  mkdir -p "$iconset"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$src" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    d=$((size * 2))
    sips -z "$d" "$d" "$src" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$iconset" -o "$out"
  rm -rf "$(dirname "$iconset")"
}

echo "==> rendering AppIcon.icns"
# assets/app-icon.png is the app icon SLOT: a generic path, so swapping
# the icon is dropping a new square PNG there rather than editing this
# script and main.rs's include_bytes! in lockstep (both read this file).
# The alternates in assets/icons/ become the icon by being copied over it.
ICON_SRC="$ROOT/assets/app-icon.png"
[ -f "$ICON_SRC" ] || { echo "error: $ICON_SRC missing" >&2; exit 1; }
make_icns "$ICON_SRC" "$RESOURCES/AppIcon.icns"

# --- Info.plist ------------------------------------------------------------
sed \
  -e "s/{{VERSION}}/$VERSION/g" \
  -e "s/{{BUILD}}/$GIT_HASH/g" \
  -e "s/{{BUNDLE_ID}}/$BUNDLE_ID/g" \
  "$ROOT/packaging/Info.plist.in" > "$CONTENTS/Info.plist"

# --- bundle dylibs -----------------------------------------------------
# Walk the dependency graph (main binary, then every dylib we copy in) and
# rewrite non-system LC_LOAD_DYLIB entries to @rpath/<name>, copying each
# referenced dylib into Contents/Frameworks exactly once. This is a small
# hand-rolled dylibbundler so the build has no extra tool dependency.
#
# Tracks "already bundled" as a newline-delimited string rather than an
# associative array: macOS ships bash 3.2 (no `declare -A`, GPLv3 froze it
# there), and this script has to run on whatever bash /usr/bin/env finds,
# not whatever bash the developer happens to have from Homebrew.
echo "==> bundling linked dylibs into Contents/Frameworks"
SEEN=$'\n'
DYLIB_COUNT=0

is_system_dep() {
  case "$1" in
    /usr/lib/*|/System/*) return 0 ;;
    *) return 1 ;;
  esac
}

is_seen() {
  case "$SEEN" in
    *$'\n'"$1"$'\n'*) return 0 ;;
    *) return 1 ;;
  esac
}

bundle_deps_of() {
  local target="$1"
  local dep base dest
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    is_system_dep "$dep" && continue
    base="$(basename "$dep")"
    dest="$FRAMEWORKS/$base"
    if ! is_seen "$base"; then
      SEEN="${SEEN}${base}"$'\n'
      DYLIB_COUNT=$((DYLIB_COUNT + 1))
      if [ ! -e "$dest" ]; then
        if [ ! -e "$dep" ]; then
          echo "    warning: $dep (needed by $(basename "$target")) not found, skipping" >&2
          continue
        fi
        cp "$dep" "$dest"
        chmod +w "$dest"
        install_name_tool -id "@rpath/$base" "$dest"
      fi
      # Recurse into what the newly-copied dylib itself depends on.
      bundle_deps_of "$dest"
    fi
    install_name_tool -change "$dep" "@rpath/$base" "$target"
  done < <(otool -L "$target" | tail -n +2 | awk '{print $1}')
}

bundle_deps_of "$MACOS/$BIN_NAME-bin"
install_name_tool -add_rpath "@executable_path/../Frameworks" "$MACOS/$BIN_NAME-bin"
for dylib in "$FRAMEWORKS"/*.dylib; do
  [ -e "$dylib" ] || continue
  install_name_tool -add_rpath "@loader_path/." "$dylib" 2>/dev/null || true
done
echo "    bundled $DYLIB_COUNT dylib(s)"

# --- codesign ------------------------------------------------------------
# install_name_tool invalidates any existing signature; an unsigned (or
# stale-signed) arm64 binary refuses to launch at all, so this step isn't
# optional even for ad-hoc local builds.
echo "==> codesigning (identity: $SIGN_IDENTITY)"
for dylib in "$FRAMEWORKS"/*.dylib; do
  [ -e "$dylib" ] || continue
  codesign --force --sign "$SIGN_IDENTITY" "$dylib"
done
[ "$WITH_CLI_TOOLS" = 1 ] && for tool in "$MACOS/ffmpeg" "$MACOS/ffprobe"; do
  [ -e "$tool" ] && codesign --force --sign "$SIGN_IDENTITY" "$tool"
done
codesign --force --sign "$SIGN_IDENTITY" "$MACOS/$BIN_NAME-bin"
codesign --force --sign "$SIGN_IDENTITY" "$MACOS/$BIN_NAME"
codesign --force --deep --sign "$SIGN_IDENTITY" "$APP"

echo "==> verifying"
codesign --verify --deep --strict "$APP"
otool -L "$MACOS/$BIN_NAME-bin" | sed 's/^/    /'

echo
echo "built: $APP"

if [ "$INSTALL_AFTER" = 1 ]; then
  DEST="/Applications/$APP_NAME.app"
  echo "==> installing to $DEST"
  rm -rf "$DEST"
  ditto "$APP" "$DEST"
  APP="$DEST"
  echo "installed: $APP"
fi

# --- LaunchServices refresh -----------------------------------------------
# Replacing a bundle IN PLACE (rm -rf + ditto over the same path, which is
# what --install does) leaves Finder and the Dock showing the icon of the
# copy that used to be there: LaunchServices keys its record — icon
# included — by bundle path + id, and neither changes, so nothing tells it
# to re-read. A drag-install in Finder does this refresh for you; a script
# has to ask. `touch` moves the bundle's mtime so IconServices treats its
# cached image as stale, and `lsregister -f` re-reads the plist.
#
# Stale registrations for bundles that no longer exist — old target/ builds,
# a deleted worktree — can also answer for the id ahead of the real one, so
# a periodic `lsregister -kill -r -domain local -domain system -domain user`
# is the bigger hammer when an icon still won't budge. It is NOT run here:
# it rebuilds the whole database for every app on the machine.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
echo "==> refreshing LaunchServices for $APP"
touch "$APP"
[ -x "$LSREGISTER" ] && "$LSREGISTER" -f "$APP" || true

if [ "$OPEN_AFTER" = 1 ]; then
  open "$APP"
else
  echo "run with:  open \"$APP\""
  echo "or:        \"$APP/Contents/MacOS/$BIN_NAME\" a.mp4 b.mp4"
fi
