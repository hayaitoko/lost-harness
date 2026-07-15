<script lang="ts" module>
  import type { Snippet } from "svelte";

  /** A single actionable row in a `ContextMenu`. */
  export interface ContextMenuItem {
    id: string;
    label: string;
    /** Optional leading icon, supplied as a Snippet so callers can inline SVG. */
    icon?: Snippet;
    /** Destructive action (e.g. Delete) — tints red. */
    danger?: boolean;
  }
</script>

<script lang="ts">
  // Fixed-position right-click menu (sidebar conversation rows, etc). Position and
  // visibility are fully controlled by the caller. Maps to `.ctx-menu` / `.menu-opt`.
  interface Props {
    /** Rows in order; use the string `'separator'` for a `.menu-sep` divider. */
    items: (ContextMenuItem | "separator")[];
    /** Fixed-position coordinates, e.g. from the triggering right-click event. */
    x: number;
    y: number;
    open: boolean;
    onselect?: (id: string) => void;
  }

  let { items, x, y, open, onselect }: Props = $props();
</script>

{#if open}
  <div
    class="fixed z-[95] min-w-[192px] rounded-[var(--r)] border border-border-strong bg-surface p-[5px] shadow-[var(--shadow-pop)]"
    style="left: {x}px; top: {y}px;"
  >
    {#each items as item, i (item === "separator" ? "sep-" + i : item.id)}
      {#if item === "separator"}
        <div class="my-[2px] h-px bg-border"></div>
      {:else}
        <button
          type="button"
          onclick={() => onselect?.(item.id)}
          class="flex w-full items-center gap-[9px] border-0 bg-transparent px-3 py-2 text-left text-[12.5px] hover:bg-surface-hover
            {item.danger ? 'text-blocked' : 'text-text'}"
        >
          {#if item.icon}
            <span
              class="grid w-4 place-items-center {item.danger
                ? 'text-blocked'
                : 'text-text-3'}"
            >
              {@render item.icon()}
            </span>
          {/if}
          {item.label}
        </button>
      {/if}
    {/each}
  </div>
{/if}
