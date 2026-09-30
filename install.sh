#!/usr/bin/env bash
# NutBar installer — builds cashud, installs the systemd unit and the
# Omarchy plugin. Run from the repo root: ./install.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_DIR="${HOME}/.config/omarchy/plugins/nutbar"
UNIT_DIR="${HOME}/.config/systemd/user"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }

[[ -f "${REPO_ROOT}/daemon/Cargo.toml" ]] || { echo "Run from the repo root"; exit 1; }
command -v cargo >/dev/null || { echo "cargo (rustup) not found"; exit 1; }
command -v nmcli >/dev/null || echo "note: nmcli not found — TollGate wifi features will be disabled"

say "Building cashud (release)"
cargo build --release --manifest-path "${REPO_ROOT}/daemon/Cargo.toml"

say "Installing binary to ~/.local/bin/cashud"
install -Dm755 "${REPO_ROOT}/daemon/target/release/cashud" "${HOME}/.local/bin/cashud"

say "Installing systemd user unit"
install -Dm644 "${REPO_ROOT}/deploy/omarchy-cashud.service" "${UNIT_DIR}/omarchy-cashud.service"

say "Installing plugin to ${PLUGIN_DIR}"
mkdir -p "${PLUGIN_DIR}"
cp "${REPO_ROOT}"/plugin/nutbar/* "${PLUGIN_DIR}/"

systemctl --user daemon-reload
systemctl --user enable --now omarchy-cashud.service

say "Done. Restart the shell to load the plugin:"
echo "    omarchy restart shell"
echo
echo "The bar widget appears in the right section (⚡ + balance)."
echo "Set your mint:  ~/.config/omarchy-cashu/env  →  CASHUD_MINT=https://..."
