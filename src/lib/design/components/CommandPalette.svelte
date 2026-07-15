<script lang="ts" module>
  import type { Snippet } from "svelte";

  /** A single option in a `CommandPalette`, grouped under a `.pal-group` header. */
  export interface CommandPaletteItem {
    group: string;
    label: string;
    /** Right-aligned hint text (e.g. a keyboard shortcut like "⌘N"). */
    hint?: string;
    /** Optional custom icon; falls back to the default arrow glyph. */
    icon?: Snippet;
  }
</script>

<script lang="ts">
  // The `⌘K` command palette card — a search input over a grouped, filterable
  // option list. Renders only the `.palette` card itself, not the full-screen
  // `.overlay` backdrop; the caller owns the overlay and its open/close state.
  interface Props {
    /** Options in display order; consecutive items sharing a `group` render under one header. */
    items: CommandPaletteItem[];
    placeholder?: string;
    onselect?: (label: string) => void;
  }

  let { items, placeholder, onselect }: Props = $props();

  let query = $state("");

  let filtered = $derived.by(() => {
    const q = query.trim().toLowerCase();
    return items.filter(
      (item) =>
        !q ||
        item.label.toLowerCase().includes(q) ||
        item.group.toLowerCase().includes(q),
    );
  });

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === "Enter" && filtered.length > 0) {
      e.preventDefault();
      onselect?.(filtered[0].label);
    }
  }

  // Whether the item at index `i` starts a new group header.
  function showGroup(i: number): boolean {
    return i === 0 || filtered[i].group !== filtered[i - 1].group;
  }
</script>

<div
  role="dialog"
  aria-modal="true"
  aria-label="Command palette"
  class="w-[min(560px,94vw)] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-bg shadow-[var(--shadow-pop)]"
>
  <div class="flex items-center gap-[10px] border-b border-border px-[15px] py-[13px]">
    <svg
      class="text-text-3"
      width="18"
      height="18"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
    >
      <circle cx="11" cy="11" r="7" />
      <path d="m20 20-3-3" />
    </svg>
    <!-- svelte-ignore a11y_autofocus -->
    <input
      bind:value={query}
      onkeydown={handleKeyDown}
      placeholder={placeholder ?? "Type a command or search…"}
      autofocus
      class="flex-1 border-0 bg-transparent text-[15px] text-text outline-none placeholder:text-text-3"
    />
  </div>

  <div class="max-h-[340px] overflow-y-auto p-[6px]">
    {#if filtered.length === 0}
      <div
        class="px-[10px] pb-[4px] pt-[9px] text-[10px] font-[650] uppercase tracking-[.05em] text-text-3"
      >
        No matches
      </div>
    {/if}
    {#each filtered as item, i (`${item.group}-${item.label}-${i}`)}
      {#if showGroup(i)}
        <div
          class="px-[10px] pb-[4px] pt-[9px] text-[10px] font-[650] uppercase tracking-[.05em] text-text-3"
        >
          {item.group}
        </div>
      {/if}
      <button
        type="button"
        onclick={() => onselect?.(item.label)}
        class="flex w-full items-center gap-[11px] rounded-[var(--r)] border-0 bg-transparent px-[11px] py-[9px] text-left text-[13px] text-text transition-[background] duration-[60ms] hover:bg-surface-hover
          {i === 0 ? 'bg-surface-hover' : ''}"
      >
        <span class="grid w-[18px] place-items-center text-text-3">
          {#if item.icon}
            {@render item.icon()}
          {:else}
            <svg
              width="15"
              height="15"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
            >
              <path d="M5 12h14M13 6l6 6-6 6" />
            </svg>
          {/if}
        </span>
        {item.label}
        {#if item.hint}
          <span class="ml-auto text-[10.5px] text-text-3">{item.hint}</span>
        {/if}
      </button>
    {/each}
  </div>
</div>
