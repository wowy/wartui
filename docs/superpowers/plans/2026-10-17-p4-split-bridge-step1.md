# P4 Split Bridge, Step 1 (Radio and Link) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Waveshare ESP32-P4 board whose C6 runs the radio becomes a wartui bridge the host
uses exactly like a single-chip one, with every feature except the panel and GPS.

**Architecture:**
- The ESP-NOW handling moves out of `firmware/bridge` into `wartui-firmware`'s new `radio`
  module, which both the single-chip bridge and a new C6 firmware (`firmware/p4-radio`) use.
- The C6 talks to a new P4 firmware (`firmware/p4-bridge`) over a 40 MHz SPI link. The link's
  framing and its frozen hello live in `wartui-proto::spi`.
- The P4 speaks the existing link protocol to the host over USB high speed (CDC-ACM).
- Every piece of logic that needs no chip lives in `wartui-proto`, where `cargo test` reaches
  it:
  - framing
  - queues
  - the P4's pair state machine
  - the USB packetizer
  - compile-time size parsing

**Tech Stack:** Rust 2024, MSRV 1.95, esp-hal 1.2.2, esp-radio 1.0.0-beta.1, esp-rtos 0.4.0,
embassy-executor 0.10, embassy-sync 0.8, embassy-usb 0.6, postcard, heapless.

**Spec:** `docs/superpowers/specs/2026-10-17-p4-split-bridge-design.md`. Read it first, then
`docs/p4-spi-link-findings.md` (PR #214). The spike code is on the local branch
`spike/p4-spi-link` under `firmware/spike/`. Read it with `git show spike/p4-spi-link:<path>`.

## Global Constraints

- `AGENTS.md` governs. The main session plans, and the `rust-developer` agent writes code.
  Commit only as each task's last step says. Never push. Never upload to WDGWars.
- Prefer async or `select`-style waits over polling wherever a driver offers one. Poll only
  where no async form exists, with a comment saying why.
- `LINK_PROTO_VERSION` stays at 1. Every new link field or variant goes on the **end** of its
  enum or struct.
- The SPI link's frozen core never changes after Task 3 lands, even before 1.0:
  - `TRANSFER_LEN`
  - the header layout
  - the `Hello` layout
  - the kind numbers
  - the READY handshake
- Every queue that can drop has a compile-time size, set in the firmware's `src/config.rs`
  through `wartui_proto::config::env_usize(option_env!("<NAME>"), <default>)`, and reports
  capacity, high-water mark and drops in `Status`.
- One firmware image serves both the ESP32-P4-WIFI6-POE-ETH and the ESP32-P4-WIFI6. There is
  no board feature.
- Pins:
  - P4 SPI2: SCLK GPIO18, MOSI GPIO19, MISO GPIO14, CS GPIO15.
  - READY: P4 GPIO6, with an input pull-down.
  - C6 enable: P4 GPIO54.
  - C6 SPI2 slave: SCLK GPIO19, MOSI GPIO18, MISO GPIO20, CS GPIO21.
  - C6 READY: GPIO2.
  - SDIO D2/D3 (P4 GPIO16/17, C6 GPIO22/23) stay inputs.
  - P4 console UART0: TX GPIO37.
- The USB ID until the Espressif PID is granted is `0x1209:0x0001`, with product string
  `wartui p4 bridge` and manufacturer `wartui`. The host matches the exact VID:PID pair **and** a
  product string starting `wartui`.
- `unsafe_code = "deny"` in every new crate, and `clippy::all` at deny. Each new firmware is its
  own workspace with `rust-toolchain.toml` matching `firmware/bridge`'s. The P4 target is
  `riscv32imafc-unknown-none-elf`.
- Match the surrounding code's comment density and idiom. Every module gets a `//!` saying why
  it exists, as `firmware/bridge/src/main.rs` does.
- Bench-board references use the last two MAC octets:
  - P4: `8B:65`
  - its C6: `D8:1C`
  - nodes: `F5:50`, `9A:24`, `0A:CC`
  - single-chip bridges: T-Dongle `44:C0` and C6 `75:40`

## Review Focus

1. **A C6 that restarts under a running P4 must read to the host as a new bridge life.** The
   P4's next `Ready` carries an `uptime_ms` lower than the last one (`is_a_new_life` in
   `crates/wartui-bridge/src/serial.rs`), so the host re-adds peers. Task 5 pins it with
   `pair_announces_new_life_when_hello_arrives_after_link_up`.
2. **A host that holds the port open but stops reading must not stall anything.** The P4's
   queues fill and count drops. A `SendResult` still reaches the host ahead of queued `Rx`
   frames once it reads again, and the SPI link keeps running. Task 2 pins it with
   `host_queue_sends_priority_frame_before_queued_bulk` and
   `frame_ring_evicts_oldest_whole_frame_when_full`.
3. **A P4 and C6 built from different trees must fail loudly, not silently.** The P4
   refuses to bridge, answers every `SendEspNow` with `SendStatus::Rejected`, and reports an
   `Error` naming both revisions. Task 5 pins it with
   `pair_rejects_sends_and_reports_when_link_revision_differs`. Task 3's golden test catches a
   revision nobody bumped.
4. **A C6 that is absent or unflashed must still leave a P4 the host can find.** Within
   `RADIO_WAIT_MS` the P4 enumerates with its own MAC, answers `Identify` with `radio: None`,
   and reports an `Error`. Task 5 pins it with
   `pair_announces_own_mac_and_error_when_radio_never_answers`.
5. **A USB transfer ending exactly on a 512-byte packet boundary must not be held by the
   host's `cdc_acm`.** The trailing lone `0x00` rule, as in `wartui-proto`'s outbox. Task 2
   pins it with `host_queue_appends_terminator_packet_when_frame_ends_on_packet_boundary`.

---

### Task 1: Baseline the single-chip bridge under burst load (main session, with the user)

This runs **before** Task 6 changes the single-chip bridge's send path. The spec requires a
before-and-after measurement.

**Files:**
- Create: `docs/p4-bridge-findings.md` (the baseline section only)

- [ ] **Step 1:** Ask the user to attach a single-chip bridge.
  - Prefer the C6 `75:40`. Flash it from `main` with `cd firmware/bridge && cargo run --release
    --features esp32c6`.
  - Ask for the burst nodes `F5:50` and `9A:24` too, plus `0A:CC` if it is attached.
