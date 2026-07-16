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
  // The buttons offered are driven by the tool's RISK class (Q8 grant×risk
  // matrix), served server-side on each request:
  //   - dangerous → Deny + Allow once only (never a standing grant — inv. #8)
  //   - external  → Deny + Allow once only (no whole-tool standing for egress)
  //   - write/safe → Deny + Allow once + Allow for this session
  // The server (`resolve_grant`) is the enforcement; hiding a button only stops
  // us from training a habit the matrix would then have to break — a bypassed
  // button still can't widen the grant. "Always" (persist across restarts)
  // lands with the SQLite tool_rules store (Q8 commit 3).

  import { onMount } from "svelte";
  import {
    onToolApprovalRequest,
    resolveToolApproval,
    type RiskClass,
    type ToolApprovalRequest,
  } from "$lib/api/tauri";

  let queue = $state<ToolApprovalRequest[]>([]);
  const current = $derived(queue[0] ?? null);
  // Guards against a double-click resolving the same request twice while the
  // async round-trip is in flight.
  let resolving = $state(false);

  // Risk → label + badge colors. Unknown/absent risk falls back to a neutral
  // treatment (a server that predates the risk field, or a future variant).
  const RISK_META: Record<RiskClass, { label: string; badge: string }> = {
    safe: { label: "Safe", badge: "border-emerald-500/40 bg-emerald-500/10 text-emerald-300" },
    write: { label: "Write", badge: "border-sky-500/40 bg-sky-500/10 text-sky-300" },
    external: { label: "External", badge: "border-amber-500/40 bg-amber-500/10 text-amber-300" },
    dangerous: { label: "Dangerous", badge: "border-red-500/50 bg-red-500/15 text-red-300" },
  };
  const riskMeta = $derived(
    (current && RISK_META[current.risk]) ?? {
      label: "Unknown",
      badge: "border-neutral-600 bg-neutral-800 text-neutral-300",
    },
  );
  // Only reversible, on-machine mutations get a whole-tool session grant.
  // Egress (external) and irreversible (dangerous) never do — every call
  // re-confirms.
  const allowSession = $derived(current?.risk === "write" || current?.risk === "safe");

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
      const delivered = await resolveToolApproval(current.id, decision, scope, target);
      if (!delivered) {
        // The backend had no such pending request — it was already answered or
        // it timed out and denied by default. The card is stale; just drop it.
        console.info("[approval] request already resolved or expired:", current.id);
      }
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
        <span
          class="ml-auto rounded-full border px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide {riskMeta.badge}"
          data-testid="approval-risk-badge"
          title="Risk class — decides what standing approval is allowed">{riskMeta.label}</span>
      </div>

      <p class="mb-2 text-sm text-neutral-300">
        The agent wants to use
        <code class="rounded bg-neutral-900 px-1 py-0.5 text-neutral-100">{current.tool_name}</code
        >:
      </p>
      <!-- The exact call, so the user can vet it. Untrusted, display-only:
           Svelte escapes it and it is never executed. -->
      <pre
        class="mb-3 max-h-32 overflow-auto whitespace-pre-wrap break-all rounded bg-neutral-900 p-2 text-[11px] leading-snug text-neutral-300"
        data-testid="approval-command">{current.command}</pre>
      {#if current.destination}
        <!-- Where an egress call goes IS the consent — surface it prominently. -->
        <p class="mb-3 text-xs text-amber-300" data-testid="approval-destination">
          Sends to <span class="font-medium">{current.destination}</span>
        </p>
      {/if}
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
        {#if allowSession}
          <!-- Secondary: allow every call to this tool this session. Offered
               only for reversible on-machine mutations (write/safe) — the
               matrix refuses whole-tool standing for external/dangerous. -->
          <button
            type="button"
            onclick={() => answer("approve", "session", "tool")}
            disabled={resolving}
            class="rounded-md border border-neutral-700 px-3 py-1.5 text-xs text-neutral-300 transition hover:bg-neutral-900 disabled:opacity-50"
            data-testid="approval-session"
          >
            Allow for this session
          </button>
        {/if}
        <!-- Primary (narrowest): just this one call. The default, so a hurried
             click grants the least. For dangerous/external this is the ONLY
             approve option. -->
        <button
          type="button"
          onclick={() => answer("approve", "once", "action")}
          disabled={resolving}
          class="rounded-md bg-neutral-200 px-3 py-1.5 text-xs font-medium text-neutral-900 transition hover:bg-white disabled:opacity-50"
          data-testid="approval-once"
        >
          Allow once
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
