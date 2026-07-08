// Lost Harness — Chat state (Svelte stores).
//
// Exposes the conversation list, the active conversation id, and a derived
// "currently streaming" message. Components read these directly via the
// `$store` syntax. Mutations are exposed as plain functions (`sendMessage`,
// `createConversation`) so call sites don't reach into the stores directly.
//
// Storage: the store is in-memory only for the M1 stub. The real M1 will
// hydrate from SQLite (`storage::storage`) and persist on every mutation.

import { writable, derived, get, type Readable } from "svelte/store";
import * as api from "../api/tauri";
import type { SendMessageResponse, StreamTokenPayload } from "../api/tauri";

// ── Types ───────────────────────────────────────────────────────────────────

export type Binding = "auto" | "public" | "private";
export type MessageRole = "user" | "assistant";

export interface Message {
  id: string;
  role: MessageRole;
  content: string;
  /** ms epoch — used for ordering and stream:token routing. */
  created_at: number;
  /** While true, the assistant message is still being streamed. */
  streaming: boolean;
}

export interface Conversation {
  id: string;
  name: string;
  pinned: boolean;
  /** Default sensitivity routing for this chat. Real M1: TRM + binding cycle. */
  binding: Binding;
  messages: Message[];
  created_at: number;
}

export interface StreamingMessageView {
  conversationId: string;
  messageId: string;
  partial: string;
}

// ── Stores ──────────────────────────────────────────────────────────────────

/** All conversations, in display order (pinned first, then newest first). */
export const conversations = writable<Conversation[]>([]);

/** Id of the conversation currently shown in the main panel. */
export const activeConversationId = writable<string | null>(null);

/**
 * The assistant message currently being streamed (if any). `null` when no
 * stream is in progress. Components use this to render the animated "…"
 * indicator and to know which message the next token belongs to.
 */
export const streamingMessage = writable<StreamingMessageView | null>(null);

// Derived: the conversation object matching `activeConversationId`. Falls
// back to the first conversation, then to null when there are no chats yet.
export const activeConversation: Readable<Conversation | null> = derived(
  [conversations, activeConversationId],
  ([$conversations, $activeId]) => {
    if ($conversations.length === 0) return null;
    if ($activeId) {
      return $conversations.find((c) => c.id === $activeId) ?? null;
    }
    return $conversations[0];
  },
);

// ── ID helpers ──────────────────────────────────────────────────────────────
// We use crypto.randomUUID where available (browsers, modern node) and fall
// back to a small pseudo-uuid for older runtimes / SSR.

function newId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return "id-" + Math.random().toString(36).slice(2) + Date.now().toString(36);
}

// ── Mutations ───────────────────────────────────────────────────────────────

/**
 * Creates a new empty conversation, inserts it at the top of the list, and
 * marks it active. Returns the new conversation's id.
 */
export function createConversation(name?: string): string {
  const id = newId();
  const conv: Conversation = {
    id,
    name: name ?? defaultName(),
    pinned: false,
    binding: "auto",
    messages: [],
    created_at: Date.now(),
  };
  conversations.update((list) => [conv, ...list]);
  activeConversationId.set(id);
  return id;
}

/**
 * Sends `content` from the user in the currently active conversation. The
 * flow:
 *   1. Append a user message and a pending (streaming) assistant message
 *      to the active conversation.
 *   2. Call the Rust `send_message` command (or browser fallback).
 *   3. Listen for `stream:token` events, appending each to the assistant
 *      message's content.
 *   4. When `send_message` resolves, mark the assistant message as done
 *      (and replace its content with the canonical final text from the
 *      response, in case any tokens were dropped).
 *
 * If there is no active conversation, this creates one first.
 */
