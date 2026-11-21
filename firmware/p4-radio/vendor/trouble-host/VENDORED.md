# trouble-host 0.8.0, vendored

This directory is the published `trouble-host` 0.8.0 crate, the Bluetooth host the C6 runs
(`firmware/p4-radio`). It is here so the C6 can build it on `bt-hci` 0.11 before a
`trouble-host` release does. [`../../README.md`](../../README.md) § "Dependency versions" has the
reasoning.

## Provenance

| | |
|---|---|
| Project | TrouBLE, by the Embassy project contributors |
| Repository | <https://github.com/embassy-rs/trouble>, directory `host` |
| Crate | `trouble-host` 0.8.0, from crates.io |
| Upstream commit of the release | `42e3e04de00db3951b126741e1f3602bfdf5df47` (`.cargo_vcs_info.json`) |
| SHA-256 of `trouble-host-0.8.0.crate` | `19a566cc37ceaff5cc2a6ff8cc02debd1ee6b95d59ced8830d005da8b080cbe9`, the checksum `firmware/p4-radio/Cargo.lock` held for it |
| Left out | the crate's `Cargo.lock` and cargo's own `.cargo-ok` marker |
| Added | `LICENSE-MIT` and `LICENSE-APACHE`, from the repository at tag `trouble-host-v0.8.0`, and this file |
| Changes | the `host/` part of upstream commit `44f91293e618008b8d85369376a1ac97f5f6e728` ("chore: update bt-hci to 0.11", embassy-rs/trouble), in `Cargo.toml` (with `Cargo.toml.orig`), `src/connection.rs`, `src/host.rs` and `src/lib.rs`; its `Disconnect` call in `ControlRunner::run` keeps 0.8.0's shape |

## License

TrouBLE is licensed under MIT or Apache-2.0, at your option. [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE) are the repository's license files, unmodified. The published
crate carries neither. The rest of this repository is under its own
[`LICENSE`](../../../../LICENSE).

## Removing it

Delete this directory, and the `[patch.crates-io]` entry that points at it, when a
`trouble-host` release builds on `bt-hci` 0.11 or later. Dependabot's pull request for that
release is the signal.