- [ ] **Step 2:** Flash each node with the burst-test build on branch `bench/burst-load-1.2`
  (commits ca5a6de, f67ec33):
  - Make a worktree: `git worktree add ../wartui-burst bench/burst-load-1.2`.
  - Flash from it as `docs/p4-spi-link-findings.md` § "bench notes" says, with C5:
    `--features esp32c5`, XIAO C6: `--features esp32c6,xiao-external-antenna`.
- [ ] **Step 3:** Run `wartui run --capture <scratchpad>/baseline.db` for 10 minutes in a pty,
  as memory `wartui-run-needs-a-pty` describes, with flags after `run`.
  - Then `wartui analyze <scratchpad>/baseline.db`.
  - Then `wartui status` once the run has stopped.
- [ ] **Step 4:** Record in `docs/p4-bridge-findings.md`:
  - batches lost per node
  - `dropped_tx`
  - `rx_count`
  - the median and maximum assignment latency the capture records. That is the `latency_us`
    stored per admin send (`crates/wartui-core/src/engine/admin.rs:187`, heartbeat `rx_us` to
    `SendResult` `tx_us`); query it from the capture database.
  - run length
  - bridge firmware commit
- [ ] **Step 5:** Commit with `git add docs/p4-bridge-findings.md && git commit -m "Baseline the
  single-chip bridge under burst load"`.

---

### Task 2: Compile-time sizes, the frame ring and the host queue (`wartui-proto`)

**Files:**
- Create: `crates/wartui-proto/src/config.rs`
- Create: `crates/wartui-proto/src/frame_ring.rs`
- Create: `crates/wartui-proto/src/host_queue.rs`
- Modify: `crates/wartui-proto/src/lib.rs` (declare the three modules, crate docs line each)
- Test: `crates/wartui-proto/tests/frame_ring.rs`, `crates/wartui-proto/tests/host_queue.rs`, unit
  tests in `config.rs`

**Interfaces:**
- Produces:
  - `pub const fn env_usize(value: Option<&str>, default: usize) -> usize`.
    - `None` returns `default`.
    - Decimal digits, optionally with `_` separators, parse.
    - Anything else panics in const evaluation with the message
      `"a WARTUI_* size must be a decimal number"`, which makes it a build error.
  - `pub struct FrameRing<const N: usize>`, a byte ring of whole frames. Each frame is stored
    with a `u16` length prefix, and `N` is total bytes including the prefixes.
    - `pub const fn new() -> Self`
    - `pub fn push(&mut self, frame: &[u8]) -> bool`
      - Evicts the oldest whole frames until `frame` fits.
      - Never evicts the frame a reader is part-way through.
      - Returns `false` and counts a drop only when `frame` cannot fit even in an empty ring,
        or the in-progress frame blocks it.
      - Each eviction counts one drop.
    - `pub fn read(&mut self, out: &mut [u8]) -> (usize, bool)` copies the next bytes of the
      current frame, starting the next frame when none is in progress. It returns
      `(bytes, frame_finished)`.
    - `pub fn mid_frame(&self) -> bool`
    - `pub fn is_empty(&self) -> bool`
    - `pub fn len_bytes(&self) -> usize`
    - `pub fn high_water(&self) -> usize` (bytes)
    - `pub fn dropped(&self) -> u32`
    - `pub const CAPACITY: usize = N`
  - `pub struct HostQueue<const P: usize, const B: usize>` holds a priority and a bulk
    `FrameRing`. Frames pushed are complete COBS link frames, terminator included, as
    `link::encode_frame` writes them.
    - `pub fn push(&mut self, frame: &[u8], priority: bool) -> bool`
    - `pub fn next_packet(&mut self, out: &mut [u8; HS_PACKET]) -> usize` fills up to 512 bytes:
      - It continues the frame in progress from whichever ring it came from.
      - At each frame boundary it takes priority first.
      - It returns 0 when both rings are empty and nothing is owed.
      - When the previous packet was exactly 512 bytes and nothing followed, it returns a
        1-byte packet `[0x00]` once.
    - `pub fn stats(&self) -> [QueueStat; 2]` reports `HostPriority` then `HostBulk` (Task 4
      defines `QueueStat`). Until Task 4 lands, return the raw triples, and Task 4 switches
      them.
    - `pub const HS_PACKET: usize = 512`.
  - `pub fn is_priority(msg: &BridgeToHost) -> bool` lives in `host_queue.rs`. It is the same
    split as `outbox`: Ready, SendResult, Status and Error are priority. Reuse or move
    `outbox`'s private classifier rather than writing a second one.

- [ ] **Step 1: Write the failing tests.**
  - `config_env_usize_returns_default_when_unset`: `env_usize(None, 4096) == 4096`.
  - `config_env_usize_parses_digits_and_underscores`: `env_usize(Some("262_144"), 1) == 262_144`.
  - `frame_ring_round_trips_frames_in_order`: push `b"abc"`, `b"de"`. Reading with a 2-byte
    `out` yields `(2,false)` "ab", `(1,true)` "c", `(2,true)` "de".
  - `frame_ring_evicts_oldest_whole_frame_when_full`: with `FrameRing<16>`, push three 5-byte
    frames, where each takes 7 bytes with its prefix. Then:
    - the second push evicts nothing
    - the third evicts the first
    - `dropped() == 1`
    - reads return frames 2 then 3
  - `frame_ring_keeps_in_progress_frame_when_evicting`: start reading frame 1 (1 byte), then
    push until the ring is full. Frame 1's remaining bytes still come out intact, and the
    evictions start at frame 2.
  - `frame_ring_reports_high_water_in_bytes`: after pushing a 5-byte frame and reading it all,
    `high_water() == 7` and `len_bytes() == 0`.
  - `host_queue_sends_priority_frame_before_queued_bulk`: push bulk A (600 B), then priority P
    (20 B) after the first packet has started A.
    - Packet 1 is 512 bytes of A.
    - Packet 2 is A's last 88 bytes, then P, then nothing more of bulk until P is finished.
  - `host_queue_appends_terminator_packet_when_frame_ends_on_packet_boundary`: push one bulk
    frame of exactly 512 bytes. Packet 1 is 512, packet 2 is `[0x00]`, and packet 3 returns 0.
  - `host_queue_sends_no_terminator_when_more_follows_full_packet`: push 512 then 10. Packets
    are 512, then 10, then 0.
- [ ] **Step 2:** Run `cargo test -p wartui-proto --test frame_ring --test host_queue` and
  `cargo test -p wartui-proto config`. Expected: FAIL, because the modules are not declared.
