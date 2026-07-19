# M6 — Voice: design doc (Wave 5, flagship 5.2)

> **STATUS: design-pass draft (2026-07-18). Skeptical review verdict: NEEDS-REVISION.** Read the **Design Review** at the bottom before building — it flags concrete architecture gaps to fold in during the build phase.


**Status:** design pass, not built. First deliverable per BUILD-MANIFEST Wave 5 ("DESIGN
PASS FIRST, then build"). Read alongside `docs/PLAN.md` §6 (voice as first-class modality,
"opposite polarity — on-device by default, barge-in as a real latency requirement") + §8 M6
+ §12, and `docs/BUILD-MANIFEST.md` row 5.2. Existing code is a stub:
`src-tauri/src/audio/mod.rs:1` (`// M6: Audio engine, VAD, TTS pipeline`).

Every "add" below cites where it plugs into the existing spine.

---

## 1. Goal / scope / non-goals

**Goal.** A hands-free spoken loop — *speak → agent → hear* — that runs **fully on-device by
default**, streams the reply as it's spoken, and lets the user **interrupt (barge-in)** mid-reply
with sub-200ms latency. Voice is a **mode**, not a second agent: the transcript is just `content`
handed to the existing `AgentLoop::process_message` (`src-tauri/src/agent/loop_mod.rs:290`), so it
inherits the whole privacy/routing/hook spine unchanged.

**Scope.**
- Cross-platform mic capture + speaker playback (macOS/Windows/Linux).
- VAD + endpointing; local STT (default); local TTS with streaming playback (default).
- Barge-in: full-duplex VAD-during-playback with echo cancellation; cancel model stream + TTS on speech onset.
- The **audio-egress privacy check** — the NEW invariant (§3): raw audio → cloud STT, and reply
  text → cloud TTS, are *independent egresses the text gate never evaluated*, gated with a floor
  stricter than typed egress.
- A per-profile **settings toggle** (default OFF; first-enable downloads voice models).

