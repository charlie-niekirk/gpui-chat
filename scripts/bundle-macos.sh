#!/usr/bin/env bash
# Builds the macOS release artifacts (PR 7.1): a universal (arm64 + x86_64) binary, the
# `<APP_NAME>.app` bundle around it, and a drag-to-Applications DMG.
#
#   scripts/bundle-macos.sh                    # universal release build -> dist/
#   scripts/bundle-macos.sh --arch native      # this Mac's architecture only (faster)
#   scripts/bundle-macos.sh --bin target/debug/gpui-chat --no-dmg   # wrap a binary you already built
#   scripts/bundle-macos.sh --version 0.1.0 --out /tmp/dist
#   scripts/bundle-macos.sh --sign "Developer ID Application: Name (TEAMID)" --notarize
#
# Signing: ad-hoc (`codesign -s -`) unless `--sign IDENTITY` (or $MACOS_SIGN_IDENTITY) names a real
# identity in the keychain. `--notarize` additionally submits the app and the DMG to Apple with an
# App Store Connect API key ($APPLE_API_KEY_PATH, $APPLE_API_KEY_ID, $APPLE_API_ISSUER_ID) and staples
# the tickets; it needs a real identity and is skipped by CI when the secrets are absent
# (docs/RELEASING.md).
#
# The app name and bundle identifier are read from `APP_NAME` / `APP_ID` in crates/core/src/lib.rs,
# the version from `[workspace.package]` in Cargo.toml. Output (all under --out, default dist/):
#   <APP_NAME>.app
#   <APP_NAME>-<version>-macos-<universal|arm64|x86_64>.dmg (+ .sha256)
set -euo pipefail

cd "$(dirname "$0")/.."
root="$PWD"

# The oldest macOS GPUI supports (gpui-pre-macos refuses to run below 12.0, and its Objective-C is
# compiled with -mmacosx-version-min=12.0).
min_macos="12.0"

arch="universal"
bin=""
version=""
out="dist"
sign="${MACOS_SIGN_IDENTITY:-}"
notarize=0
dmg=1
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) arch="$2"; shift 2 ;;
    --arch=*) arch="${1#*=}"; shift ;;
    --bin) bin="$2"; shift 2 ;;
    --bin=*) bin="${1#*=}"; shift ;;
    --version) version="$2"; shift 2 ;;
    --version=*) version="${1#*=}"; shift ;;
    --out) out="$2"; shift 2 ;;
    --out=*) out="${1#*=}"; shift ;;
    --sign) sign="$2"; shift 2 ;;
    --sign=*) sign="${1#*=}"; shift ;;
    --notarize) notarize=1; shift ;;
    --no-dmg) dmg=0; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }
log() { echo "==> $*"; }

[ "$(uname -s)" = Darwin ] || die "this script builds a macOS bundle and only runs on macOS"
case "$arch" in universal|native|arm64|x86_64) ;; *) die "--arch must be universal, native, arm64 or x86_64" ;; esac
if [ "$arch" = native ]; then arch="$(uname -m)"; fi
[ -z "$sign" ] && sign="-"
if [ "$notarize" = 1 ] && [ "$sign" = "-" ]; then
  die "--notarize needs a real signing identity (--sign or \$MACOS_SIGN_IDENTITY); ad-hoc signatures cannot be notarized"
fi

# ---- identity and version, from the single sources of truth --------------------------------------
core=crates/core/src/lib.rs
app_name="$(sed -n 's/^pub const APP_NAME: &str = "\(.*\)";$/\1/p' "$core")"
app_id="$(sed -n 's/^pub const APP_ID: &str = "\(.*\)";$/\1/p' "$core")"
[ -n "$app_name" ] && [ -n "$app_id" ] || die "could not read APP_NAME / APP_ID from $core"
if [ -z "$version" ]; then
  version="$(awk '/^\[workspace.package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}' Cargo.toml)"
fi
[ -n "$version" ] || die "could not read the workspace version from Cargo.toml"
case "$version" in *[!0-9A-Za-z.+-]*|"") die "unusable version: $version" ;; esac
log "$app_name $version ($app_id), $arch, signing as $([ "$sign" = - ] && echo 'ad-hoc' || echo "$sign")"

mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/bundle-macos.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# ---- the binary ---------------------------------------------------------------------------------
if [ -n "$bin" ]; then
  [ -f "$bin" ] || die "--bin $bin does not exist"
  binary="$work/$app_name"
  cp "$bin" "$binary"
