# C6 First Flash Over SDIO Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `wartui flash-bridge` takes a factory-fresh Waveshare ESP32-P4-WIFI6 to a working bridge over its one Type-C cable. The C6 is loaded through ESP-Hosted's slave OTA instead of the soldered H4 header.

**Architecture:** There are two throwaway loaders, and the production firmware is unchanged.
- **The P4 loader** (ESP-IDF C) pushes the C6 loader through the factory ESP-Hosted's begin/write/end OTA. It then relays the C6 loader's verdict as `wartui-loader:` lines on the CH343.
- **The C6 loader** (esp-hal) copies p4-radio's app, the bootloader and our partition table into place, then resets.
- **`flash-bridge`** notices a silent CH343 that `espflash board-info` reads as an ESP32-P4. It asks y/N, writes the P4 loader, reads its verdict, writes the P4 bridge's merged image, and succeeds only once a `Ready` carries the C6's address.

**Tech Stack:**
- Rust: the host crates; esp-hal 1.2.2 for the C6 loader.
- ESP-IDF v5.5.4 with `espressif/esp_hosted` 2.12.13 for the P4 loader.
- espflash 4.7.
- GitHub Actions with `espressif/esp-idf-ci-action`.

**Spec:** GitHub issue #395, <https://github.com/wowy/wartui/issues/395> (`gh issue view 395`). Read it first: this plan argues from its design, its bench record and its order of writes. Where the issue says *(builder's call)*, this plan makes the call. Each call is marked **Call:** so a reviewer can check it.

## Global Constraints

- **No full real MAC address, BSSID, SSID or serial number** in any tracked file, commit or PR. Name the bench board `A9:76`. Tests use locally administered addresses (`02:00:5E:10:44:C0`).
- **Branch first:** `git switch -c c6-first-flash` before the first commit. Nothing is committed to `main`, and the work lands through a PR. This plan file is committed on that branch in Task 1.
- **Pins:**
  - ESP-IDF **v5.5.4**, `espressif/esp_hosted` **==2.12.13**.
  - `espressif/esp_wifi_remote` at the version the probe resolved (`<scratchpad>/p4-probe/dependencies.lock`). Commit the loader's `dependencies.lock`.
- The C6 loader's crates are p4-radio's: `esp-hal` 1.2.2 (`esp32c6`, `unstable`), `esp-bootloader-esp-idf` 0.6.0, `esp-storage` 0.10.0, stable toolchain, target `riscv32imac-unknown-none-elf`.
- p4-radio's images are always saved with `--flash-mode dio --flash-freq 40mhz --flash-size 4mb` (its runner's). Then the app the C6 loader writes carries the digest of the image the P4 bundles.
- **Release asset names:** `wartui-p4-c6-loader-esp32p4.bin` (new) and `wartui-p4-bridge-fw-esp32p4.bin` (exists).
- **Pre-1.0:** nothing is kept for compatibility.
- **AGENTS.md conventions:**
  - Test names follow `component_action_when_condition`.
  - `//!` docs say why, in the present tense.
  - American spelling.
  - rustfmt `max_width = 100`.
  - Each firmware is its own workspace with its own fmt and clippy.
- **Who writes what (CLAUDE.md):** the `rust-developer` agent writes the Rust tasks from this plan. The ESP-IDF C task (Task 4) goes to a general agent or the main session. Bench steps run in the main session.
- **Bench rules:**
  - Every flash of `A9:76` needs the user's go-ahead that day.
  - Every listener gets a `timeout`.
  - Open the CH343 first, then `stty -f <port> 115200 raw -echo clocal` while holding it (`<scratchpad>/listen.pl` does this).
  - ESP-IDF lives in the scratchpad: `IDF_TOOLS_PATH=<scratchpad>/idf-tools . <scratchpad>/esp-idf/export.sh`.
- **`<scratchpad>`** is the session's scratchpad directory. It holds `esp-idf/`, `idf-tools/`, `p4-probe/`, `c6-probe/` and `listen.pl`, and a reboot clears it. If it is gone, reinstall per memory `bench-testing-from-the-bash-tool` (about 10 minutes).

## The bench board is a one-shot

`A9:76` is the only factory board. A real C6 loader run overwrites its ESP-Hosted, and nothing here can put ESP-Hosted back. So the C6 loader has a **`rehearse` feature**:
- It reads, plans and verifies everything and writes nothing.
- It then erases the newest `otadata` sector, as the probe did, and resets into ESP-Hosted.

Tasks 3, 4 and 6 bench against the rehearsal, which can run any number of times. Only Task 9 runs the real loader.

## Review Focus

1. **A silent CH343 that is not a P4** must never be written: another device, or a P4 whose port another program holds. `board-info` fails or names another chip, so the board is not offered, and the refusal says what `board-info` said. *(Task 5: `setup_refuses_when_no_silent_ch343_is_a_p4`)*
2. **The P4's own ESP-IDF log lines interleave with `wartui-loader:` lines.** That includes ANSI color, `\r\n`, lines split across reads and a run with no newline. Only our lines count. *(Task 5: `setup_ignores_lines_when_they_are_idf_log_or_noise`, `setup_reads_loader_lines_when_split_across_reads`, `setup_caps_line_when_no_newline_arrives`)*
3. **A setup stopped partway is rerun by running `flash-bridge` again.** That covers a ctrl-c, a timeout or a failed verdict. The P4 then runs the loader, stays silent and is offered again. Every failure message says so, and names the header as the fallback. *(Task 6: `setup_says_rerun_and_header_when_loader_fails`)*
4. **A C6 whose running slot overlaps a write is refused before anything is written.** That covers a C6 whose ESP-Hosted sits in `ota_1`, a C6 with `otadata` erased, and a radio image too large to fit. The refusal also puts ESP-Hosted back. *(Task 2: `first_flash_refuses_plan_when_*`; Task 3 restores on refusal)*
5. **A corrupt or foreign `otadata` entry must not pick the wrong slot.** The bootloader skips an entry whose CRC fails, and so must we. *(Task 2: `first_flash_ignores_entry_when_crc_does_not_match`)*

---

### Task 1: One build of p4-radio for every image that carries it

Both `firmware/p4-bridge/build.rs` and the new C6 loader build p4-radio and save its image. Move that code into a shared build-dependency crate, so the two can never drift apart. Changing an input list or a flash argument in one copy would give the two images different digests.

**Files:**
- Create: `firmware/radio-image/Cargo.toml`, a host (std) library.
  - It has its own empty `[workspace]`.
  - Dependencies: `sha2`, plus `wartui-proto` by path, at the versions `firmware/p4-bridge`'s build-dependencies use now.
  - Package `wartui-radio-image`.
- Create: `firmware/radio-image/src/lib.rs`
- Modify: `firmware/p4-bridge/build.rs`, which keeps `linker_be_nice` and `radio.rs`'s contents.
- Modify: `firmware/p4-bridge/Cargo.toml` (build-dependency on `../radio-image`), plus its `Cargo.lock`.
- Modify: `.github/workflows/firmware-p4-bridge.yml`: add `firmware/radio-image/**` to both path lists.

