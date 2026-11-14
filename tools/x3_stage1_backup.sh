#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENV_PY="${ROOT_DIR}/.venv/bin/python"
BACKUP_DIR="${ROOT_DIR}/backups"
PORT="${1:-/dev/ttyACM0}"

if [[ ! -x "${VENV_PY}" ]]; then
    echo "Missing virtualenv python at ${VENV_PY}" >&2
    exit 1
fi

mkdir -p "${BACKUP_DIR}"

echo "[Stage 1] Watching for ${PORT} (60s window — wake or replug Xteink X3 now if asleep)..."
found=0
for _ in $(seq 1 3000); do
    if [[ -e "${PORT}" ]]; then
        found=1
        break
    fi
    sleep 0.02
done

if [[ "${found}" -eq 0 ]]; then
    echo "Timed out waiting for ${PORT}. Hold/press Power or replug USB while running this script." >&2
    exit 1
fi

echo "[Stage 1] Detected ${PORT}! Catching ROM bootloader (READ-ONLY)..."
FLASH_ID_OUT="$("${VENV_PY}" -m esptool --chip esp32c3 --port "${PORT}" --before usb-reset --after no-reset flash-id)"
echo "${FLASH_ID_OUT}" | tee "${BACKUP_DIR}/flash_id.txt"

FLASH_HEX="$(echo "${FLASH_ID_OUT}" | "${VENV_PY}" -c '
import re, sys
txt = sys.stdin.read()
m = re.search(r"Detected flash size:\s*(\d+)MB", txt, re.I)
if not m:
    sys.exit(2)
mb = int(m.group(1))
sizes = {4: "0x400000", 8: "0x800000", 16: "0x1000000"}
if mb not in sizes:
    sys.exit(3)
print(sizes[mb])
')"
echo "[Stage 1] Verified flash size argument: ${FLASH_HEX}"

echo "[Stage 1] Reading eFuse summary (READ-ONLY)..."
"${VENV_PY}" -m espefuse --chip esp32c3 --port "${PORT}" --before no-reset --after no-reset summary > "${BACKUP_DIR}/efuse_summary.txt"

B1="${BACKUP_DIR}/x3_factory_backup_1.bin"
B2="${BACKUP_DIR}/x3_factory_backup_2.bin"

echo "[Stage 1] Reading full factory flash pass 1/${FLASH_HEX} -> ${B1}..."
"${VENV_PY}" -m esptool --chip esp32c3 --port "${PORT}" --before no-reset --after no-reset read-flash 0x0 "${FLASH_HEX}" "${B1}"

echo "[Stage 1] Reading full factory flash pass 2/${FLASH_HEX} -> ${B2}..."
"${VENV_PY}" -m esptool --chip esp32c3 --port "${PORT}" --before no-reset --after no-reset read-flash 0x0 "${FLASH_HEX}" "${B2}"

H1="$(sha256sum "${B1}" | awk '{print $1}')"
H2="$(sha256sum "${B2}" | awk '{print $1}')"
if [[ -z "${H1}" || "${H1}" != "${H2}" ]]; then
    echo "ERROR: Factory flash backup checksums DO NOT MATCH (${H1} vs ${H2})!" >&2
    exit 1
fi

sha256sum "${B1}" "${B2}" > "${BACKUP_DIR}/SHA256SUMS.txt"
echo "[Stage 1] VERIFIED IDENTICAL BACKUPS: SHA-256 = ${H1}"

echo "[Stage 1] Parsing ESP-IDF partition table @ 0x8000..."
"${VENV_PY}" - "${B1}" <<'PY' | tee "${BACKUP_DIR}/partition_table.txt"
import struct, sys

with open(sys.argv[1], "rb") as f:
    f.seek(0x8000)
    pt = f.read(3072)

TYPES = {0x00: "app", 0x01: "data"}
APP_SUB = {0x00: "factory", **{0x10 + i: f"ota_{i}" for i in range(16)}, 0x20: "test"}
DATA_SUB = {0x00: "ota", 0x01: "phy", 0x02: "nvs", 0x03: "coredump", 0x04: "nvs_keys", 0x05: "efuse", 0x80: "esphttpd", 0x81: "fat", 0x82: "spiffs"}

print(f"{'Name':<16} {'Type':<6} {'SubType':<10} {'Offset':<10} {'Size':<10}")
print("-" * 56)
for i in range(0, len(pt), 32):
    ent = pt[i:i+32]
    if len(ent) < 32:
        break
    magic, ptype, subtype, off, size = struct.unpack("<HBBL L", ent[:12])
    if magic == 0xEBEB:
        break
    if magic != 0x50AA:
        continue
    name = ent[12:28].split(b"\x00", 1)[0].decode("utf-8", "replace")
    tname = TYPES.get(ptype, hex(ptype))
    if ptype == 0x00:
        sname = APP_SUB.get(subtype, hex(subtype))
    elif ptype == 0x01:
        sname = DATA_SUB.get(subtype, hex(subtype))
    else:
        sname = hex(subtype)
    print(f"{name:<16} {tname:<6} {sname:<10} 0x{off:06X}   0x{size:06X} ({size // 1024} KB)")
PY

echo "[Stage 1] Complete! Stock firmware was NOT modified."
