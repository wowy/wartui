# firmware/c6-loader

A one-shot esp-hal app for a factory Waveshare ESP32-P4-WIFI6's ESP32-C6. The P4 pushes it
through the OTA of ESP-Hosted, the firmware the C6 ships with. It then writes what a wired flash of
[`firmware/p4-radio`](../p4-radio/README.md#first-flash) writes, and resets into p4-radio.

It exists because the P4 reaches neither the C6's IO9 nor its UART0, so ESP-Hosted is the only
way in without soldering header H4. The design is
[#395](https://github.com/wowy/wartui/issues/395), and what the bench measured is in
[`docs/c6-first-flash-findings.md`](../../docs/c6-first-flash-findings.md).

## What it carries

`build.rs` builds p4-radio through [`radio-image`](../radio-image), the crate the P4 bridge's
build also uses, so both embed the same app. From p4-radio's merged image it takes the bootloader
(0x0..0x8000) and the table sector (0x8000..0x9000), saved with the wired flash's flash mode,
frequency and size.

## Order of writes

The loader reads the factory table and `otadata`, finds the slot it booted from, and plans every
write from p4-radio's table (`wartui_proto::first_flash`). A write that would land on the loader
is refused before anything is written. A refusal, or an embedded image whose SHA-256 is wrong,
erases the newest `otadata` entry, so the next reset boots ESP-Hosted again.

| Step | Writes | A power cut here |
| --- | --- | --- |
| 1 | p4-radio's app into our `ota_0` at 0x10000 | restarts the loader |
| 2 | nothing: reads the app back and checks its SHA-256 | restarts the loader |
| 3 | erases `trial` (0x3D0000) | restarts the loader |
| 4 | **our bootloader at 0x0**, read back and compared | **needs the header** |
| 5 | **our table at 0x8000**, read back and compared | **needs the header** |
| 6 | erases `otadata`, so the C6 boots `ota_0` | restarts the loader |

Until step 6, the factory table and `otadata` still select the loader, and our bootloader runs it
too. Steps 4 and 5 are the two risky windows, back to back and a few sectors long.

## What it reports

Lines on UART1 (TX GPIO20, RX GPIO18) at 115200 8N1, each ending `\r\n`. The P4 loader relays
them.

| Line | When |
| --- | --- |
| `c6-loader: start` | first |
| `c6-loader: step <text>` | before each step, as `write the bootloader at 0x0..0x8000` |
| `c6-loader: done` | all six steps done; then it resets |
| `c6-loader: done rehearsal` | a rehearsal checked and ESP-Hosted selected again; then it resets |
| `c6-loader: failed <why>` | repeated every 5 s, and it never resets |

## Rehearsal

Built with `--features rehearse`, the loader makes every check, sends `step would <text>` for each
of the six steps, then erases the newest `otadata` entry and resets into ESP-Hosted. It writes
nothing else, so it benches the plan on a factory board and leaves the board as it shipped.

## Building

```sh
cargo build --release                       # needs espflash on the PATH, as p4-radio's bundling does
cargo build --release --features rehearse
```

The ELF is `target/riscv32imac-unknown-none-elf/release/wartui-c6-loader-fw`. The P4 loader's
`build.sh` ([`firmware/p4-c6-loader`](../p4-c6-loader)) builds it and saves the image it embeds.
