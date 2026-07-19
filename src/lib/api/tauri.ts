// Lost Harness — TypeScript bridge to the Tauri IPC layer.
//
// This module is the *only* place the frontend should call into the Rust
// core. Stores import from here; components import from the stores. That
// keeps the Tauri dependency surface narrow and makes it trivial to mock
// the entire backend for browser-based development (Vite dev server with
// no Tauri shell).
//
// Backend contract (must stay in sync with `src-tauri/src/ipc/mod.rs`):
//   - `get_app_version() -> String`
//   - `get_active_profile() -> String`
//   - `list_profiles() -> Vec<String>`
//   - `list_providers() -> Vec<ProviderInfo>`
//   - `remove_provider(id) -> bool`
//   - `send_message(args: { content, conversation_id, binding, provider_id, model, profile, mode }) -> SendMessageResponse`
//   - `add_provider(args: { name, base_url, api_key, kind, supports_native_tools }) -> ProviderInfo`
//   - `list_models(args: { provider_id }) -> Vec<String>`
//   - `list_conversations(args: { profile }) -> Vec<ConversationInfo>`
//   - `create_conversation(args: { name, binding, profile }) -> ConversationInfo`
//   - `get_messages(args: { profile, conversation_id }) -> Vec<MessageInfo>`
//
// IMPORTANT — Tauri v2 argument shape:
//   Every Rust command above whose signature is `(..., args: SomeStruct)`
//   receives its fields NESTED under the parameter name `args`. The JS call
//   must therefore be `invoke("cmd", { args: { ...snake_case_fields } })`,
//   NOT flat top-level fields. Tauri's camelCase→snake_case conversion only
//   applies to the top-level command parameter names (e.g. `id`), never to
//   fields inside a struct parameter — those are deserialized by serde with
//   the struct's own (snake_case) field names. `remove_provider(id)` takes a
//   bare scalar param, so it stays `{ id }` (no wrapper).
//
// Events:
//   - `stream:token`  — payload: { token, conversation_id, message_id }
//   - `stream:error`  — payload: { error, conversation_id, source }
//
// If you add or rename a command or event on the Rust side, update the
// matching type, constant, or function below. The TypeScript types here
// are the source of truth for the frontend.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";

// ── Shared types (mirror the Rust serde structs) ────────────────────────────

/** Mirrors `SendMessageResponse` in ipc/mod.rs. */
export interface SendMessageResponse {
  message_id: string;
  content: string;
  conversation_id: string;
  /** Profile id that handled the message (e.g. "personal"). */
  profile: string;
  /** "allow" | "route_local" — which branch of the gate served this message. */
  routing_decision: string;
  /** Epoch ms the response was finalized. */
  completed_at: number;
}

/** Mirrors `ProviderInfo` in ipc/mod.rs. API key is omitted by the backend. */
export interface ProviderInfo {
  id: string;
  name: string;
  base_url: string;
  /** "local" | "cloud" | "custom" */
  kind: string;
  /** Whether the provider's endpoint is a private/LAN address. */
  is_private: boolean;
  /** Q1: whether the endpoint supports OpenAI-style native structured tool calls. */
  supports_native_tools: boolean;
}

/** Mirrors `ConversationInfo` in ipc/mod.rs. */
export interface ConversationInfo {
  id: string;
  name: string;
  pinned: boolean;
  binding: string;
  folder_id: string | null;
  color: string | null;
  created_at: number;
  updated_at: number;
}

/** Mirrors `MessageInfo` in ipc/mod.rs. */
export interface MessageInfo {
  id: string;
  conversation_id: string;
  /** "user" | "assistant" */
  role: string;
  content: string;
  model: string | null;
  provider_id: string | null;
  routing_decision: string | null;
  thinking_content: string | null;
  error: string | null;
  aborted: boolean;
  created_at: number;
}

/** Payload of the `stream:token` event. Mirrors `StreamTokenPayload` in loop_mod.rs. */
export interface StreamTokenPayload {
  token: string;
  conversation_id: string;
  message_id: string;
}

/**
 * Payload of the `stream:error` event. Mirrors `StreamErrorPayload` in
 * loop_mod.rs. `source` is one of `"gate"`, `"routing"`, `"model"`.
 */
export interface StreamErrorPayload {
  error: string;
  conversation_id: string;
  source: string;
}

/** Callback shape for `onStreamToken`. */
export type StreamTokenCallback = (payload: StreamTokenPayload) => void;

/** Callback shape for `onStreamError`. */
export type StreamErrorCallback = (payload: StreamErrorPayload) => void;

/**
 * Payload of the `tool:approval_request` event. Mirrors
 * `ToolApprovalRequestPayload` in `ipc/approval.rs`. Raised when a tool call
 * needs the user's confirmation; answer with `resolveToolApproval(id, ...)`.
 */
