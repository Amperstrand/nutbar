# Changelog

## v0.1.0 — 2026-10-01

First public release.

### Features
- Cashu wallet in the Omarchy status bar (🥜 balance + ⇅ data meter)
- TollGate auto-pay: validates kind-10021 advertisements, pays via Cashu ecash, renews automatically
- Full wallet panel: send, receive, QR codes, mint history
- Offline stash: pre-split tokens for instant payments without mint round-trips
- WiFi watchdog: auto-fallback to a known network if TollGate connection dies
- Multi-mint support (9+ Cashu mints)

### Security
- Keys and proofs stored locally (`~/.local/share/omarchy-cashu/`)
- No data leaves the machine except to the mint you configure
- Signed firmware verification (SSHSIG manifests with pinned keys)

### Hardware support
- Desktop: Omarchy (Quickshell QML + Rust cashud daemon)
- Phone: Android via TollGate captive portal (TIP-03)
- Embedded: M5 Atom / ESP32 (via [Micronuts](https://github.com/Amperstrand/micronuts))
- Router: any OpenWrt device via [tollgate-module-basic-go](https://github.com/OpenTollGate/tollgate-module-basic-go)

### Bug fixes during development
- `min_steps=0` in TollGate advertisements caused all v1 clients to reject pricing (fixed: default to 1)
- Pixel phones use CDC-NCM (not RNDIS) for USB tethering — requires `kmod-usb-net-cdc-ncm`
- Android 14+ blocks programmatic tethering via shell — ADB UI automation works as workaround
- Chromium on Wayland crashes without `--use-webgpu-adapter=opengles`
