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
//   - `send_message(args: { content, conversation_id, binding, provider_id, model, profile }) -> SendMessageResponse`
//   - `add_provider(args: { name, base_url, api_key, kind }) -> ProviderInfo`
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
  prompt: string;
  /** Which hook raised it: "permission" | "first_use_confirm". */
  by: string;
  fingerprint: string;
}

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
): Promise<ProviderInfo> {
  if (isTauri()) {
    return tauriInvoke<ProviderInfo>("add_provider", {
      args: {
        name,
        base_url: baseUrl,
        api_key: apiKey || null,
        kind,
      },
    });
  }
  return browserAddProvider(name, baseUrl, apiKey, kind);
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
 * `scope` / `target` only matter for an approval. Returns false if the request
 * id is unknown — already answered, or it timed out and denied by default.
 */
export async function resolveToolApproval(
  id: string,
  decision: "approve" | "deny",
  scope: ApprovalScope = "once",
  target: ApprovalTarget = "action",
): Promise<boolean> {
  if (isTauri()) {
    return tauriInvoke<boolean>("resolve_tool_approval", {
      args: { id, decision, scope, target },
    });
  }
  return true;
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
): ProviderInfo {
  const id = crypto.randomUUID();
  const info: ProviderInfo = {
    id,
    name,
    base_url: baseUrl,
    kind,
    is_private: kind === "local",
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

// Exported for tests / mock-driven scenarios.
export { emitBrowserError };
