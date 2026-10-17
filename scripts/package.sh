#!/usr/bin/env bash
# Unified release packaging for not-a-vpn.
#
# One script, both platforms:
#   ./scripts/package.sh              -> package for the current host OS
#   ./scripts/package.sh --macos      -> force macOS packaging
#   ./scripts/package.sh --windows    -> force Windows packaging (cross-compiles from macOS)
#   ./scripts/package.sh --all        -> package both
#   ./scripts/package.sh --universal  -> macOS only: lipo arm64 + x86_64
#
# Outputs (in ./dist):
#   macOS:   not-a-vpn-<version>-macos-<arch|universal>.app.zip
#   Windows: not-a-vpn-<version>-windows-x64.zip
#
# Gatekeeper ("Apple could not verify ..."):
#   Without a real Apple Developer ID signature + notarization, ANY app a user
#   downloads on macOS is flagged by Gatekeeper - no packaging tool can change
#   that. Two supported paths:
#
#   1. Recommended (removes the warning entirely):
#        CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
#        NOTARY_PROFILE="notarytool-keychain-profile" \
#          ./scripts/package.sh --macos
#      (NOTARY_PROFILE is stored once via:
#        xcrun notarytool store-credentials "notarytool-keychain-profile"
#            --apple-id you@example.com --team-id TEAMID --password APP-SPECIFIC-PASSWORD)
#      Alternative env vars instead of NOTARY_PROFILE:
#        APPLE_ID, APPLE_PASSWORD (app-specific), APPLE_TEAM_ID
#
#   2. Fallback (no Apple account): the zip ships a
#      "Fix Gatekeeper (double-click me).command" helper that strips the
#      com.apple.quarantine flag from the unpacked app and opens it, and a
#      README explaining the right-click -> Open / "Open Anyway" bypass.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

NAME="not-a-vpn"
APP_DISPLAY="not-a-vpn.app"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
DIST="$ROOT/dist"

log() { printf '\n\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ----------------------------------------------------------------- args -----
MODES=()
ALL_TARGETS=0
for arg in "${@:-}"; do
  case "$arg" in
    --macos)      MODES+=("macos") ;;
    --windows)    MODES+=("windows") ;;
    --all)        MODES+=("macos" "windows") ;;
    --all-targets|--universal) ALL_TARGETS=1 ;;
    *) die "unknown option: $arg (use --macos, --windows, --all, --universal)" ;;
  esac
done
if [ ${#MODES[@]} -eq 0 ]; then
  case "$(uname -s)" in
    Darwin)               MODES=("macos") ;;
    MINGW*|MSYS*|CYGWIN*) MODES=("windows") ;;
    *) die "Unsupported host OS; pass --macos and/or --windows explicitly" ;;
  esac
fi

command -v cargo >/dev/null || die "cargo not found; install rust first"

rm -rf "$DIST"
mkdir -p "$DIST"

# zip_of <out.zip> <workdir> <items...>  -- cross-platform archive helper.
zip_of() {
  local zipfile="$1" workdir="$2"
  shift 2
  if command -v zip >/dev/null; then
    (cd "$workdir" && zip -q -r -y "$zipfile" "$@" -x '*.DS_Store')
  elif command -v powershell.exe >/dev/null; then
    local paths item
    paths=""
    for item; do paths+="\"$(cygpath -w "$workdir/$item")\","; done
    powershell.exe -NoProfile -Command \
      "Compress-Archive -Force -Path ${paths%,} -DestinationPath \"$(cygpath -w "$workdir/$zipfile")\""
  else
    die "need 'zip' or powershell.exe to create archives"
  fi
}

