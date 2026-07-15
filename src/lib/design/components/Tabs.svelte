<script lang="ts">
  // Generic tab-rail primitive. Grayscale chrome only — the active tab gets
  // --surface-hover + --text + heavier weight (same treatment the hand-rolled
  // Settings submenu used), as a reusable component in both orientations.
  // Maps to `.tab-rail` (+ vertical/horizontal) with `.tab-item` children (`.on`
  // when selected). Never carries the routing signal — compose RouteDot/
  // RoutingBadge in the `icon` slot if a tab must communicate local/cloud/blocked.
  import type { Snippet } from "svelte";

  interface TabItem {
    id: string;
    label: string;
    /** Optional leading glyph, e.g. a small SVG icon. Purely decorative. */
    icon?: Snippet;
  }
  interface Props {
    items: TabItem[];
    /** The currently selected tab's id. */
    value: string;
    onchange?: (id: string) => void;
    /** 'vertical' is the Settings submenu; 'horizontal' is a panel tab rail. */
    orientation?: "horizontal" | "vertical";
  }

  let { items, value, onchange, orientation = "horizontal" }: Props = $props();

  const railBase = "flex gap-0.5";
  const railOrient = {
    vertical: "flex-col items-stretch",
    horizontal: "flex-row flex-wrap items-center border-b border-border",
  };

  // Shared active/inactive item treatment.
  const itemBase =
    "flex items-center gap-2 border-0 bg-transparent font-inherit text-left cursor-pointer transition-[background-color,color] duration-100 text-[12.5px]";
</script>

<div class="{railBase} {railOrient[orientation]}" role="tablist" aria-orientation={orientation}>
  {#each items as item (item.id)}
    <button
      type="button"
      role="tab"
      aria-selected={value === item.id}
      onclick={() => onchange?.(item.id)}
      class="{itemBase} {orientation === 'horizontal'
        ? 'px-3 py-[7px] rounded-t-[var(--r-sm)]'
        : 'px-[10px] py-[7px] rounded-[var(--r-sm)]'} {value === item.id
        ? 'bg-surface-hover text-text font-semibold [&_.ti-ico]:text-text-2'
        : 'text-text-2 font-medium hover:bg-surface-hover hover:text-text'}"
    >
      {#if item.icon}
        <span class="ti-ico grid place-items-center shrink-0 text-text-3">
          {@render item.icon()}
        </span>
      {/if}
      {item.label}
    </button>
  {/each}
</div>
