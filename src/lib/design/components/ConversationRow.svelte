<script lang="ts">
  // A row in the sidebar conversation list. `.conv` — a full-width button with a
  // routing dot, title, and optional meta. `active` gets the surface-2 fill and
  // an accent bar on the left edge (the `.active::before` knob → `before:` utils).
  import type { Route } from "../types";
  import RouteDot from "./RouteDot.svelte";

  interface Props {
    title: string;
    /** The conversation's disposition dot. */
    route: Route | "auto";
    /** Timestamp / meta text (e.g. "2m"). */
    meta?: string;
    active?: boolean;
    onclick?: () => void;
    /** Right-click for the context menu (Pin / Rename / …). */
    oncontextmenu?: (e: MouseEvent) => void;
  }

  let { title, route, meta, active = false, onclick, oncontextmenu }: Props =
    $props();

  const base =
    "relative flex w-full items-center gap-[9px] rounded-[var(--r-sm)] px-[10px] py-[7px] text-left transition-[background,color] duration-100";
</script>

<button
  type="button"
  {onclick}
  {oncontextmenu}
  class="{base} {active
    ? 'bg-surface-2 text-text before:absolute before:left-0 before:top-[7px] before:bottom-[7px] before:w-0.5 before:rounded-full before:bg-accent'
    : 'bg-transparent text-text-2 hover:bg-surface-hover hover:text-text'}"
>
  <RouteDot {route} />
  <span
    class="flex-1 overflow-hidden text-ellipsis whitespace-nowrap text-[12.5px]"
    >{title}</span
  >
  {#if meta}
    <span class="shrink-0 text-[10.5px] text-text-3">{meta}</span>
  {/if}
</button>