- [ ] **Step 3:** Implement the three modules to the interfaces above, in `no_std` with no heap.
  `FrameRing` is a plain array plus head, tail and a read cursor.
- [ ] **Step 4:** Run the same commands. Expected: PASS. Then run `cargo clippy --workspace
  --all-targets` and `cargo fmt --check`.
- [ ] **Step 5:** Commit with `git add crates/wartui-proto && git commit -m "Add compile-time
  sizes, a frame ring and the USB host queue to wartui-proto"`.

---

### Task 3: The SPI link's framing, frozen hello and messages (`wartui-proto::spi`)

**Files:**
- Create: `crates/wartui-proto/src/spi/mod.rs` (framing and constants)
- Create: `crates/wartui-proto/src/spi/hello.rs`
- Create: `crates/wartui-proto/src/spi/message.rs` (`Up`, `Down`, record packing)
- Modify: `crates/wartui-proto/src/lib.rs`
- Test: `crates/wartui-proto/tests/spi.rs`

**Interfaces:**
- Consumes: `link::crc16`, `link::{BridgeToHost, HostToBridge, Chip, LoopPhase}`,
  `reset::ResetCause`, `link::ShortStr`.
- Produces (frozen; the module `//!` says so in bold and lists exactly what is frozen):
  - `pub const TRANSFER_LEN: usize = 2048;`
  - `pub const MAGIC: u16 = 0x5754;` ("WT" little-endian)
  - `pub const CORE_VERSION: u8 = 1;`
  - `pub const HEADER_LEN: usize = 12;`
  - `pub const CRC_OFFSET: usize = TRANSFER_LEN - 2;`
  - `pub const PAYLOAD_MAX: usize = CRC_OFFSET - HEADER_LEN;` (2034)
  - The header, little-endian, has these fields:

    | Offset | Size | Field |
    |---|---|---|
    | 0 | 2 | `MAGIC` |
    | 2 | 1 | `CORE_VERSION` |
    | 3 | 1 | kind |
    | 4 | 4 | seq (the sender's own counter, +1 per transfer) |
    | 8 | 2 | len |
    | 10 | 2 | reserved, zero |

    The payload sits at 12. Bytes past `len` are zero. The CRC-16/CCITT-FALSE at
    `CRC_OFFSET` covers bytes `0..CRC_OFFSET`.
  - `#[repr(u8)] pub enum Kind { Idle = 0, Hello = 1, Messages = 2, Update = 3 }`. `Update` is
    reserved for step 3 and nothing sends it yet.
  - `pub fn seal(buf: &mut [u8; TRANSFER_LEN], kind: Kind, seq: u32, len: usize)` writes the
    header, zero-fills past `len` and writes the CRC. The payload has already been written at
    `HEADER_LEN`.
  - `pub fn verify(buf: &[u8; TRANSFER_LEN]) -> Result<Transfer<'_>, VerifyError>` checks magic,
    then CRC, then core version, then `len`, then kind.
    - `pub struct Transfer<'a> { pub kind: Kind, pub seq: u32, pub payload: &'a [u8] }`
    - `pub enum VerifyError { BadMagic, BadCrc, OtherCore(u8), BadLen, UnknownKind(u8) }`
  - `pub struct SeqCheck`, with `fn note(&mut self, seq: u32) -> SeqNote` where
    `SeqNote { InOrder, First, Repeat, Gap }`.
  - `hello.rs` has a fixed layout, not postcard, so it is readable by any build:

    | Offset | Size | Field |
    |---|---|---|
    | 0 | 2 | `link_rev: u16` |
    | 2 | 1 | `chip` (`Chip` variant index) |
    | 3 | 6 | `mac` |
    | 9 | 32 | `fw_version`, UTF-8 zero-padded |
    | 41 | 1 | `reset_cause` (`ResetCause` variant index) |
    | 42 | 1 | `last_phase` (`LoopPhase` variant index) |
    | 43 | 4 | `uptime_ms: u32` |
    | 47 | 4 | `heap_free: u32` |

    `pub const HELLO_LEN: usize = 51`. It also provides:
    - `pub struct Hello { pub link_rev: u16, pub chip: Chip, pub mac: Mac, pub fw_version:
      ShortStr, pub reset_cause: ResetCause, pub last_phase: LoopPhase, pub uptime_ms: u32,
      pub heap_free: u32 }`
    - `Hello::encode(&self, out: &mut [u8]) -> usize`
    - `Hello::decode(payload: &[u8]) -> Option<Hello>`. Unknown enum indexes decode as
      `Chip`'s nearest meaning: `ResetCause::Unknown`, `LoopPhase::Unknown`, and a `chip`
      index past the end reads as `None`, which makes the whole hello `None`.

    The P4 sends a `Hello` too, with `chip: Esp32P4`. The C6 reads only `link_rev` from it.
  - `pub const LINK_REVISION: u16 = 1;` is not frozen. It names the `Up`/`Down` encoding, and
    is bumped whenever `message.rs`'s types or `link`'s enums change shape.
  - `message.rs`:
    - `#[derive(Serialize, Deserialize)] pub enum Down { Command(HostToBridge), GetRadioStatus }`
    - `pub enum Up { Message(BridgeToHost), RadioStatus(RadioStatus) }`
    - `pub struct RadioStatus { pub peer_count: u8, pub rx_count: u32, pub rx_oversize: u32, pub
      heap_free: u32, pub uptime_ms: u32, pub queues: heapless::Vec<QueueStat, 4>, pub
      crc_fail: u32, pub bad_magic: u32, pub seq_gap: u32 }`. Until Task 4 adds `QueueStat`,
      leave `queues` out and add it in Task 4. Bump `LINK_REVISION` there and update the golden
      bytes.
    - A `Messages` payload is records of `[len: u16 LE][postcard bytes]`, ending at `len == 0`
      or the payload's end.
    - `pub struct Packer<'a>` over the payload slice:
      - `fn new(payload: &'a mut [u8]) -> Self`
      - `fn push_encoded(&mut self, record: &[u8]) -> bool` returns `false` when the record does
        not fit, leaving the packer unchanged.
      - `fn len(&self) -> usize`
    - `pub fn encode_up(msg: &Up, out: &mut [u8]) -> Result<usize, postcard::Error>` and
      `encode_down`.
    - `pub fn records(payload: &[u8]) -> impl Iterator<Item = &[u8]>`
    - `pub fn decode_up(record: &[u8]) -> Result<Up, postcard::Error>` and `decode_down`.

- [ ] **Step 1: Write the failing tests** in `tests/spi.rs`:
  - `spi_round_trips_messages_transfer`: pack two encoded `Up::Message(Log)` records, then seal
    and verify. `records()` yields both, and both decode equal.
  - `spi_rejects_flipped_byte_with_bad_crc`: flip byte 1000, and `verify` returns `BadCrc`.
  - `spi_rejects_other_core_version`: set byte 2 to 2 and recompute the CRC, and the result is
    `OtherCore(2)`.
  - `spi_hello_has_frozen_layout`: encode a `Hello` with known values and assert the exact 51
    bytes (a literal array in the test). The test's doc comment says these bytes must never
    change.
  - `spi_hello_decodes_unknown_reset_cause_as_unknown`.
  - `spi_packer_refuses_record_that_does_not_fit`: a 2034-byte payload holds one 1490-byte
    `Down::Command(SendEspNow)` with a 1470-byte payload. A second refuses, and `len()` is
    unchanged.
  - `spi_largest_messages_fit_one_transfer`: the encoded `Down::Command(SendEspNow{1470})` and
    the encoded `Up::Message(Rx{1024})` are each `+2 <= PAYLOAD_MAX`.
  - `spi_link_revision_golden_bytes`: assert the exact postcard bytes of one sample `Down` and
    one `Up`. The failure message reads `"Up/Down encoding changed: bump spi::LINK_REVISION and
    update these bytes"`.
  - `spi_seq_check_reports_gap_and_repeat`.
- [ ] **Step 2:** Run `cargo test -p wartui-proto --test spi`. Expected: FAIL (no module).
- [ ] **Step 3:** Implement. Reuse `link::crc16`, and do not copy the spike's CRC.
- [ ] **Step 4:** Run `cargo test -p wartui-proto --test spi`. Expected: PASS. Then run clippy
  and fmt.
- [ ] **Step 5:** Commit with `git commit -m "Add the P4-C6 SPI link framing and frozen hello to
  wartui-proto"`.

---

### Task 4: The link additions and what the host does with them

**Files:**
- Modify: `crates/wartui-proto/src/link/mod.rs`:
  - `Chip`: append `Esp32P4`
  - `LoopPhase`: append `SpiTransfer`
  - `BridgeToHost::Ready`: append `radio: Option<RadioInfo>`
  - `BridgeToHost::Status`: append `queues: QueueStats` and `spi: Option<SpiStats>`
  - new types
- Modify: `crates/wartui-proto/src/spi/message.rs` (add `RadioStatus.queues`, bump
  `LINK_REVISION` to 2, update the golden bytes)
- Modify: `crates/wartui-proto/src/host_queue.rs` (`stats()` returns `[QueueStat; 2]`)
- Modify: `firmware/bridge/src/main.rs`:
  - `announce` sends `radio: None`
  - `GetStatus` sends `queues` holding the send-job queue once Task 6 lands. Until then it
    sends empty `queues` and `spi: None`.
- Modify: `crates/wartui-bridge/src/sim.rs`, `crates/wartui-core/src/engine/mod.rs` (`..` in the
  `Status` arm), `crates/wartui/src/status.rs` (print the new fields), every test fixture that
  builds `Ready` or `Status` (`grep -rn 'Status {\|Ready {' crates`)
- Test: `crates/wartui-proto/tests/link.rs`, `crates/wartui/src/status.rs` unit tests

**Interfaces:**
- Produces in `link`:
  - `pub struct RadioInfo { pub chip: Chip, pub mac: Mac, pub fw_version: ShortStr, pub
    reset_cause: ResetCause, pub last_phase: LoopPhase, pub uptime_ms: u32 }`
  - `pub enum QueueId { RadioUp, RadioSendJobs, RadioDown, HostPriority, HostBulk }`. The doc on
    each variant says whether it counts bytes or entries: `RadioSendJobs` counts entries, the
    rest bytes.
  - `pub struct QueueStat { pub queue: QueueId, pub capacity: u32, pub high_water: u32, pub
    dropped: u32 }`
  - `pub type QueueStats = heapless::Vec<QueueStat, 8>`
  - `pub struct SpiStats { pub transfers: u32, pub crc_fail: u32, pub bad_magic: u32, pub
    seq_gap: u32, pub ready_timeouts: u32, pub ready_recovered: u32, pub radio_crc_fail: u32,
    pub radio_bad_magic: u32, pub radio_seq_gap: u32 }`. The `radio_*` fields are the C6's view
    of P4→C6 transfers, from its `RadioStatus`.
- The `MAX_BODY` const asserts in `link/frame.rs` gain one for a `Status` with 8 queues and
  `spi: Some`, and one for a `Ready` with `radio: Some`.
- `wartui status` prints one line per queue as `  queue <name>  <high_water>/<capacity>
  <unit>  dropped <n>`, then an `spi` line when present. With empty queues and no spi it prints
  exactly what it prints today.

- [ ] **Step 1: Write the failing tests.**
  - `link_round_trips_status_with_queues_and_spi` and `link_round_trips_ready_with_radio` in
    `tests/link.rs`.
  - In `status.rs`: `status_prints_queue_lines_when_queues_present` (assert the exact line for
    `HostBulk 1024/262144 bytes dropped 0`) and `status_prints_nothing_extra_when_queues_empty`.
- [ ] **Step 2:** Run `cargo test --workspace`. Expected: compile FAIL.
- [ ] **Step 3:** Implement the types and fix every construction site. Update `LoopPhase`'s doc
  for `SpiTransfer`, which is the C6's and P4's wait on a transfer.
- [ ] **Step 4:** Run `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
  `cargo fmt --check`, then in `firmware/bridge` `cargo clippy --release --features esp32c6`
  and `--features esp32c5`. Expected: all clean.
- [ ] **Step 5:** Commit with `git commit -m "Carry radio, queue and SPI diagnostics in Ready and
  Status"`.

---

### Task 5: The P4's pair state machine (`wartui-proto::split`)

The P4's decisions, kept out of the firmware so they are tested:
- whether the C6 is up
- what `Ready` says
- what happens to a host command

**Files:**
- Create: `crates/wartui-proto/src/split.rs`
- Modify: `crates/wartui-proto/src/lib.rs`
- Test: `crates/wartui-proto/tests/split.rs`

**Interfaces:**
- Consumes: `spi::{Hello, LINK_REVISION, RadioStatus}`, `link::*`.
- Produces:
  - `pub const RADIO_WAIT_MS: u64 = 2_000;`. The P4 waits this long for the C6's first hello
    before enumerating USB with its own MAC.
  - `pub enum RadioState { Waiting, Up(Hello), Mismatch(Hello), Absent }`
  - `pub struct Pair` holds the state, the P4's own identity, and the latest `RadioStatus`.
    - `pub fn new(own_mac: Mac, own_fw: ShortStr, own_cause: ResetCause, own_phase: LoopPhase,
      boot_ms: u64) -> Self`
    - `pub fn on_hello(&mut self, hello: Hello, now_ms: u64) -> PairEvent`
      - It moves to `Up` if `hello.link_rev == LINK_REVISION`, else `Mismatch`.
      - It returns `PairEvent::NewLife` when a hello arrives in `Up` or `Mismatch`, meaning the
        C6 restarted, or the first time it reaches `Up`/`Mismatch`.
      - Otherwise it returns `PairEvent::None`.
    - `pub fn on_tick(&mut self, now_ms: u64) -> PairEvent` moves `Waiting` to `Absent` at
      `RADIO_WAIT_MS` and returns `NewLife`.
    - `pub fn usb_serial_mac(&self) -> Option<Mac>`: `None` while `Waiting`, the C6's MAC in
      `Up`/`Mismatch`, and `own_mac` in `Absent`.
    - `pub fn ready(&self, now_ms: u64) -> Option<BridgeToHost>`: `None` while `Waiting`.
      Otherwise a `Ready` with:
      - `chip: Esp32P4`
      - `mac`: the C6's in `Up`/`Mismatch`, own in `Absent`
      - `fw_version`: own
      - `proto_version: LINK_PROTO_VERSION`
      - `reset_cause` and `last_phase`: own
      - `heap_free`: the C6's (`hello.heap_free`), or 0 when absent
      - `uptime_ms`: **the C6's uptime now**, which is `hello.uptime_ms + (now_ms -
        hello_at_ms)`, or the P4's own uptime when absent
      - `panel: None`
      - `gps: false`
      - `radio: Some(RadioInfo)` from the hello in `Up`/`Mismatch`, else `None`
    - `pub fn fault(&self) -> Option<LogStr>`:
      - `Mismatch` gives `"radio speaks SPI link revision {theirs}, this P4 speaks {ours};
        update the radio"`.
      - `Absent` gives `"the radio coprocessor is not answering"`.
      - `Up`/`Waiting` give `None`.
    - `pub fn route(&self, cmd: &HostToBridge) -> Route`:
      - `Route::Local` for Identify, GetStatus, Reset, ShowPanel and GpsWrite.
      - In `Up`, `Route::Forward` for AddPeer, RemovePeer, SendEspNow and SetTxPower.
      - When not `Up`, `Route::Refuse(Option<BridgeToHost>)`: `SendEspNow{id,..}` gives
        `Some(SendResult{ id, status: Rejected, tx_us: 0 })` and the others give `None`.
    - `pub fn note_radio_status(&mut self, status: RadioStatus)`
    - `pub fn status(&self, now_ms: u64, host: [QueueStat; 2], down: QueueStat, spi:
      SpiStats, host_dropped: u32) -> BridgeToHost` builds `Status`:
      - `peer_count`, `rx_count`, `rx_oversize` come from the last `RadioStatus` (0 if none).
      - `dropped_tx` is `host_dropped` plus the C6's `RadioUp` drops.
      - `uptime_ms` is as in `ready`.
      - `queues` holds the C6's queues, then `down`, then `host`.
      - `spi` is `spi` with `radio_*` filled from the `RadioStatus`.

