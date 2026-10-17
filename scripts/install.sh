#!/bin/sh
# not-a-vpn installer for macOS.
#
# Usage:
#   curl -fsSL <host>/install.sh | sh
# or review first:
#   curl -fsSL <host>/install.sh -o install.sh && sh install.sh
#
# Downloads the latest release asset, verifies its SHA-256 sidecar and
# installs the app WITHOUT touching it with Finder/Archive Utility, so the
# com.apple.quarantine attribute is never applied and Gatekeeper never
# shows the "Apple could not verify" warning.
#
# Env overrides:
#   NOT_A_VPN_REPO   GitHub "owner/name" to query for the latest release
#                    (default: okasi/not-a-vpn)
#   NOT_A_VPN_URL    direct .app.zip URL (skip the GitHub lookup entirely)

set -eu

REPO="${NOT_A_VPN_REPO:-okasi/not-a-vpn}"
URL="${NOT_A_VPN_URL:-}"
APP_ID="io.github.okasi.not-a-vpn"

say() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v shasum >/dev/null 2>&1 || die "shasum is required"

ARCH="$(uname -m)"
case "$ARCH" in
  arm64 | x86_64) ;;
  *) die "unsupported architecture: $ARCH" ;;
esac

TMP="$(mktemp -d "${TMPDIR:-/tmp}/not-a-vpn-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

# ------------------------------------------------------- find the asset ----
if [ -n "$URL" ]; then
  ASSET="$URL"
  say "Using provided URL: $ASSET"
else
  say "Looking up the latest release of $REPO"
  JSON="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")" \
    || die "could not fetch release info (check your network)"
  ASSET="$(printf '%s\n' "$JSON" \
             | grep -o 'https://[^"]*\.app\.zip' \
             | grep -E -- "-macos-[0-9.]+-${ARCH}\.app\.zip$" | head -n 1 || true)"
  [ -n "$ASSET" ] || ASSET="$(printf '%s\n' "$JSON" \
             | grep -o 'https://[^"]*\.app\.zip' \
             | grep -E -- '-macos-[0-9.]+-universal\.app\.zip$' | head -n 1 || true)"
  [ -n "$ASSET" ] || die "no macOS asset found in the latest release"
  say "Latest asset: $(basename "$ASSET")"
fi

# --------------------------------------------------------- download it ----
ZIP="$TMP/$(basename "$ASSET")"
say "Downloading"
curl -fL "$ASSET" -o "$ZIP" || die "download failed"

say "Verifying checksum"
if curl -fsSL "$ASSET.sha256" -o "$TMP/checksum.sha256" 2>/dev/null; then
  (cd "$TMP" && shasum -a 256 -c "checksum.sha256" >/dev/null 2>&1) \
    || die "checksum mismatch - refusing to install a corrupted download"
  say "Checksum OK"
else
  say "No checksum sidecar published; skipping verification"
fi

# ----------------------------------------------------------- install ------
[ "$(uname -s)" = Darwin ] || die "this installer is macOS only"

say "Unpacking"
ditto -x -k "$ZIP" "$TMP/app" || die "could not unpack the download"
APP="$(find "$TMP/app" -maxdepth 1 -name '*.app' -mindepth 1 | head -n 1)"
[ -n "$APP" ] || die "the archive contains no .app bundle"

# Belt and braces: CLI extractors never apply quarantine, but make sure.
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

say "Quitting any running copy"
killall not-a-vpn 2>/dev/null || true
sleep 1

TARGET="/Applications"
if [ ! -w /Applications ]; then
  TARGET="$HOME/Applications"
  mkdir -p "$TARGET"
fi
DEST="$TARGET/$(basename "$APP")"

if [ -d "$DEST" ]; then
  mv "$DEST" "$TMP/previous.app" 2>/dev/null \
    || die "could not move the existing app out of the way"
fi

if ! mv "$APP" "$DEST" 2>/dev/null; then
  if [ -d "$TMP/previous.app" ]; then mv "$TMP/previous.app" "$DEST"; fi
  die "could not install into $TARGET"
fi
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

say "Installed: $DEST"
say "Starting"
open "$DEST"
echo "not-a-vpn is running - look for the icon in the menu bar." >&2
