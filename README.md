# VVBook — Folio OS for Xteink X3

A pure-Rust e-reader operating system, bare-metal ESP32-C3 firmware, and desktop emulator for the **Xteink X3**.

> **Why "VV"?**  
> The **VV** in *VVBook* comes from **Vladimir Vladimirovich Nabokov**.

## Highlights

- **Memory-safe core**: `#![forbid(unsafe_code)]` across all shared OS, parser, typesetting, graphics, security, and HAL protocol crates; bare-metal `no_std` ESP32-C3 XIP firmware (`ota_0`).
- **Direct multi-format library**: Streaming support for **EPUB**, **FB2 / FB2.ZIP**, **MOBI**, **TXT**, **Markdown**, and pre-indexed **VVB** books with automatic UTF-8, Windows-1251, and KOI8-R encoding detection.
- **Book-grade typography**: Knuth-style hyphenated line breaking (English & Russian), multi-size baked serif/sans/mono fonts, table of contents, live `Aa` typography HUD, and crisp 1-bit / 2-bit E-Ink rendering on the SSD1677 panel.
- **VVDrop WPA2 web portal**: Local soft-AP (`VVBook-Folio`) powered by `smoltcp` (DHCPv4 + DNS + concurrent streaming HTTP/80 slots at `http://192.168.4.1`) for wireless book uploads over browser.
- **Extras & bilingual UI**: Full English and Russian interface, Literary / Cupertino / Pip sleep posters, built-in Chess engine, and Pocket Pip.
- **Non-destructive dual-slot install**: Installs exclusively into `ota_0` (`0x10000`), leaving the factory bootloader (`0x0`), NVS (`0x9000`), and stock firmware in `ota_1` (`0x780000`) untouched, with ~2-second rollback to stock.

## Repository Layout

- `crates/vvbook-format` — Streaming EPUB, FB2/FB2.ZIP, MOBI, TXT, Markdown, and VVB parsers
- `crates/vvbook-typeset` & `crates/vvbook-gfx` — Pagination, hyphenation, baked fonts, dithering, icons, and QR codes
- `crates/vvbook-userspace` & `crates/vvbook-games` — Folio OS UI, reader, settings, Chess, and Pocket Pip
- `crates/vvbook-x3-hal` & `crates/vvbook-security` — SSD1677 EPD, FAT32 SD, ADC button ladder, BQ27220/SHTC3/RX8130CE drivers, and `smoltcp` VVDrop portal
- `crates/vvbook-emu` & `crates/vvbook-cli` — Desktop emulator and X3 Hardware Twin test suite
- `firmware/vvbook-x3-fw` & `firmware/vvbook-x3-ramprobe` — Bare-metal ESP32-C3 firmware and Stage-2 RAM hardware probe
- `tools/` — ESP-IDF OTA image packer (`vvbook-espimage`), font baker (`vvbook-fontbake`), backup, flash, and stock-restore scripts

## Desktop Emulator & Tests

```bash
# Run full workspace test suite (including X3 Hardware Twin & smoltcp portal tests)
cargo test --workspace --release

# Launch the desktop emulator
cargo run --release -p vvbook-emu
```

## Building & Flashing (`Xteink X3`)

```bash
# 1. One-time dual-pass 16 MB factory flash backup (read-only)
./tools/x3_stage1_backup.sh /dev/ttyACM0

# 2. Build bare-metal ESP32-C3 firmware and pack ESP-IDF OTA image
(cd firmware/vvbook-x3-fw && RUSTC_BOOTSTRAP=1 cargo build --release --target riscv32imc-unknown-none-elf -Z build-std=core,alloc)
cargo run --release -p vvbook-espimage -- firmware/vvbook-x3-fw/target/riscv32imc-unknown-none-elf/release/vvbook-x3-fw -o dist/vvbook-x3-ota0.bin

# 3. Flash ONLY ota_0 (0x10000) + otadata (0xE000); stock ota_1 remains untouched
./tools/x3_stage3_flash_ota0.sh /dev/ttyACM0

# Optional: switch back to untouched stock firmware in ~2 seconds
./tools/x3_restore_stock.sh quick /dev/ttyACM0
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
