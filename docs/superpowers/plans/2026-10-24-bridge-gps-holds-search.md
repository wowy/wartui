# Bridge GPS Holds the Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** While the connected bridge's GPS receiver is sending sentences, the host's serial GPS
search stands down and `g` reports on the bridge's receiver, fix or no fix.

**Architecture:** `Gps::has_fresh_fix` is replaced by `Gps::is_talking`: a checksum-valid
sentence within `max_age` of now, since the stream last restarted. The two callers,
`runtime::stand_host_down` and `FleetEngine::shown_gps`, switch to it. Everything else stays: a
port named with `--gps` never stands down, a reconnect needs a new sentence, the position chain's
order is untouched.

**Tech Stack:** Rust, `wartui-core`.

**Spec:** the user's request: "If the bridge (P4, C5, C6, doesn't matter) reports that a GPS
module is attached and sending sentences, do not continue to search for another module. Doing so
means you can't watch the detailed GPS output to see the bridge's GPS finding satellites."

## Global Constraints

- Branch `feat/bridge-gps-holds-search` off `main`. Commit there. No push, no PR until asked.
- Test names follow `component_action_when_condition` (AGENTS.md § Conventions). American spelling.
- Prose says what the project does, never what it used to do: no "no longer", "now", "previously".
- `//!` docs carry the reasoning. Update it where the rule changes; don't add a history note.
- Verify with `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --check`.

## Out of scope

A GPS receiver on the bridge and another on the host at once is unrealistic. Don't reason about
it, document it or guard against it.

## Review Focus

1. **Bridge receiver unplugged or stalled mid-run.** Bytes stop. The search resumes once the last
   sentence is older than `max_age`, because the runtime tick re-decides the stand-down. Covered
   by `runtime_resumes_host_gps_when_bridge_sentences_go_stale` (Task 2).
2. **Reconnect.** Sentences from before `reset_stream` don't hold the search down. Covered by
   `gps_is_not_talking_when_stream_resets_until_a_new_sentence_arrives` (Task 1) and
   `runtime_keeps_host_gps_searching_when_bridge_reconnects_until_a_new_sentence` (Task 2).
3. **Noise that isn't a sentence.** A line with a bad checksum, or UBX only (the rate ack), must
   not count. Covered by `gps_is_not_talking_when_bytes_hold_no_valid_sentence` (Task 1).
4. **Bridge without a GPS port.** Never stands the host down, whatever arrives. Kept in
   `runtime_suspends_host_gps_when_bridge_sends_sentences` (Task 2).
5. **A port named with `--gps`.** Still never stands down, and still the receiver `g` shows. The
   existing tests `gps_ignores_suspension_when_port_is_pinned` and
   `engine_snapshot_shows_pinned_host_receiver_when_bridge_fix_is_fresh` keep passing unchanged.

---

### Task 1: `Gps::is_talking`

**Files:**
- Modify: `crates/wartui-core/src/gps.rs` (`Inner.fix_in_stream` ~207, `feed_line` ~305,
  `has_fresh_fix` ~456, `reset_stream` ~478, tests ~1370-1400)

**Interfaces:**
- Produces: `pub fn is_talking(&self, now_ms: i64) -> bool` on `Gps`, `#[must_use]`.
  `has_fresh_fix` is deleted in Task 2 once nothing calls it. Keep it here so this task builds.

- [ ] **Step 1: Write the failing tests** in the `gps.rs` test module, next to the
  `has_fresh_fix` tests, using its `streamed`, `GGA`, `GGA_NO_FIX` and `DEFAULT_MAX_AGE`:

```rust
#[test]
fn gps_is_talking_when_no_fix_sentences_are_within_max_age() {
    // A receiver with no sky still sends every epoch. That is the bridge's receiver at work.
    let gps = Gps::bridged(DEFAULT_MAX_AGE);
    assert!(!gps.is_talking(1_000), "nothing heard yet");
    gps.feed_bytes(&streamed(GGA_NO_FIX), 1_000);
    assert!(gps.is_talking(6_000));
    assert!(!gps.is_talking(6_001));
    gps.feed_bytes(b"$GPGSV,1,1,00*79\r\n", 6_001);
    assert!(gps.is_talking(6_001));
}

#[test]
fn gps_is_not_talking_when_bytes_hold_no_valid_sentence() {
    let gps = Gps::bridged(DEFAULT_MAX_AGE);
    // GGA_NO_FIX with its checksum broken.
    gps.feed_bytes(b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*00\r\n", 1_000);
    gps.feed_bytes(&ACK_VALSET, 1_000);
    assert!(!gps.is_talking(1_000));
}

#[test]
fn gps_is_not_talking_when_stream_resets_until_a_new_sentence_arrives() {
    let gps = Gps::bridged(DEFAULT_MAX_AGE);
    gps.feed_bytes(&streamed(GGA_NO_FIX), 1_000);
    gps.reset_stream();
    // Heard within the age limit, but before the reconnect.
    assert!(!gps.is_talking(2_000));
    gps.feed_bytes(&streamed(GGA_NO_FIX), 3_000);
    assert!(gps.is_talking(3_000));
}
```

  `ACK_VALSET` is `crate::ubx::ACK_VALSET`, which the test module already imports.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p wartui-core --lib gps_is_`
Expected: compile error, `no method named is_talking`.

- [ ] **Step 3: Implement `is_talking`**

Add `Inner.heard_in_stream: bool`. Doc: "Whether a sentence has parsed since the
stream last restarted ([`Gps::reset_stream`])." Set it in `feed_line` wherever `heard_ms` is set
(`parsed.is_ok()`). `reset_stream` clears it.
`is_talking` is `heard_in_stream && heard_ms` within `max_age` of `now_ms`, through `is_fresh`,
the same shape as `has_fresh_fix`. Doc: "Whether a checksum-valid sentence arrived within the
staleness limit of `now_ms`, and since the stream last restarted. A sentence without a fix counts."

Add `heard_in_stream` beside `fix_in_stream` rather than renaming it, so `has_fresh_fix` keeps
working until Task 2 deletes both.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test -p wartui-core --lib gps_`
Expected: PASS, including the existing `gps_has_fresh_fix_*` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/wartui-core/src/gps.rs
git commit -m "Tell when the bridge's receiver is sending sentences"
```

  Body: the stand-down will key on this, so a bridge receiver still acquiring holds the search.

---

### Task 2: Stand the search down, and show the bridge's receiver, while it talks

**Files:**
- Modify: `crates/wartui-core/src/runtime.rs` (`//!` lines 8-14, `stand_host_down` 265-280,
  tests ~428-475)
- Modify: `crates/wartui-core/src/engine/snapshot.rs` (`gps` field doc 148-153, `shown_gps`
  195-212)
- Modify: `crates/wartui-core/src/gps.rs` (`//!` lines 7-19, `suspended` field doc 360, `bridged`
  doc 399, `set_suspended` log line 447, delete `has_fresh_fix` and `fix_in_stream` and their
  three tests)
- Modify: `crates/wartui-core/src/position.rs` (`//!` lines 8-10)
- Modify: `crates/wartui-core/tests/engine.rs` (~1589-1615)
- Modify: `crates/wartui/README.md` (GPS section, ~801-815)
- Modify: `AGENTS.md` (line 158)

**Interfaces:**
- Consumes: `Gps::is_talking(&self, now_ms: i64) -> bool` from Task 1.
- Produces: `stand_host_down` keeps its signature.

- [ ] **Step 1: Rewrite the runtime tests**

In `runtime.rs`'s test module:

- `runtime_keeps_host_gps_searching_when_bridge_receiver_sends_no_fix` becomes
  `runtime_suspends_host_gps_when_bridge_sends_no_fix_sentences`. Same feed (`GGA_NO_FIX`, `GSV`
  at 1 000). Before the feed: `!host.suspended()` (nothing heard). After it: `host.suspended()`.
- `runtime_suspends_host_gps_when_bridge_fix_is_fresh` becomes
  `runtime_suspends_host_gps_when_bridge_sends_sentences`. Feed `GSV` at 1 000. With
  `bridge_has_gps = false`: not suspended ("sentences on a bridge without a GPS port"). With
  `true`: suspended.
- `runtime_resumes_host_gps_when_bridge_fix_goes_stale` becomes
  `runtime_resumes_host_gps_when_bridge_sentences_go_stale`. Feed `GSV` at 1 000. Suspended at
  6 000. Not suspended at 6 001.
- `runtime_keeps_host_gps_searching_when_bridge_reconnects_until_a_new_fix` becomes
  `runtime_keeps_host_gps_searching_when_bridge_reconnects_until_a_new_sentence`. Same shape with
  `GSV` in place of `GGA`.

`GGA` may become unused there. Delete it if so.

- [ ] **Step 2: Rewrite the engine tests**

In `tests/engine.rs`:

