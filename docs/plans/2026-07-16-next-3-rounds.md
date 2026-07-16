# Plan — Next 3 rounds (2026-07-16)

Executing the next three unblocked roadmap rounds in recommended order. Item 4
(native tool-use, Q1) is **skipped** — it's blocked on configuring a
native-tool-capable model endpoint, per ROADMAP "Blocked / waiting."

Baseline: `cargo test --lib` → **342 passing**, tree clean. Each round is
plan → implement (small commits) → adversarial review → QA → commit, and updates
HANDOFF.md + ROADMAP.md.

---

## Round 1 — Per-profile classifier thresholds (finishes item 3 (b))

**Problem.** Classifier strictness is hardcoded (`TAU_BLOCK=0.5`, `TAU_BAND=0.05`,
`classifier/engine.rs:41-42`) on a single global `Arc<dyn Classifier>`. Users
can't tune how paranoid the privacy filter is. The frontend already has the
controls (`ClassifierControls.svelte`, Settings "Privacy guard"/"Routing"
sections) but they're 100% mocked local `$state` with no persistence or IPC.

**Non-goals.** Redaction on/off + hard-block category toggles (they're inert
until Round 2 builds redaction — shipping dead toggles is a smell; deferred).
Per-profile *classifier instances* (the one shared instance stays; config is
threaded per-call). `WINDOW`/`STRIDE` stay const.

**Design (chosen).** Thread a small `ClassifierConfig { tau_block, tau_band }`
into the classify decision per-call, keyed by the active profile:
- New `ClassifierConfig` (in `classifier/mod.rs`) with `Default` = today's
  consts. Validated/clamped on construction (0<band≤block≤1).
- New trait method `Classifier::classify_with(&self, text, cfg) -> Classification`
  with a **default impl that calls `classify(text)`** (back-compat: RulesClassifier
  and the stub inherit it — rules ignore thresholds, they always fire). The
  ensemble overrides `classify_with` to use `cfg.tau_block/tau_band`; its
  `classify(text)` delegates to `classify_with(text, &Default)`. So the consts
  become the default, nothing else changes for callers that don't pass a config.
- `PrivacyGate::check` gains a `cfg: &ClassifierConfig` param → passes to
  `classify_with`. (`Public`/`Private` bindings still bypass the classifier.)
