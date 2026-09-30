# NutBar

**A Cashu wallet in your Omarchy status bar — with TollGate auto-pay.**

![NutBar demo](docs/demo.gif)

NutBar is an [Omarchy](https://omarchy.org) shell plugin plus a local daemon
(`cashud`) that gives your desktop a Chaumian ecash balance widget: top up,
send and receive Cashu tokens (with QR codes), and pay
[TollGate](https://github.com/OpenTollGate) captive-portal wifi sessions
without leaving the bar.

```
┌─────────────────────────────────────────────────────────┐
│  Omarchy bar   [ ⚡ 42 sats ] ← click to open the panel │
└─────────────────────────────────────────────────────────┘
        │ QML (curl → JSON)
        ▼
  cashud (Rust, 127.0.0.1:3939)  ──►  Cashu mint
        │                              (topup / send / melt)
        ▼
  TollGate gateway (captive wifi) ──► paid internet session
```

The panel is a **thin view** — every money operation (proofs, swaps,
token serialization, TollGate payments) happens inside the daemon, on
your machine. Keys and proofs live in `~/.local/share/omarchy-cashu/`
(NUT-02 keysets, NUT-09 restore).

## How money works here (read this first)

Ecash **works like cash: whoever holds the token holds the money.**
A Cashu "token" is bearer instrument — treat exported tokens like
banknotes.

- The **mint is a custodian**. It can print or rug; only hold what
  you're willing to hand to that operator. Set your own mint with
  `CASHUD_MINT`.
- The **default mint is a public test mint** (`testnut.cashu.space`).
  Sats there have **no money value** — fine for trying NutBar, wrong
  for real funds.
- The seed phrase **does not restore your balance** on a new device:
  it regenerates your keys, and only a NUT-09 restore sweep (wired
  into `POST /restore`) brings proofs back from the mint. To move a
  balance, use **Send all** and treat the exported token like cash.
- If the mint is unreachable your **balance is safe on this
  computer** — proofs are local. A payment that failed *before* the
  token left your wallet was not sent; ambiguous outcomes are parked
  in a pending queue and retried, never silently dropped.

## Installation

Requires: Omarchy (Hyprland), Rust (rustup), `nmcli`, systemd user
session.

```bash
git clone https://github.com/Amperstrand/nutbar.git
cd nutbar
./install.sh
```

`install.sh` builds `cashud` in release mode, installs the binary to
`~/.local/bin/`, the systemd user unit, and the plugin to
`~/.config/omarchy/plugins/nutbar/`. Then restart the shell:

```bash
omarchy restart shell
```

The bar widget (⚡ and your balance) appears in the right section.
Click it for the full panel.

### Manual install

```bash
cargo build --release --manifest-path daemon/Cargo.toml
install -Dm755 daemon/target/release/cashud ~/.local/bin/cashud
install -Dm644 deploy/omarchy-cashud.service ~/.config/systemd/user/
mkdir -p ~/.config/omarchy/plugins/nutbar
cp plugin/nutbar/* ~/.config/omarchy/plugins/nutbar/
systemctl --user daemon-reload && systemctl --user enable --now omarchy-cashud
```

## Configuration

All configuration is environment variables on the
`omarchy-cashud.service` unit. The cleanest way to set them:

```bash
mkdir -p ~/.config/omarchy-cashu
cat > ~/.config/omarchy-cashu/env <<EOF
CASHUD_MINT=https://your-mint.example
EOF
systemctl --user restart omarchy-cashud
```

| Variable | Default | What it does |
|---|---|---|
| `CASHUD_MINT` | `https://testnut.cashu.space` | Mint URL. **Set your own** for anything real. |
| `CASHUD_LISTEN` | `127.0.0.1:3939` | Daemon HTTP bind. Loopback only by default. |
| `CASHUD_WIFI` | `0` | `1` enables the nmcli watcher: detects TollGate APs, manages fallback. |
| `CASHUD_AUTOPAY` | `0` | `1` enables TollGate auto-pay: first connect on a TollGate AP and renewals are paid automatically (cost-capped by `CASHUD_MAX_PAYMENT_SATS`, bounded by `CASHUD_MAX_BLIND_PAYMENTS`). |
| `CASHUD_STASH_TARGET` | `20` | Sats kept as pre-split offline tokens, for paying captive TollGates before the mint is reachable. |
| `CASHUD_MAX_PAYMENT_SATS` | `20` | Hard per-payment ceiling — a safety brake on gateway costs. |
| `CASHUD_MAX_BLIND_PAYMENTS` | `3` | Max blind (offline) TollGate payments before requiring mint contact. |
| `CASHUD_RENEWAL_OFFSET_MS` | `15000` | Renew a session when ≤ this many ms remain. |
| `CASHUD_RENEWAL_OFFSET_BYTES` | `20971520` | Renew a session when ≤ this many bytes remain (20 MiB). |
| `CASHUD_COMPACTION_SECS` | `300` | How often the operation journal is compacted. |
| `CASHUD_DATA_DIR` | `~/.local/share/omarchy-cashu` | State directory (proofs, keysets, journal). |

## Usage

- **Balance widget** — always visible in the bar; click to open the panel.
- **Top up** — the panel mints new ecash via Lightning invoice (mint
  quote → pay → tokens land in your wallet).
- **Send** — export sats as a Cashu token string (or QR). Remember:
  the token spends like cash.
- **Receive** — paste or scan a token to redeem it to your wallet.
- **TollGate wifi** — join a TollGate AP and pay the session with one
  click from the panel; with `CASHUD_AUTOPAY=1` the daemon pays
  first-connect and renewals automatically while you stay connected
  (opt-in; every payment is cost-capped and visible as
  `gateway_spent_sats` in `/status`).

### Daemon HTTP API

The daemon speaks JSON on `127.0.0.1:3939` — the panel is just a
client, and so can your scripts be:

```bash
curl -s http://127.0.0.1:3939/status | jq .
curl -s --json '{"amount_sats":10}' http://127.0.0.1:3939/send | jq -r .token
```

Core routes: `/status`, `/mint`, `/send`, `/receive`, `/restore`,
`/history`, `/mints`, `/tollgate/pay`, `/tollgate/session`,
`/parse-invoice`, `/melt-quote`, `/wifi/*`.

## Development

```bash
cargo test --manifest-path daemon/Cargo.toml   # unit + property tests
cargo build --manifest-path daemon/Cargo.toml  # debug build
```

`daemon/src/bin/mockgate.rs` is a tiny mock TollGate gateway for
testing the payment flow without hardware.

## License

MIT — see [LICENSE](LICENSE).