- [ ] **Step 1: Write the failing tests** in `tests/split.rs`:
  - `pair_waits_then_announces_radio_mac_when_hello_arrives`: `ready()` is `None` before the
    hello. After it, `mac` is the hello's, `chip == Esp32P4` and `radio.is_some()`.
  - `pair_announces_own_mac_and_error_when_radio_never_answers`: tick at 2000 gives `NewLife`,
    `mac == own`, `radio == None`, and `fault()` contains "not answering".
  - `pair_announces_new_life_when_hello_arrives_after_link_up`: hello at t=1000 with C6 uptime
    900, then at t=60_000 a hello with uptime 50. That gives `NewLife`, and
    `ready(60_000).uptime_ms == 50`, lower than the earlier `ready(59_999)` value.
  - `pair_reports_c6_uptime_advancing_with_p4_clock`: hello uptime 900 at t=1000, so
    `ready(3000).uptime_ms == 2900`.
  - `pair_rejects_sends_and_reports_when_link_revision_differs`: hello with `link_rev =
    LINK_REVISION + 1`.
    - `route(SendEspNow{id:7,..})` is `Refuse(Some(SendResult{id:7, status: Rejected, ..}))`.
    - `fault()` names both revisions.
  - `pair_routes_radio_commands_forward_when_up` and `pair_routes_identify_local`.
  - `pair_status_sums_host_and_radio_drops`.
