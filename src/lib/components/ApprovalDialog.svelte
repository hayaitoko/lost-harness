<script lang="ts">
  // Interactive tool-approval prompt. Always mounted at the app level; renders
  // an overlay only when the backend raises a `tool:approval_request` event
  // (i.e. a tool call hit an `Ask` in the gating chain and the dispatcher is
  // parked waiting for the user's answer — see src-tauri/src/ipc/approval.rs).
  //
  // Requests are queued so a burst never drops one; we show the head of the
  // queue and advance as each is answered. Answering calls back into the Rust
  // core via `resolveToolApproval`, which unblocks the parked dispatch. If the
  // user never answers, the backend times out and denies by default.
  //
  // Scopes offered here are "once" (this exact action) and "session" (any call
  // to this tool until restart). "Always" (persist across restarts) is
  // deliberately not offered yet — it needs the persistent policy store that
  // lands with M4.

  import { onMount } from "svelte";
  import {
    onToolApprovalRequest,
    resolveToolApproval,
    type ToolApprovalRequest,
  } from "$lib/api/tauri";

  let queue = $state<ToolApprovalRequest[]>([]);
  const current = $derived(queue[0] ?? null);
  // Guards against a double-click resolving the same request twice while the
  // async round-trip is in flight.
  let resolving = $state(false);

  onMount(() => {
    let unlisten: (() => void) | undefined;
    onToolApprovalRequest((req) => {
      queue = [...queue, req];
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  });

  function advance() {
    queue = queue.slice(1);
    resolving = false;
  }

  async function answer(
    decision: "approve" | "deny",
    scope: "once" | "session" = "once",
    target: "action" | "tool" = "action",
  ) {
    if (!current || resolving) return;
    resolving = true;
    try {
      await resolveToolApproval(current.id, decision, scope, target);
    } catch (e) {
      console.warn("[approval] resolve failed", e);
    } finally {
      advance();
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (!current) return;
    // Esc denies the current prompt (the safe default).
    if (e.key === "Escape") {
      e.preventDefault();
      void answer("deny");
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if current}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
    role="dialog"
    aria-modal="true"
    aria-labelledby="approval-title"
    data-testid="tool-approval-dialog"
  >
    <div
      class="w-full max-w-md rounded-lg border border-neutral-800 bg-neutral-950 p-5 shadow-2xl"
    >
      <div class="mb-3 flex items-center gap-2">
        <svg class="h-4 w-4 text-amber-400" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M8 1.5 1 14.5h14L8 1.5z M8 6 v4 M8 11.5 v0.5"
            fill="none"
            stroke="currentColor"
            stroke-width="1.3"
            stroke-linejoin="round"
            stroke-linecap="round"
          />
        </svg>
        <h2 id="approval-title" class="text-sm font-semibold text-neutral-100">
          Allow this tool to run?
        </h2>
      </div>

      <p class="mb-1 text-sm text-neutral-300">
        The agent wants to use
        <code class="rounded bg-neutral-900 px-1 py-0.5 text-neutral-100">{current.tool_name}</code
        >.
      </p>
      <p class="mb-4 text-xs text-neutral-500">{current.prompt}</p>

      <div class="flex flex-wrap justify-end gap-2">
        <button
          type="button"
          onclick={() => answer("deny")}
          disabled={resolving}
          class="rounded-md border border-neutral-700 px-3 py-1.5 text-xs text-neutral-300 transition hover:bg-neutral-900 disabled:opacity-50"
          data-testid="approval-deny"
        >
          Deny
        </button>
        <button
          type="button"
          onclick={() => answer("approve", "once", "action")}
          disabled={resolving}
          class="rounded-md border border-neutral-700 px-3 py-1.5 text-xs text-neutral-200 transition hover:bg-neutral-900 disabled:opacity-50"
          data-testid="approval-once"
        >
          Allow once
        </button>
        <button
          type="button"
          onclick={() => answer("approve", "session", "tool")}
          disabled={resolving}
          class="rounded-md bg-neutral-200 px-3 py-1.5 text-xs font-medium text-neutral-900 transition hover:bg-white disabled:opacity-50"
          data-testid="approval-session"
        >
          Allow for this session
        </button>
      </div>

      {#if queue.length > 1}
        <p class="mt-3 text-right text-[11px] text-neutral-600">
          {queue.length - 1} more waiting
        </p>
      {/if}
    </div>
  </div>
{/if}
