# Upstream internet options for a TollGate router

How a TollGate router gets its own internet (the upstream it resells as
per-step ecash wifi). Two source families: WiFi STA and USB tethering.

| Source | Status | Automation |
|---|---|---|
| WiFi STA (open or WPA upstream) | production-ready | fully automated |
| USB tethering — manual toggle | works | manual (one tap on the phone) |
| USB tethering — ADB auto-toggle | works (Android 4.x–15) | automated on plug |
| USB tethering — Shizuku | untested here | needs a small app on the phone |
| USB tethering — OpenTether/SOCKS5 | untested here | different architecture, no tethering API |

Design context for source switching (USB preferred, WiFi fallback):
[OpenTollGate/tollgate-os#29](https://github.com/OpenTollGate/tollgate-os/issues/29).

## WiFi STA upstream

Join an upstream AP in STA mode while still broadcasting the TollGate
AP — fully automated, production-tested, and what most deployments
should start with. Supported by the OpenWrt image out of the box
(`uci wireless` STA iface bound to the WAN network).

Field-tested against open guest networks (e.g. `w3.hub Guests`) and
WPA networks. Switching profiles is a uci change + `wifi reload`.

## USB tethering

A phone on the router's USB port provides the upstream — much lower
phone battery draw than running a hotspot, and more stable.

### Package set (the one that works)

```
apk add adb kmod-usb-net kmod-usb-net-rndis kmod-usb-net-cdc-ether kmod-usb-net-cdc-ncm kmod-usb2 usbutils
```

**`kmod-usb-net-cdc-ncm` is mandatory on modern Android.** Pixel-class
phones (tested: Pixel 10, Android 15) expose USB tethering via
**CDC-NCM** (USB interface class 0x02/0x0a), not RNDIS. Without the NCM
kmod the kernel cannot bind to the phone's network interface at all —
and every remote-enable attempt then looks like it "did nothing".

### The Android 14+ security wall (test evidence)

Programmatic tethering enablement via ADB is **blocked by Android
security** on 14+. All of these were tested from the router (phone ADB-
authorized) against a Pixel 10:

| Command | Result |
|---|---|
| `adb shell svc usb setFunctions rndis` | ack'd but `sys.usb.config` stays `adb`; no `usb0` |
| `adb shell svc usb setFunctions rndis,adb` / `ncm` | same |
| `adb shell cmd usb setFunctions rndis` | "No shell command implementation" |
| `adb shell service call connectivity 33 i32 1 s16 usb0` | "Invalid UID range" (shell UID rejected: PERMISSION_DENIED) |
| `adb shell settings put global usb_tethering 1` | flag set, service not activated |
| `adb shell cmd connectivity tether usb enable` | "Unknown command: tether" |

Root cause: Android 14+ requires a shell-attributed `ContextImpl` to
call `TetheringManager.startTethering()`; plain ADB shell (UID 2000)
lacks it, and the connectivity `service call` validates the caller's
UID range. Additionally, tethering resets to OFF on every USB
re-enumeration.

### What DOES work: ADB UI automation

UI interaction needs no privileged API — it simulates a user tapping
the screen (the same uiautomator + `input tap` approach PRTA phone
tests use):

```bash
adb shell input keyevent KEYCODE_WAKEUP; sleep 1          # wake
adb shell am start -a android.settings.TETHER_SETTINGS; sleep 3
adb shell uiautomator dump /sdcard/ui.xml                 # dump the UI tree
# parse ui.xml: find the "USB tethering" row bounds → the Switch widget
# at the same vertical position → tap its center
adb shell input tap <switch_x> <switch_y>; sleep 3        # → usb0 appears
```

This is wired into the [conwrt](https://github.com/Amperstrand/conwrt)
`tether-android-adb` use case (PR
[#92](https://github.com/Amperstrand/conwrt/pull/92)): the USB hotplug
script tries the legacy `svc` switch first (Android ≤ 12), then the UI
toggle (Android 14+), with exponential backoff — so replug-and-forget
works despite the state reset.

### Verified end-to-end sequence (Pixel 10 → GL.iNet MT3000)

1. Install the package set above on the router
2. Plug the phone into the router's USB-A port
3. Approve ADB debugging on the phone (one-time RSA approval)
4. ADB toggle fires (manually or via hotplug): wake → settings → tap
5. `usb0` appears on the router (`cdc_ncm` binds automatically)
6. `udhcpc -i usb0` gets a lease from the phone
7. Internet flows via the phone's mobile data — measured: ping 8.8.8.8
   ~67 ms avg, HTTP 200

Full research thread with the failed-command table:
[conwrt-bench#46](https://github.com/Amperstrand/conwrt-bench/issues/46).

### Alternatives (untested here)

- **Manual toggle** — always works: Settings → Hotspot & Tethering →
  USB Tethering. `usb0` appears immediately; zero tooling needed.
- **Shizuku** — runs privileged code with proper context via ADB;
  [v2rayNG has a working Shizuku tethering implementation]
  (https://github.com/eliotcougar/v2rayNG/blob/shizuku-tethering/SHIZUKU_TETHERING.md).
  Needs a small app installed on the phone.
- **OpenTether / SOCKS5** — avoids the tethering API entirely:
  [a SOCKS5 proxy on the phone via ADB]
  (https://github.com/HelaFaye/opentether), router routes through it.
- **Older Android (10–12)** — the legacy
  [`service call connectivity 33`](https://github.com/adisubagja/tetringUSB-OPENWRT)
  path reportedly works there.

## Upstream switching

Preferred-source behavior (USB when present, WiFi STA fallback) is
metric-based interface preference — design in
[tollgate-os#29](https://github.com/OpenTollGate/tollgate-os/issues/29).
For bench/demo switching today, the `upstream-test.sh` script in the
tollgate-demo checkout (x280 laptop) drives WiFi↔USB transitions over
SSH; conwrt's `tether`/`tether-android-adb` use cases provision the USB
side on any router.