export interface ToolApprovalRequest {
  id: string;
  conversation_id: string;
  tool_name: string;
  /** Canonical `name {args}` — what the user is approving. Display-only. */
  command: string;
  prompt: string;
  /** Which hook raised it: "permission" | "first_use_confirm". */
  by: string;
  fingerprint: string;
  /**
   * The tool's risk class — server-derived. Drives the risk badge and which
   * grant buttons are offered (Dangerous hides session/always; External hides
   * whole-tool standing). The server (`resolve_grant`) is the enforcement; the
   * button layout is legibility, not the gate.
   */
  risk: RiskClass;
  /** For External tools, where the call goes. `null` for non-egress tools. */
  destination: string | null;
}

/** Tool risk class — mirrors `RiskClass::as_str()` in Rust. */
export type RiskClass = "safe" | "write" | "external" | "dangerous";

/** Callback shape for `onToolApprovalRequest`. */
export type ToolApprovalCallback = (payload: ToolApprovalRequest) => void;

/** How long a granted approval lasts. */
export type ApprovalScope = "once" | "session" | "always";
/** What an approval covers: this exact call, or any call to the tool. */
export type ApprovalTarget = "action" | "tool";

// ── Tauri runtime detection ─────────────────────────────────────────────────
//
// `window.__TAURI_INTERNALS__` is injected by the Tauri webview before any
// frontend code runs. In a plain browser (Vite dev without `tauri dev`) it
// is undefined, and we fall back to a JS-only mock so the UI still works
// for layout work and screenshot tests.

declare global {
  interface Window {
    __TAURI_INTERNALS__?: unknown;
  }
}

const isTauri = (): boolean =>
  typeof window !== "undefined" && typeof window.__TAURI_INTERNALS__ !== "undefined";

// ── Event channel names (must match Rust `app.emit(...)`) ───────────────────

export const STREAM_TOKEN_EVENT = "stream:token";
export const STREAM_ERROR_EVENT = "stream:error";
export const TOOL_APPROVAL_REQUEST_EVENT = "tool:approval_request";
export const MEMORY_EVENT = "memory:event";

/** Payload of `memory:event`. Mirrors `MemoryEventPayload` in `loop_mod.rs`. */
export interface MemoryEvent {
  conversation_id: string;
  /**
   * "recalled" — relevance-gated notes were injected for this answer.
   * "remembered" — a new note was saved (from a conversation turn, or a
   * manual save from Settings — the latter emits with an empty
   * `conversation_id`, so it won't surface a chat banner).
   */
  kind: "recalled" | "remembered";
  count: number;
}

// ── Command functions ───────────────────────────────────────────────────────

/** Returns the app version string, e.g. "0.1.0-m1". */
export async function getAppVersion(): Promise<string> {
  if (isTauri()) {
    return tauriInvoke<string>("get_app_version");
  }
  return "0.1.0-m1-browser";
}

/** Returns the id of the currently active profile. */
export async function getActiveProfile(): Promise<string> {
  if (isTauri()) {
    return tauriInvoke<string>("get_active_profile");
  }
  const stored = localStorage.getItem("lh.activeProfile");
  return stored ?? "personal";
}

/** Returns the list of profile ids known to the app. */
export async function listProfiles(): Promise<string[]> {
  if (isTauri()) {
    return tauriInvoke<string[]>("list_profiles");
  }
  // Mirror the four-profile design from the spec.
  return ["personal", "work", "school", "developer"];
}

// ── Providers + models ──────────────────────────────────────────────────────

/** Lists all configured providers. API keys are omitted by the backend. */
export async function listProviders(): Promise<ProviderInfo[]> {
  if (isTauri()) {
    return tauriInvoke<ProviderInfo[]>("list_providers");
  }
  return browserListProviders();
}

/** Adds a new provider. Returns the created ProviderInfo (with server-assigned id). */
export async function addProvider(
  name: string,
  baseUrl: string,
  apiKey: string | null,
  kind: string,
  supportsNativeTools: boolean,
): Promise<ProviderInfo> {
  if (isTauri()) {
    return tauriInvoke<ProviderInfo>("add_provider", {
      args: {
        name,
        base_url: baseUrl,
        api_key: apiKey || null,
        kind,
        supports_native_tools: supportsNativeTools,
      },
    });
  }
  return browserAddProvider(name, baseUrl, apiKey, kind, supportsNativeTools);
}

/** Removes a provider by id. */
export async function removeProvider(id: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("remove_provider", { id });
  }
  return browserRemoveProvider(id);
}

/** Lists the models available for a given provider. */
export async function listModels(providerId: string): Promise<string[]> {
  if (isTauri()) {
    return tauriInvoke<string[]>("list_models", { args: { provider_id: providerId } });
  }
  return browserListModels(providerId);
}

