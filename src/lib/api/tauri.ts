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
//   - `send_message(content, conversation_id) -> SendMessageResponse`
//   - `stream_token` event: payload = { token, conversation_id, message_id }
//
// If you add or rename a command or event on the Rust side, update the
// matching type, constant, or function below. The TypeScript types here
// are the source of truth for the frontend.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";

// ── Shared types (mirror the Rust serde structs) ────────────────────────────

export interface SendMessageResponse {
  message_id: string;
  content: string;
  conversation_id: string;
  /** Profile id that handled the message ("personal" until TRM is wired). */
  profile: string;
  /** Epoch ms the response was finalized. */
  completed_at: number;
}

export interface StreamTokenPayload {
  token: string;
  conversation_id: string;
  message_id: string;
}

/** Callback shape for `onStreamToken`. */
export type StreamTokenCallback = (payload: StreamTokenPayload) => void;

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

// ── Event channel name (must match Rust `app.emit("stream:token", ...)`) ────

export const STREAM_TOKEN_EVENT = "stream:token";

// ── Command functions ───────────────────────────────────────────────────────

/** Returns the app version string, e.g. "0.1.0-m0". */
export async function getAppVersion(): Promise<string> {
  if (isTauri()) {
    return tauriInvoke<string>("get_app_version");
  }
  return "0.1.0-m0-browser";
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
): Promise<SendMessageResponse> {
  if (isTauri()) {
    return tauriInvoke<SendMessageResponse>("send_message", {
      content,
      conversationId,
    });
  }
  return browserSendMessage(content, conversationId);
}

/**
 * Subscribes to `stream:token` events. Returns an unlisten function that
 * detaches the listener. In browser mode, this is a no-op subscription —
 * the mock `sendMessage` already drives the same callback path internally.
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

// ── Browser fallback (used when running outside Tauri) ──────────────────────

const browserStreamListeners: StreamTokenCallback[] = [];

function emitBrowserToken(payload: StreamTokenPayload): void {
  for (const cb of browserStreamListeners) cb(payload);
}

async function browserSendMessage(
  content: string,
  conversationId: string,
): Promise<SendMessageResponse> {
  // Match the Rust stub: 500ms think, then a few token chunks ~30ms apart.
  await sleep(500);
  const replyBody = `Echo: "${content}". This is the M1 stub reply — the real agent loop, TRM classification, and model streaming land in subsequent milestones.`;
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