**Interfaces:**
- Produces:
  ```rust
  /// p4-radio's images, saved with its runner's flash mode, frequency and size.
  pub struct Radio {
      /// The app-only image, as one app slot holds it.
      pub app: Vec<u8>,
      /// Where `app` was saved, for `include_bytes!`.
      pub app_path: std::path::PathBuf,
      /// The merged image (bootloader at 0x0, table at 0x8000, app at 0x10000), when asked for.
      pub merged: Option<Vec<u8>>,
      /// SHA-256 of `app`, which the C6's receiver checks.
      pub sha256: [u8; 32],
      /// The digest espflash appended to `app`, which names the build.
      pub digest: [u8; 32],
  }
  /// Build firmware/p4-radio and save its images into `out_dir`. Prints every
  /// `cargo:rerun-if-changed` / `rerun-if-env-changed` line. Panics as today's build.rs does.
  pub fn build(out_dir: &Path, merged: bool) -> Radio;
  ```
- The paths are absolute and found from this crate's own `env!("CARGO_MANIFEST_DIR")`: `../p4-radio`, `../common` and `../../crates/wartui-proto`.
- `RADIO_INPUTS`, `RADIO_ENV`, `OUTER` and `run()` move here unchanged.
- The merged image is saved with `--merge --skip-padding` and the same flash args. When it is asked for, `build` asserts that `merged[0x10000..0x10000 + app.len()] == app`, and panics with `"the merged radio image's app differs from the app-only image"` if not. This is the check that makes Task 3's C6 equal the P4's bundle.

- [ ] **Step 1:** `git switch -c c6-first-flash`. Commit this plan file: `git add docs/superpowers/plans/2026-11-28-c6-first-flash.md && git commit -m "Plan the C6's first flash over SDIO"`.
- [ ] **Step 2:** Record the current bundle. Run `cd firmware/p4-bridge && cargo build --release && shasum -a 256 target/riscv32imafc-unknown-none-elf/release/build/wartui-p4-bridge-fw-*/out/radio.bin`, and note the hash of the newest `out` directory.
- [ ] **Step 3:** Create the crate. Make `bundle_radio()` in `p4-bridge/build.rs` call `wartui_radio_image::build(&out, false)` and write `radio.rs` from `Radio`, with the same three items as today.
- [ ] **Step 4:** Run `cargo build --release` in `firmware/p4-bridge`, then the shasum again. Expected: the same hash as Step 2.
- [ ] **Step 5:** Run `cargo fmt --all -- --check && cargo clippy --release -- -D warnings` in `firmware/p4-bridge`, then `cargo fmt --check && cargo clippy -- -D warnings` in `firmware/radio-image`. Expected: clean.
- [ ] **Step 6:** Commit `firmware/radio-image`, `firmware/p4-bridge/{build.rs,Cargo.toml,Cargo.lock}` and the workflow. Message: "Build p4-radio's images in one crate both bundlers share".

---

### Task 2: The C6 loader's layout logic, testable on the host

The safety checks that decide whether the C6 loader writes anything live in `wartui-proto`, as `split.rs` does for the P4, so host tests cover them. These are table parsing, the boot slot the bootloader picks, and the overlap check.

**Files:**
- Create: `crates/wartui-proto/src/first_flash.rs`, with its tests in a `#[cfg(test)]` module.
- Modify: `crates/wartui-proto/src/lib.rs` (`pub mod first_flash;`)
- Modify: `crates/wartui-proto/src/crc.rs`. Add the ROM's CRC beside `crc32` and share the table.

**Interfaces:**
- Produces:
  ```rust
  // crc.rs
  /// `esp_rom_crc32_le(crc, data)`: CRC-32/ISO-HDLC's table, register `!crc` in, `!` out.
  /// The bootloader checks an `otadata` entry with `esp_rom_crc32_le(u32::MAX, &seq_le)`.
  pub fn esp_rom_crc32_le(crc: u32, data: &[u8]) -> u32;

  // first_flash.rs
  pub const TABLE_AT: u32 = 0x8000;
  pub const TABLE_LEN: usize = 0xC00;
  pub const SECTOR: u32 = 0x1000;
  /// Our bootloader's span: everything below the table.
  pub const BOOTLOADER: Span = Span { start: 0, end: TABLE_AT };
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct Span { pub start: u32, pub end: u32 } // half-open
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct Partition { pub kind: u8, pub subtype: u8, pub offset: u32, pub size: u32, pub label: [u8; 16] }
  impl Partition { pub fn span(&self) -> Span; pub fn label(&self) -> &str /* NULs trimmed */ }
  /// Entries in order, stopping at the first without the 0xAA 0x50 magic (the MD5 row, or 0xFF).
  pub fn partitions(table: &[u8]) -> impl Iterator<Item = Partition> + '_;
  /// The `otadata` sector (0 or 1) holding the newest entry the bootloader accepts: seq neither
  /// 0 nor u32::MAX, and the CRC at bytes 28..32 equal to esp_rom_crc32_le(u32::MAX, seq).
  pub fn newest(otadata: &[[u8; 32]; 2]) -> Option<usize>;
  /// The app slot the bootloader boots: newest entry's (seq - 1) % (number of ota_N app
  /// partitions), counted in table order of subtype 0x10.., else `ota_0` when no entry is valid.
  pub fn booted(table: &[u8], otadata: &[[u8; 32]; 2]) -> Option<Partition>;
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct Plan { pub app: Span, pub otadata: Span, pub trial: Span, pub table: Span, pub bootloader: Span }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Refusal {
      /// Our table has no partition by this name (`ota_0`, `otadata`, `trial`).
      Missing(&'static str),
      /// The radio image is `len` bytes, and our `ota_0` holds `slot`.
      TooLarge { len: u32, slot: u32 },
      /// Writing `what` would overwrite the running loader in `running`.
      Overlaps { what: &'static str, running: Span },
  }
  impl core::fmt::Display for Refusal; // the text after "c6-loader: failed "
  /// What the loader writes, from our table and the radio app's length, or why it must not.
  /// `app` is ota_0's offset to the app length rounded up to SECTOR.
  pub fn plan(ours: &[u8], running: Partition, app_len: u32) -> Result<Plan, Refusal>;
  ```
- The partition entry layout is 32 bytes: magic `AA 50`, type, subtype, offset LE u32, size LE u32, 16-byte label, flags u32.
- The otadata entry layout is 32 bytes: seq LE u32 at 0, label 20 bytes, state LE u32 at 24, CRC LE u32 at 28.
- **Call:** `trial` is found by label, because "data, undefined" is not unique.
- **Call:** the whole region below 0x8000 is rewritten as the bootloader. The trailing 0xFF writes are no-ops, which keeps the bootloader's length out of the plan.

