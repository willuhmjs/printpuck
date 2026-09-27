# PrintPuck

A round ESP32-S3 desk display for Bambu Lab printers. Bare-metal Rust
(no ESP-IDF, no FreeRTOS), embassy-async, for the Spotpear ESP32-S3 1.28"
round touch box (GC9A01 240x240 + CST816D touch).

PrintPuck talks straight to the printer over its local MQTT interface
(TLS on :8883) - no cloud account, no Home Assistant, nothing between the
puck and the printer.

## What it shows

- Print progress (outer dial fills as the print advances)
- State (printing / paused / preparing / finished / failed) with stage detail
- Remaining time, layer count
- Nozzle and bed temperatures
- Active AMS tray colors
- Printer error state (HMS/print_error) with the raw code
- Chamber light toggle (tap the center while printing)

## Setup

First boot starts a setup hotspot (`PrintPuck-Setup`, open). Join it and
open http://192.168.4.1 to enter Wi-Fi credentials and the printer's IP
address, serial number, and access code (Printer settings > Network >
Access code). The puck restarts and joins your network.

## Build

```
rustup toolchain install esp   # xtensa fork of rustc, once
source ~/export-esp.sh
cargo build --release
espflash flash --monitor --chip esp32s3 target/xtensa-esp32s3-none-elf/release/printpuck
```

## Hardware layer provenance

The board bring-up (GPIO map, GC9A01/LCD driver wiring, CST816D touch
driver, SoftAP setup portal plumbing, flash settings-record scheme,
dirty-rect display flushing) is shared with the author's own
[esp32s3-ai-assistant](https://github.com/willuhmjs/esp32s3-ai-assistant)
firmware, which runs on the same board. Everything you see on screen - the
layout, palette, typography, and interaction model - is designed for
PrintPuck from scratch.

## Protocol

Bambu Lab's local printer interface (MQTT topics, JSON report fields,
gcode state names, ledctrl command) is a factual interface specification
and is used as documented. No code, assets, fonts, or design elements are
taken from any other project.

## License

MIT
