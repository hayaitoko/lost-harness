// Lost Harness — Chat state (Svelte stores).
//
// Exposes the conversation list, the active conversation id, and a derived
// "currently streaming" message. Components read these directly via the
// `$store` syntax. Mutations are exposed as plain functions (`sendMessage`,
// `createConversation`) so call sites don't reach into the stores directly.
//
// M1: hydrates from SQLite via the real IPC layer (`list_conversations`,
// `get_messages`, `create_conversation`). Streaming tokens arrive via the
// `stream:token` event; privacy-gate / routing / model failures arrive via
// `stream:error` and are surfaced inline as the assistant message body.

import { writable, derived, get, type Readable } from "svelte/store";
import * as api from "../api/tauri";
import type {
  SendMessageResponse,
  StreamTokenPayload,
  StreamErrorPayload,
  ConversationInfo,
  MessageInfo,
  ServedBy,
} from "../api/tauri";
import { getActiveProfileId } from "./profiles";

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
  /** Set when a stream:error landed for this message. */
  error?: string;
  /**
   * Source of the error: "gate" | "gate_confirm" | "routing" | "model" | null.
   *
   * H-12: `"gate_confirm"` is an ACTIONABLE hold, not a dead error — a
   * `Public`-bound message hit the un-tunable structured-secret floor and the
   * user may authorise one send (`confirmPublicSend` + re-send).
   */
  error_source?: string | null;
  /**
   * H-12: the user text this turn was holding, kept only for a
   * `"gate_confirm"` hold so the "send it once anyway" affordance can re-send
   * byte-identical text (the confirmation is fingerprinted over it).
   */
  held_content?: string | null;
  /**
   * The §7 gate's decision for this turn: "allow" | "route_local" | "block".
   * `null`/undefined while a fresh send hasn't resolved yet. A `Block`
   * decision never gets a persisted row (see loop_mod.rs), so this is only
   * ever "block" on a message that's live in this session — hydrated
   * history will show "allow" or "route_local".
   */
  routing_decision?: string | null;
  /** Model name that served this turn, when known. */
  model?: string | null;
  /**
   * Provider id that served this turn. Identity only — do NOT cross-reference
   * `providersStore` to work out the trust zone: that reads the registry as it
   * is now, and a provider that has since been edited or deleted would rewrite
   * what a past turn was. The zone comes stamped on `served_by.zone`.
   */
  provider_id?: string | null;
  /**
   * The endpoint that ACTUALLY served this turn (provider id + name +
   * base URL), from the persisted assistant row.
   *
   * Until the send resolves this is undefined and `provider_id` holds the
   * composer's pre-send prediction. Per docs/TECH-DEBT.md §1 the final
   * authoritative state wins: on a privacy reroute the serving endpoint is a
   * DIFFERENT provider than the composer picked, and this is what says so.
   */
  served_by?: ServedBy | null;
}

