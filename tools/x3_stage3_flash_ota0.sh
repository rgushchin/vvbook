#!/usr/bin/env bash
set -euo pipefail

# Stage 3: Dual-slot install of VVBook Folio OS into INACTIVE ota_0 (0x10000).
# Leaves ESP-IDF bootloader (0x0), NVS (0x9000), and stock crosspoint-reader 1.6.5 (ota_1 @ 0x780000)
# 100% UNTOUCHED on flash! Rollback to stock takes ~2 seconds via ./tools/x3_restore_stock.sh quick.

PORT="${1:-/dev/ttyACM0}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [[ ! -f dist/vvbook-x3-ota0.bin ]]; then
  echo "ERROR: dist/vvbook-x3-ota0.bin not found. Build firmware/vvbook-x3-fw first." >&2
  exit 1
fi

echo "[Stage 3] Verifying factory backup SHA-256 before any flash write..."
sha256sum -c backups/SHA256SUMS.txt

echo "[Stage 3] Verifying dist/vvbook-x3-ota0.bin header & checksum..."
.venv/bin/python -m esptool --chip esp32c3 image-info dist/vvbook-x3-ota0.bin

echo "[Stage 3] Generating 8 KB otadata selecting ota_0 (seq=5 > stock seq=4)..."
TMP_OTADATA="$(mktemp /tmp/x3_vvbook_otadata.XXXXXX.bin)"
trap 'rm -f "$TMP_OTADATA"' EXIT
.venv/bin/python -c "
import struct, zlib
data = open('backups/x3_factory_backup_1.bin', 'rb').read()
# Verify factory otadata CRC formula against active ota_1 sector at 0xF000 (seq=4 -> crc=0x709d68a8)
seq_f, _, crc_f = struct.unpack_from('<I20xII', data, 0xF000)
expected_crc = zlib.crc32(struct.pack('<I', seq_f), 0xFFFFFFFF)
assert crc_f == expected_crc, f'Unexpected factory CRC {hex(crc_f)} vs {hex(expected_crc)}'

# Preserve factory sec1 (0xF000, ota_1 seq=4) untouched; update ONLY sec0 (0xE000) to select ota_0 (new_seq=5).
sec0 = bytearray([0xFF] * 4096)
sec1 = bytearray(data[0xF000:0x10000])
new_seq = seq_f + 1
while (new_seq - 1) % 2 != 0:
    new_seq += 1
raw_seq = struct.pack('<I', new_seq)
new_crc = zlib.crc32(raw_seq, 0xFFFFFFFF)
sec0[:32] = raw_seq + (b'\xff' * 20) + struct.pack('<II', 0x00000002, new_crc) # ESP_OTA_IMG_VALID = 2
open('$TMP_OTADATA', 'wb').write(sec0 + sec1)
print(f'Prepared otadata selecting ota_0 (new_seq={new_seq}, crc={hex(new_crc)}), preserving ota_1 sec1 seq={seq_f}')
"

echo "[Stage 3] Flashing ONLY ota_0 @ 0x10000 and otadata @ 0xE000 (ota_1 @ 0x780000 untouched)..."
.venv/bin/python -m esptool --chip esp32c3 --port "$PORT" \
  write-flash --flash-mode dio --flash-freq 80m --flash-size 16MB \
  0xE000 "$TMP_OTADATA" \
  0x10000 dist/vvbook-x3-ota0.bin
echo "[Stage 3] Complete! VVBook Folio OS is now running from ota_0."