// ── Conversations + messages ────────────────────────────────────────────────

/** Lists conversations for the given profile. */
export async function listConversations(profile: string): Promise<ConversationInfo[]> {
  if (isTauri()) {
    return tauriInvoke<ConversationInfo[]>("list_conversations", { args: { profile } });
  }
  return browserListConversations();
}

/** Creates a new conversation. Returns the created ConversationInfo. */
export async function createConversation(
  name: string,
  profile: string,
  binding: string = "auto",
): Promise<ConversationInfo> {
  if (isTauri()) {
    return tauriInvoke<ConversationInfo>("create_conversation", {
      args: {
        name,
        profile,
        binding,
      },
    });
  }
  return browserCreateConversation(name, binding);
}

/** Lists messages in a conversation for the given profile. */
export async function getMessages(
  conversationId: string,
  profile: string,
): Promise<MessageInfo[]> {
  if (isTauri()) {
    return tauriInvoke<MessageInfo[]>("get_messages", {
      args: {
        profile,
        conversation_id: conversationId,
      },
    });
  }
  return browserGetMessages(conversationId);
}

// ── send_message ────────────────────────────────────────────────────────────

/**
 * Sends a user message to the agent and (in the Tauri path) receives a
 * stream of `stream:token` events as the model generates the response. The
 * returned `SendMessageResponse` carries the final, fully-assembled text.
 *
 * In browser fallback mode, this also emits fake tokens via the same
 * `stream:token` channel so consumers don't need to special-case anything.
 */
export async function sendMessage(
  content: string,
  conversationId: string,
  binding: string,
  providerId: string,
  model: string,
  profile: string,
  mode: string = "normal",
): Promise<SendMessageResponse> {
  if (isTauri()) {
    return tauriInvoke<SendMessageResponse>("send_message", {
      args: {
        content,
        conversation_id: conversationId,
        binding,
        provider_id: providerId,
        model,
        profile,
        // Q11 permission mode: "normal" | "plan" | "accept_edits".
        mode,
      },
    });
  }
  return browserSendMessage(content, conversationId);
}

// ── Event subscriptions ─────────────────────────────────────────────────────

/**
 * Subscribes to `stream:token` events. Returns an unlisten function that
 * detaches the listener. In browser mode, the mock `sendMessage` drives
 * the registered callbacks internally.
 */
export async function onStreamToken(
  callback: StreamTokenCallback,
): Promise<UnlistenFn> {
  if (isTauri()) {
    return tauriListen<StreamTokenPayload>(STREAM_TOKEN_EVENT, (event) => {
      callback(event.payload);
    });
  }
  // Browser fallback: register the callback so the mock `sendMessage` can
  // drive it. Last-registered wins — fine for the dev-only fallback.
  browserStreamListeners.push(callback);
  return () => {
    const i = browserStreamListeners.indexOf(callback);
    if (i >= 0) browserStreamListeners.splice(i, 1);
  };
}

/**
 * Subscribes to `stream:error` events. These fire when the privacy gate
 * blocks a message, a routing decision fails, or the model stream errors.
 * Returns an unlisten function. In browser mode this is a no-op (the mock
 * never emits errors).
 */
export async function onStreamError(
  callback: StreamErrorCallback,
): Promise<UnlistenFn> {
  if (isTauri()) {
    return tauriListen<StreamErrorPayload>(STREAM_ERROR_EVENT, (event) => {
      callback(event.payload);
    });
  }
  // Browser fallback: register so a mock could drive it if needed.
  browserErrorListeners.push(callback);
  return () => {
    const i = browserErrorListeners.indexOf(callback);
    if (i >= 0) browserErrorListeners.splice(i, 1);
  };
}

/**
 * Subscribes to `memory:event` — the non-silent memory signal (PLAN §9),
 * raised when the agent recalls saved notes for an answer. Returns an unlisten
 * function. In browser mode this is a no-op (the mock backend has no memory).
 */
export async function onMemoryEvent(
  callback: (e: MemoryEvent) => void,
): Promise<UnlistenFn> {
  if (isTauri()) {
    return tauriListen<MemoryEvent>(MEMORY_EVENT, (event) => {
      callback(event.payload);
    });
  }
  return () => {};
}

// ── Tool approval ───────────────────────────────────────────────────────────

/**
 * Subscribes to `tool:approval_request` events — raised when a tool call needs
 * the user's confirmation. Answer with `resolveToolApproval(id, ...)`. Returns
 * an unlisten function. In browser mode this is a no-op (the mock backend has
 * no gated tools to approve).
 */