export interface Conversation {
  id: string;
  name: string;
  pinned: boolean;
  /** Default sensitivity routing for this chat. */
  binding: Binding;
  messages: Message[];
  /** True after `hydrateMessages` has loaded the transcript from the backend. */
  hydrated: boolean;
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

// ── Hydration from backend ──────────────────────────────────────────────────

/**
 * Loads the conversation list from the backend (or browser fallback) into
 * the store. Call once on app start. Idempotent — replaces the entire list.
 */
export async function hydrateConversations(): Promise<void> {
  try {
    const profile = getActiveProfileId();
    const remote = await api.listConversations(profile);
    const existing = get(conversations);
    const byId = new Map(existing.map((c) => [c.id, c]));
    // Merge, don't wipe: refresh metadata from the backend but carry over
    // any messages/hydrated state already loaded locally (or an in-flight
    // send). A wholesale replace here would blank out a transcript that
    // hydrateMessages() had already populated for the active conversation.
    const mapped: Conversation[] = remote.map((info) => {
      const base = convFromInfo(info);
      const prev = byId.get(info.id);
      if (prev && (prev.hydrated || prev.messages.length > 0)) {
        return { ...base, messages: prev.messages, hydrated: prev.hydrated };
      }
      return base;
    });
    // Only the browser fallback owns local-only conversations. Keeping an
    // unknown local row in the installed app would resurrect a failed create
    // as though it had been durably saved.
    const localOnly = api.isTauriRuntime()
      ? []
      : existing.filter((c) => !remote.some((m) => m.id === c.id));
    const next = [...localOnly, ...mapped];
    conversations.set(next);

    // Cold start: if nothing is selected yet, select the first conversation
    // and load its transcript so the main panel doesn't render blank.
    if (!get(activeConversationId) && next.length > 0) {
      activeConversationId.set(next[0].id);
      await hydrateMessages(next[0].id);
    }
  } catch (err) {
    console.error("hydrateConversations failed", err);
  }
}

/**
 * Loads messages for a conversation from the backend if not already loaded.
 * Call when the user switches to a conversation. No-op if already hydrated.
 */
export async function hydrateMessages(conversationId: string): Promise<void> {
  const conv = get(conversations).find((c) => c.id === conversationId);
  if (!conv?.hydrated) {
    try {
      const profile = getActiveProfileId();
      const remote = await api.getMessages(conversationId, profile);
      const messages = remote.map(msgFromInfo);
      conversations.update((list) =>
        list.map((c) =>
          c.id === conversationId
            ? { ...c, messages, hydrated: true }
            : c,
        ),
      );
    } catch (err) {
      console.error("hydrateMessages failed", err);
    }
  }
}

/** Update the active chat's routing intent only after the profile-scoped
 * backend write succeeds. This prevents a restarted app from silently
 * reverting a choice the UI had presented as saved. */
export async function setConversationBinding(
  conversationId: string,
  binding: Binding,
): Promise<void> {
  const info = await api.setConversationBinding(
    conversationId,
    getActiveProfileId(),
    binding,
  );
  conversations.update((list) =>
    list.map((conversation) =>
      conversation.id === conversationId
        ? { ...conversation, binding: info.binding as Binding }
        : conversation,
    ),
  );
}

/**
 * Creates a conversation via the backend IPC and inserts it into the store.
 * Returns the new conversation's id. Falls back to the local-only path in
 * browser mode (where `api.createConversation` returns a mock ConversationInfo).
 */
export async function createConversationViaBackend(
  name?: string,
  binding: Binding = "auto",
): Promise<string> {
  const profile = getActiveProfileId();
  const displayName = name ?? defaultName();
  try {
    const info = await api.createConversation(displayName, profile, binding);
    const conv: Conversation = convFromInfo(info);
    conversations.update((list) => [conv, ...list]);
    activeConversationId.set(conv.id);
    return conv.id;
  } catch (err) {
    console.error("createConversationViaBackend failed", err);
    if (api.isTauriRuntime()) throw err;
    return createConversation(displayName, binding);
  }
}

// ── Local-only conversation creation (browser fallback / error path) ───────

/**
 * Creates a new empty conversation locally (no backend round-trip), inserts
 * it at the top of the list, and marks it active. Returns the new id.
 */
export function createConversation(name?: string, binding: Binding = "auto"): string {
  const id = newId();
  const conv: Conversation = {
    id,
    name: name ?? defaultName(),
    pinned: false,
    binding,
    messages: [],
    hydrated: true, // locally created — nothing to fetch from backend
    created_at: Date.now(),
  };
  conversations.update((list) => [conv, ...list]);
  activeConversationId.set(id);
  return id;
}

// ── Send message ────────────────────────────────────────────────────────────

/**
 * Sends `content` from the user in the currently active conversation. The
 * flow:
 *   1. Append a user message and a pending (streaming) assistant message
 *      to the active conversation.
 *   2. Call the Rust `send_message` command (or browser fallback), passing
 *      the conversation's binding, the selected provider/model, and the
 *      active profile.
 *   3. Listen for `stream:token` events, appending each to the assistant
 *      message's content. Also listen for `stream:error` events and surface
 *      them inline.
 *   4. When `send_message` resolves, mark the assistant message as done
 *      (and replace its content with the canonical final text from the
 *      response, in case any tokens were dropped).
 *
 * If there is no active conversation, this creates one first (via the
 * backend if available).
 */
export async function sendMessage(
  content: string,
  providerId: string | null,
  model: string | null,
  bindingOverride?: Binding,
  mode: string = "normal",
): Promise<void> {
  if (!content.trim()) return;

  // Fail closed on the endpoint, BEFORE anything is written to the store or
  // sent anywhere. This used to coerce a missing selection to `""` and let the
  // backend reject it — which meant a half-configured composer produced a
  // turn, a message row, and an error, instead of simply refusing.
  //
  // Wording mirrors `NO_ENDPOINT_SELECTED` / `NO_MODEL_SELECTED` in
  // src-tauri/src/agent/loop_mod.rs so the user reads one sentence regardless
  // of which layer caught it.
  if (!providerId || !providerId.trim()) {
    throw new Error("no model endpoint is selected — pick a model in the composer");
  }
  if (!model || !model.trim()) {
    throw new Error("no model is selected for this endpoint — pick a model in the composer");
  }

  // Ensure we have an active conversation.
  let activeId = get(activeConversationId);
  if (!activeId) {
    activeId = await createConversationViaBackend(undefined, bindingOverride ?? "auto");
  }
  const conversationId = activeId;

  // Resolve the binding: an explicit per-send override (e.g. a composer's
  // Auto/Public/Private control) wins over the conversation's stored default.
  const conv = get(conversations).find((c) => c.id === conversationId);
  const binding = bindingOverride ?? conv?.binding ?? "auto";

  // Resolve profile.
  const profile = getActiveProfileId();

  // Guaranteed non-blank by the precondition above — passed through verbatim,
  // never substituted, so what the picker selected is exactly what the IPC
  // call carries.
  const providerIdArg = providerId;
  const modelArg = model;

  // Append the user message + a pending assistant message.
  const userMsg: Message = {
    id: newId(),
    role: "user",
    content: content.trim(),
    created_at: Date.now(),
    streaming: false,
  };
  const assistantId = newId();
  const assistantMsg: Message = {
    id: assistantId,
    role: "assistant",
    content: "",
    created_at: Date.now() + 1,
    streaming: true,
    model: modelArg,
    // The composer's PREDICTION of where this turn will go. Replaced by the
    // authoritative `served_by` the moment the send resolves.
    provider_id: providerIdArg,
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

  // Subscribe to stream:token + stream:error events for the duration of
  // this send. We capture the assistant id in the closure so we can patch
  // the right message even if the user switches conversations mid-stream.
  let resolvedMessageId: string | null = null;
  let streamError: { error: string; source: string } | null = null;

  const unlistenToken = await api.onStreamToken((payload: StreamTokenPayload) => {
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

  const unlistenError = await api.onStreamError((payload: StreamErrorPayload) => {
    if (payload.conversation_id !== conversationId) return;
    streamError = { error: payload.error, source: payload.source };
    // Surface the error immediately on the assistant message.
    const targetId = resolvedMessageId ?? assistantId;
    setErrorOnMessage(conversationId, targetId, payload.error, payload.source);
  });

  try {
    const response: SendMessageResponse = await api.sendMessage(
      content.trim(),
      conversationId,
      binding,
      providerIdArg,
      modelArg,
      profile,
      mode,
    );
    if (streamError) {
      // A stream:error arrived while send_message still resolved Ok — this
      // is the privacy-gate Block path. The backend persisted NO message row
      // for this turn, so response.message_id points at an unrelated earlier
      // assistant message (or a throwaway uuid). Do NOT adopt it — keep the
      // local placeholder id to avoid a duplicate-key collision in the store.
      const { error, source } = streamError;
      const targetId = resolvedMessageId ?? assistantId;
      // A gate-sourced stream:error IS the "block" decision — the backend
      // never persists a routing_decision for it (no row is written), so
      // this live flag is the only place that fact is ever recorded.
      // H-12: `"gate_confirm"` is the same "nothing was sent" shape, but it is
      // recoverable — stash the exact held text so the inline affordance can
      // re-send it byte-identically after `confirmPublicSend` (the grant is
      // fingerprinted over the text, so a re-typed variant would not match).
      finalizeMessage(
        conversationId,
        targetId,
        `⚠ ${error}`,
        error,
        source,
        source === "gate" || source === "gate_confirm" ? "block" : undefined,
        source === "gate_confirm" ? content.trim() : undefined,
      );
    } else {
      // Success path. If no token stream established the id yet (e.g. an
      // empty completion), adopt the canonical server id now. Only reached
      // when the backend actually persisted an assistant row.
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
      // Clear streaming flag and (defensively) reconcile content to the
      // canonical response text in case a token was lost. Also stamp the
      // real routing_decision ("allow" | "route_local") the backend used.
      finalizeMessage(
        conversationId,
        resolvedMessageId,
        response.content,
        undefined,
        undefined,
        response.routing_decision,
        undefined,
        // ...and the endpoint that actually served it, which overrides the
        // composer's pre-send prediction.
        response.served_by,
      );
    }
  } catch (err) {
    // Surface the error inline rather than throwing — the chat panel
    // shows it as the assistant message body.
    const msg = err instanceof Error ? err.message : String(err);
    finalizeMessage(
      conversationId,
      resolvedMessageId ?? assistantId,
      `⚠ ${msg}`,
      msg,
      "model",
    );
  } finally {
    await unlistenToken();
    await unlistenError();
    streamingMessage.set(null);
  }
}

/**
 * Cancels the in-flight stream (C7 cooperative cancel). Asks the backend to
 * stop the active turn for the streaming conversation; the backend persists
 * the partial with `aborted:true` and the stream ends, which resolves the
 * pending `sendMessage` via its normal completion path. Safe no-op when
 * nothing is streaming.
 */
export async function cancelActiveStream(): Promise<void> {
  const current = get(streamingMessage);
  if (!current) return;
  try {
    await api.cancelMessage(current.conversationId);
  } catch (err) {
    // Cancel is best-effort — a failure just means the stream runs on.
    console.warn("cancel_message failed:", err);
  }
}

// ── Internal helpers ────────────────────────────────────────────────────────

function convFromInfo(info: ConversationInfo): Conversation {
  return {
    id: info.id,
    name: info.name,
    pinned: info.pinned,
    binding: normalizeBinding(info.binding),
    messages: [],
    hydrated: false,
    created_at: info.created_at * 1000, // backend is seconds, frontend is ms
  };
}

function msgFromInfo(info: MessageInfo): Message {
  const role: MessageRole = info.role === "assistant" ? "assistant" : "user";
  return {
    id: info.id,
    role,
    content: info.error ? `⚠ ${info.error}` : info.content,
    created_at: info.created_at * 1000,
    streaming: false,
    error: info.error ?? undefined,
    error_source: null,
    routing_decision: info.routing_decision,
    model: info.model,
    provider_id: info.provider_id,
    served_by: info.served_by ?? null,
  };
}

function normalizeBinding(b: string): Binding {
  if (b === "public" || b === "private") return b;
  return "auto";
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

function setErrorOnMessage(
  conversationId: string,
  messageId: string,
  error: string,
  source: string,
): void {
  conversations.update((list) =>
    list.map((c) =>
      c.id === conversationId
        ? {
            ...c,
            messages: c.messages.map((m) =>
              m.id === messageId
                ? { ...m, content: `⚠ ${error}`, error, error_source: source }
                : m,
            ),
          }
        : c,
    ),
  );
}

function finalizeMessage(
  conversationId: string,
  messageId: string,
  finalContent: string,
  error?: string,
  errorSource?: string,
  routingDecision?: string,
  heldContent?: string,
  servedBy?: ServedBy | null,
): void {
  conversations.update((list) =>
    list.map((c) =>
      c.id === conversationId
        ? {
            ...c,
            messages: c.messages.map((m) =>
              m.id === messageId
                ? {
                    ...m,
                    content: finalContent,
                    streaming: false,
                    error: error ?? m.error,
                    error_source: errorSource ?? m.error_source,
                    routing_decision: routingDecision ?? m.routing_decision,
                    held_content: heldContent ?? m.held_content,
                    ...endpointPatch(m, servedBy),
                  }
                : m,
            ),
          }
        : c,
    ),
  );
  streamingMessage.update((s) => (s ? null : s));
}

/**
 * docs/TECH-DEBT.md §1: the FINAL authoritative route state wins over the
 * pre-send prediction.
 *
 * When the backend reports which endpoint served the turn, that replaces the
 * composer's guess. If it names a DIFFERENT provider than the composer picked
 * — a privacy reroute or a redacted send — the predicted model name is
 * dropped too: it was the model on the other endpoint, and displaying it
 * beside the real provider would be a small, confident lie. The reloaded
 * transcript carries the true persisted model.
 */
function endpointPatch(
  m: Message,
  servedBy: ServedBy | null | undefined,
): Partial<Message> {
  if (!servedBy) return {};
  const rerouted = servedBy.provider_id !== m.provider_id;
  return {
    served_by: servedBy,
    provider_id: servedBy.provider_id,
    ...(rerouted ? { model: null } : {}),
  };
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