- [ ] **Step 2:** Run `cargo test -p wartui-proto --test split`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p wartui-proto --test split`. Expected: PASS. Then run clippy
  and fmt.
- [ ] **Step 5:** Commit with `git commit -m "Add the P4 bridge's pair state machine"`.

---

### Task 6: Move the radio half into `wartui-firmware`, and send without blocking

**Files:**
- Create: `firmware/common/src/phase.rs` (moved from `firmware/bridge/src/main.rs:189-263`:
  `PHASE`, `PHASE_VALID`, `PHASE_MAGIC`, `HOST_PRESENT`, `mark`, `boot_phase`,
  `boot_speaks_first`). `boot_phase` learns code 10 (`SpiTransfer`).
- Create: `firmware/common/src/radio.rs`
- Modify: `firmware/common/Cargo.toml`:
  - Feature `esp32p4 = ["esp-hal/esp32p4"]`.
  - Feature `radio`, with optional deps `esp-radio`, `embassy-sync`, `embassy-futures` at the
    versions `firmware/bridge/Cargo.toml` pins.
  - Chip features forward `"esp-radio?/esp32c5"` etc.
- Modify: `firmware/common/src/lib.rs`:
  - The chip check accepts exactly one of c5, c6 and p4.
  - `compile_error!` when `radio` is set with `esp32p4`.
  - Declare `phase` and `#[cfg(feature = "radio")] radio`.
  - Docs.
- Create: `firmware/bridge/src/config.rs` (`SEND_JOBS`)
- Modify: `firmware/bridge/src/main.rs` and `firmware/bridge/Cargo.toml` (enable
  `wartui-firmware/radio`)
