#!/usr/bin/env bash
set -euo pipefail

# Safely restore the Xteink X3 to stock crosspoint-reader 1.6.5 firmware.
# Usage:
#   ./tools/x3_restore_stock.sh [quick|full] [/dev/ttyACM0]
#   - quick (default): resets otadata (0xE000, 8 KB) back to factory ota_1 in ~2 seconds
#   - full: verifies backups/SHA256SUMS.txt and restores the exact 16 MB factory image

MODE="${1:-quick}"
PORT="${2:-/dev/ttyACM0}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [[ ! -d .venv ]]; then
  echo "ERROR: .venv not found in $REPO_ROOT" >&2
  exit 1
fi

if [[ "$MODE" == "quick" ]]; then
  echo "[Restore] Checking factory backup integrity first..."
  sha256sum -c backups/SHA256SUMS.txt
  echo "[Restore] Extracting factory otadata (0xE000, 8 KB -> active ota_1)..."
  TMP_OTA="$(mktemp /tmp/x3_factory_otadata.XXXXXX.bin)"
  trap 'rm -f "$TMP_OTA"' EXIT
  .venv/bin/python -c "
data = open('backups/x3_factory_backup_1.bin', 'rb').read()
open('$TMP_OTA', 'wb').write(data[0xE000:0x10000])
"
  .venv/bin/python -m esptool --chip esp32c3 --port "$PORT" \
    write-flash --flash-mode dio --flash-freq 80m --flash-size 16MB \
    0xE000 "$TMP_OTA"
  echo "[Restore] Done! Device booted back to untouched stock crosspoint-reader 1.6.5 in ota_1."
elif [[ "$MODE" == "full" ]]; then
  echo "[Restore] Verifying 16 MB factory backups against backups/SHA256SUMS.txt..."
  sha256sum -c backups/SHA256SUMS.txt
  echo "[Restore] Writing full 16 MB factory backup to flash (will take ~2 minutes)..."
  .venv/bin/python -m esptool --chip esp32c3 --port "$PORT" \
    write-flash --flash-mode dio --flash-freq 80m --flash-size 16MB \
    0x0 backups/x3_factory_backup_1.bin
  echo "[Restore] Full 16 MB factory image restored cleanly."
else
  echo "Usage: $0 [quick|full] [/dev/ttyACM0]" >&2
  exit 2
fi
