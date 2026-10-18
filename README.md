<div align="center">
  <img src="assets/icon-master.png" width="128" alt="not-a-vpn icon">
</div>

# 🕶️ not-a-vpn

> It's not a VPN. It's a tiny menu-bar proxy that routes your traffic through
> encrypted DNS-over-HTTPS and sneaks past DPI with TLS ClientHello fragmentation.

🪟 **macOS & Windows** · 🐳 **100% Rust** · 🚫 **no admin rights needed**

🎉 **This is my first Rust app!**

## ✨ Features

- 🔒 **Local HTTP proxy** bound to loopback, set as the system proxy with one click
- 🌐 **DoH resolvers** with TLS + caching: AdGuard, Quad9, Control D, Cloudflare Security
- 🥷 **DPI evasion** modes: SNI split, small random split, multi-chunk — or off
- 🍽️ **Menu-bar / tray UI**: enable, pick provider & evasion mode, quit
- 🔁 **Autostart** and settings persistence (resume where you left off)
- 📦 **Single binary** — no runtime dependencies

## 🚀 Usage

```bash
cargo run --release
```

Click the tray icon to toggle. That's it.

## 🍽️ Install (macOS, no Gatekeeper warning)

macOS Gatekeeper only flags apps that carry the `com.apple.quarantine`
attribute, which Finder/Archive Utility apply when you double-click a
downloaded zip. The curl-based installer never triggers it:

```bash
curl -fsSL https://github.com/okasi/not-a-vpn/raw/main/scripts/install.sh -o install.sh
sh install.sh
```

It checks the SHA-256 sidecar, unpacks the release with `ditto` (no
quarantine) and installs into /Applications. Once installed, the app's own
"Check for updates" menu item downloads and swaps in new versions the same
quarantine-free way.

## 📦 Build & Package

```bash
./scripts/package.sh                                        # current host OS
./scripts/package.sh --macos      # macos app bundle
./scripts/package.sh --windows    # windows x64 (cross-compiles from macOS)
./scripts/package.sh --all        # both
./scripts/package.sh --universal  # macOS universal (arm64 + x86_64)
```

Artifacts land in `dist/` with SHA-256 sidecars and `install.sh` for macOS.
Optional notarization on macOS via `CODESIGN_IDENTITY` + `NOTARY_PROFILE`
(see `scripts/package.sh` header).

## 🛠️ Tech

- [smol](https://docs.rs/smol) async runtime, [tao](https://docs.rs/tao) + [tray-icon](https://docs.rs/tray-icon) for the tray
- Hand-rolled DNS wire protocol over DoH (RFC 8484)
- Windows system proxy via WinInet registry (`winreg`); macOS via `scutil`

## 🙏 Credits

Evasion techniques inspired by [GreenTunnel](https://github.com/SadeghHayeri/GreenTunnel) (MIT) and
[goodbyedpi](https://github.com/ValdikSS/goodbyedpi) (Apache-2.0). This codebase is an original Rust implementation.

## 📄 License

MIT