export async function onToolApprovalRequest(
  callback: ToolApprovalCallback,
): Promise<UnlistenFn> {
  if (isTauri()) {
    return tauriListen<ToolApprovalRequest>(TOOL_APPROVAL_REQUEST_EVENT, (event) => {
      callback(event.payload);
    });
  }
  return () => {};
}

/**
 * Answers a pending tool-approval prompt. `decision` is "approve" or "deny";
 * `scope` / `target` only matter for an approval. For scope="always", `pattern`
 * is the persisted rule's glob ("*" = whole tool). Returns false if the request
 * id is unknown — already answered, or it timed out and denied by default.
 */
export async function resolveToolApproval(
  id: string,
  decision: "approve" | "deny",
  scope: ApprovalScope = "once",
  target: ApprovalTarget = "action",
  pattern: string = "*",
): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("resolve_tool_approval", {
      args: { id, decision, scope, target, pattern },
    });
  }
  return true;
}

// ── ask_human (the blocking "ask the user" tool) ────────────────────────────

export const ASK_HUMAN_REQUEST_EVENT = "tool:ask_human_request";

/**
 * Payload of `tool:ask_human_request`. Mirrors `AskHumanRequestPayload` in
 * `ipc/ask_human.rs`. Raised when the agent calls `ask_human`; answer with
 * `resolveAskHuman(id, text)` or decline with `resolveAskHuman(id, null)`.
 */
export interface AskHumanRequest {
  id: string;
  conversation_id: string;
  /** The question — model-authored, display-only (render as text, not markup). */
  question: string;
}

/** Callback shape for `onAskHumanRequest`. */
export type AskHumanCallback = (payload: AskHumanRequest) => void;

/**
 * Subscribes to `tool:ask_human_request` events. Returns an unlisten function.
 * In browser mode this is a no-op (the mock backend never asks).
 */
export async function onAskHumanRequest(callback: AskHumanCallback): Promise<UnlistenFn> {
  if (isTauri()) {
    return tauriListen<AskHumanRequest>(ASK_HUMAN_REQUEST_EVENT, (event) => {
      callback(event.payload);
    });
  }
  return () => {};
}

/**
 * Delivers the user's answer to a pending `ask_human` question. Pass the typed
 * text, or `null` to decline (the tool reports "not answered"). Returns false
 * if the id is unknown — already answered, or it timed out.
 */
export async function resolveAskHuman(id: string, answer: string | null): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("resolve_ask_human", { args: { id, answer } });
  }
  return true;
}

/** A profile's model-usage roll-up. Mirrors `UsageSummaryInfo` in `ipc/mod.rs`. */
export interface UsageSummary {
  total_calls: number;
  /** Summed KNOWN cost (local $0 + priced cloud calls). */
  known_cost_usd: number;
  /** Cloud calls we couldn't price — an honest "flying blind" count, not $0. */
  unknown_cost_calls: number;
}

/** The active profile's model-call cost ledger roll-up (Wave 3.2). */
export async function getUsageSummary(profile: string): Promise<UsageSummary> {
  if (isTauri()) {
    return tauriInvoke<UsageSummary>("get_usage_summary", { args: { profile } });
  }
  return { total_calls: 0, known_cost_usd: 0, unknown_cost_calls: 0 };
}

/** A saved skill. Mirrors `SkillInfo` in `ipc/mod.rs`. Skills are global, not
 *  profile-scoped, so these calls take no profile argument. */
export interface SkillInfo {
  id: string;
  name: string;
  description: string;
  content: string;
  capabilities_required: string[];
  /** "pending" | "approved" | "rejected" — the review gate. */
  approval_status: string;
  version: string;
  created_at: number;
}

/** Every saved skill (all statuses), for the Settings "Skills" review view. */
export async function listSkills(): Promise<SkillInfo[]> {
  if (isTauri()) {
    return tauriInvoke<SkillInfo[]>("list_skills", {});
  }
  return [];
}

/** Approve / reject a skill (the review gate). An unknown status fails to "pending". */
export async function setSkillApproval(id: string, status: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("set_skill_approval", { args: { id, status } });
  }
  return false;
}

/** Delete a saved skill. Returns true if a row was removed. */
export async function deleteSkill(id: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("delete_skill", { args: { id } });
  }
  return false;
}

/** A catalog model annotated with its fit against the machine (Wave 5.3 / M8). */
export interface CatalogModel {
  id: string;
  name: string;
  description: string;
  family: string;
  quantization: string;
  params_billions: number;
  url: string;
  sha256: string;
  size_bytes: number;
  /** "fits" | "tight" | "too_large" */
  fit: string;
  /** false when the entry's sha256 is still a release-curation placeholder. */
  installable: boolean;
}

export interface HardwareProfile {
  total_ram_bytes: number;
  cpu_cores: number;
  os: string;
  arch: string;
}

