# UI Tech Debt — Composer and Route Controls

This note tracks the work needed to turn the current composer UI from an
honest visual prototype into fully-backed product behavior. It is intentionally
specific: a control should not imply a capability that the agent loop, IPC
contract, and model adapter cannot yet enforce.

## Current state

- The model picker changes the active provider/model.
- The permission shield selects the existing per-turn `normal`, `plan`, or
  `accept_edits` mode and sends it with a chat request.
- Send and the brand knot are colored from the best pre-send information the
  frontend currently has: conversation binding plus the selected provider.
- The context ring estimates visible user/assistant text against an **assumed
  8k presentation scale**. The detail panel explicitly marks system/tool/skill/
  MCP/file/photo usage as unmetered.
- Attachment, thinking-strength, and voice controls have UI surfaces but do
  not yet have complete end-to-end product wiring.

## 1. Authoritative route state for Send and Knot — high priority

**Gap:** The amber/green/blue route colors are a frontend prediction. In Auto
mode, the real classifier and route decision happen only during the agent run;
the UI cannot know the final disposition beforehand.

**Needed:**

1. Define a shared route-state contract, for example `filter`, `local`,
   `cloud`, and `held`, rather than translating between UI labels such as
   “public” and backend terms such as “cloud”.
2. Expose a pre-send route preview through IPC when one can be computed safely,
   including the active binding, provider, and draft classification result.
3. Emit an authoritative route event when the privacy gate actually decides,
   and return the same final state in the completed-send response.
4. Drive both the Send button and the brand knot from that shared state. The
   final state must win over the preview, including local reroutes, redaction,
   and guard holds.
5. Add frontend tests for Auto → local, Auto → cloud, explicit Private → local,
   public/cloud, redaction, reroute, and blocked/held cases.

Likely touchpoints: `src/lib/design/screens/MainScreen.svelte`,
`src/lib/stores/chat.ts`, `src/lib/api/tauri.ts`, `src-tauri/src/ipc/mod.rs`,
and the agent-loop route events.

## 2. Per-conversation context accounting — high priority

**Gap:** The ring cannot yet show how full the *actual* model context window is.
It only estimates visible text; it does not know the selected model’s context
limit or account for hidden prompt material.

**Needed:**

1. Add model metadata for context-window size and reasoning capability. A list
   of model names alone is not sufficient.
2. Capture authoritative token counts from provider usage when available; use a
   model-specific tokenizer only as a clearly-labelled fallback.
3. Persist a per-conversation, per-turn context ledger with at least:
   system prompt, skills, tools, MCP results, files, photos, user input,
   assistant output, and the model context-window limit.
4. Provide `get_conversation_context_usage(conversation_id)` plus a stream
   event so the composer ring and its popover update while an agent is working.
5. Make compaction visible in the ledger: what was removed, summarized, or
   retained must affect the fill percentage.

Until this lands, the UI must continue labelling the 8k scale and its text
counts as estimates rather than showing a fabricated percentage.

## 3. Attachments — high priority before enabling the plus button

**Gap:** The attachment control deliberately says that sending is not wired.

**Needed:**

1. Build a native file/photo picker and a draft attachment model (name, MIME,
   size, hash, local path, thumbnail state, and source).
2. Store drafts in the profile-confined workspace or a dedicated encrypted
   attachment store; do not treat arbitrary picker paths as durable message
   attachments.
3. Extend the chat IPC request and storage schema to attach the selected files
   to a message, with durable cleanup and retry semantics.
4. Classify attachment content and metadata before any cloud egress. Use the
   existing image/multimodal model path only when the chosen seat supports it;
   otherwise provide a local extraction or an explicit refusal.
5. Surface attached files in the thread, include their token/image cost in the
   context ledger, and record every external transfer in the audit trail.

## 4. Thinking strength — medium priority

**Gap:** Light, Balanced, and Deep currently change only the picker’s visual
state. They do not affect a request.

**Needed:**

1. Add a typed `thinking_strength` field to the frontend send call and Rust
   `SendMessageArgs`.
2. Translate that field only for providers/models that advertise compatible
   reasoning controls; reject or disable it when unsupported rather than
   silently ignoring it.
3. Persist the chosen strength at the intended scope (conversation default or
   per-message override) and include the effective value in message metadata.
4. Account for reasoning tokens in the context ledger where a provider exposes
   them.

## 5. Voice input — medium priority

**Gap:** The microphone tries browser speech recognition when the webview
supports it. That is not a dependable desktop voice feature.

**Needed:**

1. Implement native microphone capture, permission handling, device selection,
   interruption, and cancellation for the supported desktop targets.
2. Route local and cloud STT through the existing audio-egress policy; cloud
   transcription must not bypass the privacy gate.
3. Stream a provisional transcript into the composer, then show the finalized
   transcript and any failure state accessibly.
4. Define how voice files, transcript tokens, and optional retained audio are
   represented in the attachment and context ledgers.

## 6. Permission-mode scope and audit — medium priority

**Gap:** The shield correctly sends a per-turn mode, but its selected state is
currently ephemeral UI state. “Bypass local edits” must never be read as a
blanket bypass of approvals.

**Needed:**

1. Decide and document the scope: one message, one conversation, profile
   default, or all three with explicit precedence.
2. Persist the chosen scope where appropriate and restore it on reopen.
3. Return the effective mode with each send result and record it in the audit
   metadata.
4. Keep backend enforcement authoritative: dangerous and off-device actions
   must still require the existing approval gates regardless of the label.

## Definition of done

For each control, complete the UI only after there is a typed frontend API,
validated IPC contract, backend enforcement/adapter support, durable state,
live error handling, accessibility coverage, and an end-to-end test of both
the success and privacy-denied paths.
