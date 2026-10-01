# TollGate Detector Probe — M5 Atom

> **Status: superseded.** The embedded TollGate client has moved to the
> [Micronuts](https://github.com/OpenTollGate) project. This firmware is
> kept as a TollGate v1 protocol reference — the ad validator and
> detector state machine are test-covered (26 tests) and document the
> wire protocol end-to-end.

An autonomous always-on TollGate health monitor for the
[M5 Atom](https://docs.m5stack.com/en/core/atom_lite) (ESP32-PICO,
SK6812 RGB LED, one button). It scans for TollGate APs, joins them,
validates the kind-10021 advertisement, and — when funded with a Cashu
token — pays for a session. The RGB LED reports health at a glance and
a line-based serial protocol reports every event for logging.

Adapted from the [M5StickC Plus paying
client](../esp32-tollgate-client/) (same ad parser, payment flow, and
/usage check); adds the auto-scan state machine, LED driver, and button.

## Hardware

No wiring needed — the M5 Atom Lite is all-in-one:

| Peripheral | GPIO | Notes |
|---|---|---|
| SK6812 RGB LED | 27 | led_strip component (RMT backend) |
| Button | 39 | active-low, internal pullup |

USB-C provides power + serial console at 115200 baud.

## LED language

| Color | Meaning |
|---|---|
| RED solid | scanning, no TollGate found |
| YELLOW dim | TollGate found, validating the ad |
| GREEN pulse | ad valid, connected (session active when paid; gateway healthy in monitor-only) |
| BLUE blink | paying |
| RED fast blink | error (ad invalid, join/payment failed) — backoff 30 s, then rescan |

## Serial line protocol

Line-based tags for PRTA (or any logger) to consume. Human-readable
lines (including everything starting with `(` or lowercase) are
diagnostics.

```
TG_SCAN_START                        scan cycle begins
TG_FOUND <ssid> <bssid> <rssi>       candidate AP selected (strongest)
TG_CONNECTED <ip>                    joined, DHCP lease acquired
TG_AD_VALID <gateway_ip> <mints> <price_per_step>   ad validated
TG_AD_INVALID <reason>               ad rejected (min_steps=0, no cashu/sat option, ...)
TG_PAID <session_id> <cost_sats>     token accepted by the gateway
TG_INTERNET <http_code>              through-tunnel probe (1.1.1.1:80), once a minute
TG_DISCONNECTED                      session lost / join or payment failed
```

## Serial console

Same command set as the M5Stick client, but **auto-scan is the default**
— no `go` required:

```
ssid <name>       Lock onto one SSID (default: any TollGate* open network)
token <cashuA..>  Set a funded Cashu token → paying mode
mint <url>        Restrict pricing to one mint (optional)
go                Force a rescan now
status            Show config + state
erase             Erase stored config (same as long-press)
```

- **Monitor-only mode** (no token): scan → join → validate → green pulse.
- **Paying mode** (token set): additionally POSTs the token once after a
  valid ad, then monitors /usage. One token, one payment — set a fresh
  token to pay again.
- Button **short press** = force rescan · **long press (3 s)** = erase config.

Config persists in NVS across reboots.

## Build

Requires [ESP-IDF v5.x](https://docs.espressif.com/projects/esp-idf/):

```bash
cd esp32-atom-detector/
source ~/esp/esp-idf/export.sh
idf.py set-target esp32
idf.py build
```

## Flash

The Atom enumerates as `/dev/ttyUSB0` (CP210x) or `/dev/ttyACM0`:

```bash
esptool.py --chip esp32 -p /dev/ttyUSB0 -b 460800 \
  --before default_reset --after hard_reset write_flash \
  --flash_mode dio --flash_size 4MB --flash_freq 40m \
  0x1000 build/bootloader/bootloader.bin \
  0x8000 build/partition_table/partition-table.bin \
  0x10000 build/atom_detector.bin
```

## Fund (paying mode)

With the laptop wallet on upstream wifi:

```bash
curl -s --json '{"amount_sats":10}' http://127.0.0.1:3939/send | jq -r .token
```

Paste into the serial console: `token cashuA...` — the detector pays on
the next scan cycle automatically.

## State machine

```
SCAN ──found──► JOIN ──ok──► VALIDATE ──valid──► PAY (if token) ──ok──► MONITOR
  ▲                │            │  ▲                │                    │
  │             fail├────────────┤  └──invalid───────┴───fail──► ERROR_WAIT (30 s)
  └────────────────┴── session lost / button / 30 s timer ──────────────┘
```

`main/detector.c` holds the pure transition table;
`tests/test_detector_sm.py` mirrors it in Python (13 tests: happy paths
paying and monitor-only, error backoffs, forced rescan, candidate
predicate, tag mapping). `tests/test_ad_parser.py` (13 tests) covers
the shared ad validator.

## Limitations

- Open networks only (TollGate APs are open by design)
- One stored token, one payment per session (no renewal — the laptop's
  cashud owns renewal autopay)
- Token stored in plaintext NVS (test tokens, not production funds)
