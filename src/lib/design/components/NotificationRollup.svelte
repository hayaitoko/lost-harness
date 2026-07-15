<script lang="ts" module>
  export interface NotificationRollupItem {
    /** Short description of the missed alert, e.g. "Held an email draft from sending". */
    label: string;
    /** Drives the item's dot color: `kept`→local (green), `stop`→blocked (red), `tool`→neutral. */
    kind?: "kept" | "stop" | "tool";
  }
</script>

<script lang="ts">
  // Collapsed "…and N more while you were away" summary — the rollup shown
  // instead of a flood of individual privacy/tool alerts when the app wasn't
  // focused. Stays grayscale chrome; expanding reveals a short list where the
  // only color is each item's RouteDot (kept = local green, stop = blocked red,
  // tool = neutral). Maps to `.notif-rollup` (composes `.log-row`).
  import type { Route } from "../types";
  import RouteDot from "./RouteDot.svelte";
  import IconButton from "./IconButton.svelte";

  interface Props {
    /** Total number of missed alerts this banner summarizes. */
    count: number;
    /** A short preview list shown once expanded — not necessarily all `count` items. */
    items?: NotificationRollupItem[];
    /** Fired when the user expands the rollup to see the list. */
    onopen?: () => void;
    /** Fired when the user dismisses the banner entirely (the close ×). Omit to hide the control. */
    ondismiss?: () => void;
  }

  let { count, items = [], onopen, ondismiss }: Props = $props();

  const KIND_ROUTE: Record<
    NonNullable<NotificationRollupItem["kind"]>,
    Route | "auto"
  > = {
    kept: "local",
    stop: "blocked",
    tool: "auto",
  };

  let expanded = $state(false);

  function toggle() {
    const next = !expanded;
    expanded = next;
    if (next) onopen?.();
  }
</script>

<div
  class="overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow)]"
>
  <div class="flex items-center gap-[9px] py-[6px] pl-[13px] pr-[6px]">
    <span class="grid shrink-0 place-items-center text-text-3">
      <svg
        width="14"
        height="14"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
      >
        <path d="M6 10a6 6 0 0 1 12 0c0 4 1.5 5.5 2 6H4c.5-.5 2-2 2-6Z" />
        <path d="M10 19a2 2 0 0 0 4 0" />
      </svg>
    </span>
    <button
      type="button"
      class="min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap border-0 bg-transparent py-[5px] text-left text-[12.5px] text-text-2 transition-colors duration-100 hover:text-text"
      onclick={toggle}
    >
      …and <b class="font-[650] text-text">{count}</b> more while you were away
    </button>
    <div class="flex shrink-0 items-center gap-[2px]">
      <IconButton
        label={expanded ? "Collapse" : "Expand"}
        active={expanded}
        onclick={toggle}
      >
        <svg
          width="11"
          height="11"
          viewBox="0 0 12 12"
          fill="none"
          stroke="currentColor"
          stroke-width="1.6"
        >
          <path d={expanded ? "M2 7.5 6 4l4 3.5" : "M2 4.5 6 8l4-3.5"} />
        </svg>
      </IconButton>
      {#if ondismiss}
        <IconButton label="Dismiss" onclick={ondismiss}>
          <svg
            width="12"
            height="12"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
          >
            <path d="M6 6l12 12M18 6 6 18" />
          </svg>
        </IconButton>
      {/if}
    </div>
  </div>

  {#if expanded && items.length > 0}
    <div
      class="max-h-[240px] overflow-y-auto border-t border-border px-[13px] py-[2px]"
    >
      {#each items as item, i (i)}
        <div
          class="flex items-start gap-[10px] border-b border-border py-[11px] last:border-b-0"
        >
          <span class="mt-[6px]">
            <RouteDot route={KIND_ROUTE[item.kind ?? "tool"]} />
          </span>
          <div class="min-w-0 flex-1">
            <div class="mb-[1px] text-[13px] font-[550]">{item.label}</div>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>