/** Probe this machine (RAM/cores/os/arch) for model sizing. */
export async function probeHardware(): Promise<HardwareProfile | null> {
  if (isTauri()) return tauriInvoke<HardwareProfile>("probe_hardware", {});
  return null;
}

/** The curated model catalog, sized to this machine. Works offline. */
export async function listModelCatalog(): Promise<CatalogModel[]> {
  if (isTauri()) return tauriInvoke<CatalogModel[]>("list_model_catalog", {});
  return [];
}

/** Download + verify a curated catalog model. Progress arrives on the
 *  `model:download-progress` event; throws on a digest mismatch / uncurated entry. */
export async function downloadModel(id: string): Promise<{ id: string; name: string; path: string }> {
  if (isTauri()) return tauriInvoke("download_model", { args: { id } });
  return { id, name: "", path: "" };
}

/** A downloaded local model. Mirrors `LocalModelInfo` in `ipc/mod.rs`. */
export interface LocalModel {
  id: string;
  name: string;
  path: string;
  size_bytes: number;
  /** "ready" | "quarantined" */
  status: string;
}

/** List downloaded local models (M8 S6). */
export async function listLocalModels(): Promise<LocalModel[]> {
  if (isTauri()) return tauriInvoke<LocalModel[]>("list_local_models", {});
  return [];
}

/** Delete a downloaded model (file + registry row). */
export async function removeLocalModel(id: string): Promise<boolean> {
  if (isTauri()) return tauriInvoke<boolean>("remove_local_model", { args: { id } });
  return false;
}

/** Result of installing a Capability Pack. Mirrors `InstallReport` in `packs`. */
export interface PackInstallReport {
  pack_name: string;
  skills_installed: number;
  agent_types_installed: number;
  cron_jobs_installed: number;
}

/** Install a Capability Pack from its JSON (skills + agent types + cron; all
 *  land inert/pending for review). Throws on invalid JSON. */
export async function installPack(profile: string, json: string): Promise<PackInstallReport> {
  if (isTauri()) {
    return tauriInvoke<PackInstallReport>("install_pack", { args: { profile, json } });
  }
  return { pack_name: "", skills_installed: 0, agent_types_installed: 0, cron_jobs_installed: 0 };
}

/** A declarative agent-type persona. Mirrors `AgentTypeInfo` in `ipc/mod.rs`. */
export interface AgentType {
  id: string;
  name: string;
  description: string;
  tools_allowlist: string[];
  seat: string;
  /** "pending" | "approved" | "rejected" — only approved types are dispatchable. */
  approval_status: string;
  /** "builtin" | "user" | pack id. */
  source: string;
  created_at: number;
}

/** Every agent-type persona (all statuses), for the Settings review view. */
export async function listAgentTypes(): Promise<AgentType[]> {
  if (isTauri()) {
    return tauriInvoke<AgentType[]>("list_agent_types", {});
  }
  return [];
}

/** Approve / reject an agent type (the dispatch trust gate). */
export async function setAgentTypeApproval(id: string, status: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("set_agent_type_approval", { args: { id, status } });
  }
  return false;
}

/** Delete an agent-type persona. */
export async function deleteAgentType(id: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("delete_agent_type", { args: { id } });
  }
  return false;
}

/** A per-profile model-seat binding. Mirrors `SeatBindingInfo` in `ipc/mod.rs`. */
export interface SeatBinding {
  seat: string;
  provider_id: string;
  model: string;
  updated_at: number;
}

/** List a profile's model-seat bindings (Wave 3.1). Seats are per-profile. */
export async function listSeatBindings(profile: string): Promise<SeatBinding[]> {
  if (isTauri()) {
    return tauriInvoke<SeatBinding[]>("list_seat_bindings", { args: { profile } });
  }
  return [];
}

/** Bind a (user-defined) seat name to a provider+model for this profile. */
export async function setSeatBinding(
  profile: string,
  seat: string,
  providerId: string,
  model: string,
): Promise<void> {
  if (isTauri()) {
    await tauriInvoke("set_seat_binding", {
      args: { profile, seat, provider_id: providerId, model },
    });
  }
}

/** Unbind a seat (it then resolves to the caller's model). */
export async function deleteSeatBinding(profile: string, seat: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("delete_seat_binding", { args: { profile, seat } });
  }
  return false;
}

/** Is autonomous skill drafting (Wave 4.2) on? Global; default off. */
export async function getSkillReflectEnabled(): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("get_skill_reflect_enabled", {});
  }
  return false;
}

/** Turn autonomous skill drafting on/off. Drafts land Pending for your review. */
export async function setSkillReflectEnabled(enabled: boolean): Promise<void> {
  if (isTauri()) {
    await tauriInvoke("set_skill_reflect_enabled", { args: { enabled } });
  }
}