- Modify: `firmware/bridge/README.md` § "Transmitting" (the send no longer blocks the loop) and
  `firmware/common`'s description string.

**Interfaces:**
- Produces in `wartui_firmware::radio`, moved verbatim with their comments where it says
  "moved", and changed only where stated:
  - `pub const fn peer(mac: &Mac) -> PeerInfo` (moved)
  - `pub fn add_peer(manager: &EspNowManager, mac: &Mac) -> Result<bool, EspNowError>` (moved)
  - `pub fn set_peer_rate(manager: &EspNowManager, mac: &Mac) -> bool` (moved)
  - `pub enum Said { Nothing, Log(LogLevel, &'static str), Error(&'static str) }`. It carries
    the exact strings the bridge sends today.
  - `pub fn setup(controller: &mut WifiController<'_>, manager: &EspNowManager) ->
    SetupReport`, where `SetupReport { tx_power: Option<i8>, said: heapless::Vec<Said, 3> }`.
    It does what `main` and `finish_radio_setup` do today: set the default transmit power, park
    on `CONTROL_CHANNEL` and set the broadcast rate. It reports failures with today's strings.
  - `pub fn add_peer_command(manager: &EspNowManager, mac: &Mac) -> Said` and
    `remove_peer_command(..) -> Said`. These are today's `AddPeer`/`RemovePeer` arms.
  - `pub fn set_tx_power_command(controller: &mut WifiController<'_>, power: i8, known: &mut
    Option<i8>) -> Said`, today's `SetTxPower` arm.
  - `pub enum Heard { Forward(BridgeToHost), Oversize, Foreign }` and `pub fn hear(received:
    &ReceivedData, rx_us: u32) -> Heard`. This is today's `forward`, with the RSSI comment kept.
    It builds `BridgeToHost::Rx`.
  - `pub struct SendJob { pub id: u16, pub dst: Mac, pub ensure_peer: bool, pub payload:
    SendPayload }`
  - `pub struct SendDone { pub id: u16, pub status: SendStatus, pub done_at:
    esp_hal::time::Instant }`
  - `pub type SendJobs<const N: usize> = embassy_sync::channel::Channel<NoopRawMutex, SendJob,
    N>` and `SendDones<N>` likewise.
  - `pub async fn run_sends<const N: usize>(manager: &EspNowManager, sender: &mut EspNowSender,
    jobs: &SendJobs<N>, done: &SendDones<N>) -> !` loops forever:
    - Receive a job.
    - Do today's `transmit` peer handling synchronously, including the broadcast-slot
      give-up.
    - `sender.send_async(&dst, &payload).await`.
    - Map the result to `SendStatus` exactly as today: `Broadcast` for `BROADCAST`, `AckOk`,
      `AckFail`, `NoPeer`, `PeerTableFull`, `Rejected`.
    - `done.send(SendDone{..}).await`.
  - `pub fn queue_send<const N: usize>(jobs: &SendJobs<N>, job: SendJob) -> Option<SendDone>`
    does `try_send`. A full queue returns `Some(SendDone{status: Rejected, ..})` for the caller
    to report.
- The single-chip bridge after this task:
  - `main` ends in `join(main_loop(..), radio::run_sends(&manager, &mut sender, jobs, dones))`.
  - `jobs` and `dones` are `static` `StaticCell`s sized `config::SEND_JOBS`
    (`WARTUI_BRIDGE_SEND_JOBS`, default 4).
  - `handle`'s `SendEspNow` arm calls `queue_send`.
  - `idle_wait` gains an arm on `dones.receive()`.
  - Every pass drains `dones.try_receive()` into `SendResult` frames, with `tx_us` from
    `done_at` on the bridge's clock.
  - `LoopPhase::Transmit` is no longer marked: nothing blocks there now. Keep the variant.
  - `Status.queues` carries one `QueueStat { queue: RadioSendJobs, capacity: SEND_JOBS,
    high_water, dropped }`. Track the high-water mark at `queue_send`.

- [ ] **Step 1:** Move `phase` and the radio functions, and build. Run `cd firmware/common &&
  cargo clippy --release --features esp32c6,radio`, then `esp32c5,radio`, then `esp32p4`.
  Expected: clean.
- [ ] **Step 2:** Switch the bridge to the shared code and the async send. Run `cd
  firmware/bridge && cargo clippy --release --features esp32c6`, then `esp32c5`, then
  `esp32c5,t-dongle-c5,t-dongle-c5-gps`, then `esp32c6,xiao-external-antenna`, plus `cargo fmt
  --all -- --check`. Expected: clean, with no `large_stack_frames` hit; keep the channels in
  `static`s.
- [ ] **Step 3:** Run `cd firmware/node && cargo clippy --release --features esp32c6` (the node
  uses `wartui-firmware` too). Expected: clean.
- [ ] **Step 4:** Commit with `git commit -m "Share the bridge's radio half and stop blocking the
  loop on a send"`.

---

### Task 7: Re-measure the single-chip bridge (main session, with the user)

- [ ] **Step 1:** Flash `75:40` with Task 6's bridge. Repeat Task 1 steps 3–4 with the same
  nodes, load and duration.
- [ ] **Step 2:** Compare the result with the baseline. The bridge must lose no more batches and
  drop no more frames than before. If it does, stop and run superpowers:systematic-debugging
  before going on.
- [ ] **Step 3:** Record the result in `docs/p4-bridge-findings.md` and commit with `git commit -m
  "Re-measure the single-chip bridge with the non-blocking send"`.

---

### Task 8: The C6 radio firmware (`firmware/p4-radio`)

**Files:**
- Create: `firmware/p4-radio/{Cargo.toml, build.rs, rust-toolchain.toml, .cargo/config.toml,
  README.md}`. Copy `firmware/bridge`'s, with the package `wartui-p4-radio-fw` and features
  `esp32c6` only.
- Create: `firmware/p4-radio/src/main.rs`, `src/config.rs`, `src/link.rs`

**Interfaces:**
- Consumes: Task 3's `spi::*`, Task 6's `wartui_firmware::{radio, phase, reset_cause,
  reboot}`, Task 2's `FrameRing`, `config::env_usize`.
- `config.rs`:
  - `pub const UP_QUEUE_BYTES: usize = env_usize(option_env!("WARTUI_RADIO_UP_QUEUE_BYTES"),
    96 * 1024);` holds encoded `Up` records waiting for a transfer.
  - `pub const SEND_JOBS: usize = env_usize(option_env!("WARTUI_RADIO_SEND_JOBS"), 4);`
  - The module `//!` says the C6 has 512 KB of SRAM and the radio blobs' heap takes 100 KB, so
    these defaults leave room. A bench `wartui status` high-water mark is how to right-size
    them.
