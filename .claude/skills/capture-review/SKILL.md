---
name: capture-review
description: Review a finished (or still-running) wartui capture database, optionally with its --log-file, to find oddities, host-software bugs, firmware issues and improvements to make. Use this whenever the user points at a capture `.db` (e.g. `wartui-2026-10-06-*.db`, `brz-bt.db`), a drive, a run, or a wartui log and asks what happened, what went wrong, why something was lost or missing, whether anything looks off, or what to fix or improve — even if they only say "look at last night's drive" or "anything weird in this capture?".
---

# Capture review

A capture is one run of `wartui run`: a SQLite file the store wrote (schema in
`crates/wartui-core/src/store.rs`, `SCHEMA`), and sometimes a `--log-file` from the same run. The
review turns those into a short list of findings, each backed by numbers from the capture and a
cause traced to the code. The user drives these runs to improve the fleet, so the value is in
findings they can act on: a bug to fix, a firmware change worth a reflash, instrumentation the
next capture needs, or a reason to stop worrying about something.

## 1. Inputs

- **Database** (required). If none is named, list `*.db` in the repo root and ask which one
  when there is more than one. Captures live there with the `.log`/`.csv` from the same run.
- **Log** (optional). A `--log-file` written in append mode can hold several runs; the survey
  reports that, and only lines inside the capture window belong to this review.
- Never write to the capture. It may still be recording, and it is the system of record.
  Scratch files (exports, scripts) go in a scratch directory, never the repo.

## 2. Gather the facts

Run both, from the repo root:

```sh
python3 -I .claude/skills/capture-review/scripts/survey.py <db> [--log <log>] [--node 57:84]
cargo run --release -q -p wartui -- analyze --db <db> --heartbeat-windows
```

The survey opens the file read-only, tolerates schemas from other builds, and ends with a
`FLAGS` list, each with the SQL that reproduces it. `wartui analyze` is the project's own loss
accounting; if it refuses the file (another build's schema fingerprint), say so and carry on
with the survey — a refusal is expected for older captures, not a finding.

The survey is a starting point, not the review. Its thresholds are blunt: read every section, not
just the flags, and look for what it does not check (a node whose numbers are unlike its
siblings', a figure that moves at the same minute as another).

Then look at what the capture produced for the user, which the survey does not open:

- **The export.** Run `wartui export --db <db> -o <scratch dir>/export.csv`. Always pass `-o`:
  without it the CSV is written beside the capture, which puts it in the repo. Read a few dozen
  rows. Check the formatting of each column (precision, empty fields, channel and
  frequency, odd SSIDs), and that the row count matches `analyze`. A capture is only useful once
  exported, so a defect here reaches WiGLE and WDGWars.
- **Uploads.** The survey lists the `upload` rows. When the uploaded row count differs from
  today's export, explain the difference, because the build that uploaded may have exported
  differently. Report and leave it: never upload, retry or query the site.
- **Code paths behind a finding.** When a finding points at a function, read it. A discarded
  return value, an unchecked status field or a column that is never written is often the real
  finding.

## 3. Triage each lead

For every flag or anomaly worth mentioning:

1. **Confirm it in the data.** Write the follow-up SQL — when did it happen, which node, does it
   coincide with a bridge reboot, a GPS gap, a host stall, a settings change, a speed? Compare
   the odd node against its siblings in the same minutes; a fault on one node during a window when
   the others were fine points at that node, while all nodes at once points at the bridge, host or
   environment. For per-node loss, compare at equal link RSSI (the survey's `miss by rssi`), not
   just overall. Compare across the speed and place of the drive too, and against the node's share
   of channels, because the node holding channel 6 shares the control channel with every
   heartbeat. If the user has other captures in the repo root, check whether the same node misbehaves
   in them as well. A pattern that repeats across drives is worth more than one capture's numbers.
2. **Read the code that owns it** before naming a cause. `references/checks.md` maps each symptom
   to the module and findings doc to read; AGENTS.md § "Invariants that are easy to break" says
   which behaviours are deliberate. Cite `path:line`.
3. **Classify** it as one of:
   - *environment* — RF, distance, mounting, GPS sky view, power supply
   - *host software* — fixable with a `cargo run`
   - *firmware* — fixable only with a reflash (say whether the bridge, the nodes, or both)
   - *configuration* — an operator choice or a remembered setting
   - *expected* — the design working as intended; say which invariant, so it stops being a worry
4. **Rate confidence**: *confirmed* (data and code agree), *likely*, or *hypothesis* (say what
   capture would test it).

Answer loss questions from the capture, never by proposing a bench test: loss comes from busy
air while driving, which a desk does not reproduce. Figures that are estimated rather than counted
get a `~` and "inferred from timing" (or "from counters").

### Not findings

Drop these, they are settled policy:

- Schema version bumps, migrations or compatibility shims for older captures or firmware — nothing
  is compatible before 1.0, and the fix for another build's file is a new `--db`.
- Safeguards for more than twenty nodes.
- `--lat/--lon` as a GPS fallback. It is test-only, and it marks a capture as test data.
- Uploading. A review never runs `wartui upload` or touches the WDGWars endpoints.

## 4. Report

Reply in chat with this structure. Keep sentences short and use the boards' last two octets
(`57:84`) as the operator does.

```markdown
## Capture
One paragraph: when and how long, the nodes and their jobs, the bridge, the build, and anything
that limits this review (no log, `record_raw=false`, still running, test data).

## Findings
| # | Category | Finding | Evidence | Likely cause | Confidence |
|---|----------|---------|----------|--------------|------------|
Most severe first. Evidence is numbers from this capture. Likely cause cites path:line.

Then a short paragraph under any finding that needs more than a table cell: the timeline, the
comparison with sibling nodes, why the cause is believed.

## Improvements
Grouped as host (cargo run), firmware (reflash), and instrumentation (what this capture could
not answer and which column or log line would answer it next time).

## Next capture
What to record or change next drive to settle the open hypotheses.

## Reproduce
The survey and analyze commands, and the key SQL from above.
```

Some captures will be clean. Say so plainly and keep the report short. Do not pad it with
low-value flags. A finding that is *expected* belongs in the table only if the user is likely to
worry about it.
