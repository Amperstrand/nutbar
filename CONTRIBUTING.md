# Contributing to NutBar

## Development setup

```bash
# Clone
git clone https://github.com/Amperstrand/nutbar.git
cd nutbar

# Build the daemon
cd daemon
cargo build --release
cp target/release/cashud ~/.local/bin/

# Install the plugin
mkdir -p ~/.config/omarchy/plugins/nutbar
cp plugin/nutbar/* ~/.config/omarchy/plugins/nutbar/

# Add to your Omarchy shell.json (right section):
# {"id": "nutbar"}

# Start the daemon
systemctl --user enable --now omarchy-cashud
```

## Running tests

```bash
# Daemon unit tests
cd daemon && cargo test

# Advertisement parser tests
python3 tests/ad_validate.py tests/fixtures/ad-valid.json  # should pass
python3 tests/ad_validate.py tests/fixtures/ad-min-steps-zero.json  # should fail (min_steps=0)

# Clippy
cd daemon && cargo clippy -- -D warnings
```

## Architecture

```
plugin/nutbar/          QML UI (BarWidget, Panel, Model.js)
  ├── BarWidget.qml     Status bar widget (balance + data meter)
  ├── Panel.qml         Full wallet panel (send/receive/TollGate)
  └── Model.js          JSON parsing + formatting helpers

daemon/                 Rust daemon (cashud)
  ├── src/main.rs       HTTP API on 127.0.0.1:3939
  ├── src/wifi.rs       WiFi manager (nmcli integration)
  ├── src/tollgate.rs   TollGate client (ad parsing, payment)
  └── src/led_matrix.rs LED status (for ESP32 builds)

esp32/                  ESP32 TollGate client (superseded by Micronuts)
tests/                  Test fixtures + ad parser
docs/                   Documentation
```

## The daemon API

The daemon speaks plain JSON on `127.0.0.1:3939`:

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/status` | GET | Balance, session, wifi state |
| `/wifi` | GET | Current SSID, TollGate scan |
| `/wifi/connect` | POST | Join a WiFi network |
| `/wifi/fallback` | POST | Switch to fallback network |
| `/tollgate/pay` | POST | Pay a TollGate gateway |
| `/tollgate/session` | GET | Current session state |
| `/send` | POST | Send Cashu tokens |
| `/mints` | GET | List mints + balances |
| `/mints/add` | POST | Add a mint |

## Money safety rules

- Proofs are bearer instruments — treat exported tokens like banknotes
- Keys live in `~/.local/share/omarchy-cashu/` — back this up
- The mint is a custodian — only hold what you're willing to trust them with
- Default mint is a public test mint (`testnut.cashu.space`) — set your own with `CASHUD_MINT`

## Submitting changes

1. Fork the repo
2. Create a feature branch
3. Make changes
4. Run tests: `cd daemon && cargo test`
5. Run clippy: `cd daemon && cargo clippy -- -D warnings`
6. Submit a PR with a clear description
