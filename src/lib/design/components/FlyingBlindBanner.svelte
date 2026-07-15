<script lang="ts">
  // Small self-contained amber alert: cost for the current period (or a
  // specific run) can't be computed. Amber/--warn is a *meaning* signal here
  // ("we don't know what this cost"), so saturated color is allowed — it's the
  // cost signal, distinct from the local/cloud/blocked routing signal. Maps to
  // `.flying-blind-banner`.
  import type { Snippet } from "svelte";

  interface Props {
    /** Headline; defaults to a generic "flying blind" statement. */
    title?: string;
    /** Explanatory body, e.g. "This provider doesn't report per-turn cost yet." */
    children?: Snippet;
  }

  let { title = "Flying blind on cost", children }: Props = $props();
</script>

<div
  role="status"
  class="mb-[14px] flex items-start gap-[11px] rounded-[var(--r)] border bg-warn-soft px-[13px] py-[11px] border-[color-mix(in_srgb,var(--warn)_30%,transparent)]"
>
  <span class="mt-px shrink-0 text-warn">
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
      <path d="M12 3.5 21.5 20h-19L12 3.5Z" stroke-linejoin="round" />
      <path d="M12 10v4" stroke-linecap="round" />
      <circle cx="12" cy="17.3" r="0.9" fill="currentColor" stroke="none" />
    </svg>
  </span>
  <div class="min-w-0 flex-1">
    <div class="mb-[2px] text-[12.5px] font-semibold text-text">{title}</div>
    {#if children}
      <div class="text-[12px] leading-[1.5] text-text-2">{@render children()}</div>
    {/if}
  </div>
  <span
    class="shrink-0 whitespace-nowrap rounded-[var(--r-sm)] px-[7px] py-[2px] text-[10px] font-[650] uppercase tracking-[0.04em] text-warn bg-[color-mix(in_srgb,var(--warn)_22%,transparent)]"
  >
    flying blind
  </span>
</div>