**Non-goals (v1).** Wake-word always-on listening (opt-in later — see OQ-1); multi-speaker
diarization; voice cloning; a voice-driven computer-use fusion (that's 5.1/M5); phone/telephony
(that's the separate SiteWright/Hermes line, not this product). Translation/cross-lingual voice is
gated behind OQ-3.

---

## 2. Architecture — how it slots into the spine

### 2.1 The loop (data flow)

```
mic ─▶ capture ─▶ [AEC] ─▶ VAD/endpoint ─▶ STT(local) ─▶ transcript:String
                    ▲                                          │
                    │                                          ▼
              playback queue ◀─ TTS(local) ◀─ sentence      process_message(content=transcript, …)
                    │            aggregator      ▲            (UNCHANGED agent loop)
                    │                            │                    │ ResultSink::token(delta)
                    └──── barge-in: duck+stop ───┘◀───────────────────┘  via VoiceResultSink (tees tokens)
```

The two seams that make this "a mode, not a fork":

1. **Input seam = `content`.** STT emits a `String`; it is passed verbatim to
   `process_message(content, conversation_id, binding, provider_id, model, profile, session_mode,
   sink)` (`loop_mod.rs:290`). The existing `PrivacyGate::check_detailed` at `loop_mod.rs:324-326`
   gates model routing on that transcript **exactly as if typed** — Private/Uncertain → RouteLocal,
   Private binding + cloud → Block. No new input-side gate for the *model* path; voice reuses it.

2. **Output seam = `ResultSink`.** The loop already streams through the `ResultSink` trait
   (`src-tauri/src/agent/result_sink.rs:31`), built precisely so `process_message` isn't welded to a
   live `AppHandle` (Wave 4.3c). Voice adds a **third impl** alongside `TauriResultSink` (:62) and
   `HeadlessSink` (:123): a `VoiceResultSink` decorator that forwards every call to the inner
   `TauriResultSink` **and** tees `token()` (:33) into a sentence aggregator → TTS. `send_message`
   already constructs the sink at one site (`src-tauri/src/ipc/mod.rs:450`) — the voice session
   constructs `VoiceResultSink` there-equivalently. **No change to `process_message`'s signature.**

### 2.2 New module layout — `src-tauri/src/audio/`

| file | contents |
|---|---|
| `mod.rs` | public surface, `VoiceConfig` (toggle state), `VoiceSession` orchestrator + state machine |
| `capture.rs` | `trait AudioInput` + per-OS backend (via `cpal`); PCM ring buffer |
| `playback.rs` | `trait AudioOutput` + duck/flush/stop; playback queue |
| `aec.rs` | acoustic echo cancellation so TTS output never self-triggers the VAD |
| `vad.rs` | voice-activity detection + endpointing; the barge-in trigger |
| `stt.rs` | `trait SpeechToText`; `LocalWhisper` (default) + `CloudStt` (opt-in) |
| `tts.rs` | `trait TextToSpeech`; `LocalTts` (default) + `CloudTts` (opt-in); sentence aggregator |
| `privacy.rs` | `AudioEgressGate` — the NEW invariant; reuses `agent::gate::PrivacyGate` |
| `sink.rs` | `VoiceResultSink` (decorates `TauriResultSink`, tees tokens into `tts`) |

Traits so backends are swappable and testable: `AudioInput`/`AudioOutput`/`SpeechToText`/
`TextToSpeech` each have a real backend + a fake for unit tests (mirrors how `Classifier` at
`classifier/mod.rs:224` has trained + heuristic impls behind one trait).

### 2.3 Capability + RiskClass + hook-chain fit

- **Capability.** `Capability::Audio` **already exists** (`src-tauri/src/tools/mod.rs:59`, *"A
  microphone/speaker exists"*) and is **already in `BodyEnv::app_default()`** (mod.rs:112) and
  **absent from `headless_server_default()`** (mod.rs:120-129). So the modality is auto-present in
  the app and auto-absent on the headless twin — no new capability needed. Any voice-exposed *tool*
  (e.g. an optional `speak` tool the agent could call) declares `requires() -> &[Capability::Audio]`
  (`mod.rs:326`) and is filtered out of headless environments for free (mod.rs:331 `available`).

- **RiskClass** (`tools/mod.rs:252`). The pipeline maps cleanly onto the taxonomy:
  - Mic capture, VAD, **local** STT, **local** TTS/playback → `Safe` (on-device, no egress).
  - **Cloud** STT (raw audio egress) and **Cloud** TTS (reply-text egress) → `External` — they
    "reach beyond this machine" (mod.rs:257). This is what routes them through the egress checks.

- **Hook chain.** Voice does not bypass the `PreToolUse` chain
  (`[PrivacyFilter, Sandbox, ProtectedPath, SessionMode, Permission, FirstUse]`,
  `hooks/mod.rs:530`). Two integration points:
  - Tool calls the agent makes *during* a voice turn already flow through the chain unchanged
    (voice changed only how the turn started/ended, not the loop body).
  - The **cloud STT/TTS egress** is gated by `AudioEgressGate` (§3), which *wraps*
    `PrivacyGate::check` exactly the way `PrivacyFilterHook` (`hooks/privacy_filter.rs:31`) wraps it
    for the tool chain — same primitive, new boundary.

### 2.4 Barge-in / interrupt model

Barge-in is the load-bearing latency requirement (PLAN §6). Mechanism:

- The VAD runs **continuously, including during TTS playback** (full-duplex). `aec.rs` subtracts the
  known playback signal so the assistant's own voice doesn't register as user speech (the single
  hardest cross-platform bit — see §4).
- On **speech onset** the `VoiceSession` state machine (`Idle→Listening→Thinking→Speaking`) does, in
  order, within a **≤200ms** budget:
  1. **Duck then stop** playback + flush the device buffer (`AudioOutput::stop`, ~≤50ms).
  2. **Abort the in-flight turn.** The session runs `process_message` as a spawned task and holds its
     `JoinHandle`; barge-in calls `handle.abort()`. Dropping the future drops the `reqwest` byte
     stream → cancels the model HTTP call. **No change to `process_message` needed** — abort-on-drop
     is the cancellation. (Verify the stream_lock at `loop_mod.rs:305` releases on drop — it does,
     it's a guard.)
  3. **Persist the spoken-so-far text** as the assistant turn + an `[interrupted]` marker (OQ-4), so
     the conversation history matches what the user actually heard — not the full ungenerated reply.
  4. Transition `Speaking→Listening` and open capture for the new utterance.

Latency budget (design targets, measured in Slice 4's gate): VAD onset ≤150ms · playback stop
≤50ms · **barge-in-to-silence < 200ms** · endpoint→first-audio-out < ~1.5s (Slice 3 gate).

---

## 3. The flagship-specific privacy invariant (NEW)

PLAN §6 calls out one new invariant per flagship. For M6 it is the **audio-egress check**
("withhold sensitive audio from cloud TTS without confirmation"). Stated precisely:

> **Voice egress ≤ text egress, never more — and cloud STT/TTS carries a stricter floor than typed
> egress, because raw audio is unredactable and a microphone captures non-consenting bystanders.**

**Why a new gate is needed at all** (the key insight): the model-routing gate at
`loop_mod.rs:324` decides *which model* sees the transcript. It says nothing about **TTS**. When the
reply text is voiced by a **cloud** TTS service, that is a **second, independent egress of that text
that no gate ever evaluated** — the reply already exists; sending it to `api.some-tts.com` is a fresh
network boundary. Symmetrically, **cloud STT ships the raw waveform *before* any transcription**, so
the text classifier can't even run on it (you can't redact a waveform, and it may contain a
bystander's voice). These two boundaries are exactly what `AudioEgressGate` owns.

### 3.1 `AudioEgressGate` (in `audio/privacy.rs`)

Reuses the existing primitives — no reimplementation:
- `PrivacyGate::check_detailed(binding, text, is_cloud, cfg) -> (GateDecision, Option<Classification>)`
  (`agent/gate.rs:97`) — returns the label **and** the detected `spans` (`classifier/mod.rs:53`).
- `agent::egress::is_private_endpoint(base_url)` (`egress.rs:24`) — the *same* source of truth for
  "is this endpoint cloud" that the model path uses. The TTS/STT provider `base_url` runs through it.
- `classifier::{redact, Redaction}` (`classifier/mod.rs:21`) — used to black out spans before local
  fallback (mirrors the model path's `plan_redaction`, `loop_mod.rs:349`).

**TTS egress** — `check_tts_egress(chunk, binding, tts_is_cloud)` per sentence chunk before it's
handed to a TTS engine:

| binding | local TTS | cloud TTS |
|---|---|---|
| `Private` | Allow (no egress) | **Block** — voice with local TTS or stay silent (mirrors gate.rs:113-120) |
| `Auto` | Allow | classify chunk: Public→Allow; **Private/Uncertain→withhold from cloud, speak the chunk with LOCAL TTS** (the `RouteLocal` analog, gate.rs:129-135). Emit non-silent `voice:privacy_withheld`. |
| `Public` | Allow | Allow — **but** if the chunk trips the un-tunable rules floor (SSN/keys/email, always fires per `privacy_filter.rs:52`) → **one confirm** ("about to voice sensitive content through a cloud service"). This is the "without confirmation" teeth. |

**STT egress** — `check_stt_egress(binding, stt_is_cloud)`: raw audio is unclassifiable pre-transcription,
so the floor is *stricter than text*. Default v1: **cloud STT permitted only under `Public` binding or
an explicit per-session opt-in; `Auto`/`Private` force local STT** (OQ-2 asks whether we ship cloud
STT at all). No silent cloud STT ever.

### 3.2 Composition with the text gate (no double-gating, no gaps)

- **Model routing** of the transcript: unchanged, the existing gate at `loop_mod.rs:324` owns it.
- **TTS egress** of the reply: `AudioEgressGate`, *additive* — it re-vets the reply text for the
  TTS boundary that the model gate never saw. Local TTS (default) → always no-op Allow, so the common
  offline case pays nothing.
- **STT egress** of raw audio: `AudioEgressGate`, the only place that can gate a pre-transcription
  waveform. Local STT (default) → no-op Allow.
- **Invariant enforced structurally:** a `Private`-binding conversation is *exactly as local spoken
  as typed*; a chunk withheld from cloud TTS uses the same label/polarity as the text gate; the
  non-silent `voice:privacy_withheld` event mirrors `stream:local_reroute`
  (`result_sink.rs:39`) so the withhold is visible, never silent.

---

## 4. Cross-platform strategy

Portability via `cpal` for capture/playback; **AEC is the platform-divergent hard part** and is where
the effort goes.

| | capture/playback | AEC + VAD | mic permission |
|---|---|---|---|
| **macOS** | Core Audio / AUHAL (or `cpal`). Prior art: the Live-Translate app used Core Audio taps + whisper large-v3. | **Voice Processing IO audio unit** (`kAudioUnitSubType_VoiceProcessingIO`) gives AEC+AGC+VAD *natively* — biggest single win; prefer it over a userspace AEC on mac. | `NSMicrophoneUsageDescription` in Info.plist + `AVCaptureDevice` authorization (TCC prompt). Wire like M5's macOS accessibility permission flow (PLAN §8 M5). |
| **Windows** | WASAPI (shared-mode capture + render). | Voice Capture DMO / Communications APO for AEC; else userspace `webrtc-audio-processing`. | Windows mic privacy capability (no per-call prompt); surface a "mic blocked in Windows settings" state. |
| **Linux** | PipeWire preferred, else PulseAudio/ALSA (all via `cpal`). | PipeWire `echo-cancel` module, else `webrtc-audio-processing` APM. | `xdg-desktop-portal` device access. |

- **STT engine:** `whisper-rs` (whisper.cpp bindings — Metal/CUDA/CPU) OR an `ort`-based whisper. For
  a low-latency *assistant* prefer **distil-whisper / whisper base-small**, NOT large-v3 (large-v3 is
  the Live-Translate translate-mode model and is too slow for barge-in-grade turns — see OQ-3).
- **TTS engine:** **piper** (ONNX voices) is the natural default — it runs on the **`ort` runtime
  already vendored for the classifier** (`Cargo.toml:52`), so no new inference stack. Kokoro is an
  alternate.
- **AEC fallback:** where native AEC is unavailable, `webrtc-audio-processing` (APM) is the common
  userspace path across all three OSes.

---

## 5. Build-slice plan (each committable, each with a gate)

**Slice 1 — Audio I/O + capability/feature plumbing.** `AudioInput`/`AudioOutput` traits + `cpal`
backends (macOS first). Add a `voice` cargo feature (§6). `Capability::Audio` already wired — add a
`voice_probe` IPC reporting device + permission availability.
*Gate:* mic→speaker loopback echoes on macOS; `--no-default-features` still builds; unit test that a
`requires([Audio])` tool is absent from `headless_server_default()` (mod.rs:120).

**Slice 2 — VAD + endpointing + local STT.** capture → `aec` (mac VPIO) → `vad` segmentation →
`LocalWhisper` → transcript.
*Gate:* speak a sentence → correct `voice:transcript_final`; endpoint fires within ~500ms of trailing
silence; a fake `SpeechToText` drives deterministic unit tests.

**Slice 3 — STT→agent→local-TTS loop + streaming playback.** `VoiceResultSink` decorates
`TauriResultSink` (`result_sink.rs:62`), tees `token()` into the sentence aggregator → `LocalTts` →
playback queue. Feed transcript to `process_message` (loop_mod.rs:290) via the voice session.
*Gate:* full spoken round-trip; first audio out < ~1.5s after endpoint; **integration test: a
`Private`-binding voice turn stays on the local model** (proves input rides the existing gate,
loop_mod.rs:324) — reuse the `HeadlessSink`/mock-app harness from the loop tests.

**Slice 4 — Barge-in + AEC.** Full-duplex VAD-during-playback; onset → duck+stop + `handle.abort()`
the turn task + persist spoken-so-far + reopen capture. `VoiceSession` state machine.
*Gate:* measured **barge-in-to-silence < 200ms**; TTS output does **not** self-trigger the VAD (AEC
verified with a played-back clip); an aborted turn persists only the spoken text + `[interrupted]`.

**Slice 5 — Audio-egress privacy check (cloud STT/TTS + the NEW invariant).** `AudioEgressGate`
(`audio/privacy.rs`) reusing `PrivacyGate::check_detailed` (gate.rs:97) + `is_private_endpoint`
(egress.rs:24). Cloud-TTS chunk withhold→local; Public+floor→confirm; cloud STT gated to
Public/opt-in. `voice:privacy_withheld` event.
*Gate:* unit + integration — under `Auto`, a reply chunk containing an SSN is **never** POSTed to the
cloud-TTS `base_url` (spoken locally instead); under forced-cloud it raises exactly one confirm; an
egress-capture test asserts no cloud request body contains the sensitive span. Mirrors the existing
gate_tests style (`agent/gate_tests.rs`).

**Slice 6 — Settings toggle + model download + per-profile polish.** Voice section in
`Settings.svelte` (`src/lib/design/screens/Settings.svelte`): enable, STT/TTS engine (local|cloud),
voice picker, push-to-talk vs wake (OQ-1), hotkey. `get/set_voice_settings` IPC registered in
`lib.rs:234`. First-enable downloads whisper+piper assets (like the classifier models; M8 catalog
pattern). Per-profile: the voice turn inherits the conversation `binding` (loop already threads it,
`tools/mod.rs:242`).
*Gate:* toggle persists per-profile; default install has voice **OFF**; when enabled with no engine
override, STT+TTS are both **local** and the loop works **fully offline**; disabling tears down capture
with no residual mic hold.

---

## 6. `--no-default-features` / local-first impact

- **Cargo feature `voice`** (heavy native deps: `cpal`, whisper backend, AEC, TTS), following the
  exact `onnx-classifier` precedent (`Cargo.toml:68-75`). Default-on for the app; `--no-default-features`
  drops it → the app compiles with the audio module `#[cfg(feature="voice")]`-guarded out,
  `Capability::Audio` simply not offered, and the voice IPC commands compiled out (or returning
  "voice unavailable"). **CI / rules-only / headless builds are unaffected** — same guarantee the
  classifier feature already gives.
- **Local-first polarity (the whole point of 5.2):** default STT **and** TTS are local; **voice works
  with zero network.** Cloud STT/TTS are strictly opt-in and pass §3. Reusing the vendored `ort`
  runtime for piper means the default voice stack adds no new inference dependency.
- **Model download:** whisper + piper voices are downloadable assets gated behind first-enable (they
  ride M8's hardware-sized catalog / download-verify machinery, PLAN §8 M8). Until downloaded, the
  toggle reads "Enable voice (downloads ~N MB)."
- **Not a fork:** compiled in, runtime-gated by the settings toggle (default off) — satisfies
  manifest 5.2 "ships as a settings toggle, not an architecture fork."

---

## 7. Open questions (need Lukas) vs sensible defaults

Genuine product/security decisions **not** settled by the specs:

- **OQ-1 — Default activation: push-to-talk vs wake-word.** Wake-word = always-on mic (a standing
  privacy posture + a wake model); push-to-talk (hotkey/button) is privacy-conservative. *Sensible
  default I'll build on:* **push-to-talk** in v1, wake-word opt-in later. Confirm.
- **OQ-2 — Ship cloud STT at all?** Raw audio is unredactable and captures bystanders. Option A: STT
  is **local-only forever** (cloud allowed only for TTS). Option B: cloud STT under Public/explicit
  opt-in (as §3.1 currently designs). *Default:* build the local-only path first; leave cloud-STT
  behind a flag pending your call. This is a security-posture decision.
- **OQ-3 — Translation / cross-lingual voice.** Do we want whisper large-v3 translate mode (the
  Live-Translate prior art) as a voice feature, accepting its latency (bad for barge-in), or optimize
  purely for low-latency same-language with distil-whisper? *Default:* distil-whisper for latency;
  large-v3 as an optional downloadable "translate mode." Product-scope call.
- **OQ-4 — Barge-in turn persistence.** On interrupt, persist (a) full generated text, (b)
  spoken-so-far only, or (c) spoken-so-far + `[interrupted]` marker? Touches conversation coherence +
  cost accounting. *Default:* (c). Flagging because it shapes the audit/cost story; overridable.

Everything else (module layout, the trait seams, reuse of `ResultSink`/`PrivacyGate`/`is_private_endpoint`,
the feature-gate shape, the slice order) is a sensible default I'll proceed on without a decision.


---

## Design Review (skeptical pass, 2026-07-18)

*An independent staff-engineer critique of the design above, grounded in the actual codebase. Address these in the build phase.*

I've verified the doc against the actual codebase. Nearly every file:line citation is accurate. Here is the review.

---

## VERDICT: NEEDS-REVISION

This is a strong design pass — unusually well-grounded. I spot-checked ~20 cited anchors and they're real: `process_message` signature (`loop_mod.rs:290`), the gate call (`loop_mod.rs:324-326`), the `ResultSink` trait + `TauriResultSink`/`HeadlessSink` (`result_sink.rs:31/62/123`), `Capability::Audio` present in `app_default` and absent from `headless_server_default` (`tools/mod.rs:59/105-129`), `RiskClass` (`tools/mod.rs:252`), `is_private_endpoint` (`egress.rs:24`), the classifier surface (`classifier/mod.rs:21/53/224`), the `PreToolUse` chain (`hooks/mod.rs:530`), the single sink-construction site (`ipc/mod.rs:450`), and the `ort`/`onnx-classifier` feature precedent (`Cargo.toml:52,68-75`). The reuse of `PrivacyGate`/`ResultSink`/`Capability`/`is_private_endpoint` is genuine, not parallel machinery. It does NOT invent a second agent, and the model-routing invariant is correctly inherited. But three things need fixing before build.

### Top 3 gaps (most-severe first)

**1. The barge-in interrupt path contradicts "no change to `process_message` needed" — there is no persistence on abort.**
Assistant-turn persistence lives at the *end* of `stream_to_provider` (`loop_mod.rs:1446-1461`), reached only after the stream loop completes. §2.4 step 2 cancels the turn with `handle.abort()`, which drops the future at its next await point *inside* the stream loop — so the code at 1446-1461 never runs and **nothing is persisted for the interrupted turn**. Yet §2.4 step 3 requires persisting "spoken-so-far + `[interrupted]`". That write must therefore be done by `VoiceSession` reaching directly into storage and reconstructing the `assistant_id` / `provider_id` / `routing_decision` / `model` stamps that are private locals inside `stream_to_provider` — i.e. exactly the parallel machinery the design claims to avoid, OR a real change to `process_message` (thread a cancellation token so it persists-what-it-has on cancel). Compounding it: today's only call site, `send_message`, **awaits `process_message` inline** (`ipc/mod.rs:456`) and then **re-queries the persisted assistant row to return it** (`ipc/mod.rs:472`). Voice's spawn-a-task-and-abort model shares none of that, so "constructs `VoiceResultSink` there-equivalently" (§2.1) understates the work: voice needs its own IPC command with its own spawn/abort/persist-on-cancel logic. The headline "voice is a mode, not a fork; spine unchanged" holds for the *happy path* but not the interrupt path — which is the flagship's whole point. (Fixable, and see the OQ-4 note below — the house pattern already exists.)

**2. Per-sentence-chunk TTS classification can leak what whole-message classification catches — it weakens the doc's own invariant.**
The text gate classifies the whole message once (`loop_mod.rs:324`). §3.1's `Auto` row classifies *each sentence chunk independently* before cloud TTS. Sensitive content split across a sentence boundary (a name in chunk N, the associated number/condition in chunk N+1) can score each fragment `Public` while the full reply would score `Private`/`Uncertain`. That makes voice cloud-egress *exceed* text egress for identical content — the direct negation of the stated invariant "Voice egress ≤ text egress, never more." The doc never addresses chunk-boundary context loss. Fix: classify the full reply (or a rolling cumulative window) before any cloud-TTS egress, or force local TTS whenever the whole reply isn't provably `Public`.

**3. The invariant's "teeth" (Public-binding floor-confirm) can't be built from the primitives §3.1 lists.**
§3.1's `Public` row = "if the chunk trips the un-tunable rules floor → one confirm." But under `Binding::Public`, `PrivacyGate::check_detailed` returns `(Allow, None)` and **never runs the classifier** (`gate.rs:107`). §3.1 names `check_detailed` / `is_private_endpoint` / `redact` as the reused primitives — none surfaces a floor hit under `Public`. To get the teeth, `AudioEgressGate` must call `RulesClassifier` directly (it is `pub`, `classifier/mod.rs:22`; floor is un-tunable per `mod.rs:64-65`) — a primitive the design doesn't name. Designable, but as written the most load-bearing row of the NEW invariant is under-specified against the real gate API.

### Is the NEW invariant actually designed for?
Mostly yes, and correctly motivated — the "cloud TTS is a second egress the text gate never saw" insight is real and faithful to PLAN §6 (`PLAN.md:304-305,488-490`). It is even correctly *stricter* than text in the right place (typed `Public` bypasses the classifier entirely at `gate.rs:107`; voice adds a floor-confirm). But findings #2 and #3 mean the invariant is stated, not yet fully designed: chunk-granularity can violate it, and its Public-path enforcement isn't wired to a primitive that fires under Public.

### Secondary / lower-severity
- **"Default voice stack adds no new inference dependency" (§4/§6) is too strong.** piper-on-`ort` reuses `ort`, fine — but STT is `whisper-rs` (whisper.cpp bindings) or "an `ort`-based whisper," and the doc's own §4 admits the former; whisper.cpp is a **new native lib**, and piper needs an eSpeak-ng phonemizer (new C dep + data). The default *local* stack does add native deps beyond `ort`.
- **`RiskClass::External` is marked "Reserved"** (`tools/mod.rs:257-258`). §2.3's "this is what routes them through the egress checks" overstates — `External` isn't a live gating path; the actual gating is `AudioEgressGate`. Cosmetic, but don't lean on it.
- **Slices are genuinely committable/testable and reuse real harnesses** (`HeadlessSink`, `gate_tests`) — with two caveats: Slice 4 packs full-duplex VAD + AEC + abort + persist + state machine into one slice, and its gate ("aborted turn persists only spoken text + `[interrupted]`") is **untestable until finding #1 is resolved** (no persistence path exists on abort). Split AEC/full-duplex from the abort/persist mechanics.
- **Cross-platform section is the strongest part** and realistic (VPIO on macOS genuinely gives AEC+AGC+VAD; Windows Communications APO / Voice Capture DMO and PipeWire `echo-cancel` are real). One realism nit: macOS VPIO wants **both** capture and render inside the same voice-processing audio-unit graph — it doesn't compose cleanly with "cpal for playback." Expect a full duplex AUHAL/VPIO graph on mac, not cpal-playback + VPIO-capture.

### Open questions — one is already answered by the specs, so it shouldn't block
- **OQ-4 (barge-in persistence) is largely pre-answered.** The codebase already has the `aborted: bool` Message field (`storage/profile.rs:42`, set at `loop_mod.rs:1456`), an `update_message(..., aborted)` path (`profile.rs:467-491`), and crash-recovery's `[tool interrupted]` marker convention (`crash_recovery.rs:53,63`). The doc's default (c) *is* the house style — it should reuse the `aborted` flag + crash-recovery marker, not invent a fresh `[interrupted]` string. So OQ-4 shouldn't gate the design; the real work is finding #1 (there's no row to mark on abort).
- **OQ-2 (ship cloud STT?) has its default already dictated** by the locked local-first invariant (PLAN §6, "on-device by default"): local-only is the mandated v1 default regardless. The "ever ship cloud STT" question is genuinely open, but it does not block Slices 1-4.
- **OQ-1 (PTT vs wake-word) and OQ-3 (translation/large-v3) are genuine, unsettled product/security calls** — correctly flagged, correctly deferred.

Bottom line: an A-grade design pass that's ~99% faithful to the actual spine, but three revisions are load-bearing — the interrupt-path persistence hole (which undercuts the central "not a fork" claim), chunk-level TTS classification (which can violate the very invariant this flagship exists to add), and wiring the Public-floor teeth to a primitive that actually fires under `Public`.


---

## Revision v2 (addressing the review)

*Written after re-reading the cited code. Each subsection takes one review finding, states precisely how the mechanism ACTUALLY works (file:line), and designs within that reality reusing the real spine — no parallel machinery. The load-bearing invariants (gate fed-not-weakened, danger-floor, guard-wrap untrusted input, local-first `--no-default-features`, per-call gating) are preserved throughout.*

### R1 — Interrupt: cooperative cancellation, NOT `handle.abort()` (fixes Gap 1)

**How it actually works.** `process_message` holds `_stream_guard = self.stream_lock.lock()` for its whole body (`loop_mod.rs:305`) — a guard that releases on the method returning OR being dropped. Assistant-turn persistence is the `profile_db.add_message(&assistant_message)` at the tail of each round *inside* `stream_to_provider` (`loop_mod.rs:1445-1461`), reached only after the SSE consumption loop `while let Some(event) = sse.next_event().await { … }` (`loop_mod.rs:1381-1415`) drains. The turn's identity — `assistant_id` (minted per round at `loop_mod.rs:1336`), `provider.id`, `routing_decision`, `model`, the redaction rehydrate at `1440-1443` — are all private locals of that method. `handle.abort()` drops the future at its next await (inside `sse.next_event()`), so `1445-1461` never runs: nothing is persisted, and the drop happens mid-await rather than via a clean return. The review is correct: as written, §2.4 step 2 makes step 3 unbuildable without either reconstructing those private locals from outside (the parallel machinery we forbid) or a real loop change.

**The fix — the review's own sanctioned option: thread a cooperative cancellation token so the loop persists-what-it-has on cancel.** This is strictly better than `abort()` because it (a) runs the EXISTING persistence path, and (b) exits `process_message` by a normal `return`, releasing `stream_lock` cleanly for the next turn instead of a mid-await drop. Concretely:

1. **Signature (additive, bounded).** Add one parameter: `cancel: &tokio_util::sync::CancellationToken` to `process_message` (`loop_mod.rs:290`) and pass it through to `stream_to_provider`. The two existing callers each add one line: `send_message` (`ipc/mod.rs:456`) and the sub-agent path pass a fresh never-fired `CancellationToken::new()` — so typed sends and headless runs are byte-for-byte unchanged. This is the minimal honest cost the review flagged; it is not a fork.

2. **Cancel-aware SSE drain.** Replace the bare `while let Some(event) = sse.next_event().await` (`loop_mod.rs:1381`) with a `tokio::select!` between `sse.next_event()` and `cancel.cancelled()`. On the cancelled branch: stop draining, set a local `interrupted = true`, `break` the SSE loop AND the outer `for round` loop, then fall through to the SAME persistence block at `1445-1461`.

3. **Reuse the `aborted` flag verbatim — no new marker.** The round-cap stop already persists a cut-off turn with `aborted: is_round_cap_stop` (`loop_mod.rs:1434, 1456`); the `Message.aborted` column (`profile.rs:42`) and `update_message(…, aborted)` (`profile.rs:470-496`) exist for exactly this. Persist with `aborted: is_round_cap_stop || interrupted`. This slots into the crash-recovery contract untouched: `crash_recovery.rs:36-42` explicitly treats `aborted: true` as a *deliberate* stop, never crash damage — a barge-in interrupt is deliberate, so the boot pass correctly leaves it alone. We reuse that convention (and, for the UI, may stamp `routing_decision = "interrupted"` analogous to `crash_recovery.rs:57`'s `"crash_recovery"` label) instead of the doc's invented `[interrupted]` string.

4. **Content persisted = generated-so-far (`assembled`), rehydrated by the existing redaction un-mask (`loop_mod.rs:1440-1443`).** This resolves OQ-4 to *(a-ish) generated-so-far + `aborted:true`*, diverging from the doc's default (c) spoken-so-far — deliberately, for two code-grounded reasons: (i) **ledger consistency** — cost is booked from `round_usage` on the same persisted row (`loop_mod.rs:1484-1499`); persisting less than what was generated/billed desyncs the transcript from the ledger; (ii) **replay coherence** — history is replayed to the model next turn, and the model must see its own actual output, not a truncation at an arbitrary audio boundary. The "how much the user actually HEARD" fact is real but belongs to the UI/telemetry layer (the `VoiceResultSink` knows its last-flushed sentence boundary), not to the model-facing transcript. This is the one small knob I'd surface for a Lukas override (§ "open questions" below); it does not gate the build.

5. **New IPC command, not a reuse of `send_message`.** The review is right that `send_message` awaits `process_message` inline and re-queries the persisted row to return it (`ipc/mod.rs:456, 475-482`) — a shape voice can't share. Add a `voice_turn` command that: builds a `VoiceResultSink` (decorating `TauriResultSink`, exactly as the sole sink-construction pattern at `ipc/mod.rs:450` does), owns a `CancellationToken`, spawns `process_message` as a task, and exposes `cancel()` to the `VoiceSession`. It does NOT need the re-query dance — the assistant row (including the `aborted` marker) is what the frontend reads back on the interrupted path.

**Latency composition (the ≤200ms budget is unaffected).** Barge-in-to-silence is met on the AUDIO thread: on speech onset the `VoiceSession` (1) issues `AudioOutput::stop` + buffer flush (≤50ms) immediately, then (2) calls `token.cancel()`. The loop's cooperative unwind (break → one SQLite `add_message` → return → drop guard) runs in parallel and takes a few ms; it does not sit inside the 200ms path. It only needs to finish before the NEXT turn's `process_message` can acquire `stream_lock` — comfortably inside that next turn's endpoint→first-audio budget (~1.5s). So replacing `abort()` with `cancel()` keeps the latency target while fixing the persistence hole.

### R2 — TTS classification granularity: cumulative-prefix + one-way local latch (fixes Gap 2)

**How it actually works.** The text gate classifies the WHOLE message once: `check_detailed` → the `Auto` arm runs `self.classifier.classify_with(text, cfg)` on the full `text` (`gate.rs:126`), called from `loop_mod.rs:324` on the entire transcript. §3.1's `Auto` row classified each sentence chunk *independently*, so a name in chunk N and its number in chunk N+1 can each score `Public` while the joined reply scores `Private`/`Uncertain` — making voice cloud-egress EXCEED text egress, negating the invariant.

**The fix — re-apply the identical gate to a growing CUMULATIVE prefix, never an isolated chunk.** `AudioEgressGate` maintains `spoken_prefix` (all reply text handed to TTS so far this turn). Before the next chunk goes to *cloud* TTS it calls `PrivacyGate::check_detailed(&binding, &format!("{spoken_prefix}{chunk}"), tts_is_cloud, &cfg)` (`gate.rs:97`) — the SAME primitive, on a superset of the chunk. Two properties make this provably ≤ text egress:

- **Monotone local latch.** The first cumulative verdict that is not `Allow`/Public latches the *rest of the turn* to local TTS (mirrors the model path's one-way `stream:local_reroute` downgrade, `result_sink.rs:39`). Because every later prefix still contains the earlier sensitive span, `check_detailed` would keep returning `RouteLocal` anyway; the latch just guarantees monotonicity if the classifier is non-monotone.
- **By the final chunk, the cumulative prefix == the whole reply**, so the decision on the complete reply is byte-identical to what the text gate would compute on that text. AudioEgressGate can therefore only ever be equal-or-stricter than a single whole-reply classification — never looser. That is the formal statement of "voice egress ≤ text egress, never more."

**No split can leak an unredactable value.** A sensitive span lives in some chunk; that chunk is always classified together with its full preceding context, so the span (and its rules-floor category) is detected no later than the chunk that *completes* it, and that chunk + remainder latch local. The one residual — a structured secret straddling a flush boundary (half an SSN in chunk N) — is closed by a small aggregator guard: **the sentence aggregator must not flush a chunk to CLOUD TTS while its tail sits inside an in-progress structured-PII run** (trailing digit/`@`/token-prefix run); it defers that chunk until the run resolves or a sentence boundary clears it. Local TTS has no such constraint (no egress). This keeps first-audio latency intact for the common clean case while guaranteeing the *value* never egresses.

This composes with, does not fork, the gate: same `check_detailed`, same labels/polarity; AudioEgressGate simply re-vets the reply text (which the model gate never saw, §3) at a granularity that is a superset of the text gate's, gated by `is_private_endpoint(tts_base_url)` (`egress.rs:24`) for the cloud-ness signal.

### R3 — Public-path floor teeth wired to `rules::detect` (fixes Gap 3)

**How it actually works.** Under `Binding::Public`, `check_detailed` returns `(GateDecision::Allow, None)` and never runs the classifier (`gate.rs:107`). So the doc's Public row ("chunk trips the rules floor → one confirm") cannot be sourced from `check_detailed` — the review is right. But the floor primitive IS `pub` and un-tunable: the free function `classifier::rules::detect(text) -> Vec<Span>` (`rules.rs:342`) and `RulesClassifier` (`rules.rs:456`) run the deterministic structured-secret detectors (SSN, Luhn card, API tokens, email/phone) at confidence 1.0 regardless of any threshold or binding (`classifier/mod.rs:64-65`: "the floor can't be tuned away"). `RiskClass::External` is marked *Reserved* (`tools/mod.rs:257`) and is NOT a live hook-chain gating path — so §2.3 must not lean on it; the live enforcement is `AudioEgressGate` (auto-TTS path) and `resolve_grant`'s External arm (explicit-`speak`-tool path, below).

**The fix — call the floor primitive directly, raise the confirm through the REAL approval spine.** Under `Binding::Public` with cloud TTS, AudioEgressGate runs `classifier::rules::detect(&spoken_prefix_plus_chunk)` on the cumulative prefix (R2's window). If any returned `Span.category` is in the structured-secret set — `PiiId | Financial | Credential | PiiContact` (`rules.rs:74-92`) — it raises **exactly one** confirm before that chunk (and, latched, the rest of the turn) reaches cloud TTS. The model gate's Public bypass (`gate.rs:107`) is deliberately left untouched, so *typed* Public is unchanged and *spoken* Public gets the stricter floor — precisely the "stricter than text" asymmetry the invariant wants, now sourced from a primitive that actually fires under Public.

The confirm reuses the approval spine rather than a bespoke voice dialog, mirroring how `ToolDispatcher::dispatch` handles `HookResult::Ask` (`dispatch.rs:547-596`):

- Fingerprint the egress with `ActionFingerprint::of("speak_cloud", {tts_provider, reply_hash})` (`approval.rs:62`) so a grant pins to this exact utterance+provider and can't drift.
- Ask via the same `Arc<dyn ApprovalPrompter>::request(req)` the dispatcher holds (`approval.rs:326`), with `risk: RiskClass::External` and `destination: Some(<tts_base_url host>)` — the External destination-consent field is exactly this (`approval.rs:302`).
- Record the answer through `resolve_grant(RiskClass::External, scope, target, fp)` (`approval.rs:237, 251-254`): for External this is fingerprint-pinned only, so "always voice sensitive content to the cloud" is structurally refused — it can be granted `Once` (this utterance) or narrowed to a `Session` fingerprint, never a standing whole-tool grant. The floor-confirm thus inherits invariant #8's guarantees for free.

**Bonus — the explicit `speak` tool path needs zero new gating.** If the agent ever calls an optional `speak` tool (§2.3), declare it `risk() -> RiskClass::External`, `requires() -> &[Capability::Audio]` (`tools/mod.rs:326`), `destination() -> Some(tts_host)` (`tools/mod.rs:321`). It then flows through the normal dispatcher + `PreToolUse` chain (`hooks/mod.rs:530-559`) + approval spine unchanged — the External arm of `resolve_grant` is live for that path even though the `RiskClass` doc-comment says "Reserved." The auto-TTS (`VoiceResultSink`-teed) path is the only one that needs AudioEgressGate to invoke the prompter directly; both reuse the same four primitives (`ApprovalPrompter`, `ApprovalLedger`, `ActionFingerprint`, `resolve_grant`) — same primitive, new boundary, exactly as §2.3 framed `PrivacyFilterHook` vis-à-vis the gate.

### R4 — The corrected `AudioEgressGate` decision table (composed, not forked)

All rows evaluate on the **cumulative reply prefix** (R2), with `tts_is_cloud = !is_private_endpoint(tts_base_url)` (`egress.rs:24`) and a one-way local latch. Local TTS is always a no-op `Allow` (no egress) — the default offline stack pays nothing.

**TTS egress** — `check_tts_egress(prefix, binding, tts_is_cloud)`:

| binding | local TTS | cloud TTS |
|---|---|---|
| `Private` | Allow | **Block** → speak with local TTS or stay silent (`check_detailed` Private+cloud → Block, `gate.rs:113-119`). |
| `Auto` | Allow | `check_detailed` on the cumulative prefix: Public → Allow; **Private/Uncertain → latch to LOCAL TTS for the rest of the turn** (`gate.rs:129-135`, the RouteLocal analog). Emit non-silent `voice:privacy_withheld` (mirrors `stream:local_reroute`, `result_sink.rs:39`). |
| `Public` | Allow | Allow — **but** run `rules::detect` on the cumulative prefix (`rules.rs:342`); a structured-secret category (`rules.rs:74-92`) → **one confirm via the approval spine** (R3) before cloud egress. This is the "without confirmation" teeth, now wired to a firing primitive. |

**STT egress** — `check_stt_egress(binding, stt_is_cloud)`: raw audio is unclassifiable pre-transcription (you can't run `detect`/`check_detailed` on a waveform, and it may carry a bystander's voice), so the floor is stricter than text: **cloud STT is permitted only under `Public` binding or an explicit per-session opt-in; `Auto`/`Private` force local STT.** No silent cloud STT (OQ-2 asks whether cloud STT ships at all; see below).

**Composition, stated exactly.** (1) Model routing of the *transcript* is unchanged — the existing gate at `loop_mod.rs:324` owns it. (2) TTS egress of the *reply* is additive: AudioEgressGate re-vets reply text at the cumulative granularity the model gate never applied, and can only be equal-or-stricter (R2), so no double-gate and no gap. (3) STT egress of raw audio is the only place a pre-transcription waveform can be gated. A `Private`-binding conversation is *exactly as local spoken as typed*; a withheld chunk carries the same label/polarity the text gate would; the withhold is always visible via `voice:privacy_withheld`.

### R5 — Secondary corrections folded in

- **Dependency claim corrected.** §4/§6's "default voice stack adds no new inference dependency" is too strong and is retracted. Reality: STT via `whisper-rs` (whisper.cpp) is a NEW native lib, and piper needs an eSpeak-ng phonemizer (new C dep + data). Two honest options: (i) prefer an **`ort`-based whisper** so STT genuinely rides the vendored `ort` runtime (`Cargo.toml:52`), and pair piper (also `ort`) with a bundled/`ort`-based phonemizer — keeping the inference stack to `ort` only; or (ii) accept the added native deps. Either way they live behind the `voice` cargo feature, so `--no-default-features` / CI / rules-only / headless builds are unaffected — the local-first guarantee (the whole point of 5.2) is intact regardless of which STT/TTS backend is chosen. Piper-on-`ort` reusing the classifier runtime remains true and is still the reason TTS adds no *new* inference stack; the overstatement was only about STT.
- **`RiskClass::External` is Reserved.** Do not describe it as "what routes them through the egress checks" (§2.3). It is live only inside `resolve_grant`'s matrix (`approval.rs:251`) for the explicit-`speak`-tool path; the auto-TTS path's actual enforcement is `AudioEgressGate`. Cloud STT/TTS are tagged `External` for the tool path's grant-narrowing, not because `External` is itself a live gate.
- **macOS VPIO does not compose with cpal-playback.** Correct §4: on macOS, AEC-on runs a **single full-duplex AUHAL/VoiceProcessingIO audio-unit graph** with capture AND render inside the same VPIO unit — not VPIO-capture + cpal-playback. So on macOS the `AudioInput`/`AudioOutput` pair (`capture.rs`/`playback.rs`) is backed by one shared VPIO graph object; `cpal` remains the cross-platform default for Windows/Linux and a non-AEC macOS fallback.

### R6 — Revised build slices (with honest gates)

Slices 1–3 and 6 stand as written. **Slice 4 is split** (the review: separate AEC/full-duplex from abort/persist; the old gate was untestable with no persistence path):

**Slice 4a — Cooperative-cancel plumbing + persist-on-cancel (R1).** Thread `CancellationToken` through `process_message`; `tokio::select!` in the SSE drain; break → existing persistence with `aborted: true`; new `voice_turn` IPC command owning the token. *Gate (INTEGRATION harness, not `cargo test --lib`):* drive a real `process_message` against a fake provider that streams deltas slowly + a temp profile DB (reuse the existing loop/`gate_tests` harness that already exercises `process_message`), fire the token mid-stream, assert exactly one assistant row persisted with `aborted = 1` and `content` == the delta prefix received before cancel, and that `stream_lock` is free for an immediately-following turn. Flagged honestly: this needs the loop wired to a fake provider + temp DB — it is not a pure unit test.

**Slice 4b — Full-duplex VAD + AEC + barge-in trigger.** VAD-during-playback; onset → duck+stop (audio thread) + `token.cancel()` + reopen capture; `VoiceSession` state machine; macOS unified VPIO graph (R5). *Gate (ON-HARDWARE bench, not `cargo test`):* measured barge-in-to-silence < 200ms; a played-back TTS clip does not self-trigger the VAD (AEC verified). This is a device/integration bench by nature — call it out as such; the abort/persist correctness it depends on is already proven by 4a's harness.

**Slice 5 — Audio-egress privacy check (updated to R2+R3+R4).** `AudioEgressGate` reusing `check_detailed` (`gate.rs:97`) on the **cumulative prefix** with a one-way local latch, `rules::detect` (`rules.rs:342`) for the Public floor, and the approval spine (`ApprovalPrompter`/`ActionFingerprint`/`resolve_grant`) for the one confirm. *Gate (unit + integration, mirrors `gate_tests`):* (a) unit — a reply whose SSN is split across two sentence chunks classifies `Private` on the cumulative prefix and the SSN chunk is never cloud-routed; (b) integration/egress-capture — under `Auto`, no cloud-TTS request body ever contains a sensitive span (spoken locally instead); under `Public`+floor-hit exactly one `ApprovalPrompter::request` fires and `resolve_grant` narrows any standing answer to a `Session` fingerprint. Parts of (b) that assert "no bytes hit the TTS host" need the fake-egress harness, not `--lib`.

### R7 — Open questions after revision

- **OQ-4 (barge-in persistence): RESOLVED** to generated-so-far + reuse of the `aborted` flag / crash-recovery marker convention (R1). The one remaining overridable knob is generated-so-far vs truncate-to-spoken; default recommended, does not gate the build.
- **OQ-2 (ship cloud STT at all): default dictated, tail open.** Local-only STT is the mandated v1 default (PLAN §6 on-device-by-default); this does not block Slices 1–4b. Whether cloud STT ever ships behind a flag remains a genuine security-posture call for Lukas — but it gates only an optional later surface.
- **OQ-1 (PTT vs wake-word)** and **OQ-3 (translation / large-v3)** remain genuine, unsettled product/security calls — correctly deferred; neither gates the core loop or the invariant.
- **One new bounded decision surfaced by R1:** `process_message` gains a `cancel: &CancellationToken` parameter (additive; existing callers pass a never-fired token). This is an implementation change, not a product decision — noted for transparency, pre-approved by the review's "thread a cancellation token" guidance.

### STATUS

**BUILD-READY for Slices 1–5 (4a/4b split) on an on-target machine.** All three load-bearing gaps now have sound, code-grounded designs that reuse the real spine — cooperative cancellation into the existing persistence path (R1), cumulative-prefix classification with a local latch (R2), and the Public floor wired to `rules::detect` + the approval spine (R3) — with the load-bearing invariants intact and each slice's gate honestly labeled (`--lib` vs integration harness vs on-hardware bench). No open question blocks the local-first default loop or the audio-egress invariant; the only remaining decisions (OQ-1 activation, OQ-2 cloud-STT tail, OQ-3 translation) gate optional/later surfaces, and OQ-4 is resolved.