- Behavior:
  - **Boot.**
    - `reset_cause()`, then `phase::boot_phase`, then the same two heaps as the bridge
      (`64 KiB` reclaimed and `36 KiB`).
    - `esp_rtos::start`, then `WifiController::new(.., US)` and `radio::setup`.
    - READY low.
    - `Input` on GPIO21 with `peripheral_input()` handed to the slave's `with_cs`, so the same
      pin serves the SPI peripheral and the edge wait.
  - **Futures**, joined in one task:
    - `radio::run_sends`
    - `hear_loop`, which awaits `receiver.receive_async()`, runs `radio::hear` and pushes
      `Up::Message` records into the up queue, counting oversize and foreign frames
    - `link_loop`
  - **`link_loop`** keeps two TX and two RX DMA buffers (double buffering).
    - On a finished transfer it immediately arms the next with the TX buffer already prepared,
      and raises READY. The edge wait on CS's rising edge is polled before READY rises, as the
      spike's P4 does with `join`.
    - Then it verifies the received buffer and acts on it:
      - `Hello` notes `link_rev`.
      - `Messages` runs each `Down`:
        - `Command(AddPeer/RemovePeer/SetTxPower)` goes to the radio command functions, and the
          `Said` is pushed as `Up::Message(Log/Error)`.
        - `Command(SendEspNow)` goes to `queue_send`, and a refusal is pushed as a `SendResult`.
        - `GetRadioStatus` pushes `Up::RadioStatus`.
    - Then it prepares the other TX buffer: a `Hello` until a P4 hello with any `link_rev` has
      been seen, otherwise as many up-queue records as fit (`Packer`), otherwise `Idle`.
    - Send results come from `dones` into the up queue as `Up::Message(SendResult)`, stamped on
      the C6 clock (`rx_us` and `tx_us` share it).
    - Await the CS rising edge under a 100 ms timeout. On timeout, check `is_done()`.
    - `transfer.wait()` after the edge is the driver's only completion call. It spins only for
      the bus-idle check, so comment it as the one poll, with why: esp-hal 1.2's SPI slave has
      no async API.
    - Mark `LoopPhase::SpiTransfer` while waiting.
  - **Hello contents:**
    - `link_rev: LINK_REVISION`
    - `chip: Esp32C6`
    - `mac`: station MAC
    - `fw_version`: `CARGO_PKG_VERSION`
    - reset cause and last phase
    - `uptime_ms`
    - `heap_free`: `esp_alloc::HEAP.free()`
  - **Panic** goes to `reboot()`.

- [ ] **Step 1:** Scaffold the crate and build. Run `cargo clippy --release --features esp32c6`
  and `cargo fmt --all -- --check`. Expected: clean.
- [ ] **Step 2:** Flash `D8:1C` over the FT232 on header H7 (memory `bench-boards` has the
  procedure: IO9 jumpered to GND, P4 held in BOOT, `espflash flash --before no-reset --after
  no-reset --port /dev/ttyUSB0`). The main session does this with the user.
- [ ] **Step 3:** Commit with `git commit -m "Add the C6 radio firmware for the P4 split
  bridge"`.

---

### Task 9: The P4 bridge firmware (`firmware/p4-bridge`)

**Files:**
- Create: `firmware/p4-bridge/{Cargo.toml, build.rs, rust-toolchain.toml, .cargo/config.toml,
  README.md}`.
  - Model them on the spike's `firmware/spike/p4-bridge`. Package `wartui-p4-bridge-fw`, no
    features.
  - Dependencies: `wartui-firmware` with `esp32p4`, plus `wartui-proto`.
  - The runner is `espflash flash --monitor --chip esp32p4`.
- Create: `firmware/p4-bridge/src/main.rs`, `src/config.rs`, `src/usb.rs`, `src/link.rs`

**Interfaces:**
- Consumes: Tasks 2, 3, 5, plus `wartui_firmware::{phase, reset_cause, reboot}`.
- `config.rs`:
  - `HOST_BULK_BYTES` (`WARTUI_P4_HOST_BULK_BYTES`, default 256 KiB)
  - `HOST_PRIORITY_BYTES` (`WARTUI_P4_HOST_PRIORITY_BYTES`, 16 KiB)
  - `DOWN_QUEUE_BYTES` (`WARTUI_P4_DOWN_QUEUE_BYTES`, 16 KiB)
  - `SPI_KHZ = 40_000`, which is not configurable: it is the measured rate.
- Behavior:
  - **Boot.**
    - Hold GPIO54 low for 50 ms, then high, so the C6 restarts with the P4.
    - Start SPI as master (async DMA, `DMA_AXI_CH0`), with READY on GPIO6 pulled down.
    - Run the SPI link future at once.
    - Build the USB device **only once `Pair::usb_serial_mac()` is `Some`**, with:
      - manufacturer `wartui`
      - product `wartui p4 bridge`
      - serial: the MAC as uppercase `AA:BB:CC:DD:EE:FF`, the form `mac::parse` reads
      - `self_powered = true`, `max_power = 0`
      - CDC-ACM with 512-byte packets
  - **SPI link future.**
    - Wait for READY's rising edge under a 100 ms timeout, with the stale-high recovery from
      the spike (`ready_recovered`).
    - Each transfer sends the P4's `Hello` until the C6's hello has arrived, else packed
      `Down` records from the down `FrameRing`, else `Idle`.
    - Verify every received transfer and track the C6's seq with `SeqCheck`.
    - A `Hello` goes to `Pair::on_hello`.
    - `Up::Message(m)` is encoded with `link::encode_frame` into the host queue, with priority
      by `is_priority`. `Up::RadioStatus` goes to `Pair::note_radio_status`.
    - Count everything into `SpiStats`.
  - **USB reader future.** Read packets, push them into `FrameAccumulator<MAX_HOST_FRAME>`, and
    `decode_frame::<HostToBridge>`. Then follow `Pair::route`:
    - `Local`:
      - `Identify` pushes `ready()` plus `fault()` as an `Error`.
      - `GetStatus` pushes `Down::GetRadioStatus`, then answers with `Pair::status` when the
        next `RadioStatus` arrives or after 50 ms, whichever comes first.
      - `Reset` calls `reboot()`.
      - `ShowPanel` and `GpsWrite` are dropped.
    - `Forward` encodes `Down::Command` into the down queue. A full down queue answers a
      `SendEspNow` with `SendResult{Rejected}`.
    - `Refuse(r)` pushes `r`.
  - **USB writer future.** While DTR is held, `next_packet` then `write_packet`. Wait on a
    `Signal` raised by every push, or on a 10 ms tick to notice DTR. Transmit opens on DTR,
    which replaces the single-chip bridge's speaks-first rule, because CDC-ACM over OTG does
    not wedge as USB-Serial-JTAG does. Say so in the `//!`.
  - **Tick future.** Every 100 ms, `Pair::on_tick`. On `PairEvent::NewLife` while a host holds
    DTR, push `ready()` and `fault()`.
  - All futures share state through `Cell` and `RefCell` in one task with `join`, as the spike
    did.

