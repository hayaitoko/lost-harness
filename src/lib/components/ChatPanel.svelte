<script lang="ts">
  // Lost Harness — Chat panel (Svelte 5 runes).
  //
  // Renders the message list for the active conversation, the input area,
  // and the streaming indicator. Reads state from the chat store and the
  // settings store; mutates only via the store's exported functions.
  //
  // Layout:
  //   ┌───────────────────────────────┐
  //   │ [PrivacyIndicator]             │  ← routing chip for the active chat
  //   │ message list (scrollable)      │
  //   │   …                            │
  //   │   [user]   hello              │
  //   │   [assist] hi there…           │   ← streaming indicator on last msg
  //   ├───────────────────────────────┤
  //   │ [textarea] [ModelPicker] [Send]│
  //   └───────────────────────────────┘

  import {
    activeConversation,
    sendMessage as sendChatMessage,
    streamingMessage,
    type Message,
  } from "$lib/stores/chat";
  import { providersStore } from "$lib/stores/providers.svelte";
  import { sendOnEnter } from "$lib/stores/settings";
  import PrivacyIndicator from "./PrivacyIndicator.svelte";
  import ModelPicker from "./ModelPicker.svelte";

  // Local component state. `let x = $state(...)` makes it reactive.
  let draft = $state("");
  let isSending = $state(false);
  let textareaEl: HTMLTextAreaElement | null = $state(null);

  // Conversations are hydrated once at app start (App.svelte). This panel
  // just reads the store.

  // Derived: is the assistant currently streaming? `streamingMessage` is a
  // store — read its value with the `$` prefix, not the store object itself
  // (a store object is never null, which would pin this to `true`).
  let isStreaming = $derived($streamingMessage !== null);

  // Privacy routing decision for the active conversation. Stub: derived
  // directly from the binding so the chip is never empty. The real M1
  // wires the §7 gate / TRM interface here and returns "allow" |
  // "route_local" | "block" based on the message's sensitivity +
  // provider reachability.
  let privacyDecision = $derived.by<
    null | "allow" | "route_local" | "block"
  >(() => {
    const conv = $activeConversation;
    if (!conv) return null;
    if (conv.binding === "private") return "route_local";
    if (conv.binding === "public") return "allow";
    return null; // auto: no decision yet
  });

  // Auto-grow the textarea up to ~6 lines.
  function autoresize() {
    if (!textareaEl) return;
    textareaEl.style.height = "auto";
    textareaEl.style.height =
      Math.min(textareaEl.scrollHeight, 160) + "px";
  }

  async function handleSend() {
    const content = draft.trim();
    if (!content || isSending) return;
    isSending = true;
    draft = "";
    autoresize();
    try {
      await sendChatMessage(
        content,
        providersStore.activeProviderId,
        providersStore.activeModel,
      );
    } finally {
      isSending = false;
      // Refocus the input so the next message can be typed immediately.
      textareaEl?.focus();
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !$sendOnEnter) {
      // sendOnEnter=false → Enter inserts newline, do nothing special.
      return;
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  }

  // The streaming indicator belongs on whichever message is the current
  // streaming target. The store sets messageId to the assistant message
  // id; we look it up in the active conversation's messages.
  // Uses the auto-subscribed `$streamingMessage` value.
  function isStreamingMessage(m: Message): boolean {
    const s = $streamingMessage;
    if (!s) return false;
    return m.id === s.messageId && m.role === "assistant";
  }

  // Quick message formatter: collapse extra newlines, escape HTML-light.
  // M2 will swap in a markdown renderer.
  function formatContent(c: string): string {
    return c;
  }
</script>

<section class="chat-panel flex h-full min-h-0 flex-col">
  {#if !$activeConversation}
    <div class="flex flex-1 items-center justify-center p-8 text-center">
      <div class="max-w-sm space-y-2 text-neutral-400">
        <h2 class="text-lg font-medium text-neutral-200">No conversation yet</h2>
        <p class="text-sm">
          Click <span class="font-medium text-neutral-300">New chat</span> in
          the sidebar to start one.
        </p>
      </div>
    </div>
  {:else}
    <!-- Privacy routing chip, anchored above the message list. -->
    <div
      class="flex items-center justify-between border-b border-neutral-800/60 bg-neutral-950/40 px-4 py-2"
      data-testid="chat-privacy-bar"
    >
      <PrivacyIndicator
        binding={$activeConversation?.binding ?? "auto"}
        decision={privacyDecision}
      />
    </div>

    <!-- Message list -->
    <div
      class="messages flex-1 overflow-y-auto px-4 py-6"
      data-testid="message-list"
    >
      <div class="mx-auto flex max-w-3xl flex-col gap-4">
        {#each $activeConversation.messages as m (m.id)}
          {@const streaming = isStreamingMessage(m)}
          {@const isError = !!m.error}
          <div
            class="msg flex"
            class:msg-user={m.role === "user"}
            class:msg-assistant={m.role === "assistant"}
            class:msg-error={isError}
            data-role={m.role}
            data-error={isError ? "true" : undefined}
          >
            <div
              class="msg-bubble whitespace-pre-wrap rounded-2xl px-4 py-2.5 text-sm leading-relaxed"
            >
              <span class="msg-content">{formatContent(m.content)}</span>
              {#if streaming}
                <span
                  class="streaming-dots ml-1 inline-flex gap-0.5 align-baseline"
                  aria-label="Assistant is typing"
                >
                  <span class="dot"></span>
                  <span class="dot"></span>
                  <span class="dot"></span>
                </span>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    </div>

    <!-- Input area -->
    <div
      class="input-area border-t border-neutral-800 bg-neutral-950/60 px-4 py-3 backdrop-blur"
    >
      <form
        class="mx-auto flex max-w-3xl items-end gap-2"
        onsubmit={(e) => {
          e.preventDefault();
          handleSend();
        }}
      >
        <textarea
          bind:this={textareaEl}
          bind:value={draft}
          onkeydown={handleKeydown}
          oninput={autoresize}
          rows="1"
          placeholder="Send a message…"
          aria-label="Message"
          class="flex-1 resize-none rounded-xl border border-neutral-800 bg-neutral-900 px-3 py-2 text-sm text-neutral-100 placeholder:text-neutral-500 focus:border-indigo-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
        ></textarea>
        <ModelPicker />
        <button
          type="submit"
          disabled={isSending || draft.trim().length === 0}
          class="rounded-xl bg-indigo-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {isSending || isStreaming ? "Sending…" : "Send"}
        </button>
      </form>
      {#if isStreaming}
        <p
          class="mx-auto mt-1 max-w-3xl text-[10px] uppercase tracking-wider text-neutral-500"
        >
          streaming
        </p>
      {/if}
    </div>
  {/if}
</section>

<style>
  /* Message alignment. User messages right, assistant messages left. */
  .msg-user {
    justify-content: flex-end;
  }
  .msg-user .msg-bubble {
    background: var(--accent, #6366f1);
    color: white;
    max-width: 80%;
  }
  .msg-assistant {
    justify-content: flex-start;
  }
  .msg-assistant .msg-bubble {
    background: rgb(38 38 42);
    color: rgb(245 245 245);
    max-width: 85%;
  }

  /* Error messages (stream:error from privacy gate / routing / model). */
  .msg-error .msg-bubble {
    background: rgb(67 20 30);
    border: 1px solid rgb(127 29 29 / 0.5);
    color: rgb(252 165 165);
    max-width: 90%;
  }

  /* Streaming indicator — three pulsing dots. */
  .streaming-dots .dot {
    display: inline-block;
    width: 4px;
    height: 4px;
    border-radius: 9999px;
    background: currentColor;
    opacity: 0.4;
    animation: dot-pulse 1.2s infinite ease-in-out;
  }
  .streaming-dots .dot:nth-child(2) {
    animation-delay: 0.15s;
  }
  .streaming-dots .dot:nth-child(3) {
    animation-delay: 0.3s;
  }
  @keyframes dot-pulse {
    0%,
    80%,
    100% {
      opacity: 0.3;
      transform: translateY(0);
    }
    40% {
      opacity: 1;
      transform: translateY(-2px);
    }
  }

  /* Subtle scrollbar. */
  .messages {
    scrollbar-width: thin;
    scrollbar-color: rgb(64 64 70) transparent;
  }
  .messages::-webkit-scrollbar {
    width: 6px;
  }
  .messages::-webkit-scrollbar-thumb {
    background: rgb(64 64 70);
    border-radius: 9999px;
  }
</style>