export async function sendMessage(content: string): Promise<void> {
  if (!content.trim()) return;

  // Ensure we have an active conversation.
  let activeId = get(activeConversationId);
  if (!activeId) {
    activeId = createConversation();
  }
  const conversationId = activeId;

  // Append the user message + a pending assistant message.
  const userMsg: Message = {
    id: newId(),
    role: "user",
    content: content.trim(),
    created_at: Date.now(),
    streaming: false,
  };
  // We don't yet know the server-assigned id; use a placeholder that we
  // swap to the real one on the first token (or on the response, whichever
  // arrives first). Tracking by index avoids needing a sync.
  const assistantId = newId();
  const assistantMsg: Message = {
    id: assistantId,
    role: "assistant",
    content: "",
    created_at: Date.now() + 1,
    streaming: true,
  };

  conversations.update((list) =>
    list.map((c) =>
      c.id === conversationId
        ? { ...c, messages: [...c.messages, userMsg, assistantMsg] }
        : c,
    ),
  );
  streamingMessage.set({
    conversationId,
    messageId: assistantId,
    partial: "",
  });

  // Subscribe to stream:token events for the duration of this send. We
  // capture the assistant id in the closure so we can patch the right
  // message even if the user switches conversations mid-stream.
  let resolvedMessageId: string | null = null;
  const unlisten = await api.onStreamToken((payload: StreamTokenPayload) => {
    if (payload.conversation_id !== conversationId) return;
    // The first token may carry the canonical server-assigned id. Adopt
    // it so subsequent appends land on the same message row.
    if (!resolvedMessageId) {
      resolvedMessageId = payload.message_id;
      conversations.update((list) =>
        list.map((c) =>
          c.id === conversationId
            ? {
                ...c,
                messages: c.messages.map((m) =>
                  m.id === assistantId ? { ...m, id: payload.message_id } : m,
                ),
              }
            : c,
        ),
      );
    } else if (payload.message_id !== resolvedMessageId) {
      // A different stream arrived in the same conversation — drop it; the
      // active send owns the assistant message row for now.
      return;
    }
    appendToken(conversationId, resolvedMessageId, payload.token);
  });

  try {
    const response: SendMessageResponse = await api.sendMessage(
      content.trim(),
      conversationId,
    );
    // The Rust stub may not have sent any tokens (e.g. if the channel was
    // closed before the first emit). In that case adopt the server id here.
    if (!resolvedMessageId) {
      resolvedMessageId = response.message_id;
      conversations.update((list) =>
        list.map((c) =>
          c.id === conversationId
            ? {
                ...c,
                messages: c.messages.map((m) =>
                  m.id === assistantId ? { ...m, id: response.message_id } : m,
                ),
              }
            : c,
        ),
      );
    }
    // Finalize: clear streaming flag and (defensively) reconcile content
    // to the canonical response content in case a token was lost.
    finalizeMessage(conversationId, response.message_id, response.content);
  } catch (err) {
    // Surface the error inline rather than throwing — the chat panel
    // shows it as the assistant message body.
    const msg = err instanceof Error ? err.message : String(err);
    finalizeMessage(
      conversationId,
      resolvedMessageId ?? assistantId,
      `⚠ ${msg}`,
    );
  } finally {
    await unlisten();
    streamingMessage.set(null);
  }
}

function appendToken(
  conversationId: string,
  messageId: string,
  token: string,
): void {
  conversations.update((list) =>
    list.map((c) =>
      c.id === conversationId
        ? {
            ...c,
            messages: c.messages.map((m) =>
              m.id === messageId
                ? { ...m, content: m.content + token }
                : m,
            ),
          }
        : c,
    ),
  );
  streamingMessage.update((s) =>
    s && s.messageId === messageId ? { ...s, partial: s.partial + token } : s,
  );
}

function finalizeMessage(
  conversationId: string,
  messageId: string,
  finalContent: string,
): void {
  conversations.update((list) =>
    list.map((c) =>
      c.id === conversationId
        ? {
            ...c,
            messages: c.messages.map((m) =>
              m.id === messageId
                ? { ...m, content: finalContent, streaming: false }
                : m,
            ),
          }
        : c,
    ),
  );
  streamingMessage.update((s) => (s ? null : s));
}

function defaultName(): string {
  const ts = new Date();
  // "Chat · Jul 7, 6:42 PM" — locale-friendly, no extra deps.
  return `Chat · ${ts.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  })}`;
}
