<script lang="ts">
  // Square icon-only button (top bar, composer, panel headers) — the design
  // system's `.icon-btn`. The hover tooltip is the `[data-tip]::after`
  // pseudo-element, which Tailwind can't express, so it lives in scoped CSS.
  import type { Snippet } from "svelte";

  interface Props {
    /** Accessible label — also drives the hover tooltip. */
    label: string;
    active?: boolean;
    disabled?: boolean;
    onclick?: () => void;
    children: Snippet;
  }

  let { label, active = false, disabled = false, onclick, children }: Props = $props();

  const base =
    "relative grid h-[30px] w-[30px] place-items-center rounded-[var(--r)] border border-transparent transition-[background-color,color] duration-100";
  const state = $derived(
    disabled
      ? "cursor-not-allowed text-text-3 opacity-50"
      : active
        ? "bg-surface-2 text-text"
        : "bg-transparent text-text-3 hover:bg-surface-hover hover:text-text-2",
  );
</script>

<button
  type="button"
  class="{base} {state}"
  aria-label={label}
  data-tip={label}
  {disabled}
  {onclick}
>
  {@render children()}
</button>

<style>
  [data-tip]::after {
    content: attr(data-tip);
    position: absolute;
    left: 50%;
    top: calc(100% + 7px);
    transform: translateX(-50%);
    background: var(--surface-2);
    color: var(--text);
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    padding: 4px 8px;
    font-size: 11px;
    font-weight: 500;
    white-space: nowrap;
    box-shadow: var(--shadow-pop);
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.12s 0.3s;
    z-index: 200;
  }
  [data-tip]:hover::after {
    opacity: 1;
  }
</style>