- [ ] **Step 1:** Scaffold and build. Run `cargo clippy --release` and `cargo fmt --all --
  --check` in `firmware/p4-bridge`. Expected: clean.
- [ ] **Step 2:** Flash `8B:65`. On the POE-ETH that means the user holds BOOT while plugging
  in the Type-C, runs `cargo run --release` (or `espflash flash --chip esp32p4`), then
  replugs to run. The main session does this with the user.
- [ ] **Step 3:** Run `wartui ports`, then `wartui status --bridge 14:C1:9F:00:D8:1C`. Expected:
  a Status with the new queue lines, `spi` with zero errors, and `peer_count` 0 or 1.
- [ ] **Step 4:** Commit with `git commit -m "Add the P4 bridge firmware"`.

---

### Task 10: The host finds and refuses-to-flash the P4 bridge

**Files:**
- Modify: `crates/wartui-bridge/src/ports.rs`:
  - Add `pub const TEST_VID: u16 = 0x1209; pub const TEST_PID: u16 = 0x0001; pub const
    P4_PRODUCT_PREFIX: &str = "wartui";` and `pub fn is_p4_bridge(c: &PortCandidate) -> bool`.
  - `could_be_a_bridge` becomes Espressif-or-`is_p4_bridge`.
  - `could_be_a_receiver` becomes `!could_be_a_bridge`, minus the usable-path check, so the
    two stay complements.
  - Update the `//!` paragraph "decided by vendor ID and nothing else" to name the one exact
    pair.
- Modify: `crates/wartui/src/flash/bridge.rs`. When the board it would flash `is_p4_bridge`,
  bail with "this is a P4 bridge: flash it by hand until single-cable updates land, as
  firmware/p4-bridge/README.md describes".
- Test: unit tests in `ports.rs`, and the existing flash tests' style in
  `crates/wartui/src/flash/`.

- [ ] **Step 1: Write the failing tests.**
  - `ports_could_be_a_bridge_when_test_pair_and_wartui_product`
  - `ports_not_a_bridge_when_test_pair_without_wartui_product`
  - `ports_bridge_and_receiver_stay_complements` (over a table of candidates)
  - `flash_bridge_refuses_p4_bridge_with_readme_hint`
- [ ] **Step 2:** Run `cargo test -p wartui-bridge ports` and `cargo test -p wartui flash`.
  Expected: FAIL.
- [ ] **Step 3:** Implement. `candidate()` gains no new parameter. Tests set `product` on the
  returned struct.
- [ ] **Step 4:** Run `cargo test --workspace`, clippy and fmt. Expected: PASS.
- [ ] **Step 5:** Commit with `git commit -m "Recognise the P4 bridge's USB ID and refuse to
  flash it automatically"`.

---

### Task 11: Docs and CI

**Files:**
- Modify: `firmware/p4-bridge/README.md` and `firmware/p4-radio/README.md`:
  - **Building.**
  - **First flash** for each chip on both boards:
    - POE-ETH: BOOT held while plugging in, replug to run.
    - WIFI6: espflash auto-resets it.
    - C6: header H7/H4 with IO9 jumpered, P4 held in BOOT, `--before no-reset --after
      no-reset`.
  - **The VBUS warning** for the high-speed port: always live on both boards, so the cable must
    leave VBUS unconnected. The WIFI6's MX1.25 pin 1 is VCC_5V.
  - **Every `WARTUI_*` size** and how to change it.
- Modify: `AGENTS.md`, the firmware check commands block. Add `firmware/p4-radio` (`cargo
  clippy --release --features esp32c6`) and `firmware/p4-bridge` (`cargo clippy --release`).
- Create: `.github/workflows/firmware-p4-radio.yml` and `firmware-p4-bridge.yml`, mirroring
  `firmware-bridge.yml`'s path filters and fmt/clippy steps. Add `rustup target add
  riscv32imafc-unknown-none-elf` for the P4.
- Modify: `.github/workflows/firmware-common.yml` to add the `esp32p4` clippy run.

- [ ] **Step 1:** Write the docs and workflows.
- [ ] **Step 2:** Run every firmware's fmt and clippy locally per the updated `AGENTS.md` block.
  Expected: clean.
- [ ] **Step 3:** Commit with `git commit -m "Document and check the P4 split bridge
  firmwares"`.

---

### Task 12: Measure the P4 bridge (main session, with the user)

- [ ] **Step 1:** With `8B:65` and `D8:1C` flashed (Tasks 8 and 9), repeat Task 1 steps 3–4
  against the P4 bridge, with the same nodes, load and duration. Afterwards run `wartui
  status` for the queue high-water marks and the SPI counters.
- [ ] **Step 2:** The P4 bridge must lose no batch the single-chip bridge kept (Task 7's
  numbers). Its SPI errors must be zero, or explained. Its median assignment latency must stay
  under 10 ms, against the node's 100 ms window. Compare it with Task 7's latency, which shows
  what the SPI hop costs.
- [ ] **Step 3:** Run a normal 10-minute `wartui run` with production node firmware, so the
  fleet table, assignments and transmit power behave as on a single-chip bridge.
- [ ] **Step 4:** Restart the C6 alone mid-run by pulsing the C6's EN through a quick test
  build, or by power-cycling only if the bench allows. Check that the host reports a new
  bridge life and the nodes keep reporting. If neither way is practical, record that it was
  not exercised on hardware, relying on Task 5's test.
- [ ] **Step 5:** When the ESP32-P4-WIFI6 arrives, flash both chips and repeat steps 1–3 on it.
- [ ] **Step 6:** Record everything in `docs/p4-bridge-findings.md`: the tables, the high-water
  marks, and the defaults they suggest. Commit with `git commit -m "Measure the P4 split bridge
  against the single-chip bridge"`.
