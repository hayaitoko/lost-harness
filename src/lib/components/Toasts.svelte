<script lang="ts">
  // The fixed toast stack (bottom-right) — renders `$lib/stores/toasts`.
  // Visual language mirrors PrivacyEventBar: a left accent border + soft tint
  // in the routing palette (`local` green / `warn` amber), grayscale chrome
  // otherwise. All content renders as escaped Svelte text — no `{@html}`.
  import { toasts, dismissToast } from "$lib/stores/toasts";

  const tone = {
    local: "bg-local-soft border-l-local text-local",
    warn: "bg-warn-soft border-l-warn text-warn",
  } as const;
</script>

{#if $toasts.length > 0}
  <div
    class="fixed bottom-4 right-4 z-50 flex w-[340px] flex-col gap-2"
    role="status"
    aria-live="polite"
  >
    {#each $toasts as t (t.id)}
      <div
        class="flex items-start gap-2 rounded-r-[var(--r)] border-l-2 bg-surface-2 px-[13px] py-[10px] shadow-lg {tone[
          t.kind
        ]}"
      >
        <div class="min-w-0 flex-1">
          <div class="text-[12.5px] font-medium">{t.title}</div>
          {#if t.body}
            <div class="mt-0.5 text-[12px] text-text-2">{t.body}</div>
          {/if}
        </div>
        <button
          class="flex-shrink-0 text-text-3 hover:text-text"
          aria-label="Dismiss notification"
          onclick={() => dismissToast(t.id)}
        >
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M18 6 6 18M6 6l12 12" />
          </svg>
        </button>
      </div>
    {/each}
  </div>
{/if}