- `engine_snapshot_shows_host_receiver_when_bridge_receiver_has_no_fix` becomes
  `engine_snapshot_shows_bridge_receiver_when_it_sends_no_fix_sentences`. Its comment becomes:
  "A receiver with no sky still talks every epoch, and the view follows it finding satellites."
  It asserts `shows_bridge_gps(&engine, clock.at(2))`, and `!shows_bridge_gps(&engine,
  clock.at(8))` ("the last sentence is past the age limit").
- `engine_snapshot_shows_bridge_receiver_when_its_fix_is_fresh` becomes
  `engine_snapshot_shows_bridge_receiver_when_it_sends_sentences`. Assertions unchanged, with its
  "the fix is past the age limit" message reading "the last sentence is past the age limit".
- `engine_snapshot_shows_host_receiver_when_bridge_receiver_is_silent` and
  `engine_snapshot_shows_pinned_host_receiver_when_bridge_fix_is_fresh` stay as they are.

- [ ] **Step 3: Run them and see them fail**

Run: `cargo test -p wartui-core runtime_ engine_snapshot_shows_`
Expected: the renamed tests FAIL on their suspended / via-bridge assertions.

- [ ] **Step 4: Switch both callers to `is_talking`**

- `stand_host_down`: `host.set_suspended(bridge_has_gps && bridge.is_some_and(|gps|
  gps.is_talking(now_ms)))`. Its doc says the rule and the reason: stand down while the connected
  bridge has a GPS port and its receiver is sending sentences, fix or not, so the operator can
  watch it acquire. A GPS-built bridge whose receiver is unplugged or silent leaves the search
  running.
- `shown_gps`: `Some(bridge) if bridge.is_talking(now_ms) => Some(bridge)`. Update its doc and
  the `Snapshot::gps` field doc to "while it is sending sentences" instead of "while it has a
  fresh fix".
- `set_suspended`'s log line: `"gps search stands down: the bridge's receiver is sending sentences"`.
- Delete `has_fresh_fix`, `fix_in_stream`, and the tests `gps_has_no_fresh_fix_when_receiver_sends_only_no_fix_sentences`,
  `gps_has_fresh_fix_when_fix_is_within_max_age` and
  `gps_has_no_fresh_fix_when_stream_resets_until_a_new_fix_arrives`. Task 1's tests cover the
  replacement.

- [ ] **Step 5: Rewrite the reasoning where the rule is stated**

- `gps.rs` `//!` lines 9-19: the puck reader stands down while the connected bridge's receiver is
  sending sentences (`Gps::is_talking`). Keep "lets go of its port and opens nothing, so one
  capture reads one receiver". Replace the "keys on a fix rather than on sentences" reasoning with:
  the rule keys on sentences so the view can follow the bridge's receiver acquiring, and a
  receiver that stops sending lets the search resume after the staleness limit. The reconnect paragraph says "a sentence from the new stream" for "a
  fix from the new stream".
- `gps.rs` `suspended` field doc and `bridged` doc: name `is_talking` for `has_fresh_fix`.
- `runtime.rs` `//!` lines 10-12: "While the connected bridge's receiver is sending sentences …
  it resumes when the last one ages past the limit or the bridge disconnects."
- `position.rs` `//!` lines 9-10: "stands down while that receiver is sending sentences". The
  bridge stays above a searched receiver because, while the search is stood down, it is the only
  receiver being read.
- `AGENTS.md` line 158: "standing down while the bridge's receiver is sending sentences".
- `crates/wartui/README.md` GPS bullet (~806-815): the host's search stands down while the
  bridge's receiver is sending sentences, with or without a fix, so `g` shows it finding
  satellites. The search
  resumes once the last sentence is older than `--gps-max-age`, or the bridge disconnects. After
  a reconnect it stands down again on the first new sentence. `g` reports on the bridge's
  receiver while it is sending sentences. Keep the rest of the bullet.

Search for leftovers: `git grep -n -i -E "fresh fix|has_fresh_fix|has a fix" -- crates AGENTS.md`
should return nothing about the stand-down.

- [ ] **Step 6: Run the whole suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets && cargo fmt --check`
Expected: all pass, no warnings.

- [ ] **Step 7: Commit**

```bash
git add -A crates AGENTS.md
git commit -m "Hold the GPS search while the bridge's receiver is sending sentences"
```

  Body: the search kept running, and `g` kept showing the puck, until the bridge's receiver had a
  fix, so its acquisition could not be watched. The stand-down and the view key on sentences
  instead.
