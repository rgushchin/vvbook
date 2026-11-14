#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENV_PY="${ROOT_DIR}/.venv/bin/python"
BIN_PATH="${ROOT_DIR}/dist/vvbook-x3-ramprobe.bin"
BACKUP_DIR="${ROOT_DIR}/backups"
LOG_PATH="${BACKUP_DIR}/stage2_ramprobe.log"
PORT="${1:-/dev/ttyACM0}"

if [[ ! -x "${VENV_PY}" ]]; then
    echo "Missing virtualenv python at ${VENV_PY}" >&2
    exit 1
fi

if [[ ! -f "${BIN_PATH}" ]]; then
    echo "Missing Stage 2 SRAM binary at ${BIN_PATH}" >&2
    exit 1
fi

mkdir -p "${BACKUP_DIR}"

if [[ ! -e "${PORT}" ]]; then
    echo "[Stage 2] Watching for ${PORT} (90s window — press/hold Power or replug USB-C on Xteink X3 now)..."
    found=0
    for _ in $(seq 1 4500); do
        if [[ -e "${PORT}" ]]; then
            found=1
            break
        fi
        sleep 0.02
    done

    if [[ "${found}" -eq 0 ]]; then
        echo "Timed out waiting for ${PORT}. Hold/press Power or replug USB-C while running this script." >&2
        exit 1
    fi
fi

echo "[Stage 2] Detected ${PORT}! Catching ROM bootloader and loading pure-SRAM probe (ZERO flash writes)..."
"${VENV_PY}" - "${PORT}" "${BIN_PATH}" <<'PY' | tee "${LOG_PATH}"
import sys
import termios
import time
import esptool.cmds as cmds


def clear_hupcl(fd: int) -> None:
    attrs = termios.tcgetattr(fd)
    attrs[2] &= ~termios.HUPCL
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


def detect_with_udev_wait(port: str):
    last_err = None
    for _ in range(15):
        try:
            return cmds.detect_chip(
                port=port,
                baud=115200,
                connect_mode="usb-reset",
                connect_attempts=4,
            )
        except (PermissionError, OSError) as exc:
            last_err = exc
            time.sleep(0.03)
        except Exception as exc:
            last_err = exc
            try:
                return cmds.detect_chip(
                    port=port,
                    baud=115200,
                    connect_mode="no-reset",
                    connect_attempts=3,
                )
            except Exception as inner_exc:
                last_err = inner_exc
                time.sleep(0.03)
    raise RuntimeError(f"Failed to connect to {port}: {last_err}")


def main() -> int:
    port = sys.argv[1]
    bin_path = sys.argv[2]

    esp = detect_with_udev_wait(port)
    clear_hupcl(esp._port.fileno())
    print(f"[Stage 2] Connected to {esp.CHIP_NAME} on {port} (HUPCL cleared)", flush=True)

    cmds.load_ram(esp, bin_path)
    clear_hupcl(esp._port.fileno())
    esp._port.timeout = 0.25

    print(
        "[Stage 2] SRAM probe running! Reading live telemetry for 20s (try pressing chin/edge buttons now)...",
        flush=True,
    )
    deadline = time.monotonic() + 20.0
    buf = bytearray()
    while time.monotonic() < deadline:
        chunk = esp._port.read(512)
        if chunk:
            buf.extend(chunk)
            while b"\n" in buf:
                idx = buf.index(b"\n")
                line = bytes(buf[:idx]).decode("utf-8", errors="replace").rstrip("\r")
                del buf[: idx + 1]
                print(line, flush=True)
    if buf:
        print(bytes(buf).decode("utf-8", errors="replace"), flush=True)

    print(
        "[Stage 2] Complete! Flash was NOT modified (probe remains active in SRAM until reset/power-cycle).",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
PY
