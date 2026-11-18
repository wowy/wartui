# WardriveESP protocol, vendored

`PROTOCOL.md` is the WardriveESP protocol document, copied unmodified from the WardriveESP
package. wartui's bridge speaks the Bluetooth part of it to phone apps, and this copy is the
reference that implementation is written from. [`../../wardrive-esp.md`](../../wardrive-esp.md)
says which part the bridge implements and where it differs.

## Provenance

| | |
|---|---|
| Project | WardriveESP, by RocketGod |
| Package | <https://rocketgod.dev/WardriveESP/WardriveESP.zip> |
| File in the package | `PROTOCOL.md`, at its root and again under `source/`, byte-identical |
| Package files dated | 2026-11-11 |
| Firmware version in the package | 1.7.8 (`FW_VERSION`) |
| Document title | "WardriveESP protocol v1.6" |
| Size | 53,773 bytes |
| SHA-256 | `7628a9a75b2cc2866b5fc88ae54f8c5020b7fdf2027f80e7034255bc2db7e5c7` |
| Changes | none |

Check the copy with `sha256sum docs/vendor/wardrive-esp/PROTOCOL.md`.

## License

WardriveESP is MIT licensed, Copyright (c) 2026 RocketGod. [`LICENSE`](LICENSE) is the package's
license file, unmodified, and it covers `PROTOCOL.md`. The rest of this repository is under its
own [`LICENSE`](../../../LICENSE).

## What is here and what is not

- `PROTOCOL.md` and `LICENSE` are the only files taken from the package.
- None of the package's source code, tools, fonts or firmware images is in this repository.
- wartui's implementation is its own code, written from the protocol. It copies none of
  WardriveESP's source.

The package's `NOTICE.md` says of the protocol's own origin: WardriveESP "contains no GhostESP
source code; its BLE UUIDs, frame layout and `WD:AP` line format match GhostESP's GhostLink bridge
for interoperability only." wartui uses the same UUIDs, frame layout and line formats for the same
reason.

## Updating

Replace `PROTOCOL.md` and `LICENSE` with the new package's files, unmodified, and update the table
above. Then read the difference against [`../../wardrive-esp.md`](../../wardrive-esp.md).
