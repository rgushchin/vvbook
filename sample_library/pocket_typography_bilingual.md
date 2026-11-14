# VVBook Folio OS · User Guide

Folio OS transforms the Xteink X3 E-Ink reader into a serene, high-craft digital book crafted with Cupertino minimalism and pure bare-metal Rust.

## Беспроводная передача VVDrop

Откройте экран VVDrop, отсканируйте QR-код с телефона или компьютера и введите 6-значный PIN. Все книги в форматах FB2, FB2.ZIP, EPUB, Markdown, HTML, TXT и PDF мгновенно попадают на карту памяти.

## Zero-Drain Battery Architecture

A 16-to-32 block SRAM prefetch window powers down the microSD rail (`GPIO 13`) immediately after loading upcoming pages, while high-voltage panel rails sleep after 2.5 seconds.