/** A persisted "Always allow" rule. Mirrors `ToolRuleInfo` in `ipc/mod.rs`. */
export interface ToolRule {
  id: string;
  tool_name: string;
  pattern: string;
  action: string;
  created_at: number;
}

/** List a profile's persisted "Always allow" rules (newest first). */
export async function listToolRules(profile: string): Promise<ToolRule[]> {
  if (isTauri()) {
    return tauriInvoke<ToolRule[]>("list_tool_rules", { args: { profile } });
  }
  return [];
}

/** Revoke one persisted rule by id. Returns true if a row was removed. */
export async function deleteToolRule(profile: string, id: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("delete_tool_rule", { args: { profile, id } });
  }
  return false;
}

// ── classification explainability (PLAN §11 — the "why" sidebar) ────────────

/** One detected sensitive span. Mirrors `ClassificationSpan` in `ipc/mod.rs`. */
export interface ClassificationSpan {
  /** Char offset of the span start (inclusive). */
  start: number;
  /** Char offset of the span end (exclusive). */
  end: number;
  text: string;
  /** Machine category, e.g. "PII_CONTACT", "PROPRIETARY". */
  category: string;
  /** Human-friendly label for the legend, e.g. "contact info". */
  label: string;
  /** The specific rule that fired, e.g. "email". */
  rule: string;
  /** "rule" (deterministic) or "model" (the ensemble). */
  layer: "rule" | "model";
  /** Hard-block category (can never leave, no override). */
  hard: boolean;
}

/** The classifier's explanation of a piece of text. Mirrors `ClassificationExplanation`. */
export interface ClassificationExplanation {
  label: "private" | "public" | "uncertain";
  confidence: number;
  spans: ClassificationSpan[];
}

/**
 * Classify `text` and return the label + annotated spans, so the UI can show
 * *why* a message was held/redacted (PLAN §11). Pass the active `profile` so the
 * explanation uses that profile's classifier thresholds (matches the real
 * routing decision). In browser dev mode (no Tauri shell) returns an empty,
 * public explanation.
 */
export async function explainClassification(
  text: string,
  profile?: string,
): Promise<ClassificationExplanation> {
  if (isTauri()) {
    return tauriInvoke<ClassificationExplanation>("explain_classification", {
      args: { text, profile: profile ?? null },
    });
  }
  return { label: "public", confidence: 0, spans: [] };
}

// ── classifier settings (PLAN §11 — per-profile strictness) ─────────────────

/** A profile's classifier tuning. Mirrors `ClassifierSettingsInfo` in `ipc/mod.rs`. */
export interface ClassifierSettingsInfo {
  /** Detection strictness, 0 (permissive) – 100 (paranoid). */
  strictness: number;
  /** How wide the "unsure — keep local" band is. */
  uncertainty_band: "narrow" | "medium" | "wide";
  /** Raw fusion thresholds (display/debug only). */
  tau_block: number;
  tau_band: number;
  /** Whether partial-delegation redaction is enabled (PLAN §11). */
  redaction_enabled: boolean;
}

const DEFAULT_CLASSIFIER_SETTINGS: ClassifierSettingsInfo = {
  strictness: 50,
  uncertainty_band: "medium",
  tau_block: 0.5,
  tau_band: 0.05,
  redaction_enabled: true,
};

/** The active classifier settings for a profile (defaults when unset). */
export async function getClassifierSettings(profile: string): Promise<ClassifierSettingsInfo> {
  if (isTauri()) {
    return tauriInvoke<ClassifierSettingsInfo>("get_classifier_settings", { args: { profile } });
  }
  return { ...DEFAULT_CLASSIFIER_SETTINGS };
}

/** Persist a profile's classifier tuning. Returns the stored settings. */
export async function setClassifierSettings(
  profile: string,
  strictness: number,
  uncertaintyBand: "narrow" | "medium" | "wide",
): Promise<ClassifierSettingsInfo> {
  if (isTauri()) {
    return tauriInvoke<ClassifierSettingsInfo>("set_classifier_settings", {
      args: { profile, strictness, uncertainty_band: uncertaintyBand },
    });
  }
  return { ...DEFAULT_CLASSIFIER_SETTINGS, strictness, uncertainty_band: uncertaintyBand };
}

/** Toggle a profile's partial-delegation redaction. Returns the stored settings. */
export async function setRedactionEnabled(
  profile: string,
  enabled: boolean,
): Promise<ClassifierSettingsInfo> {
  if (isTauri()) {
    return tauriInvoke<ClassifierSettingsInfo>("set_redaction_enabled", {
      args: { profile, enabled },
    });
  }
  return { ...DEFAULT_CLASSIFIER_SETTINGS, redaction_enabled: enabled };
}

