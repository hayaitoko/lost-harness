# HI2 (HIGH) — `ToolDispatcher` run state survived P14 as a single shared slot

Branch: `fix/hi2-dispatcher-run-state`

## The defect

P14 (M-08) replaced `AgentLoop`'s global `stream_lock: Mutex<()>` with a
per-conversation lock map, deliberately letting different conversations stream
concurrently. That packet is correct in itself.

But production's `AppState.tools` is ONE `Arc<ToolDispatcher>` shared by every
conversation, and its `run_state` (`dispatch_count`, `recent_fingerprints`,
`run_nonce`) was a single mutable slot. Its own doc comments — on the field
(`dispatch.rs` ~176-181) and on `begin_run` (~546-551) — asserted the exact
invariant P14 removed:

> Safe as a single mutable slot because `AgentLoop::stream_lock` serializes
> `process_message` (Q10 single-in-flight) — only one run is ever in flight
> against a given dispatcher. If concurrent runs are ever allowed, this must
> become per-run.

Concurrent runs became allowed. Nobody made it per-run.

Failure traced by the reviewer: conversation A starts a turn, `begin_run()`
stamps nonce N1 and A accrues fingerprints. While A streams, the user selects
conversation B and sends; B takes a *different* conversation lock, runs
concurrently, and calls `begin_run()` — which zeroed `dispatch_count`, cleared
`recent_fingerprints` and re-stamped the nonce to N2 **on A's live run**. If A's
model then re-emitted an identical mutating tool call it had already made (a
known LLM repetition mode), repeat detection no longer suppressed it and the
action executed twice (e.g. the same `send_email`). `dispatch_count` was
likewise shared, mis-accounting per-run ceilings across conversations.

## The fix

`run_state: Mutex<RunState>` → `run_states: Mutex<HashMap<String, RunState>>`,
keyed by conversation id.

**Why keyed-by-conversation rather than a per-run handle/guard.** P14 still
serializes runs *within* one conversation via its per-conversation stream lock,
so "one live run per conversation" is exactly the granularity that holds — a
conversation key IS a run key. Two other reasons decided it:

- The nonce read in `run_journaled` and the budget check in `drive` both already
  have a `ctx.conversation_id` in hand and nothing else that identifies a run.
  Threading a run handle would have meant a new parameter on `dispatch`,
  `run_turn`, `drive` and every reroute re-entry (`deny_and_continue_turn`,
  `resume_after_local_switch`) — a wide, hot-file change for no extra safety.
- It mirrors the lifetime discipline P14 itself established for
  `AgentLoop::stream_locks`: keyed by conversation id, entries reused across
  runs, never accumulating per run. One small entry per conversation that has
  begun a run (a counter, ≤ `PER_RUN_DISPATCH_CEILING` short hex fingerprints,
  and a UUID).

Call sites that had to change: exactly one in production —
`AgentLoop::stream_to_provider` (`agent/loop_mod.rs:1590`) now passes
`&conversation_id`. Plus three pre-existing test call sites in `dispatch.rs`.
`grep -rn "\.begin_run("` confirms there are no others in `src-tauri/src`,
`src-tauri/tests` or `src-tauri/benches`.

Both now-false doc comments were rewritten to state the real invariant (field
doc at `dispatch.rs` ~173-192, `begin_run` doc ~558-568), plus three inline
comments that referenced `run_state` by name.

P14's behaviour is not regressed: nothing here reintroduces cross-conversation
serialization, and the new concurrent test *depends* on two conversations being
in flight against the dispatcher at once.

## Tests added (2)

Both in `src-tauri/src/tools/dispatch.rs`:

1. `concurrent_runs_keep_independent_dedup_state` — the real thing. A new
   `ParkableMutateTool` (`RiskClass::Write`) parks *inside* `Tool::run` on a
   `tokio::sync::Notify`, so conversation A is genuinely in flight while
   conversation B runs. A's single turn emits 4 blocks: `mutate_thing {"x":1}`
   twice (filling its fingerprint ring), then a parked call, then `{"x":1}`
   again. While A is parked, B calls `begin_run("conv-B")` and dispatches its
   own two calls, then releases A. Driven with `tokio::join!`. Asserts:
   - A's 4th block is `denied by budget` with `repeat detected — same call,
     same args`;
   - the duplicate mutating action executed **exactly twice**, never a third
     time (counted from the tool's own execution log, not from feedback text);
   - `dispatch_count` is 3 for conv-A and 2 for conv-B — two numbers one shared
     slot could not hold;
   - A's `run_nonce` is byte-identical to the value captured before B started,
     and B's differs;
   - B's own two calls both ran (B is unaffected by A).

2. `begin_run_in_one_conversation_leaves_another_conversations_budget_alone` —
   the sequential, deterministic sibling: conv-A spends 3 dispatches, conv-B
   calls `begin_run`, conv-A's count is still 3 and conv-B's is 0.

Two `#[cfg(test)]` accessors were added next to `begin_run`
(`run_dispatch_count`, `run_nonce_of`) — the counter is otherwise invisible
until a ceiling denial fires 50 dispatches in.

## Mutation test (run, not asserted)

Restored the shared-slot behaviour by collapsing every key to a constant
`"SHARED"` in `begin_run`, in `drive`'s budget block, **and in both test
accessors** (so they read the shared slot rather than trivially returning 0).

- `concurrent_runs_keep_independent_dedup_state` → **FAILED**, on the
  load-bearing assertion: `A's 4th call (a duplicate re-emitted after B began
  its run) must still be denied; got: [tool mutate_thing → ok]` — i.e. the
  mutation reproduces the exact double-execution the reviewer described.
- `begin_run_in_one_conversation_leaves_another_conversations_budget_alone` →
  **FAILED**: `conv-B's begin_run must not zero conv-A's budget, left: 0,
  right: 3`.

Mutation reverted via `git checkout --`; the restored file was byte-compared
against a copy taken outside the repo before mutating (`diff -q` → identical),
and `git status --porcelain` was empty.

## Verification

- `cargo test --lib` → **908 passed / 0 failed / 1 ignored** (integrated
  baseline 906 / 0 / 1; +2 = the two tests above).
- `cargo clippy --lib --tests` → 0 errors; no new warnings in the touched
  ranges (the three `dispatch.rs` clippy hits are at lines 377, 378, 769 —
  all pre-existing).
- `rustfmt --edition 2021 --check src/tools/dispatch.rs src/agent/loop_mod.rs`
  → clean.

## Provenance note

The worktree arrived with 2 uncommitted files from an agent that died
mid-flight (`tools/dispatch.rs`, `agent/loop_mod.rs`). I inspected the diff,
found it already implemented the keyed-by-conversation design correctly and had
updated the false doc comments, and **built on it** rather than discarding it.
It had no test, which is what this packet added. The inherited bytes were
committed as a checkpoint (`188873b`) before any further work.

## Files changed

- `src-tauri/src/tools/dispatch.rs`
- `src-tauri/src/agent/loop_mod.rs` (one call site)
- `review-fixes/progress/HI2.md` (this note)