- [ ] **Step 1: Write the failing tests.** Build tables with a test helper `entry(label, kind, subtype, offset, size) -> [u8; 32]`, and otadata with `ota_entry(seq, state, crc) -> [u8; 32]`. Use the bench's factory C6 table (`nvs 0x9000/0x4000`, `otadata 0xd000/0x2000`, `phy_init 0xf000/0x1000`, `ota_0 0x10000/0x180000`, `ota_1 0x190000/0x180000`) and our table (`firmware/p4-radio/partitions.csv`'s rows).
  ```rust
  #[test] fn first_flash_computes_otadata_crc_when_given_bench_entries() {
      // Read off A9:76's otadata on 2026-11-27.
      assert_eq!(crc::esp_rom_crc32_le(u32::MAX, &1u32.to_le_bytes()), 0x4743_989A);
      assert_eq!(crc::esp_rom_crc32_le(u32::MAX, &2u32.to_le_bytes()), 0x55F6_3774);
  }
  #[test] fn first_flash_finds_booted_slot_when_newest_entry_selects_ota_1()
      // otadata [seq 1 state 2 crc 0x4743989A, seq 2 state 0xFFFFFFFF crc 0x55F63774]
      // → booted(factory).offset == 0x190000, size 0x180000; newest == Some(1)
  #[test] fn first_flash_finds_ota_0_when_otadata_is_erased()          // [[0xFF;32];2] → 0x10000, newest None
  #[test] fn first_flash_ignores_entry_when_crc_does_not_match()       // seq 2 with crc 0 → booted 0x10000, newest Some(0)
  #[test] fn first_flash_stops_listing_partitions_when_entry_lacks_magic() // MD5 row 0xEB 0xEB then FF → 5 partitions
  #[test] fn first_flash_plans_writes_when_loader_runs_from_factory_ota_1() {
      // 658_208 bytes: p4-radio's app on 2026-11-27.
      let plan = plan(&ours, factory_ota_1, 658_208).unwrap();
      assert_eq!(plan.app, Span { start: 0x10000, end: 0xB1000 });
      assert_eq!(plan.otadata, Span { start: 0xD000, end: 0xF000 });
      assert_eq!(plan.trial, Span { start: 0x3D_0000, end: 0x3D_1000 });
      assert_eq!(plan.table, Span { start: 0x8000, end: 0x9000 });
      assert_eq!(plan.bootloader, Span { start: 0, end: 0x8000 });
  }
  #[test] fn first_flash_refuses_plan_when_running_slot_is_our_ota_0_span()
      // running = factory ota_0 (0x10000..0x190000) → Err(Overlaps { what: "radio image", .. })
  #[test] fn first_flash_refuses_plan_when_image_reaches_running_slot()
      // running factory ota_1, app_len 0x180001 → Err(Overlaps { what: "radio image", .. })
  #[test] fn first_flash_refuses_plan_when_image_exceeds_our_ota_0()
      // app_len 0x1E0001 → Err(TooLarge { len: 0x1E0001, slot: 0x1E0000 })
  #[test] fn first_flash_refuses_plan_when_our_table_lacks_trial()     // → Err(Missing("trial"))
  #[test] fn first_flash_words_refusal_when_displayed()
      // plan(ours, factory_ota_1, 0x180001) → Err displayed as
      // "the radio image at 0x10000..0x191000 would overwrite this loader at 0x190000..0x310000"
  ```
- [ ] **Step 2:** Run `cargo test -p wartui-proto first_flash`. Expected: FAIL, because `first_flash` doesn't exist.
- [ ] **Step 3:** Implement `esp_rom_crc32_le` and `first_flash.rs`. The `//!` doc says why these checks live on the host and why the loader trusts the bootloader's rule rather than `OtaUpdater` (see `firmware/common/src/ota.rs` `//!` on its erased-`otadata` reading).
- [ ] **Step 4:** Run `cargo test -p wartui-proto first_flash && cargo test -p wartui-proto crc`. Expected: PASS.
- [ ] **Step 5:** Run `cargo clippy --workspace --all-targets && cargo fmt --check`, then `firmware/common`'s three clippy lines from AGENTS.md, since `wartui-proto` reaches the firmware. Expected: clean.
- [ ] **Step 6:** Commit with the message "Plan the C6 loader's writes from the table it boots under".

---

### Task 3: `firmware/c6-loader`, and its rehearsal on `A9:76`

**Files:**
- Create: `firmware/c6-loader/Cargo.toml`.
  - It has its own `[workspace]`, and the lints copied from p4-radio.
  - Dependencies: `esp-hal`, `esp-bootloader-esp-idf`, `esp-storage` (`esp32c6`, `embedded-storage`), `embedded-storage` 0.3, `sha2` with `default-features = false`, and `wartui-proto` by path.
  - Build-dependency: `wartui-radio-image` by path.
  - Features: `rehearse = []`.
  - Release profile: p4-radio's.