/** Revert a profile's classifier tuning to defaults. Returns the defaults. */
export async function resetClassifierSettings(profile: string): Promise<ClassifierSettingsInfo> {
  if (isTauri()) {
    return tauriInvoke<ClassifierSettingsInfo>("reset_classifier_settings", { args: { profile } });
  }
  return { ...DEFAULT_CLASSIFIER_SETTINGS };
}

// ── memory (PLAN §9) ────────────────────────────────────────────────────────

/** A memory fact for the UI. Mirrors `MemoryInfo` in `ipc/mod.rs`. */
export interface MemoryInfo {
  id: string;
  content: string;
  tags: string | null;
  created_at: number;
  pinned: boolean;
  /** "shared" (may inform cloud turns) | "private_local" (local-only). */
  sensitivity: "shared" | "private_local";
}

/** Result of a memory save. Mirrors `SaveMemoryResult`. */
export interface SaveMemoryResult {
  /** The route the classifier chose. `never_persist` means it was dropped. */
  sensitivity: "shared" | "private_local" | "never_persist";
  fact: MemoryInfo | null;
}

/** A profile's memory settings. Mirrors `MemorySettingsInfo` in `ipc/mod.rs`. */
export interface MemorySettingsInfo {
  /** Whether saved notes get a semantic (embedding) fingerprint for recall. */
  semantic_search_enabled: boolean;
  /** Whether this profile's memory store is physically separate ("walled") vs shared. */
  walled: boolean;
}

const DEFAULT_MEMORY_SETTINGS: MemorySettingsInfo = {
  semantic_search_enabled: true,
  // Shared by default — matches the backend default (§7). A profile only
  // becomes a walled island when the user explicitly turns it on.
  walled: false,
};

/** List a profile's memory facts (both buckets — the user's own local view). */
export async function listMemory(profile: string): Promise<MemoryInfo[]> {
  if (isTauri()) {
    return tauriInvoke<MemoryInfo[]>("list_memory", { args: { profile } });
  }
  return [];
}

/** Save a memory fact, routed by sensitivity. Returns the saved fact (or null if dropped). */
export async function saveMemory(profile: string, content: string): Promise<SaveMemoryResult> {
  if (isTauri()) {
    return tauriInvoke<SaveMemoryResult>("save_memory", { args: { profile, content } });
  }
  return { sensitivity: "shared", fact: null };
}

/** Forget a memory fact by id, within the given profile's store. */
export async function deleteMemory(profile: string, id: string): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("delete_memory", { args: { profile, id } });
  }
  return false;
}

/** Pin/unpin a fact into the always-loaded curated summary, within the given profile's store. */
export async function setMemoryPinned(
  profile: string,
  id: string,
  pinned: boolean,
): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("set_memory_pinned", { args: { profile, id, pinned } });
  }
  return false;
}

/** The active memory settings for a profile (defaults when unset). */
export async function getMemorySettings(profile: string): Promise<MemorySettingsInfo> {
  if (isTauri()) {
    return tauriInvoke<MemorySettingsInfo>("get_memory_settings", { args: { profile } });
  }
  return browserGetMemorySettings(profile);
}

/** Persist a profile's memory settings. Returns the stored settings. */
export async function setMemorySettings(
  profile: string,
  semanticSearchEnabled: boolean,
  walled: boolean,
): Promise<MemorySettingsInfo> {
  if (isTauri()) {
    return tauriInvoke<MemorySettingsInfo>("set_memory_settings", {
      args: {
        profile,
        semantic_search_enabled: semanticSearchEnabled,
        walled,
      },
    });
  }
  return browserSetMemorySettings(profile, {
    semantic_search_enabled: semanticSearchEnabled,
    walled,
  });
}

// ── Browser fallback (used when running outside Tauri) ──────────────────────

const browserStreamListeners: StreamTokenCallback[] = [];
const browserErrorListeners: StreamErrorCallback[] = [];

// Distinct from the providers store's own key (`lh.providers.v1` in
// providers.svelte.ts). The two layers persist DIFFERENT shapes
// (snake_case ProviderInfo[] here vs camelCase Provider[] in the store);
// sharing a key corrupts both in browser-dev mode.
const BROWSER_PROVIDERS_KEY = "lh.providers.browser.v1";
const BROWSER_CONVERSATIONS_KEY = "lh.conversations.v1";

function emitBrowserToken(payload: StreamTokenPayload): void {
  for (const cb of browserStreamListeners) cb(payload);
}

function emitBrowserError(payload: StreamErrorPayload): void {
  for (const cb of browserErrorListeners) cb(payload);
}

// ── Provider browser fallback ───────────────────────────────────────────────

function browserListProviders(): ProviderInfo[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const raw = localStorage.getItem(BROWSER_PROVIDERS_KEY);
    return raw ? (JSON.parse(raw) as ProviderInfo[]) : [];
  } catch {
    return [];
  }
}

