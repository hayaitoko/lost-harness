<script lang="ts">
  // The blocking "ask the user" prompt. Always mounted at the app level;
  // renders an overlay only when the backend raises a `tool:ask_human_request`
  // event — i.e. the agent called the `ask_human` tool and the dispatch is
  // parked waiting for the user's answer (see src-tauri/src/ipc/ask_human.rs).
  //
  // Requests are queued so a burst never drops one; we show the head of the
  // queue and advance as each is answered. Submitting calls back into the Rust
  // core via `resolveAskHuman`, which unblocks the parked dispatch with the
  // typed text. "Skip" (or Esc) declines — the tool reports "not answered" and
  // the agent proceeds without an answer. If the user never responds, the
  // backend times out and declines by default.

  import { onMount } from "svelte";
  import { onAskHumanRequest, resolveAskHuman, type AskHumanRequest } from "$lib/api/tauri";

  let queue = $state<AskHumanRequest[]>([]);
  const current = $derived(queue[0] ?? null);
  let answer = $state("");
  // Guards against a double-submit resolving the same request twice while the
  // async round-trip is in flight.
  let resolving = $state(false);

  onMount(() => {
    let unlisten: (() => void) | undefined;
    onAskHumanRequest((req) => {
      queue = [...queue, req];
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  });

  function advance() {
    queue = queue.slice(1);
    answer = "";
    resolving = false;
  }

  async function respond(text: string | null) {
    if (!current || resolving) return;
    resolving = true;
    try {
      const delivered = await resolveAskHuman(current.id, text);
      if (!delivered) {
        console.info("[ask_human] request already resolved or expired:", current.id);
      }
    } catch (e) {
      console.warn("[ask_human] resolve failed", e);
    } finally {
      advance();
    }
  }

  function submit() {
    // An all-whitespace answer is a decline (the backend normalizes it too).
    respond(answer.trim() === "" ? null : answer);
  }

  function onKeydown(e: KeyboardEvent) {
    if (!current) return;
    if (e.key === "Escape") {
      e.preventDefault();
      void respond(null); // skip = decline
    }
    // Cmd/Ctrl+Enter submits from within the textarea.
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void submit();
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if current}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
    role="dialog"
    aria-modal="true"
    aria-labelledby="ask-human-title"
    data-testid="ask-human-dialog"
  >
    <div class="w-full max-w-md rounded-lg border border-neutral-700 bg-neutral-950 p-4 shadow-xl">
      <div class="mb-3 flex items-center gap-2">
        <svg class="h-4 w-4 text-sky-400" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M8 1.5a4 4 0 0 0-4 4M8 10.5v.01M8 8c1.5-.5 2-1.4 2-2.5A2 2 0 0 0 8 3.5"
            fill="none"
            stroke="currentColor"
            stroke-width="1.3"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        </svg>
        <h2 id="ask-human-title" class="text-sm font-semibold text-neutral-100">
          The assistant has a question
        </h2>
      </div>

      <!-- The question. Model-authored → untrusted; Svelte escapes it and it is
           rendered as text, never executed. -->
      <p
        class="mb-3 whitespace-pre-wrap break-words rounded bg-neutral-900 p-2 text-sm text-neutral-200"
        data-testid="ask-human-question"
      >
        {current.question}
      </p>

      <!-- svelte-ignore a11y_autofocus -->
      <textarea
        bind:value={answer}
        rows="3"
        autofocus
        placeholder="Type your answer… (⌘/Ctrl+Enter to send)"
        class="mb-3 w-full resize-y rounded border border-neutral-700 bg-neutral-900 p-2 text-sm text-neutral-100 placeholder:text-neutral-500 focus:border-sky-500 focus:outline-none"
        data-testid="ask-human-input"
      ></textarea>

      <div class="flex justify-end gap-2">
        <button
          type="button"
          onclick={() => respond(null)}
          disabled={resolving}
          class="rounded-md border border-neutral-700 px-3 py-1.5 text-xs text-neutral-300 transition hover:bg-neutral-900 disabled:opacity-50"
          data-testid="ask-human-skip"
        >
          Skip
        </button>
        <button
          type="button"
          onclick={submit}
          disabled={resolving}
          class="rounded-md border border-sky-500/60 bg-sky-500/15 px-3 py-1.5 text-xs font-medium text-sky-200 transition hover:bg-sky-500/25 disabled:opacity-50"
          data-testid="ask-human-submit"
        >
          Send answer
        </button>
      </div>
    </div>
  </div>
{/if}