else
  export MACOSX_DEPLOYMENT_TARGET="$min_macos"
  case "$arch" in
    universal) triples="aarch64-apple-darwin x86_64-apple-darwin" ;;
    arm64) triples="aarch64-apple-darwin" ;;
    x86_64) triples="x86_64-apple-darwin" ;;
  esac
  slices=()
  for triple in $triples; do
    # The targets come from rust-toolchain.toml (`targets`), installed by `rustup toolchain install`.
    log "cargo build --release --target $triple"
    cargo build --locked --release -p gpui-chat --target "$triple" ||
      die "build for $triple failed (is its Rust target installed? rust-toolchain.toml lists it: run \`rustup toolchain install\`)"
    slices+=("target/$triple/release/$app_name")
  done
  binary="$work/$app_name"
  if [ "${#slices[@]}" -gt 1 ]; then
    log "lipo -create"
    lipo -create -output "$binary" "${slices[@]}"
  else
    cp "${slices[0]}" "$binary"
  fi
fi
strip -x "$binary"
archs="$(lipo -archs "$binary")"
log "binary architectures: $archs"
if [ -n "$bin" ]; then
  # A prebuilt binary decides the architecture label (and the DMG name) itself.
  case "$archs" in *" "*) arch=universal ;; arm64) arch=arm64 ;; *) arch="$archs" ;; esac
fi
if [ "$arch" = universal ]; then
  case " $archs " in *" arm64 "*) ;; *) die "expected a universal binary, got: $archs" ;; esac
  case " $archs " in *" x86_64 "*) ;; *) die "expected a universal binary, got: $archs" ;; esac
fi

# ---- the icon: assets/icon/icon-1024.png -> AppIcon.icns ----------------------------------------
iconset="$work/AppIcon.iconset"
mkdir "$iconset"
src_icon=assets/icon/icon-1024.png
[ "$(sips -g pixelWidth -g pixelHeight "$src_icon" | awk '/pixel(Width|Height)/{printf "%s ", $2}')" = "1024 1024 " ] ||
  die "$src_icon must be 1024x1024"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  sips -z "$size" "$size" "$src_icon" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z "$double" "$double" "$src_icon" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil --convert icns --output "$work/AppIcon.icns" "$iconset"

# ---- the bundle ---------------------------------------------------------------------------------
app="$out/$app_name.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/$app_name"
chmod 755 "$app/Contents/MacOS/$app_name"
cp "$work/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
[ -f LICENSE ] && cp LICENSE "$app/Contents/Resources/LICENSE"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>$app_name</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIdentifier</key><string>$app_id</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>$app_name</string>
  <key>CFBundleDisplayName</key><string>$app_name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.social-networking</string>
  <key>LSMinimumSystemVersion</key><string>$min_macos</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null
printf 'APPL????' > "$app/Contents/PkgInfo"

# ---- signing and notarization -------------------------------------------------------------------
sign_path() {
  if [ "$sign" = - ]; then
    codesign --force --sign - "$1"
  else
    # Hardened runtime + secure timestamp are what notarization requires.
    codesign --force --options runtime --timestamp --sign "$sign" "$1"
  fi
}

notarize_path() { # path (a .zip or .dmg)
  [ -n "${APPLE_API_KEY_PATH:-}" ] && [ -n "${APPLE_API_KEY_ID:-}" ] && [ -n "${APPLE_API_ISSUER_ID:-}" ] ||
    die "--notarize needs APPLE_API_KEY_PATH, APPLE_API_KEY_ID and APPLE_API_ISSUER_ID"
  log "notarizing $(basename "$1") (this waits for Apple, usually a few minutes)"
  xcrun notarytool submit "$1" --wait \
    --key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID"
}

log "codesign $app_name.app"
sign_path "$app"
codesign --verify --strict --verbose=2 "$app"
if [ "$notarize" = 1 ]; then
  ditto -c -k --keepParent "$app" "$work/$app_name-notarize.zip"
  notarize_path "$work/$app_name-notarize.zip"
  xcrun stapler staple "$app"
fi

# ---- the DMG ------------------------------------------------------------------------------------
if [ "$dmg" = 1 ]; then
  dmg_path="$out/$app_name-$version-macos-$arch.dmg"
  stage="$work/dmg"
  mkdir "$stage"
  ditto "$app" "$stage/$app_name.app"
  ln -s /Applications "$stage/Applications"
  # `-ov` replaces a previous DMG; UDZO is the compressed read-only format.
  rm -f "$dmg_path"
  hdiutil create -quiet -volname "$app_name" -srcfolder "$stage" -format UDZO -fs HFS+ -ov "$dmg_path"
  log "codesign $(basename "$dmg_path")"
  sign_path "$dmg_path"
  if [ "$notarize" = 1 ]; then
    notarize_path "$dmg_path"
    xcrun stapler staple "$dmg_path"
  fi
  (cd "$out" && shasum -a 256 "$(basename "$dmg_path")" > "$(basename "$dmg_path").sha256")
  log "$dmg_path ($(du -h "$dmg_path" | cut -f1))"
fi

log "done: $app"