- `AgentLoop::process_message` loads the profile's config from per-profile
  storage and passes it to the gate. `explain_classification` IPC gains a
  `profile` arg and does the same, so the "why" sidebar reflects the profile's
  real strictness. Memory sensitivity routing (`save_memory`/`remember`) also
  uses the profile config for consistency ("the classifier behaves the same
  everywhere for a profile").

**Storage.** New per-profile table `classifier_settings` (single row,
`id INTEGER PRIMARY KEY CHECK(id=1)`, `tau_block REAL`, `tau_band REAL`,
`updated_at`), mirroring the `tool_rules` per-profile pattern. Migration v4 on
the **profile** DB + add to `PROFILE_SCHEMA_SQL` + bump `PROFILE_SCHEMA_VERSION`
3→4. A missing row = defaults.

**IPC.** `get_classifier_settings{profile} -> ClassifierSettingsInfo`,
`set_classifier_settings{profile, strictness, uncertainty_band}`,
`reset_classifier_settings{profile}`. Register in `lib.rs` handler list. Add
`tauri.ts` wrappers.

**UI mapping (intuitive → thresholds).** The UI speaks *strictness* (0–100) and
*uncertainty band* (narrow/medium/wide), not raw taus:
- `strictness s∈[0,100] → tau_block = lerp(0.85 … 0.15)` (higher strictness ⇒
  lower block threshold ⇒ more flagged Private). Default 0.5 ≈ s≈50.
- band narrow/medium/wide → `tau_band ∈ {0.15, 0.05, 0.01}` (wider band ⇒ lower
  tau_band ⇒ more borderline text caught as Uncertain→local). Default medium=0.05.
- Pure functions `strictness_to_tau_block` / `band_to_tau_band` (+ inverses for
  display) live in Rust (the source of truth), unit-tested at the boundaries.
Wire the existing Settings "Privacy guard" section to real load/save via a
`$effect` keyed on `$activeProfileId` (Permissions/Memory tab pattern).

**Failure modes.** Corrupt/out-of-range stored taus → clamp to valid + default
on parse fail (fail toward *stricter*, never looser). Config load error in
`process_message` → fall back to `Default` (never block the send path on a
settings read). Threshold change never weakens the rules-layer floor or the
`Private`-binding block.

**Test plan.** Unit: `ClassifierConfig` clamp; strictness/band mapping endpoints;
ensemble `classify_with` flips a borderline score across a tuned threshold (env
ONNX test) + a pure-logic test on the fusion given an injected score; storage
round-trip + default-on-missing; IPC arg shape (contract test). QA: browser
preview — change strictness in Settings, confirm persistence across reopen +
profile switch.

---

## Round 2 — Partial-delegation redact-and-send (finishes item 3 (a))

**Problem.** When Auto binding + cloud endpoint sees Private/Uncertain text, the
whole turn routes local (all-or-nothing). PLAN §11 wants: redact the sensitive
spans, send the safe remainder to cloud, rehydrate the reply — non-lossy privacy.

**Hard constraint (from recon).** Spans exist **only** from the rules layer.
Model-only detections (health/semantic) return **empty spans** — nothing to
locate/redact. So redact-and-send is attempted **only when redactable spans
exist**; otherwise the existing route-local/block behavior is unchanged. This is
honest and safe.

**Design.**
- Surface the classification: add `PrivacyGate::check_detailed(...) ->
  (GateDecision, Option<Classification>)` (or return the `Classification`
  alongside), so `process_message` can see the spans. Keep `check` as a thin
  wrapper for existing callers/tests.
- `redact(text, spans) -> (redacted, Vec<Redaction>)`: replace each span using
  its **`Span.text`/byte offsets** with `[REDACTED:CATEGORY:n]` placeholders
  (deterministic, reversible). New `classifier/redact.rs`, heavily unit-tested
  (overlapping spans, multibyte, adjacent).
- **Re-classify the redacted text and fail closed**: only send to cloud if the
  redacted text now classifies clean (Public / no spans) under the profile
  config. If still dirty → route local (today's path). This is the load-bearing
  safety check — never trust redaction blindly.
- New gate outcome path: when redaction enabled (profile toggle, added here) +
  cloud + redactable + re-classify clean → send redacted text to the **original
  cloud provider**; persist the **original** text locally (transcript fidelity),
  send only redacted text on the wire.
- **Rehydration**: keep the placeholder→original map for the turn; after the
  reply streams, substitute placeholders back in the persisted/displayed
  assistant text. (Model sees placeholders; user sees originals.)
- **Non-silent**: emit a `privacy:redacted` event + render a `PrivacyEventBar`
  "redacted N items, sent the rest" with a link into the why-sidebar. Wire
  `WhyPanel.svelte`'s existing kept/sent two-column view (currently unused).
- Round-1's deferred **redaction on/off toggle** becomes live here (governs
  whether this path is attempted); default **on**.

**Non-goals.** Redacting model-only (span-less) detections — needs token-level
attribution the binary ensemble can't give (documented boundary). Streaming
rehydration mid-token (rehydrate on the assembled final text is enough for v1).

**Safety review focus.** Private span bytes must never reach the wire; the
re-classify-clean gate must be un-bypassable; fail closed on any redaction/
re-classify error → route local. This round gets the heaviest adversarial review.

**Test plan.** `redact()` unit battery; end-to-end: a message with an email +
SSN under Auto+cloud+redaction-on → asserts the wire payload is redacted, the
re-classify gate passed, the persisted user text is original, the reply is
rehydrated; a message with model-only Private (empty spans) → still routes local;
redaction-off → routes local. QA: browser preview with a seeded classifier.

---

## Round 3 — Memory made live in conversations (item 5 remaining)

**Problem.** `curated_summary()` and the memory stores are built + tested but
**nothing injects memory into a live turn**. `ExecCtx` can't tell a tool whether
the turn is cloud or local, so `recall_memory` hardcodes `allow_private=false`.
No memory events surface.

**Design.**
- **Endpoint-aware ExecCtx**: add `is_cloud: bool` to `ExecCtx`
  (`tools/mod.rs`), set at construction (`loop_mod.rs:351`) and on the
  dispatcher's `run_ctx` rebuild (`dispatch.rs:~425`), kept in lockstep with
  mid-turn reroutes. `recall_memory` then passes `allow_private = !ctx.is_cloud`
  (local turns may read private facts by design; stays `RiskClass::Safe`).
- **Curated summary at conversation start**: in `stream_to_provider`, when the
  prior history is empty (turn 1), inject `curated_summary(&profile,
  allow_private=!is_cloud, LIMIT)` as a guard-wrapped system message after the
  tool catalog. Not re-churned mid-conversation (prompt-cache stable).
- **Automatic relevance-gated injection**: after the gate resolves
  `is_cloud`/`content`, run a cheap **FTS keyword** `search_memory(content,
  allow_private=!is_cloud, SMALL_LIMIT)` scoped to the active profile; inject
  ≤1–3 snippets **only if** a hit clears a relevance bar. Guard-wrapped as
  untrusted (same framing as tool output). Most turns inject nothing. **Meaning
  lane (embedder) deferred** — keyword-only for v1, documented.
- **Non-silent memory events**: emit `memory:event {kind: "recalled"|"remembered",
  summary, count, ids}`; add a `tauri.ts` listener; widen `PrivacyEventBar` kind
  to include a `"memory"` tone (neutral/info) and render "recalled N notes for
  this answer" / "remembered: …" inline, clickable. Recall injection fires
  `recalled`; the `remember` tool save fires `remembered`.
- **Profile scoping fix**: `search_memory` gains a `profile` filter for the
  automatic-injection path (recon flagged it currently searches all profiles'
  shared facts).

**Non-goals.** The sqlite-vec meaning lane (needs a local embedder — separate
decision); walled-profile per-profile memory DB routing (bigger, its own round);
pre-compaction/new-chat write triggers (separate agent-loop work). Replacing the
`remember` approval modal with an inline-only save (UX decision — keep the
approval, add the event on top).

**Safety.** Injected memory is untrusted → guard-wrapped so it can't forge tool
calls / instructions. Cloud turns **never** query the private-local store
(`allow_private=false` structurally). Injection runs under the `stream_lock` so
it must stay cheap (FTS only, capped).

**Test plan.** ExecCtx endpoint-kind threading (local turn → private visible,
cloud turn → private excluded); curated-summary injected only on turn 1 +
endpoint-gated; relevance-gated injection caps at 1–3 + guard-wrapped + skips
below bar; `memory:event` emitted on recall + remember. QA: browser preview —
seed facts, watch the recall event bar render.

---

## Cross-cutting

- **Invariants preserved every round**: privacy filter fails closed; sandbox
  floor non-overridable; parse-only-own-output; audit logs never store
  plaintext; cloud turns never touch private-local memory.
- After each round: adversarial multi-agent review (correctness + security +
  plan-conformance lenses, verify each finding against code before fixing),
  browser QA for UI, then commit + update HANDOFF/ROADMAP.
- Models: Opus authors the security-critical edits (gate/redaction/loop) and
  synthesizes reviews; Sonnet handles recon, review lenses, and mechanical QA.