- Create: `firmware/c6-loader/rust-toolchain.toml` (a copy of p4-radio's), `.cargo/config.toml` (p4-radio's `[build]` and rustflags, no runner, no `[env]`) and `Cargo.lock`.
- Create: `firmware/c6-loader/build.rs`. It is `linker_be_nice` and `-Tlinkall.x` as in p4-radio's `build.rs`, plus the bundle.
- Create: `firmware/c6-loader/src/main.rs`
- Create: `firmware/c6-loader/README.md`: what it is, why it exists (link #395 and `docs/c6-first-flash-findings.md`), the order of writes and its two risky windows, `rehearse`, and how it is built (Task 4's `build.sh`).

**Interfaces:**
- Consumes: `wartui_radio_image::build(&out, true)` (Task 1), and `wartui_proto::first_flash::{booted, newest, plan, Plan, Refusal, TABLE_AT, TABLE_LEN, SECTOR}` (Task 2).
- `build.rs` writes `OUT_DIR/bundle.rs` with:
  - `pub static APP: &[u8]` (`include_bytes!` of `app_path`)
  - `pub const APP_SHA256: [u8; 32]`
  - `pub static BOOTLOADER: &[u8; 0x8000]` (`merged[..0x8000]`)
  - `pub static TABLE: &[u8; 0x1000]` (`merged[0x8000..0x9000]`)

  It writes the slices out as files in `OUT_DIR` and `include_bytes!`es those. It panics if the merged image is shorter than 0x10000.
- Produces the line protocol over UART1, TX GPIO20, RX GPIO18, 115200 8N1, each line ending `\r\n`. Task 4 relays it:

  | Line | When |
  | --- | --- |
  | `c6-loader: start` | first |
  | `c6-loader: step <text>` | before each step (and `would <text>` steps under `rehearse`) |
  | `c6-loader: done` | all six writes done; then it resets |
  | `c6-loader: done rehearsal` | rehearsal checked and ESP-Hosted put back; then it resets |
  | `c6-loader: failed <why>` | repeated every 5 s from then on, never resets |

**The flow** in `main.rs`, numbered as #395 § "Order of writes":
- Read `TABLE_LEN` bytes at `TABLE_AT`, then both `otadata` sectors' first 32 bytes, with the `otadata` partition found in that table. Then `running = booted(..)`, else `failed no app slot boots under this table`.
- `plan(TABLE, running, APP.len())`. On `Err(refusal)`: erase the `otadata` sector `newest()` names, which puts the factory app back as the boot choice, then `failed <refusal>, and ESP-Hosted is selected again`. **Call:** a refusal undoes the switch, so it changes nothing, as #395 requires.
- Hash `APP` with `sha2`, and compare with `APP_SHA256` before any write. A mismatch is `failed the embedded image is damaged`.
- **`rehearse`:** send a `step would ...` line per write, with its span in hex (`step would write the radio image at 0x10000..0xb1000`). Then erase the newest `otadata` sector, send `done rehearsal`, wait 100 ms and call `esp_hal::system::software_reset()`.
- **Real:**
  1. Erase `plan.app` and write `APP`, padding its last word with 0xFF.
  2. Read back `APP.len()` bytes in 4 KiB chunks and hash them. They must equal `APP_SHA256`.
  3. Erase `plan.trial`.
  4. Erase and write `BOOTLOADER` over `plan.bootloader`, and read back and compare.
  5. Erase and write `TABLE` over `plan.table`, and read back and compare.
  6. Erase `plan.otadata`.

  Then send `done`, wait 100 ms and reset. A failed erase, write or compare is `failed <step> <what>`.

- [ ] **Step 1:** Create the crate and run `cargo build --release` in `firmware/c6-loader`, then again with `--features rehearse`. Expected: both build. The build log shows p4-radio built once and reused.
- [ ] **Step 2:** Run `cargo fmt --all -- --check && cargo clippy --release -- -D warnings`, with and without `--features rehearse`. Expected: clean.
- [ ] **Step 3:** Save the rehearsal image. Run `espflash save-image --skip-update-check --chip esp32c6 target/riscv32imac-unknown-none-elf/release/wartui-c6-loader-fw <scratchpad>/c6-loader-rehearse.bin` (the bin name is whatever `[[bin]]` names), and check it is under 0x180000 bytes.
- [ ] **Step 4: Bench the rehearsal; this needs the user's go-ahead for `A9:76` today.**
  - Copy the image over `<scratchpad>/p4-probe/main/c6_probe.bin`, then `idf.py build`.
  - Write it as the probe was written: the merged probe image with `espflash write-bin 0x0`.
  - Listen 120 s with `listen.pl`.

  Expected on the console:
  - `PROBE-P4 ota_end 0`.
  - Then `c6-loader: start`, and six `step would` lines with Task 2's spans (app `0x10000..0xb1000` for a 658,208-byte app).
  - Then `c6-loader: done rehearsal`.

  Then run `espflash reset --port <P4 port>` and listen again. Expected: `PROBE-P4 connect_to_slave 0`, which shows ESP-Hosted is back.
- [ ] **Step 5:** Commit `firmware/c6-loader` with the message "Add the C6 loader that lands p4-radio from ESP-Hosted's OTA". Record the bench lines for Task 8's findings doc in `<scratchpad>/bench-c6-loader.txt`.

---

### Task 4: `firmware/p4-c6-loader`, and its rehearsal on `A9:76`

C, built with ESP-IDF. Start from `<scratchpad>/p4-probe`: its CMake files, `idf_component.yml`, `sdkconfig.defaults` and `main.c` are the tested base.

**Files:**
- Create: `firmware/p4-c6-loader/CMakeLists.txt`, `main/CMakeLists.txt` (`EMBED_FILES "c6-loader.bin"`) and `main/idf_component.yml` (esp_hosted `==2.12.13`, esp_wifi_remote pinned exactly), plus `dependencies.lock`.
- Create: `firmware/p4-c6-loader/sdkconfig.defaults`. It is the probe's, but with these instead of `SINGLE_APP_LARGE`:
  - `CONFIG_PARTITION_TABLE_CUSTOM=y`
  - `CONFIG_PARTITION_TABLE_CUSTOM_FILENAME="partitions.csv"`
  - `CONFIG_LOG_DEFAULT_LEVEL_WARN=y`
  - `CONFIG_IDF_TARGET="esp32p4"`
- Create: `firmware/p4-c6-loader/partitions.csv`: `nvs 0x9000 0x6000`, `phy_init 0xf000 0x1000`, `factory app 0x10000 0x300000`.
- Create: `firmware/p4-c6-loader/main/main.c`
- Create: `firmware/p4-c6-loader/build.sh`, executable, `set -euo pipefail`.
- Create: `firmware/p4-c6-loader/.gitignore` (`build/`, `managed_components/`, `sdkconfig`, `sdkconfig.old`, `main/c6-loader.bin`).
- Create: `firmware/p4-c6-loader/README.md`. It covers what the loader is and its flow. It gives the line table below, and how to build with and without `rehearse`. It records that every reset reruns it, and why that is safe.

**Interfaces:**
- **`build.sh [c6|p4]`** (no argument runs both, in order):
  - `c6`: in `../c6-loader`, run `cargo build --release --locked`, plus `--features "$C6_LOADER_FEATURES"` when that is set. Unset `RUSTUP_TOOLCHAIN` and `CARGO_TARGET_DIR` first. Then `espflash save-image --skip-update-check --chip esp32c6 <elf> main/c6-loader.bin`. Refuse an image over 1572864 bytes (0x180000) with `"c6-loader.bin is N bytes, and the factory C6's ota_1 holds 1572864"`.
  - `p4`: `idf.py build`, then `idf.py merge-bin -o wartui-p4-c6-loader-esp32p4.bin`. If this IDF lacks `merge-bin`, use `cd build && esptool.py --chip esp32p4 merge_bin -o wartui-p4-c6-loader-esp32p4.bin @flash_args`.
  - It prints the merged image's absolute path as its last line. The output is `firmware/p4-c6-loader/build/wartui-p4-c6-loader-esp32p4.bin`, with the bootloader at 0x2000.
- **Lines to the host:** printed with `printf` on UART0 (the console, so the CH343) at 115200, each a whole line starting `wartui-loader: `. **Call:** this is the format #395 left to the builder, and Task 5 parses exactly this.

  | Line | Meaning |
  | --- | --- |
  | `wartui-loader: start` | the loader is running |
  | `wartui-loader: hosted <x.y.z>` / `wartui-loader: hosted unknown` | the C6's ESP-Hosted version, or none answered (the factory C6) |
  | `wartui-loader: sending <0–100>` | OTA progress, every 10 % |
  | `wartui-loader: sent` | `ota_end` (and `activate` on ≥ 2.6) succeeded |
  | `wartui-loader: c6 <text>` | each `c6-loader: <text>` line, relayed |
  | `wartui-loader: done` | the C6 loader sent `done` or `done rehearsal` |
  | `wartui-loader: failed <why>` | e.g. `ota_write at 123000: ESP_FAIL`, `c6 loader: <why>`, `no verdict from the C6 loader in 120 s` |
  | `wartui-loader: skipped <why>` | `no ESP-Hosted on the C6` |

**The flow** in `main.c`:
1. Wait 2 s, then send `start`.
2. **Pre-listen:** install UART1 on RX 14 / TX 19 at 115200 for 1.5 s. If a `c6-loader: ` line arrives, a C6 loader is already running (a rerun after a P4 reset). Skip to step 7.

   Otherwise delete the UART driver and `gpio_reset_pin` 14 and 19 before SDIO takes them. **Call:** this keeps a rerun from resetting a C6 loader mid-write, since `esp_hosted_init` resets the C6.
3. **The RTC guard:** a `RTC_NOINIT_ATTR` word with a magic value counts attempts. If `esp_reset_reason()` is not `ESP_RST_POWERON` or `ESP_RST_EXT` and an attempt is recorded, `esp_hosted` restarted the P4: send `skipped no ESP-Hosted on the C6 (the host restarted the P4)` and idle. Record the attempt, then `nvs_flash_init` and `esp_hosted_init`.
4. If `esp_hosted_connect_to_slave` fails, send `skipped no ESP-Hosted on the C6` and idle.
5. Send `hosted ...`, then `ota_begin`, then `ota_write` in 1500-byte chunks with `sending` lines, then `ota_end`. Call `activate` only when the version is ≥ 2.6. Then send `sent`. Any error is `failed <call> <esp_err_to_name>`, and the loader idles.
6. Run `esp_hosted_deinit`, then reset the C6 through GPIO54: low for 100 ms, then high.
7. **Relay:** read UART1 lines for up to 120 s, sending each `c6-loader: <text>` as `c6 <text>`.
   - On `done...`, send `done`.
   - On `failed <why>`, send `failed c6 loader: <why>`.
   - With no verdict in time, send `failed no verdict from the C6 loader in 120 s`.

   Then idle forever, and never restart.

- [ ] **Step 1:** Build with the rehearsal inside: `C6_LOADER_FEATURES=rehearse firmware/p4-c6-loader/build.sh`, with the scratchpad's IDF exported. Expected: the last line is the merged image's path. `xxd -s 0x2000 -l 1 <merged>` shows `e9`, the bootloader's magic, where a P4 boots from (`flash/mod.rs` `boot_offset`).
- [ ] **Step 2: Bench; this needs the user's go-ahead for `A9:76`.** The board's P4 runs the probe, and its C6 runs ESP-Hosted.
  - Run `espflash write-bin --port <P4 port> --chip esp32p4 --non-interactive --skip-update-check 0x0 <merged>`.
  - Then `timeout 240 perl listen.pl <port> 230 > <scratchpad>/bench-p4-loader-1.txt`, started right after the write.

  Expected, in order: `start`, `hosted unknown`, `sending 0`…`sending 100`, `sent`, `c6 start`, six `c6 step would …` lines, `c6 done rehearsal`, `done`. Note the seconds from `start` to `done`, which set Task 6's deadlines. Also note whether the C6 rebooted on its own between `sent` and the P4's reset, which is #395's second open question.
- [ ] **Step 3:** Run `espflash reset --port <P4 port>` and listen again into `bench-p4-loader-2.txt`. Expected: the same sequence, which shows the rehearsal put ESP-Hosted back and a rerun is harmless.
- [ ] **Step 4:** Commit `firmware/p4-c6-loader` with the message "Add the P4 loader that pushes the C6 loader over ESP-Hosted".

---

### Task 5: `setup.rs`: choosing the board, consent, and reading the loader

The pure parts of the host side, all tested here. Task 6 wires them into `flash-bridge`.

**Files:**
- Create: `crates/wartui/src/flash/setup.rs`. Its `//!` says why a silent CH343 is offered only after `board-info` names an ESP32-P4. It also says why a remembered bridge that is silent is never offered, and why the bridge is known by its C6's address afterwards.
- Modify: `crates/wartui/src/flash/mod.rs` (`mod setup;`)

**Interfaces:**
- Consumes these from `super`: `Ran`, `BoardInfo`, `parse_board_info`, `Skip`, `matches`, `name`, `probe_args` and `espflash`. From `wartui_bridge::serial` it takes `Probed`, and `p4::FIRST_FLASH`.
- Produces (all `pub(super)`):
  ```rust
  /// The silent CH343s flash-bridge may offer to set up, given what `target` refused:
  /// the one a `--bridge` path names; else, with nothing named or remembered and no
  /// bridge answering, every silent one; else none.
  fn blank_candidates(probed: &[Probed], named: Option<&BridgeSpec>, remembered: Option<Mac>) -> Vec<PortCandidate>;
  /// `board-info`'s answer, judged as a P4: Ok when it ran, read, and names `esp32p4`.
  fn judge_p4(probe: &Ran) -> Result<BoardInfo, Skip>;
  /// The one P4 to set up out of the judged candidates, or the refusal to print.
  fn choose(judged: Vec<(PortCandidate, Result<BoardInfo, Skip>)>) -> Result<(PortCandidate, BoardInfo)>;
  #[derive(Debug, PartialEq, Eq)] enum Consent { Given, Ask, Refused }
  fn consent(first_flash: bool, terminal: bool) -> Consent;
  fn prompt_text(board: &PortCandidate, p4: &BoardInfo) -> String;
  #[derive(Debug, Clone, PartialEq, Eq)]
  enum Said { Start, Hosted(Option<String>), Sending(u8), Sent, C6(String), Done, Failed(String), Skipped(String) }
  /// One whole line, trailing `\r` dropped, read as a loader line; None for anything else.
  fn said(line: &str) -> Option<Said>;
  /// Bytes from the CH343, joined into lines across reads; lines capped at 1024 bytes as
  /// `boot.rs` caps them.
  struct LoaderWatch { /* partial line */ }
  impl LoaderWatch { fn new() -> Self; fn feed(&mut self, bytes: &[u8]) -> Vec<Said>; }
  ```
- The `--bridge` path match uses `super::matches`. A `Mac`/`Tail` spec never names a blank P4, because the probe heard no address from it.

- [ ] **Step 1: Write the failing tests.** Use the fixtures in `super::testing` (`BRIDGE_MAC`, `NODE_MAC`, `BOARD_INFO`), and `bridge.rs`'s `ch343`/`bridge`/`silent` helpers, moved into `testing` so both files share them. A P4 transcript is `BOARD_INFO` with the chip line replaced by `Chip type:         esp32p4 (revision v3.2)` and the address by `02:00:5e:10:44:c1`.
  ```rust
  #[test] fn setup_offers_silent_ch343s_when_nothing_is_remembered_or_named()   // [silent A, silent B] → [A, B]
  #[test] fn setup_offers_named_path_when_its_ch343_is_silent()                // Path("/dev/ttyACM0") → [that one]
  #[test] fn setup_offers_nothing_when_remembered_bridge_is_silent()           // remembered Some → []
  #[test] fn setup_offers_nothing_when_named_by_address()                     // Tail("44:C1") → []
  #[test] fn setup_offers_nothing_when_a_bridge_answered()                    // [bridge, silent], none remembered → []
  #[test] fn setup_reads_board_as_p4_when_board_info_names_esp32p4()          // judge_p4 → Ok(chip "esp32p4")
  #[test] fn setup_refuses_board_when_board_info_names_another_chip()         // BOARD_INFO (c5) → Err(OtherChip("esp32c5"))
  #[test] fn setup_chooses_only_p4_when_one_silent_ch343_is_a_p4()            // [(A, Ok p4), (B, Err ProbeFailed)] → A
  #[test] fn setup_refuses_when_several_silent_ch343s_are_p4s()
      // error contains both paths and "--bridge"
  #[test] fn setup_refuses_when_no_silent_ch343_is_a_p4() {
      // A port another program holds reads as a failed board-info, and says so.
      let error = choose(vec![(a, Err(Skip::ProbeFailed("port is busy".into())))]).unwrap_err().to_string();
      assert!(error.contains("no P4 bridge answered"), "{error}");
      assert!(error.contains("/dev/ttyACM0") && error.contains("port is busy"), "{error}");
      assert!(error.contains(p4::FIRST_FLASH), "{error}");
  }
  #[test] fn setup_asks_when_on_a_terminal_without_first_flash()   // consent(false, true) == Ask
  #[test] fn setup_takes_consent_when_first_flash_is_given()       // consent(true, false) == Given, consent(true, true) == Given
  #[test] fn setup_refuses_when_no_terminal_and_no_first_flash()   // consent(false, false) == Refused
  #[test] fn setup_words_prompt_when_offering_a_board() {
      let text = prompt_text(&board, &p4);
      for part in ["/dev/ttyACM0", "44:C1", "both chips", "about two minutes",
                   "settings and Bluetooth bonds", "its C6's address", "[y/N]"] {
          assert!(text.contains(part), "{part}: {text}");
      }
  }
  #[test] fn setup_reads_each_line_when_loader_reports_it() {
      assert_eq!(said("wartui-loader: start"), Some(Said::Start));
      assert_eq!(said("wartui-loader: hosted unknown"), Some(Said::Hosted(None)));
      assert_eq!(said("wartui-loader: hosted 2.12.13\r"), Some(Said::Hosted(Some("2.12.13".into()))));
      assert_eq!(said("wartui-loader: sending 40"), Some(Said::Sending(40)));
      assert_eq!(said("wartui-loader: sent"), Some(Said::Sent));
      assert_eq!(said("wartui-loader: c6 step writing the radio image"), Some(Said::C6("step writing the radio image".into())));
      assert_eq!(said("wartui-loader: done"), Some(Said::Done));
      assert_eq!(said("wartui-loader: failed ota_begin ESP_FAIL"), Some(Said::Failed("ota_begin ESP_FAIL".into())));
      assert_eq!(said("wartui-loader: skipped no ESP-Hosted on the C6"), Some(Said::Skipped("no ESP-Hosted on the C6".into())));
  }
  #[test] fn setup_ignores_lines_when_they_are_idf_log_or_noise() {
      for line in ["\x1b[0;33mW (1234) H_SDIO_DRV: slave not ready\x1b[0m", "I (12) boot: ESP-IDF v5.5.4",
                   "ESP-ROM:esp32p4-eco7-20260109", "wartui-loader: sending 140", "wartui-loader: bogus", "  wartui-loader: done"] {
          assert_eq!(said(line), None, "{line:?}");
      }
  }
  #[test] fn setup_reads_loader_lines_when_split_across_reads()     // feed "wartui-loader: do" then "ne\r\nI (5) x\r\n" → [Done]
  #[test] fn setup_caps_line_when_no_newline_arrives()              // 2000 bytes of 'x' then "\nwartui-loader: done\n" → [Done]
  ```
- [ ] **Step 2:** Run `cargo test -p wartui setup_`. Expected: FAIL to compile, since `setup` doesn't exist yet.
- [ ] **Step 3:** Implement the items above in `setup.rs`. The refusal for several P4s reads: `"{n} silent CH343s are ESP32-P4s: {paths}. Name the one to set up with --bridge PATH"`. The refusal for none lists each path with its `Skip` text, then `"See {FIRST_FLASH}"`.
- [ ] **Step 4:** Run `cargo test -p wartui setup_ && cargo test -p wartui flash_bridge_`. Expected: PASS.
- [ ] **Step 5:** Commit with the message "Choose a blank P4 and read the loader's lines for flash-bridge".

---

### Task 6: Wire the setup into `flash-bridge`, and rehearse it on `A9:76`

**Files:**
- Modify: `crates/wartui/src/flash/bridge.rs`
  - `Args` gains `first_flash` and `loader`.
  - `run`'s refusal branch.
  - `remember` becomes `pub(super)`.
  - The `(None, [])` message in `target`.
  - The `//!` paragraph "No P4 bridge answering points at the first flash", rewritten for the setup.
  - The `args()` test helper.
- Modify: `crates/wartui/src/flash/p4.rs`
  - Add `P4_BRIDGE`.
  - `open_usb` becomes `pub(super)`.
  - Update `P4_BRIDGE_APP`'s doc.
  - Add the new fields to the test's `Args` literal.
- Modify: `crates/wartui/src/flash/setup.rs` (`run`, `P4_C6_LOADER`, `build_loader`, the watch loop, the come-up wait)
- Modify: `crates/wartui/src/main.rs`: a CLI parse test.

**Interfaces:**
- Consumes: Task 5's items, plus `super::{source, fetch, check_image, obtain, espflash, espflash_present, flash_args, probe_args, read, RELEASE_TAG, Source, Firmware}`, `p4::{open_usb, FIRST_FLASH, P4_BRIDGE}`, `bridge::remember` and `wartui_desktop::serial::open_reader`.
- **`Args` gains two flags:**
  ```rust
  /// Set up a P4 that does not answer without asking: the answer to flash-bridge's
  /// y/N. A P4 that answers is updated as always, flag or not.
  #[arg(long)]
  pub first_flash: bool,
  /// The P4 C6 loader's merged image to set up a new board with, rather than building or
  /// downloading one.
  #[arg(long, value_name = "BIN")]
  pub loader: Option<PathBuf>,
  ```
- In `p4.rs`: `pub(super) const P4_BRIDGE: Firmware = Firmware { merged: true, asset: "wartui-p4-bridge-fw-esp32p4.bin", ..P4_BRIDGE_APP };`
- In `setup.rs`: `const P4_C6_LOADER: Firmware`, with:
  - `dir`: `firmware/p4-c6-loader`
  - `elf`: `"build/wartui-p4-c6-loader-esp32p4.bin"`, the output `build.sh` leaves
  - `what`: `"P4 C6 loader"`
  - `chip`: `Esp32p4`
  - `partition_table`: `"partitions.csv"`
  - `flash_size`: `Some("16mb")`
  - `merged`: `true`
  - `asset`: `"wartui-p4-c6-loader-esp32p4.bin"`
- `pub(super) async fn run(args: &Args, named: bool, silent: Vec<PortCandidate>) -> Result<()>`
- `fn build_loader(dir: &Path) -> Result<PathBuf>`:
  - It runs `idf.py --version` first. If that is not found: `"idf.py is not on PATH, so the P4 C6 loader cannot be built; export ESP-IDF v5.5.4 or pass --loader BIN"`.
  - It then runs `dir/build.sh` with `RUSTUP_TOOLCHAIN` and `CARGO_TARGET_DIR` removed, and returns `dir.join(P4_C6_LOADER.elf)`.

**Changes to `bridge::run`:**
```rust
let (board, expected) = match target(&found, &probed, named..., remembered.mac) {
    Ok(chosen) => chosen,
    Err(refusal) => {
        let silent = setup::blank_candidates(&probed, named_spec, remembered.mac);
        if silent.is_empty() { return Err(refusal); }
        return setup::run(&args, named.is_some(), silent).await;
    }
};
```
The `(None, [])` text in `target` becomes: `"no P4 bridge answered, and no silent CH343 is attached for flash-bridge to set up. See {FIRST_FLASH}"`. Adjust the two tests that assert the old wording: keep their `"no P4 bridge answered"` and `FIRST_FLASH` checks, and drop `"a CH343 that does not answer"`.

**`setup::run`, in order:**
1. `espflash_present()`.
2. Run `board-info` on each silent CH343, then `judge_p4`, then `choose`.
3. Print `found  {path}: an ESP32-P4 ({short P4 mac}) whose bridge did not answer`. Print `loader {source.describe(dry)}`, where the source is `source(&P4_C6_LOADER, args.loader, None, RELEASE_TAG)`. Print `image  {..describe}` for `source(&P4_BRIDGE, None, args.firmware_dir, RELEASE_TAG)`.
4. With `--dry-run`, print `{path} would be set up: the loader, then the bridge firmware` and return Ok. A `--loader` file is still checked with `check_image(.., Esp32p4, true)`.
5. Ask for consent:
   - `Ask`: print `prompt_text` and read one stdin line. Only `y` or `yes`, in any case, goes ahead. Anything else prints `{path} not set up` and returns Ok.
   - `Refused`: bail with `"standard input is not a terminal, so nothing can answer; pass --first-flash to set up {path}"`.
6. Obtain both images before touching the board. The loader uses `build_loader` for `Source::Build`, else `fetch`, then `check_image(merged)`. The bridge uses `obtain(&P4_BRIDGE, ..)`.
7. Run `espflash(&flash_args(path, Chip::Esp32p4, loader))`. On failure, bail: `"writing the loader to {path} failed: {last_line}"`.
8. **Watch:** on a blocking task, `open_reader(path, 115_200, 100 ms)` feeds `LoaderWatch`. Print one line per `Said`: `sending` at each tenth, and `c6` lines as `C6     {text}`.
   - **Deadlines:** 60 s with no loader line, and 300 s in all. **Call:** Task 4's bench timing confirms or changes these.
   - `Done` and `Skipped(why)` go on. A skip prints `{path}  the C6 has no ESP-Hosted ({why}); writing the bridge firmware`.
   - A `Failed(why)`, a deadline, or the port closing bails with `loader_failed_text(path, &why)`.
9. Drop the reader. Write the bridge image the same way as step 7.
10. **Come-up:** until 90 s pass, call `p4::open_usb(label, path)`.
    - A `core.mac` other than the P4's `board-info` address is the C6's: success.
    - The P4's own address, or an open error, waits 2 s and tries again.
    - At the deadline, bail with `c6_silent_text(path)`. Nothing is remembered.
11. Print `{short c6}  set up; the bridge is known by its C6's address`. Then `bridge::remember(args, named, &board, c6_mac)`, which saves nothing for a `--bridge` path, as today.

**Failure texts (exact):**
- `loader_failed_text(path, why)`: `"setting up {path} stopped: {why}. The P4 runs the loader, and running flash-bridge again retries. The C6's header is the fallback, per {FIRST_FLASH}"`
- `c6_silent_text(path)`: `"{path} runs the bridge firmware, but its C6 is not answering, so the bridge is not remembered. The C6's header is the fallback, per {FIRST_FLASH}"`

- [ ] **Step 1: Write the failing tests.**
  ```rust
  // setup.rs
  #[test] fn setup_says_rerun_and_header_when_loader_fails() {
      let text = loader_failed_text("/dev/ttyACM0", "no verdict from the C6 loader in 120 s");
      assert!(text.contains("running flash-bridge again retries") && text.contains(p4::FIRST_FLASH), "{text}");
  }
  #[test] fn setup_says_nothing_remembered_when_c6_stays_silent()   // contains "not remembered" and FIRST_FLASH
  #[test] fn setup_fetches_release_assets_when_built_for_a_release() {
      assert_eq!(source(&P4_C6_LOADER, None, None, Some("v1")),
                 Source::Release { tag: "v1".into(), asset: "wartui-p4-c6-loader-esp32p4.bin" });
      assert_eq!(source(&p4::P4_BRIDGE, None, None, Some("v1")),
                 Source::Release { tag: "v1".into(), asset: "wartui-p4-bridge-fw-esp32p4.bin" });
      assert_eq!(source(&P4_C6_LOADER, Some(Path::new("l.bin")), None, Some("v1")), Source::File("l.bin".into()));
  }
  #[test] fn setup_refuses_loader_file_when_it_is_app_only()        // check_image(p4_app_image(), Esp32p4, true) errs
  // main.rs
  #[test] fn cli_parser_accepts_flash_bridge_when_given_first_flash_and_loader()
      // ["wartui","flash-bridge","--bridge","/dev/ttyACM0","--first-flash","--loader","l.bin"]
  ```
- [ ] **Step 2:** Run `cargo test -p wartui setup_ cli_parser_accepts_flash_bridge`. Expected: FAIL.
- [ ] **Step 3:** Implement the changes above.
- [ ] **Step 4:** Run `cargo test --workspace && cargo clippy --workspace --all-targets && cargo fmt --check`. Expected: PASS and clean.
- [ ] **Step 5: Dry run on `A9:76`.** Its P4 runs Task 4's loader, so it is silent. Run `cargo run --release -p wartui -- flash-bridge --config <scratchpad>/bench.toml --dry-run --loader <rehearse merged>`. Expected:
  - the `found` line with `A9:76`'s P4 suffix and its path;
  - the `loader` and `image` source lines;
  - `would be set up`;
  - no writes (`espflash` runs `board-info` only).
- [ ] **Step 6: Rehearse on `A9:76`; this needs the user's go-ahead.** Run `timeout 600 cargo run --release -p wartui -- flash-bridge --config <scratchpad>/bench.toml --first-flash --loader <rehearse merged> > <scratchpad>/bench-setup-rehearse.txt 2>&1`. Expected:
  - The loader lines through `c6 done rehearsal` and `done`, then the bridge write.
  - The C6 is back on ESP-Hosted, so after 90 s the run fails with `c6_silent_text`.
  - `<scratchpad>/bench.toml` holds no `[bridge] mac`.

  That is the silent-C6 failure, benched for real. Afterwards the P4 runs the bridge, so a rerun takes the ordinary update path. Write the loader by hand to rehearse again.
- [ ] **Step 7:** Commit with the message "Offer to set up a silent P4-WIFI6 from flash-bridge".

---

### Task 7: CI and release

**Files:**
- Create: `.github/workflows/firmware-c6-loader.yml`. It is shaped like `firmware-p4-radio.yml`.
  - **Paths:** `firmware/c6-loader/**`, `firmware/radio-image/**` and `firmware/p4-radio/**`, the last without its README. Also `firmware/common/**`, `crates/wartui-proto/**`, the workflow itself and `rustfmt.toml`.
  - **Steps:**
    1. The toolchain with `riscv32imac-unknown-none-elf`, then `cargo install espflash --locked`, which `build.rs` needs.
    2. `cargo fmt --all -- --check`.
    3. `cargo build --release --locked`.
    4. `cargo clippy --release --locked -- -D warnings`, both with and without `--features rehearse`.
    5. In `firmware/radio-image`: `cargo fmt --check` and `cargo clippy --locked -- -D warnings`.
- Create: `.github/workflows/firmware-p4-c6-loader.yml`.
  - **Paths:** the same as above, plus `firmware/p4-c6-loader/**` without its README.
  - **Steps:**
    1. The Rust toolchain and espflash.
    2. `firmware/p4-c6-loader/build.sh c6`.
    3. `uses: espressif/esp-idf-ci-action@v1` with `esp_idf_version: v5.5.4`, `target: esp32p4`, `path: firmware/p4-c6-loader` and `command: ./build.sh p4`.
- Modify: `.github/workflows/release.yml`.
  - Add a `p4-c6-loader` job with the same steps as above. It uploads `firmware/p4-c6-loader/build/wartui-p4-c6-loader-esp32p4.bin` with `archive: false`.
  - Add it to `release.needs`.
  - Rewrite the header comment's line saying the P4 bridge's merged image "only a first flash by hand uses". Both merged images now serve `flash-bridge`'s setup.

- [ ] **Step 1:** Write the failing test in `setup.rs`:
  ```rust
  #[test] fn setup_names_assets_release_yml_publishes() {
      let workflow = include_str!("../../../../.github/workflows/release.yml");
      for asset in [P4_C6_LOADER.asset, p4::P4_BRIDGE.asset] { assert!(workflow.contains(asset), "{asset}"); }
  }
  ```
- [ ] **Step 2:** Run `cargo test -p wartui setup_names_assets`. Expected: FAIL, because the loader asset is not in `release.yml`.
- [ ] **Step 3:** Write the two workflows and the release job.
- [ ] **Step 4:** Run `cargo test -p wartui setup_names_assets`, and `actionlint` on the three files if it is installed. Expected: PASS and clean.
- [ ] **Step 5:** Commit with the message "Build both loaders in CI and publish the P4 loader with each release". Push the branch and open a draft PR.

  Check that both new workflows pass on it: `gh pr checks --watch`. If the ESP-IDF action's container lacks something `build.sh p4` needs, fix it there.

---

### Task 8: Docs

**Files:**
- Modify: `firmware/p4-radio/README.md` § "First flash".
  - Lead with `wartui flash-bridge`, which sets up a new board over the Type-C cable. Link `firmware/c6-loader/README.md`.
  - Keep the H4 steps as the fallback: for a C6 the loader refuses, a failed setup, or a board without ESP-Hosted.
  - Fix the stale "about 570 KB" image size wherever it appears (`grep -rn "570" firmware docs crates/*/README.md`). The image is now about 643 KiB.
- Modify: `firmware/p4-bridge/README.md` § "First flash". `flash-bridge` sets up a new board and writes this image whole, which clears `nvs`. The `cargo run` runner stays for a board that already ran wartui, and leaves `nvs` as it was.
- Modify: `crates/wartui/README.md` § "Flashing the bridge". Cover:
  - the setup prompt, `--first-flash` and `--loader`;
  - that the board is known by its C6's address afterwards;
  - what a failed setup leaves, and that a rerun retries.
- Create: `docs/c6-first-flash-findings.md`, a dated bench record. It holds:
  - #395 § "What the bench showed";
  - the rehearsal lines and timings from Tasks 3, 4 and 6, from the scratchpad bench files;
  - a placeholder heading for Task 9's run.

  Board suffixes only.

- [ ] **Step 1:** Write the four documents, following AGENTS.md's conventions: present tense outside the findings doc, short sentences, and links rather than repetition.
- [ ] **Step 2:** Run `cargo test -p wartui --test readme`, then `grep -rnE "([0-9A-F]{2}:){5}[0-9A-F]{2}" docs/c6-first-flash-findings.md firmware/*/README.md`. Expected: PASS, and only `02:`-prefixed or placeholder addresses.
- [ ] **Step 3:** Commit with the message "Document the C6's first flash over SDIO".

---

### Task 9: End-to-end on `A9:76`, the one-shot

**Only with the user's go-ahead.** This overwrites `A9:76`'s ESP-Hosted for good.

- [ ] **Step 1:** Make the P4 silent by blanking it: `espflash erase-region --chip esp32p4 --port <P4> --non-interactive 0x0 0x10000`. That erases the bootloader and table, so the P4 sits in its ROM and stays silent. Then `flash-bridge` writes the loader and watches it. Never write the plain loader by hand here: espflash's reset runs it at once against the factory C6, with nobody listening.
- [ ] **Step 2:** Run `cargo run --release -p wartui -- flash-bridge --config <scratchpad>/bench.toml`, driven from a `pty.fork()` script that answers `y`. Expected:
  - `c6 step` lines for all six writes, then `done`.
  - The bridge write, then `set up; the bridge is known by its C6's address`.
  - `bench.toml` holds the C6's MAC.
- [ ] **Step 3:** Run `wartui ports --config <scratchpad>/bench.toml` and `wartui status --config <scratchpad>/bench.toml`. Expected: the bridge with its C6 answering, and no "updating the radio", which shows the loader's app digest equals the P4's bundle.
- [ ] **Step 4:** Run `flash-bridge` again. Expected: the ordinary update over the link, ending `already runs this image`.
- [ ] **Step 5: The `skipped` path.**
  - Write the real P4 loader by hand: `espflash write-bin 0x0`. This is safe here, unlike Step 1: the C6 runs p4-radio by now, so the loader finds no ESP-Hosted, hears no C6 loader and writes nothing to the C6.
  - Run `flash-bridge --config <scratchpad>/bench.toml --bridge <path> --first-flash`.

  Expected: `skipped no ESP-Hosted on the C6` after the loader's 25 s listen, then the bridge comes up with the C6 answering. Record how `esp_hosted_connect_to_slave` behaved against a non-ESP-Hosted C6: did it fail fast, wait, or restart the P4? That answers #395's first open question.
- [ ] **Step 6:** Fill in Task 9's section of `docs/c6-first-flash-findings.md`. Commit with the message "Record the C6 first flash's end-to-end bench", then push.
- [ ] **Step 7:** Comment the outcome on #395 (`gh issue comment 395`). Then mark the PR ready.