function browserPersistProviders(list: ProviderInfo[]): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(BROWSER_PROVIDERS_KEY, JSON.stringify(list));
  } catch {
    // non-fatal
  }
}

function browserAddProvider(
  name: string,
  baseUrl: string,
  _apiKey: string | null,
  kind: string,
  supportsNativeTools: boolean,
): ProviderInfo {
  const id = crypto.randomUUID();
  const info: ProviderInfo = {
    id,
    name,
    base_url: baseUrl,
    kind,
    is_private: kind === "local",
    supports_native_tools: supportsNativeTools,
  };
  const list = browserListProviders();
  list.push(info);
  browserPersistProviders(list);
  return info;
}

function browserRemoveProvider(id: string): boolean {
  const list = browserListProviders();
  const next = list.filter((p) => p.id !== id);
  browserPersistProviders(next);
  return next.length < list.length;
}

function browserListModels(_providerId: string): string[] {
  // Browser fallback: return a minimal set so the picker isn't blank.
  return ["default"];
}

// ── Conversation browser fallback ───────────────────────────────────────────

function browserListConversations(): ConversationInfo[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const raw = localStorage.getItem(BROWSER_CONVERSATIONS_KEY);
    return raw ? (JSON.parse(raw) as ConversationInfo[]) : [];
  } catch {
    return [];
  }
}

function browserPersistConversations(list: ConversationInfo[]): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(BROWSER_CONVERSATIONS_KEY, JSON.stringify(list));
  } catch {
    // non-fatal
  }
}

function browserCreateConversation(name: string, binding: string): ConversationInfo {
  const now = Math.floor(Date.now() / 1000);
  const conv: ConversationInfo = {
    id: crypto.randomUUID(),
    name,
    pinned: false,
    binding,
    folder_id: null,
    color: null,
    created_at: now,
    updated_at: now,
  };
  const list = browserListConversations();
  list.unshift(conv);
  browserPersistConversations(list);
  return conv;
}

function browserGetMessages(_conversationId: string): MessageInfo[] {
  // Browser fallback: no persisted messages in the stub.
  return [];
}

// ── send_message browser fallback ───────────────────────────────────────────

async function browserSendMessage(
  content: string,
  conversationId: string,
): Promise<SendMessageResponse> {
  // Match the Rust stub: 500ms think, then a few token chunks ~30ms apart.
  await sleep(500);
  const replyBody = `Echo: "${content}". This is the M1 browser fallback — the real agent loop runs in the Tauri shell.`;
  const messageId = crypto.randomUUID();
  const tokens = chunkReply(replyBody);
  for (const token of tokens) {
    emitBrowserToken({ token, conversation_id: conversationId, message_id: messageId });
    await sleep(30);
  }
  return {
    message_id: messageId,
    content: replyBody,
    conversation_id: conversationId,
    profile: "personal",
    routing_decision: "allow",
    completed_at: Date.now(),
  };
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function chunkReply(text: string): string[] {
  // Mirror the Rust stub: split on whitespace boundaries, keeping the
  // whitespace attached to the preceding word, then group by 3 chunks.
  const pieces = text.split(/(?<=\s)/); // lookbehind keeps the trailing space
  const out: string[] = [];
  let buf = "";
  let count = 0;
  for (const piece of pieces) {
    buf += piece;
    count += 1;
    if (count >= 3) {
      out.push(buf);
      buf = "";
      count = 0;
    }
  }
  if (buf) out.push(buf);
  return out;
}

// ── Memory settings browser fallback ────────────────────────────────────────

const BROWSER_MEMORY_SETTINGS_PREFIX = "lh.memorySettings.v1.";

function browserGetMemorySettings(profile: string): MemorySettingsInfo {
  if (typeof localStorage === "undefined") return { ...DEFAULT_MEMORY_SETTINGS };
  try {
    const raw = localStorage.getItem(BROWSER_MEMORY_SETTINGS_PREFIX + profile);
    return raw ? { ...DEFAULT_MEMORY_SETTINGS, ...(JSON.parse(raw) as MemorySettingsInfo) } : { ...DEFAULT_MEMORY_SETTINGS };
  } catch {
    return { ...DEFAULT_MEMORY_SETTINGS };
  }
}

function browserSetMemorySettings(
  profile: string,
  settings: MemorySettingsInfo,
): MemorySettingsInfo {
  if (typeof localStorage !== "undefined") {
    try {
      localStorage.setItem(BROWSER_MEMORY_SETTINGS_PREFIX + profile, JSON.stringify(settings));
    } catch {
      // non-fatal
    }
  }
  return { ...settings };
}

// Exported for tests / mock-driven scenarios.
export { emitBrowserError };