checksums() {
  (cd "$DIST" && for f in ./*.zip; do
    if command -v shasum >/dev/null; then
      shasum -a 256 "$f" | tee "$f.sha256"
    else
      sha256sum "$f" | tee "$f.sha256"
    fi
  done)
}

# ----------------------------------------------------------- Gatekeeper -----
mac_readme() {
  # $1 = "with-helper" when the Gatekeeper bypass helper ships in the zip.
  if [ "${1:-}" = "with-helper" ]; then
    local helper_note
    helper_note='1. Fastest: unpack the zip, then double-click
   "Fix Gatekeeper (double-click me).command" (it sits next to the app).
   It strips the quarantine flag and launches the app. Terminal may ask for
   one confirmation before running it.
2. Or run in Terminal yourself:'
  else
    local helper_note
    helper_note='1. Fastest - in Terminal:'
  fi
  cat > "$DIST/not-a-vpn-$VERSION-macos-README.md" <<EOF
# not-a-vpn (macOS)

The app is a menu-bar (tray) utility - once running you'll see its icon in the
menu bar at the top right. Click it to enable/disable, pick a DNS provider,
an evasion mode, or tick "Start on startup".

## Install without the Gatekeeper warning (recommended)

Gatekeeper only warns about apps that carry the macOS "quarantine" flag,
which Finder/Archive Utility apply when you double-click a downloaded zip.
Install instead with the curl-based installer and the warning never appears:

    curl -fsSL https://github.com/okasi/not-a-vpn/raw/main/scripts/install.sh -o install.sh
    sh install.sh

It verifies the SHA-256 checksum, never touches the quarantine channel that
Finder uses, and installs the app into /Applications (no admin password
needed). The app's own "Check for updates" menu item swaps in new versions
the same way.

## If macOS says "Apple could not verify ... is free of malware"

$helper_note
       xattr -dr com.apple.quarantine "/path/to/not-a-vpn.app"
3. Or without Terminal: right-click the app -> Open (or double-click it, then
   System Settings -> Privacy & Security -> "Open Anyway").

## Notarized builds

If you build with an Apple Developer ID identity (see comments at the top of
scripts/package.sh), the app is notarized and stapled and this warning never
appears.
EOF
}

mac_gatekeeper_helper() {
  # Only shipped when the app is NOT notarized.
  cat > "$DIST/Fix Gatekeeper (double-click me).command" <<EOF
#!/bin/bash
# Removes the macOS quarantine flag from the app next to this script and opens it.
APP="\$(cd "\$(dirname "\$0")" && pwd)/$APP_DISPLAY"
if [ ! -d "\$APP" ]; then
  echo "not found: \$APP"
  read -n 1 -s -r -p "Press any key to close..."
  exit 1
fi
xattr -dr com.apple.quarantine "\$APP" 2>/dev/null
echo "Done: quarantine removed. Opening \$APP ..."
open "\$APP"
read -n 1 -s -r -p "Press any key to close..."
EOF
  chmod +x "$DIST/Fix Gatekeeper (double-click me).command"
}

# Returns 0 when the bundle is fully notarized.
mac_sign_and_notarize() {
  local bundle="$1" identity="${CODESIGN_IDENTITY:--}"

  # Junk xattrs (Finder/DS_Store etc.) can break signatures - strip them.
  xattr -cr "$bundle" 2>/dev/null || true

  if [ "$identity" != "-" ]; then
    log "Signing with Developer ID ($identity)"
    codesign --force --deep --sign "$identity" --options runtime --timestamp "$bundle"
  else
    log "Ad-hoc signing (no CODESIGN_IDENTITY given)"
    if ! codesign --force --deep --sign "$identity" --options runtime \
          --timestamp=none "$bundle" 2>/dev/null; then
      codesign --force --deep -s - "$bundle"
    fi
  fi
  codesign --verify --deep --strict "$bundle"

  # Notarize + staple when credentials are available.
  if [ "$identity" != "-" ] && xcrun --find notarytool >/dev/null 2>&1; then
    log "Notarizing (this can take a few minutes)"
    local notary_zip="$DIST/.notary.zip"
    ditto -c -k --keepParent "$bundle" "$notary_zip"
    local submit_ok=0
    if [ -n "${NOTARY_PROFILE:-}" ]; then
      xcrun notarytool submit "$notary_zip" \
        --keychain-profile "$NOTARY_PROFILE" --wait && submit_ok=1
    elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
      xcrun notarytool submit "$notary_zip" \
        --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" \
        --team-id "$APPLE_TEAM_ID" --wait && submit_ok=1
    else
      warn "Developer ID given but no notarization credentials; set NOTARY_PROFILE or APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID"
    fi
    rm -f "$notary_zip"
    if [ "$submit_ok" = 1 ]; then
      xcrun stapler staple "$bundle"
      codesign --verify --deep --strict "$bundle"
      if spctl -a -vv -t exec "$bundle" 2>/dev/null; then
        log "Gatekeeper accepts the notarized app (spctl passed)"
        return 0
      fi
    fi
    warn "Notarization unavailable/failed - shipping Gatekeeper bypass helper in the zip"
  fi
  return 1
}

# ---------------------------------------------------------------- macOS -----
package_macos() {
  local bundle bin res arch
  bundle="$DIST/$APP_DISPLAY"

  log "Building macOS binaries"
  if [ "$ALL_TARGETS" = 1 ]; then
    command -v rustup >/dev/null \
      || die "--universal needs rustup (brew install rustup) + Apple dual-arch targets"
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    mkdir -p "$bundle/Contents/MacOS"
    lipo -create -output "$bundle/Contents/MacOS/$NAME" \
      "target/aarch64-apple-darwin/release/$NAME" \
      "target/x86_64-apple-darwin/release/$NAME"
    arch=universal
  else
    cargo build --release
    arch="$(uname -m)"
  fi

  log "Assembling $APP_DISPLAY"
  bin="$bundle/Contents/MacOS"
  res="$bundle/Contents/Resources"
  mkdir -p "$bin" "$res"
  [ "$arch" = universal ] || cp "target/release/$NAME" "$bin/$NAME"

  # Icon: assets/icon.png -> .icns via the built-in sips/iconutil.
  if command -v iconutil >/dev/null && [ -f assets/icon.png ]; then
    local iconset="$DIST/icon.iconset"
    mkdir -p "$iconset"
    for s in 16 32 128 256 512; do
      sips -z "$s" "$s" assets/icon.png --out "$iconset/icon_${s}x${s}.png" >/dev/null
      sips -z $((s * 2)) $((s * 2)) assets/icon.png \
        --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$res/AppIcon.icns"
    rm -rf "$iconset"
  fi

  cat > "$res/LICENSE" <<'EOF'
not-a-vpn
MIT License. Third-party routed protocol techniques are inspired by
GreenTunnel (MIT) and goodbyedpi (Apache-2.0); this codebase is an
original Rust implementation and carries no third-party source.
EOF

  cat > "$bundle/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>Not A VPN</string>
  <key>CFBundleDisplayName</key>     <string>Not A VPN</string>
  <key>CFBundleIdentifier</key>      <string>io.github.okasi.not-a-vpn</string>
  <key>CFBundleExecutable</key>      <string>$NAME</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>CFBundleShortVersionString</key> <string>$VERSION</string>
  <key>CFBundleVersion</key>         <string>$VERSION</string>
  <key>CFBundleIconFile</key>        <string>AppIcon</string>
  <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
  <key>LSMinimumSystemVersion</key>  <string>10.13</string>
  <key>NSHighResolutionCapable</key> <true/>
  <key>LSUIElement</key>             <true/>
  <key>NSHumanReadableCopyright</key><string>MIT License</string>
</dict>
</plist>
EOF

  local helper_needed=1
  if mac_sign_and_notarize "$bundle"; then
    helper_needed=0
  fi

  # Ship the quarantine-free installer as a first-class release artifact.
  cp scripts/install.sh "$DIST/install.sh"
  chmod +x "$DIST/install.sh"

  log "Zipping"
  local out="$NAME-$VERSION-macos-$arch.app.zip"
  if [ "$helper_needed" = 1 ]; then
    mac_readme with-helper
    mac_gatekeeper_helper
    zip_of "$out" "$DIST" "$(basename "$bundle")" \
      "Fix Gatekeeper (double-click me).command" \
      "not-a-vpn-$VERSION-macos-README.md"
  else
    mac_readme
    zip_of "$out" "$DIST" "$(basename "$bundle")" \
      "not-a-vpn-$VERSION-macos-README.md"
  fi
  rm -rf "$bundle" \
         "$DIST/Fix Gatekeeper (double-click me).command" \
         "$DIST/not-a-vpn-$VERSION-macos-README.md"
}

# -------------------------------------------------------------- Windows -----
WINDOWS_GNU_TARGET="x86_64-pc-windows-gnu"

win_build() {
  local exe_path
  if [ "$(uname -s)" = Darwin ]; then
    # Cross-compile from macOS: needs rustup std for the GNU target plus the
    # mingw-w64 linker (brew install rustup mingw-w64). The linker is wired
    # up in .cargo/config.toml.
    log "Cross-compiling Windows x64 from macOS"
    # Prefer the rustup-managed toolchain. A standalone `brew install rust`
    # places plain rustc/cargo shims at /opt/homebrew/bin that shadow rustup's
    # and cannot find cross-target std libraries, so order is critical.
    command -v /opt/homebrew/opt/rustup/bin/cargo >/dev/null 2>&1 && \
      export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
    command -v /usr/local/opt/rustup/bin/cargo >/dev/null 2>&1 && \
      export PATH="/usr/local/opt/rustup/bin:$PATH"
    command -v rustup >/dev/null \
      || die "rustup not found; install with: brew install rustup"
    command -v x86_64-w64-mingw32-gcc >/dev/null \
      || die "mingw-w64 not found; install with: brew install mingw-w64"
    # The toolchain in use must actually carry the GNU target's std (guards
    # against a shadowing non-rustup rustc whose sysroot lacks cross targets).
    if [ ! -f "$(rustc --print sysroot)/lib/rustlib/$WINDOWS_GNU_TARGET/libstd" ] &&
       [ ! -f "$(rustc --print sysroot)/lib/rustlib/$WINDOWS_GNU_TARGET/lib/libstd.rlib" ]; then
      log "Toolchain ($(rustc --print sysroot)) lacks $WINDOWS_GNU_TARGET std; adding via rustup"
      rustup target add "$WINDOWS_GNU_TARGET"
      # If rustup cannot fix the active rustc (shadowed shim), escalate.
      if [ ! -f "$(rustc --print sysroot)/lib/rustlib/$WINDOWS_GNU_TARGET/libstd" ] &&
         [ ! -f "$(rustc --print sysroot)/lib/rustlib/$WINDOWS_GNU_TARGET/lib/libstd.rlib" ]; then
        die "$(rustc --print sysroot) does not support $WINDOWS_GNU_TARGET - \
uninstall the conflicting 'rust' formula (brew uninstall rust) or run \
cargo with the rustup-managed toolchain"
      fi
    fi
    cargo build --release --target "$WINDOWS_GNU_TARGET"
    exe_path="target/$WINDOWS_GNU_TARGET/release/$NAME.exe"
    command -v x86_64-w64-mingw32-strip >/dev/null && x86_64-w64-mingw32-strip "$exe_path" || true
  else
    log "Building Windows binary natively"
    cargo build --release
    exe_path="target/release/$NAME.exe"
  fi
  echo "$exe_path"
}

package_windows() {
  local staged exe
  staged="$DIST/$NAME-$VERSION-windows-x64"
  exe="$(win_build | tail -n 1)"
  mkdir -p "$staged"
  cp "$exe" "$staged/"
  [ -f assets/icon.png ] && cp assets/icon.png "$staged/"

  cat > "$staged/README.txt" <<EOF
not-a-vpn $VERSION (windows-x64)

1. Run not-a-vpn.exe; a tray icon appears next to the clock.
2. Use the tray menu to enable/disable, pick a DNS provider, an evasion
   mode, or "start on startup" (writes the HKCU Run key).
3. Enabling assigns the loopback system proxy - no admin rights needed.

MIT License. Techniques inspired by GreenTunnel / goodbyedpi.
EOF

  log "Zipping"
  zip_of "$NAME-$VERSION-windows-x64.zip" "$DIST" "$(basename "$staged")"
  rm -rf "$staged"
}

# ----------------------------------------------------------------- main -----
for mode in "${MODES[@]}"; do
  case "$mode" in
    macos)   package_macos ;;
    windows) package_windows ;;
  esac
done

log "Checksums"
checksums

log "Done. Artifacts in dist/"
ls -lh "$DIST"
