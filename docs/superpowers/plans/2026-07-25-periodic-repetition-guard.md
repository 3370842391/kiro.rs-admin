# Periodic Repetition Guard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop bounded multi-line output cycles and suppress exact duplicate long Thinking deltas across both upstream event sources without changing normal SSE or tool behavior.

**Architecture:** Extend `StreamContext` with one bounded line-cycle detector and one bounded exact-event fingerprint cache. Both filters run immediately before their existing text/thinking SSE exits, so terminal handling and tool state remain centralized.

**Tech Stack:** Rust, existing `StreamContext` unit-test module, Anthropic SSE state machine.

---

### Task 1: Reproduce periodic cycles

**Files:**
- Modify: `src/anthropic/stream.rs`
- Test: `src/anthropic/stream.rs`

- [x] Add `repeat_guard_trips_on_four_line_cycle_across_chunks`, feeding the four screenshot-style lines for at least eight cycles and asserting `repetition_guard_tripped()` plus an `upstream_repetition_guard` event.
- [x] Add `repeat_guard_preserves_short_cycle_and_fenced_code`, asserting three cycles and repeated lines inside a Markdown fence remain intact.
- [x] Run `cargo test repeat_guard_trips_on_four_line_cycle_across_chunks -- --exact --nocapture` and confirm the first test fails because the current guard resets on every alternating line.

### Task 2: Implement bounded periodic detection

**Files:**
- Modify: `src/anthropic/stream.rs`

- [x] Add constants for maximum period 8, maximum history 32, minimum repeated lines 16, and minimum cycles 4.
- [x] Add a private detector that buffers only complete lines, resets at channel/fence/oversize boundaries, and tests exact suffix periodicity.
- [x] Integrate it into `repeat_guard_filter` without changing the existing period-one thresholds or terminal event generation.
- [x] Run `cargo test repeat_guard_ -- --nocapture` and confirm all repetition and non-regression tests pass.

### Task 3: Reproduce and suppress duplicate long Thinking across sources

**Files:**
- Modify: `src/anthropic/stream.rs`
- Test: `src/anthropic/stream.rs`

- [x] Add `native_reasoning_drops_exact_duplicate_long_event` with two identical events longer than 256 bytes and assert the visible Thinking appears once.
- [x] Add `native_reasoning_keeps_short_and_distinct_events` and assert short identical fragments plus different long fragments are preserved.
- [x] Add `long_reasoning_dedup_applies_across_assistant_and_native_sources` and confirm it first outputs two copies before implementation.
- [x] Add a bounded exact fingerprint cache at the unified Thinking output; log only duplicate byte length and return no SSE delta for exact matches.
- [x] Reset fingerprint and periodic state at automatic-continuation upstream boundaries.
- [x] Run the native, cross-source, and continuation-boundary tests and confirm they pass.

### Task 4: Verify and review

**Files:**
- Modify: `src/anthropic/stream.rs`
- Modify: `docs/superpowers/specs/2026-07-25-periodic-repetition-guard-design.md`

- [x] Run `rustfmt --edition 2024 --check src/anthropic/stream.rs`.
- [x] Run `cargo test` and require all probe and main tests to pass.
- [x] Run `git diff --check` and inspect `git diff --stat` plus the complete source diff.
- [x] Confirm only the stream guard, its tests, and these design documents changed.
